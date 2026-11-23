//! Read declared package relationships and payload evidence from a .nipkg.

#[cfg(windows)]
#[path = "nipkg_nested.rs"]
mod nested;
#[path = "nipkg_payload.rs"]
mod payload;

use anyhow::{Context, Result, bail, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

pub(crate) use payload::ReadKind as PayloadReadKind;

const CONTROL_ARCHIVE_LIMIT: u64 = 32 * 1024 * 1024;

pub struct Control {
    pub name: String,
    pub version: String,
    pub architecture: Option<String>,
    pub relationships: BTreeMap<String, String>,
    pub dependencies: BTreeMap<String, Dependency>,
}

#[derive(Default)]
pub struct Dependency {
    pub required: bool,
    pub properties: BTreeSet<(&'static str, String)>,
}

/// A path in the package TAR, or relative to a specific nested installer.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct PackedPath {
    pub container: Option<String>,
    pub path: String,
}

impl PackedPath {
    pub(crate) fn display(&self) -> String {
        self.container.as_ref().map_or_else(
            || self.path.clone(),
            |container| format!("{container}!/{}", self.path),
        )
    }
}

#[derive(Default)]
pub(crate) struct PackedPayload {
    pub files: BTreeMap<PackedPath, Vec<u8>>,
    pub issues: Vec<String>,
    total_size: u64,
}

impl PackedPayload {
    fn add(&mut self, container: Option<String>, path: &str, bytes: Vec<u8>) -> Result<()> {
        let path = payload::normalize_path(path.as_bytes())?;
        let key = PackedPath { container, path };
        ensure!(
            !self.files.contains_key(&key),
            "duplicate packed library payload path {:?}",
            key.display()
        );
        ensure!(
            self.files.len() < payload::PPL_COUNT_LIMIT,
            "too many packed libraries in the payload"
        );
        ensure!(
            bytes.len() as u64 <= payload::PPL_FILE_LIMIT,
            "packed library {:?} exceeds the payload file limit",
            key.display()
        );
        self.total_size = self
            .total_size
            .checked_add(bytes.len() as u64)
            .filter(|size| *size <= payload::PPL_TOTAL_LIMIT)
            .context("packed library payload exceeds the total size limit")?;
        self.files.insert(key, bytes);
        Ok(())
    }
}

#[derive(Debug, Default)]
pub(crate) struct PayloadInventory {
    pub files: BTreeMap<String, PayloadFile>,
    pub issues: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct PayloadFile {
    pub sha256: String,
    pub size: u64,
    pub nested_archive: bool,
}

pub(crate) struct Package {
    pub control: Control,
    pub packed: PackedPayload,
    pub inventory: PayloadInventory,
}

/// Read control metadata and collect the requested evidence in one payload pass.
pub(crate) fn read(path: &Path, kind: PayloadReadKind) -> Result<Package> {
    let (mut reader, control, payload) = open_archive(path)?;
    let control = decode_control(&mut reader, control)?;
    let (packed, inventory) = match payload {
        Some(member) => payload::read(member_reader(&mut reader, member)?, kind)
            .context("scanning .nipkg data tar")?,
        None => (
            PackedPayload::default(),
            PayloadInventory {
                issues: vec![".nipkg has no data archive; direct payload was not inspected".into()],
                ..PayloadInventory::default()
            },
        ),
    };
    Ok(Package {
        control,
        packed,
        inventory,
    })
}

fn open_archive(path: &Path) -> Result<(BufReader<File>, ArchiveMember, Option<ArchiveMember>)> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut reader = BufReader::new(file);
    let (control, payload) = archive_members(&mut reader, len)?;
    Ok((reader, control, payload))
}

fn decode_control(reader: &mut (impl Read + Seek), control: ArchiveMember) -> Result<Control> {
    ensure!(
        control.size <= CONTROL_ARCHIVE_LIMIT,
        "control archive is too large"
    );
    let mut tar = Vec::new();
    member_reader(reader, control)?
        .take(CONTROL_ARCHIVE_LIMIT + 1)
        .read_to_end(&mut tar)
        .context("reading control archive")?;
    ensure!(
        tar.len() as u64 <= CONTROL_ARCHIVE_LIMIT,
        "decompressed control archive is too large"
    );
    parse_control(std::str::from_utf8(control_file(&tar)?).context("control file is not UTF-8")?)
}

#[derive(Clone, Copy)]
struct ArchiveMember {
    start: u64,
    size: u64,
    gzip: bool,
}

fn member_reader(
    reader: &mut (impl Read + Seek),
    member: ArchiveMember,
) -> Result<Box<dyn Read + '_>> {
    reader.seek(SeekFrom::Start(member.start))?;
    let data = reader.take(member.size);
    if member.gzip {
        Ok(Box::new(flate2::read::MultiGzDecoder::new(data)))
    } else {
        Ok(Box::new(data))
    }
}

