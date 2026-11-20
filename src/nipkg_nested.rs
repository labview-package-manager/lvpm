//! Connect packed-library rows in MSI databases to CAB members in the same package.

use crate::{nipkg_cab, nipkg_msi};
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::PackedPayload;

const STAGING_LIMIT: u64 = 4 * 1024 * 1024 * 1024;
const STAGED_FILE_LIMIT: usize = 1024;
const VOLUME_LIMIT: usize = 64;
type FileMap = BTreeMap<String, String>;
type Mappings = Vec<(String, FileMap)>;

#[derive(Default)]
pub(super) struct Staged {
    directory: Option<PathBuf>,
    files: BTreeMap<String, PathBuf>,
    bytes: u64,
}

impl Staged {
    pub(super) fn stage(&mut self, path: String, size: u64, data: &mut dyn Read) -> Result<()> {
        self.reserve_bytes(size)?;
        let suffix = if has_extension(&path, ".msi") {
            "msi"
        } else {
            "cab"
        };
        let mut file = self.new_file(path, suffix)?;
        ensure!(
            io::copy(data, &mut file)? == size,
            "truncated NIPKG installer archive"
        );
        Ok(())
    }

    fn new_file(&mut self, path: String, extension: &str) -> Result<File> {
        ensure!(
            !self.files.contains_key(&path),
            "duplicate installer archive path {path:?}"
        );
        ensure!(
            self.files.len() < STAGED_FILE_LIMIT,
            "too many installer archives in NI package"
        );
        if self.directory.is_none() {
            self.directory = Some(create_directory()?);
        }
        let directory = self
            .directory
            .as_ref()
            .expect("temporary directory reserved");
        // A name supplied by the package is only a virtual archive key.
        let destination = directory.join(format!("member-{}.{extension}", self.files.len()));
        let file = File::create_new(&destination)?;
        self.files.insert(path, destination);
        Ok(file)
    }

    fn reserve_bytes(&mut self, size: u64) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(size)
            .filter(|total| *total <= STAGING_LIMIT)
            .context("installer archives exceed the NIPKG temporary storage limit")?;
        Ok(())
    }

    pub(super) fn scan(&self, payload: &mut PackedPayload) -> Result<()> {
        let mut external = BTreeMap::<String, Mappings>::new();
        for (container, path) in self
            .files
            .iter()
            .filter(|(name, _)| has_extension(name, ".msi"))
        {
            let media = match nipkg_msi::read(path) {
                Ok(media) => media,
                Err(error) => {
                    payload.issues.push(format!("{container}: {error:#}"));
                    continue;
                }
            };
            let mut embedded = BTreeMap::new();
            for (cabinet, files) in media {
                if let Some(stream) = cabinet.strip_prefix('#') {
                    embedded.insert(stream.to_owned(), files);
                } else if let Some(name) = self.find_cabinet(container, &cabinet)? {
                    external
                        .entry(name.to_owned())
                        .or_default()
                        .push((container.clone(), files));
                } else {
                    payload
                        .issues
                        .push(format!("{container}: missing CAB {cabinet:?}"));
                }
            }
            scan_embedded(container, path, embedded, payload);
        }

        let mut pending = self
            .files
            .keys()
            .filter(|name| !has_extension(name, ".msi"))
            .cloned()
            .collect::<BTreeSet<_>>();
        while let Some(cabinet) = pending.pop_first() {
            let result = (|| {
                let first = Volume::open(cabinet.clone(), self.files[&cabinet].clone())?;
                let chain = volume_chain(first, |neighbour| {
                    self.find_cabinet(&cabinet, neighbour)?
                        .map(|key| Volume::open(key.to_owned(), self.files[key].clone()))
                        .transpose()
                })?;
                let mut mapped = Vec::new();
                for volume in &chain {
                    pending.remove(&volume.key);
                    mapped.extend(external.remove(&volume.key).unwrap_or_default());
                }
                scan_cabinet(&chain[0].key, &chain, &mapped, true, payload)
            })();
            if let Err(error) = result {
                payload.issues.push(format!("{cabinet}: {error:#}"));
            }
        }
        Ok(())
    }

    fn find_cabinet(&self, source: &str, name: &str) -> Result<Option<&str>> {
        validate_basename(name)?;
        let path = match source.rsplit_once('/') {
            Some((parent, _)) => format!("{parent}/{name}"),
            None => name.to_owned(),
        };
        let mut matches = self
            .files
            .keys()
            .filter(|key| !has_extension(key, ".msi") && key.eq_ignore_ascii_case(&path));
        let first = matches.next().map(String::as_str);
        ensure!(
            matches.next().is_none(),
            "ambiguous CAB {name:?} beside {source:?}"
        );
        Ok(first)
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        for file in self.files.values() {
            let _ = fs::remove_file(file);
        }
        if let Some(directory) = &self.directory {
            let _ = fs::remove_dir(directory);
        }
    }
}

struct Volume {
    key: String,
    path: PathBuf,
    header: nipkg_cab::VolumeHeader,
}

impl Volume {
    fn open(key: String, path: PathBuf) -> Result<Self> {
        let header = nipkg_cab::volume_header(&path)
            .with_context(|| format!("reading CAB volume {key:?}"))?;
        Ok(Self { key, path, header })
    }

    fn name(&self) -> &str {
        self.key
            .rsplit('/')
            .next()
            .expect("a CAB path has a basename")
    }
}

fn volume_chain(
    start: Volume,
    mut load: impl FnMut(&str) -> Result<Option<Volume>>,
) -> Result<Vec<Volume>> {
    let origin = start.key.clone();
    let mut head = start;
    let mut predecessors = 0;
    while let Some(name) = &head.header.previous {
        validate_basename(name)?;
        let Some(previous) = load(name)? else {
            ensure!(
                !head.header.from_previous,
                "missing preceding CAB {name:?} for {:?}",
                head.key
            );
            break;
        };
        // The previous name can point to the start of a spanning file,
        // rather than to the immediately preceding cabinet in the set.
        ensure!(
            previous.header.set_id == head.header.set_id
                && previous.header.index < head.header.index,
            "CAB volume {:?} is not a valid predecessor of {:?}",
            previous.key,
            head.key
        );
        predecessors += 1;
        ensure!(
            predecessors < VOLUME_LIMIT,
            "CAB set exceeds {VOLUME_LIMIT} volumes"
        );
        head = previous;
    }

    ensure!(
        !head.header.from_previous,
        "CAB set starts inside a split file in {:?}",
        head.key
    );
    let mut seen = BTreeSet::from([head.name().to_ascii_lowercase()]);
    let mut chain = vec![head];
    loop {
        let current = chain.last().expect("nonempty chain");
        let Some(name) = &current.header.next else {
            ensure!(
                !current.header.to_next,
                "CAB volume {:?} has a split file with no next volume name",
                current.key
            );
            break;
        };
        validate_basename(name)?;
        let Some(next) = load(name)? else {
            // Some NI cabinets advertise a next disk even though no file
            // continues. A missing advisory link does not block extraction.
            ensure!(
                !current.header.to_next,
                "missing continuation CAB {name:?} for {:?}",
                current.key
            );
            break;
        };
        ensure!(
            current.header.set_id == next.header.set_id
                && current.header.index.checked_add(1) == Some(next.header.index)
                && next
                    .header
                    .previous
                    .as_ref()
                    .is_some_and(|name| seen.contains(&name.to_ascii_lowercase())),
            "CAB volumes {:?} and {:?} do not form a consistent set",
            current.key,
            next.key
        );
        ensure!(
            chain.len() < VOLUME_LIMIT && seen.insert(next.name().to_ascii_lowercase()),
            "CAB volume chain is cyclic or exceeds {VOLUME_LIMIT} volumes"
        );
        chain.push(next);
    }
    ensure!(
        chain.iter().any(|volume| volume.key == origin),
        "CAB set does not contain {origin:?}"
    );
    Ok(chain)
}