fn archive_members(
    reader: &mut (impl Read + Seek),
    len: u64,
) -> Result<(ArchiveMember, Option<ArchiveMember>)> {
    let mut magic = [0; 8];
    reader
        .read_exact(&mut magic)
        .context("reading .nipkg signature")?;
    ensure!(&magic == b"!<arch>\n", "not an ar-format .nipkg archive");
    let mut control = None;
    let mut payload = None;
    while reader.stream_position()? < len {
        let mut header = [0; 60];
        reader
            .read_exact(&mut header)
            .context("reading .nipkg member header")?;
        ensure!(&header[58..] == b"\x60\n", "invalid .nipkg member header");
        let name = std::str::from_utf8(&header[..16])?
            .trim_end()
            .trim_end_matches('/');
        let size = std::str::from_utf8(&header[48..58])?
            .trim()
            .parse::<u64>()
            .context("invalid .nipkg member size")?;
        let start = reader.stream_position()?;
        let end = start
            .checked_add(size)
            .filter(|end| *end <= len)
            .context("truncated .nipkg member")?;
        let member = ArchiveMember {
            start,
            size,
            gzip: name.ends_with(".gz"),
        };
        match name {
            "control.tar.gz" | "control.tar" => ensure!(
                control.replace(member).is_none(),
                "duplicate .nipkg control archive"
            ),
            "data.tar.gz" | "data.tar" => ensure!(
                payload.replace(member).is_none(),
                "duplicate .nipkg data archive"
            ),
            name if name.starts_with("control.tar.") || name.starts_with("data.tar.") => {
                bail!("unsupported .nipkg member {name:?}")
            }
            _ => {}
        }
        let next = end
            .checked_add(size % 2)
            .filter(|next| *next <= len)
            .context("truncated .nipkg member padding")?;
        reader.seek(SeekFrom::Start(next))?;
    }
    Ok((control.context("no control archive in .nipkg")?, payload))
}

fn control_file(tar: &[u8]) -> Result<&[u8]> {
    let mut rest = tar;
    while let Some(header) = rest.get(..512) {
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        let size = usize::try_from(
            payload::tar_member_size(header).context("reading control tar header")?,
        )
        .context("control tar member size is too large")?;
        let body = rest[512..]
            .get(..size)
            .context("truncated control tar member")?;
        let name = header[..100]
            .split(|byte| *byte == 0)
            .next()
            .unwrap_or_default();
        let at_root = header.get(345) == Some(&0);
        if matches!(name, b"control" | b"./control") && at_root && matches!(header[156], 0 | b'0') {
            return Ok(body);
        }
        let padded = size
            .checked_add((512 - size % 512) % 512)
            .context("invalid control tar member size")?;
        rest = rest[512..]
            .get(padded..)
            .context("truncated control tar member padding")?;
    }
    bail!("no control file in control archive")
}

fn parse_control(text: &str) -> Result<Control> {
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    let mut current = None;
    for line in text.trim_start_matches('\u{feff}').lines() {
        if line.trim().is_empty() {
            current = None;
            continue;
        }
        if line.starts_with([' ', '\t']) {
            let key = current
                .as_ref()
                .context("control continuation has no field")?;
            let value = fields.get_mut(key).expect("current field was inserted");
            value.push(' ');
            value.push_str(line.trim());
            continue;
        }
        let (key, value) = line
            .split_once(':')
            .context("expected a control field and colon")?;
        let key = key.to_ascii_lowercase();
        ensure!(
            fields
                .insert(key.clone(), value.trim().to_owned())
                .is_none(),
            "duplicate control field {key:?}"
        );
        current = Some(key);
    }
    let mut required_field = |key: &str| {
        fields
            .remove(key)
            .filter(|value| !value.is_empty())
            .with_context(|| format!("control file is missing {key:?}"))
    };
    let name = required_field("package")?;
    let version = required_field("version")?;
    let architecture = fields
        .remove("architecture")
        .filter(|value| !value.is_empty());
    let mut dependencies = BTreeMap::new();
    let mut relationships = BTreeMap::new();

    for (key, label) in [
        ("depends", "Required Dependency"),
        ("recommends", "Recommended Dependency"),
        ("suggests", "Suggested Dependency"),
    ] {
        if let Some(value) = fields.remove(key).filter(|value| !value.is_empty()) {
            for clause in value.split(',').map(str::trim) {
                let alternative = clause.contains('|');
                for choice in clause.split('|').map(str::trim) {
                    let (package, constraint) = parse_choice(choice)
                        .with_context(|| format!("invalid {key} clause {clause:?}"))?;
                    let dependency: &mut Dependency =
                        dependencies.entry(package.to_owned()).or_default();
                    dependency.required |= key == "depends" && !alternative;
                    let relationship = if alternative && key == "depends" {
                        "Alternative Dependency"
                    } else {
                        label
                    };
                    dependency
                        .properties
                        .insert(("relationship", relationship.into()));
                    if alternative {
                        dependency.properties.insert(("alternative", clause.into()));
                    }
                    if let Some((operator, version)) = constraint {
                        dependency
                            .properties
                            .insert(("version-requirement", format!("{operator} {version}")));
                        match operator {
                            ">=" | ">>" => {
                                dependency
                                    .properties
                                    .insert(("min-version", version.into()));
                                let mode = if operator == ">=" {
                                    "Inclusive"
                                } else {
                                    "Exclusive"
                                };
                                dependency
                                    .properties
                                    .insert(("min-version-type", mode.into()));
                            }
                            "<=" | "<<" => {
                                dependency
                                    .properties
                                    .insert(("max-version", version.into()));
                                dependency.properties.insert((
                                    "max-version-inclusive",
                                    (operator == "<=").to_string(),
                                ));
                            }
                            _ => {}
                        }
                    }
                }
            }
            relationships.insert(key.into(), value);
        }
    }
    Ok(Control {
        name,
        version,
        architecture,
        relationships,
        dependencies,
    })
}

type Constraint<'a> = Option<(&'a str, &'a str)>;

pub(crate) fn parse_choice(choice: &str) -> Result<(&str, Constraint<'_>)> {
    let (name, constraint) = match choice.split_once('(') {
        Some((name, range)) => {
            let range = range
                .strip_suffix(')')
                .context("missing closing parenthesis")?
                .trim();
            let (operator, version) = ["<<", "<=", ">=", ">>", "!=", "="]
                .into_iter()
                .find_map(|operator| {
                    range
                        .strip_prefix(operator)
                        .map(|version| (operator, version.trim()))
                })
                .context("unsupported version operator")?;
            ensure!(
                !version.is_empty()
                    && !version
                        .chars()
                        .any(|c| c.is_whitespace() || matches!(c, '(' | ')')),
                "invalid version"
            );
            (name.trim(), Some((operator, version)))
        }
        None => (choice, None),
    };
    ensure!(
        name.starts_with(|c: char| c.is_ascii_alphanumeric())
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"+.-".contains(&byte)),
        "invalid package name {name:?}"
    );
    Ok((name, constraint))
}