fn scan_embedded(
    msi: &str,
    msi_path: &Path,
    mut media: BTreeMap<String, FileMap>,
    payload: &mut PackedPayload,
) {
    let mut staged = Staged::default();
    let initial_names = media.keys().cloned().collect::<Vec<_>>();
    while let Some((name, files)) = media.pop_first() {
        let result = (|| {
            let mut load = |name: &str| {
                validate_basename(name)?;
                let name = initial_names
                    .iter()
                    .find(|entry| entry.eq_ignore_ascii_case(name))
                    .map_or(name, String::as_str);
                if !staged.files.contains_key(name) {
                    let mut output = staged.new_file(name.to_owned(), "cab")?;
                    let loaded = nipkg_msi::read_stream(msi_path, name, &mut output);
                    let size = output.metadata()?.len();
                    drop(output);
                    if let Err(error) = loaded {
                        if let Some(path) = staged.files.remove(name) {
                            let _ = fs::remove_file(path);
                        }
                        if error.to_string() == "MSI embedded cabinet stream is missing" {
                            return Ok(None);
                        }
                        return Err(error)
                            .with_context(|| format!("reading embedded CAB stream {name:?}"));
                    }
                    staged.reserve_bytes(size)?;
                }
                Volume::open(name.to_owned(), staged.files[name].clone()).map(Some)
            };
            let first = load(&name)?.context("missing selected MSI cabinet")?;
            let chain = volume_chain(first, load)?;
            let mut all_files = files;
            for volume in &chain {
                if let Some(files) = media.remove(&volume.key) {
                    for (id, path) in files {
                        ensure!(
                            all_files.insert(id, path).is_none(),
                            "duplicate MSI file identifier in CAB set"
                        );
                    }
                }
            }
            scan_cabinet(msi, &chain, &[(msi.to_owned(), all_files)], false, payload)
        })();
        if let Err(error) = result {
            payload.issues.push(format!("{msi}!/#{name}: {error:#}"));
        }
    }
}

fn scan_cabinet(
    cabinet: &str,
    chain: &[Volume],
    mapped: &[(String, FileMap)],
    standalone: bool,
    payload: &mut PackedPayload,
) -> Result<()> {
    let selected: BTreeSet<_> = mapped
        .iter()
        .flat_map(|(_, files)| files.keys().map(String::as_str))
        .collect();
    let select =
        |entry: &str| selected.contains(entry) || (standalone && has_extension(entry, ".lvlibp"));
    let mut entries = if let [volume] = chain {
        nipkg_cab::read(&volume.path, select)?
    } else {
        let files = chain
            .iter()
            .map(|volume| (volume.name().to_owned(), volume.path.clone()))
            .collect::<Vec<_>>();
        nipkg_cab::read_volumes(&files, select)?
    };
    for (msi, files) in mapped {
        for (file_id, installed) in files {
            if let Some(bytes) = entries.get(file_id) {
                let installed = installed.replace('\\', "/");
                payload.add(Some(msi.to_owned()), &installed, bytes.clone())?;
            } else {
                payload.issues.push(format!(
                    "{msi}: CAB {cabinet:?} missing file {file_id:?} ({installed})"
                ));
            }
        }
    }
    if standalone {
        entries.retain(|name, _| !selected.contains(name.as_str()));
        for (name, bytes) in entries {
            payload.add(Some(cabinet.to_owned()), &name.replace('\\', "/"), bytes)?;
        }
    }
    Ok(())
}

fn validate_basename(name: &str) -> Result<()> {
    ensure!(
        !matches!(name, "" | "." | "..")
            && !name.contains(['\\', '/', ':'])
            && !name.chars().any(char::is_control),
        "unsafe CAB volume name {name:?}"
    );
    Ok(())
}

fn has_extension(path: &str, extension: &str) -> bool {
    path.get(path.len().saturating_sub(extension.len())..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(extension))
}

fn create_directory() -> Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root =
        fs::canonicalize(std::env::temp_dir()).context("locating NIPKG temporary directory")?;
    for _ in 0..100 {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let directory = root.join(format!("lvpm-nipkg-{}-{id}", std::process::id()));
        match fs::create_dir(&directory) {
            Ok(()) => return Ok(directory),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("creating NIPKG temporary directory"),
        }
    }
    anyhow::bail!("could not reserve a NIPKG temporary directory")
}
