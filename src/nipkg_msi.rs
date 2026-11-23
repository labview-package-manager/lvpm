//! Read the Windows Installer file inventory without running installation actions.

use anyhow::{Context, Result, bail, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

const NO_MORE_ITEMS: u32 = 259;
const MORE_DATA: u32 = 234;
// Keep table and embedded stream sizes bounded before copying values.
const ROW_LIMIT: usize = 250_000;
const STRING_LIMIT: u32 = 32_768;
const EVIDENCE_TEXT_LIMIT: usize = 32 * 1024 * 1024;
const CABINET_LIMIT: u32 = 512 * 1024 * 1024;
const ATTRIBUTES_UNCOMPRESSED: i32 = 0x2000;
const ATTRIBUTES_COMPRESSED: i32 = 0x4000;

#[link(name = "msi")]
unsafe extern "system" {
    fn MsiOpenDatabaseW(path: *const u16, persist: *const u16, database: *mut u32) -> u32;
    fn MsiDatabaseIsTablePersistentW(database: u32, table: *const u16) -> i32;
    fn MsiDatabaseOpenViewW(database: u32, query: *const u16, view: *mut u32) -> u32;
    fn MsiViewExecute(view: u32, record: u32) -> u32;
    fn MsiViewFetch(view: u32, record: *mut u32) -> u32;
    fn MsiCreateRecord(fields: u32) -> u32;
    fn MsiRecordSetStringW(record: u32, field: u32, value: *const u16) -> u32;
    fn MsiRecordGetStringW(record: u32, field: u32, buffer: *mut u16, size: *mut u32) -> u32;
    fn MsiRecordGetInteger(record: u32, field: u32) -> i32;
    fn MsiRecordDataSize(record: u32, field: u32) -> u32;
    fn MsiRecordReadStream(record: u32, field: u32, buffer: *mut u8, size: *mut u32) -> u32;
    fn MsiGetSummaryInformationW(
        database: u32,
        path: *const u16,
        count: u32,
        summary: *mut u32,
    ) -> u32;
    fn MsiSummaryInfoGetPropertyW(
        summary: u32,
        property: u32,
        kind: *mut u32,
        integer: *mut i32,
        time: *mut std::ffi::c_void,
        text: *mut u16,
        size: *mut u32,
    ) -> u32;
    fn MsiCloseHandle(handle: u32) -> u32;
}

struct Handle(u32);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: Every Handle uniquely owns the nonzero Windows Installer handle.
        unsafe { MsiCloseHandle(self.0) };
    }
}

impl Handle {
    fn new(call: impl FnOnce(*mut u32) -> u32, action: &str) -> Result<Self> {
        let mut handle = 0;
        check(call(&mut handle), action)?;
        ensure!(handle != 0, "{action}: MSI returned an empty handle");
        Ok(Self(handle))
    }

    fn string(&self, field: u32) -> Result<String> {
        let mut cap = 0;
        let mut empty = [0u16];
        // SAFETY: The buffer is writable and satisfies the API's requirement
        // for a non-null pointer when querying the size of a field.
        let status = unsafe { MsiRecordGetStringW(self.0, field, empty.as_mut_ptr(), &mut cap) };
        if status == 0 && cap == 0 {
            return Ok(String::new());
        }
        if status != MORE_DATA {
            check(status, "measuring MSI record string")?;
        }
        ensure!(cap < STRING_LIMIT, "MSI record string is too large");
        cap += 1;
        let mut buffer = vec![0u16; cap as usize];
        // SAFETY: The buffer is sized in UTF-16 code units, including the terminator.
        check(
            unsafe { MsiRecordGetStringW(self.0, field, buffer.as_mut_ptr(), &mut cap) },
            "reading MSI record string",
        )?;
        let units = buffer
            .get(..cap as usize)
            .context("invalid MSI record string length")?;
        Ok(String::from_utf16(units)?)
    }

    fn integer(&self, field: u32) -> Option<i32> {
        // SAFETY: The handle is live. MSI reports a null integer as INT_MIN.
        let value = unsafe { MsiRecordGetInteger(self.0, field) };
        (value != i32::MIN).then_some(value)
    }

    fn fetch(&self) -> Result<Option<Self>> {
        let mut record = 0;
        // SAFETY: The result slot is writable and the view handle is live.
        let status = unsafe { MsiViewFetch(self.0, &mut record) };
        if status == NO_MORE_ITEMS {
            return Ok(None);
        }
        check(status, "fetching MSI row")?;
        ensure!(record != 0, "fetching MSI row returned no record");
        Ok(Some(Self(record)))
    }
}

fn check(status: u32, action: &str) -> Result<()> {
    if status != 0 {
        let error = std::io::Error::from_raw_os_error(status as i32);
        bail!("{action}: MSI error {status} ({error})");
    }
    Ok(())
}

fn wide(units: impl Iterator<Item = u16>) -> Result<Vec<u16>> {
    let mut text = Vec::new();
    for unit in units {
        ensure!(unit != 0, "embedded null in MSI input");
        text.push(unit);
    }
    text.push(0);
    Ok(text)
}

struct Database(Handle);

impl Database {
    fn open(path: &Path) -> Result<Self> {
        let path = wide(path.as_os_str().encode_wide())?;
        // SAFETY: The path is null terminated. A null persistence argument
        // is MSIDBOPEN_READONLY. This API opens the database only.
        let database = Handle::new(
            |handle| unsafe { MsiOpenDatabaseW(path.as_ptr(), ptr::null(), handle) },
            "opening read-only MSI database",
        )?;
        Ok(Self(database))
    }

    fn has_table(&self, name: &str) -> Result<bool> {
        let name = wide(name.encode_utf16())?;
        // SAFETY: The name is null terminated and the database handle lives.
        match unsafe { MsiDatabaseIsTablePersistentW(self.0.0, name.as_ptr()) } {
            2 => Ok(false), // MSICONDITION_NONE: table does not exist.
            0 | 1 => Ok(true),
            _ => bail!("cannot check whether the MSI has a file table"),
        }
    }

    fn view(&self, sql: &str, parameter: Option<&Handle>) -> Result<Handle> {
        let sql = wide(sql.encode_utf16())?;
        // SAFETY: The query is null terminated and the handle's result slot is writable.
        let view = Handle::new(
            |handle| unsafe { MsiDatabaseOpenViewW(self.0.0, sql.as_ptr(), handle) },
            "preparing MSI table query",
        )?;
        let record = parameter.map_or(0, |record| record.0);
        // SAFETY: The view and optional parameter record live through the call.
        check(
            unsafe { MsiViewExecute(view.0, record) },
            "querying MSI table",
        )?;
        Ok(view)
    }

    fn rows(&self, sql: &str, mut row: impl FnMut(Handle) -> Result<()>) -> Result<()> {
        let view = self.view(sql, None)?;
        let mut count = 0;
        while let Some(record) = view.fetch()? {
            ensure!(count < ROW_LIMIT, "too many MSI table rows");
            row(record)?;
            count += 1;
        }
        Ok(())
    }

    fn compressed_by_default(&self) -> Result<bool> {
        // SAFETY: This opens a read-only summary handle for the live database.
        let summary = Handle::new(
            |handle| unsafe { MsiGetSummaryInformationW(self.0.0, ptr::null(), 0, handle) },
            "opening MSI summary",
        )?;
        let mut kind = 0;
        let mut words = 0;
        // SAFETY: Property 15 (PID_WORDCOUNT) is integer-valued. Optional
        // outputs besides the type and integer are null.
        check(
            unsafe {
                MsiSummaryInfoGetPropertyW(
                    summary.0,
                    15,
                    &mut kind,
                    &mut words,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            },
            "reading MSI source compression flags",
        )?;
        ensure!(
            kind == 0 || kind == 3,
            "unexpected MSI source compression property type"
        );
        Ok(kind == 3 && words & 2 != 0)
    }
}

/// Cabinet name (a leading '#' denotes an embedded cabinet), then cabinet file
/// identifier and its normalized default destination relative to TARGETDIR.
pub(crate) type Inventory = BTreeMap<String, BTreeMap<String, String>>;

struct PackedFile {
    key: String,
    name: String,
    component: String,
    sequence: i32,
    attributes: i32,
}

pub(crate) fn read(path: &Path) -> Result<Inventory> {
    let database = Database::open(path)?;
    if !database.has_table("File")? {
        return Ok(Inventory::new());
    }
    let mut files = Vec::new();
    database.rows(
        "SELECT `File`, `FileName`, `Component_`, `Sequence`, `Attributes` FROM `File`",
        |record| {
            let names = record.string(2)?;
            let name = long_name(&names);
            let attributes = record.integer(5).unwrap_or_default();
            if name
                .rsplit_once('.')
                .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("lvlibp"))
                && attributes & ATTRIBUTES_UNCOMPRESSED == 0
            {
                validate_name(name)?;
                let sequence = record
                    .integer(4)
                    .filter(|seq| *seq > 0)
                    .context("invalid MSI file sequence")?;
                files.push(PackedFile {
                    key: record.string(1)?,
                    name: name.to_owned(),
                    component: record.string(3)?,
                    sequence,
                    attributes,
                });
            }
            Ok(())
        },
    )?;
    if files.is_empty() {
        return Ok(Inventory::new());
    }

    let uses_implicit_compression = files
        .iter()
        .any(|file| file.attributes & ATTRIBUTES_COMPRESSED == 0);
    if uses_implicit_compression && !database.compressed_by_default()? {
        files.retain(|file| file.attributes & ATTRIBUTES_COMPRESSED != 0);
    }
    if files.is_empty() {
        return Ok(Inventory::new());
    }

    let mut media = Vec::new();
    database.rows(
        "SELECT `LastSequence`, `Cabinet` FROM `Media` ORDER BY `DiskId`",
        |record| {
            let sequence = record
                .integer(1)
                .filter(|seq| *seq >= 0)
                .context("invalid MSI media sequence")?;
            ensure!(
                media.last().is_none_or(|(last, _)| sequence >= *last),
                "MSI media sequence is out of order"
            );
            media.push((sequence, record.string(2)?));
            Ok(())
        },
    )?;
    let mut component_directories = BTreeMap::new();
    database.rows(
        "SELECT `Component`, `Directory_` FROM `Component`",
        |record| {
            component_directories.insert(record.string(1)?, record.string(2)?);
            Ok(())
        },
    )?;
    let directories = read_directories(&database, None)?;

    let mut result = Inventory::new();
    for file in files {
        let position = media.partition_point(|(last, _)| *last < file.sequence);
        let (_, cabinet) = media
            .get(position)
            .context("MSI packed library has no source media")?;
        ensure!(
            !cabinet.is_empty(),
            "MSI compressed packed library has no cabinet"
        );
        // The caller receives identifiers, never filesystem paths.
        validate_name(cabinet.strip_prefix('#').unwrap_or(cabinet))?;
        validate_name(&file.key)?;
        let directory = component_directories
            .get(&file.component)
            .context("missing MSI file component")?;
        let path = install_path(directory, &directories, &file.name)?;
        ensure!(
            result
                .entry(cabinet.clone())
                .or_default()
                .insert(file.key, path)
                .is_none(),
            "duplicate MSI packed library file identifier"
        );
    }
    Ok(result)
}

fn read_directories(
    database: &Database,
    mut budget: Option<&mut usize>,
) -> Result<BTreeMap<String, (String, String)>> {
    let mut directories = BTreeMap::new();
    database.rows(
        "SELECT `Directory`, `Directory_Parent`, `DefaultDir` FROM `Directory`",
        |record| {
            let id = record.string(1)?;
            let parent = record.string(2)?;
            let target = record.string(3)?;
            if let Some(budget) = budget.as_deref_mut() {
                charge(budget, [id.as_str(), &parent, &target])?;
            }
            directories.insert(id, (parent, target));
            Ok(())
        },
    )?;
    Ok(directories)
}

fn long_name(name: &str) -> &str {
    name.split_once('|').map_or(name, |(_, long)| long)
}

fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !matches!(name, "" | "." | "..")
            && !name.contains(['/', '\\', ':'])
            && !name.chars().any(char::is_control),
        "invalid MSI path component {name:?}"
    );
    Ok(())
}

fn install_path(
    directory: &str,
    directories: &BTreeMap<String, (String, String)>,
    name: &str,
) -> Result<String> {
    target_path(directory, directories, name, true)?
        .context("MSI packed library has no default destination")
}

fn target_path<'a>(
    mut directory: &'a str,
    directories: &'a BTreeMap<String, (String, String)>,
    name: &'a str,
    allow_symbolic_root: bool,
) -> Result<Option<String>> {
    let mut parts = vec![name];
    let mut seen = 0;
    while directory != "TARGETDIR" {
        ensure!(seen < 128, "MSI directory hierarchy is too deep or cyclic");
        let Some((parent, default)) = directories.get(directory) else {
            if !allow_symbolic_root {
                return Ok(None);
            }
            ensure!(seen != 0, "missing MSI file directory {directory:?}");
            // Some vendor packages refer to a parent supplied at install
            // time. Keep its identifier as a symbolic root in the graph.
            validate_name(directory)?;
            parts.push(directory);
            break;
        };
        if parent.is_empty() || parent == directory {
            if !allow_symbolic_root {
                return Ok(None);
            }
            break;
        }
        let target = long_name(default.split(':').next().unwrap_or_default());
        if target != "." {
            validate_name(target)?;
            parts.push(target);
        }
        seen += 1;
        directory = parent;
    }
    parts.reverse();
    let path = parts.join("/");
    ensure!(path.len() <= 8192, "MSI packed library path is too long");
    Ok(Some(path))
}

/// Package and file declarations read from the installer database.
#[derive(Debug, Default)]
pub(crate) struct Evidence {
    pub name: Option<String>,
    pub version: Option<String>,
    pub files: Vec<FileEvidence>,
    pub issues: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct FileEvidence {
    pub id: String,
    pub name: String,
    /// The authored target path relative to TARGETDIR when derivable.
    pub default_destination: Option<String>,
    pub declared_version: Option<String>,
    pub companion_file: Option<String>,
    pub component: String,
    /// Declared feature membership. A feature's actual selection is unknown.
    pub features: Vec<String>,
    pub condition: Option<String>,
}

impl FileEvidence {
    pub(crate) fn installation(&self) -> &'static str {
        if self.condition.is_some() || !self.features.is_empty() {
            "conditional"
        } else {
            "undetermined"
        }
    }
}

/// Read MSI table declarations only. No cabinet, source path or installer
/// action is opened to verify the declared file bytes or destination.
pub(crate) fn read_evidence(path: &Path) -> Result<Evidence> {
    let database = Database::open(path)?;
    let mut evidence = Evidence::default();
    if database.has_table("Property")? {
        database.rows(
            "SELECT `Property`, `Value` FROM `Property` WHERE `Property` = 'ProductName' OR `Property` = 'ProductVersion'",
            |record| {
                let value = record.string(2)?;
                let value = (!value.trim().is_empty()).then_some(value);
                match record.string(1)?.as_str() {
                    "ProductName" => evidence.name = value,
                    "ProductVersion" => evidence.version = value,
                    _ => {}
                }
                Ok(())
            },
        )?;
    }
    if !database.has_table("File")? {
        return Ok(evidence);
    }

    let mut budget = EVIDENCE_TEXT_LIMIT;
    let files = file_declarations(&database, &mut budget)?;
    if files.is_empty() {
        return Ok(evidence);
    }
    let components = component_declarations(&database, &mut budget)?;
    let features = component_features(&database, &mut budget)?;
    let directories = if database.has_table("Directory")? {
        read_directories(&database, Some(&mut budget))?
    } else {
        BTreeMap::new()
    };

    let mut invalid_names = 0;
    let mut missing_destinations = 0;
    let mut invalid_versions = 0;
    for (id, file) in &files {
        if validate_name(id)
            .and_then(|_| validate_name(&file.name))
            .is_err()
        {
            invalid_names += 1;
            continue;
        }
        let component = components.get(&file.component);
        let destination = component.and_then(|component| {
            target_path(&component.directory, &directories, &file.name, false)
                .ok()
                .flatten()
        });
        missing_destinations += usize::from(destination.is_none());
        let (version, companion) = match version_declaration(&file.version, id, &files) {
            FileVersion::Version(version) => (Some(version), None),
            FileVersion::Companion(id) => (None, Some(id)),
            FileVersion::Unversioned => (None, None),
            FileVersion::Invalid => {
                invalid_versions += 1;
                (None, None)
            }
        };
        let feature_ids = features.get(&file.component);
        let condition = component.and_then(|component| component.condition.as_deref());
        let text = [id.as_str(), &file.name, &file.component]
            .into_iter()
            .chain(destination.as_deref())
            .chain(version)
            .chain(companion)
            .chain(condition)
            .chain(feature_ids.into_iter().flatten().map(String::as_str));
        charge(&mut budget, text)?;
        evidence.files.push(FileEvidence {
            id: id.clone(),
            name: file.name.clone(),
            default_destination: destination,
            declared_version: version.map(str::to_owned),
            companion_file: companion.map(str::to_owned),
            component: file.component.clone(),
            features: feature_ids.into_iter().flatten().cloned().collect(),
            condition: condition.map(str::to_owned),
        });
    }
    for (count, issue) in [
        (
            invalid_names,
            "file declarations had an invalid identifier or filename and were skipped",
        ),
        (
            missing_destinations,
            "file declarations had an undetermined default destination",
        ),
        (
            invalid_versions,
            "file declarations had an unrecognized version or companion reference",
        ),
    ] {
        if count != 0 {
            evidence.issues.push(format!("{count} MSI {issue}"));
        }
    }
    Ok(evidence)
}

struct FileDeclaration {
    name: String,
    component: String,
    version: String,
}

fn file_declarations(
    database: &Database,
    budget: &mut usize,
) -> Result<BTreeMap<String, FileDeclaration>> {
    let has_version = optional_column(database, "File", "Version")?;
    let query = if has_version {
        "SELECT `File`, `FileName`, `Component_`, `Version` FROM `File`"
    } else {
        "SELECT `File`, `FileName`, `Component_` FROM `File`"
    };
    let mut files = BTreeMap::new();
    database.rows(query, |record| {
        let id = record.string(1)?;
        let name = long_name(&record.string(2)?).to_owned();
        let component = record.string(3)?;
        let version = if has_version {
            record.string(4)?
        } else {
            String::new()
        };
        charge(budget, [id.as_str(), &name, &component, &version])?;
        ensure!(
            files
                .insert(
                    id,
                    FileDeclaration {
                        name,
                        component,
                        version
                    }
                )
                .is_none(),
            "duplicate MSI file declaration"
        );
        Ok(())
    })?;
    Ok(files)
}

struct ComponentDeclaration {
    directory: String,
    condition: Option<String>,
}

fn component_declarations(
    database: &Database,
    budget: &mut usize,
) -> Result<BTreeMap<String, ComponentDeclaration>> {
    let mut components = BTreeMap::new();
    if !database.has_table("Component")? {
        return Ok(components);
    }
    let has_condition = optional_column(database, "Component", "Condition")?;
    let query = if has_condition {
        "SELECT `Component`, `Directory_`, `Condition` FROM `Component`"
    } else {
        "SELECT `Component`, `Directory_` FROM `Component`"
    };
    database.rows(query, |record| {
        let id = record.string(1)?;
        let directory = record.string(2)?;
        let condition = if has_condition {
            let value = record.string(3)?;
            (!value.trim().is_empty()).then_some(value)
        } else {
            None
        };
        charge(
            budget,
            [id.as_str(), &directory]
                .into_iter()
                .chain(condition.as_deref()),
        )?;
        ensure!(
            components
                .insert(
                    id,
                    ComponentDeclaration {
                        directory,
                        condition
                    }
                )
                .is_none(),
            "duplicate MSI component declaration"
        );
        Ok(())
    })?;
    Ok(components)
}

fn component_features(
    database: &Database,
    budget: &mut usize,
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut features = BTreeMap::<String, BTreeSet<String>>::new();
    if database.has_table("FeatureComponents")? {
        database.rows(
            "SELECT `Component_`, `Feature_` FROM `FeatureComponents`",
            |record| {
                let component = record.string(1)?;
                let feature = record.string(2)?;
                if !component.is_empty() && !feature.is_empty() {
                    charge(budget, [component.as_str(), &feature])?;
                    features.entry(component).or_default().insert(feature);
                }
                Ok(())
            },
        )?;
    }
    Ok(features)
}

fn optional_column(database: &Database, table: &'static str, column: &'static str) -> Result<bool> {
    // Both identifiers are internal constants, not installer or caller input.
    let sql =
        format!("SELECT `Name` FROM `_Columns` WHERE `Table` = '{table}' AND `Name` = '{column}'");
    Ok(database.view(&sql, None)?.fetch()?.is_some())
}

fn charge<'a>(budget: &mut usize, fields: impl IntoIterator<Item = &'a str>) -> Result<()> {
    let bytes = fields.into_iter().map(str::len).sum();
    *budget = budget
        .checked_sub(bytes)
        .context("MSI declaration inventory text limit exceeded")?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum FileVersion<'a> {
    Version(&'a str),
    Companion(&'a str),
    Unversioned,
    Invalid,
}

fn version_declaration<'a>(
    value: &'a str,
    id: &str,
    file_keys: &BTreeMap<String, FileDeclaration>,
) -> FileVersion<'a> {
    if value.is_empty() {
        return FileVersion::Unversioned;
    }
    if file_keys.contains_key(value) {
        return if value == id {
            FileVersion::Invalid
        } else {
            FileVersion::Companion(value)
        };
    }
    let mut parts = value.split('.');
    if parts.clone().count() <= 4
        && parts.all(|part| {
            !part.is_empty()
                && part.len() <= 5
                && part.bytes().all(|digit| digit.is_ascii_digit())
                && part.parse::<u16>().is_ok()
        })
    {
        return FileVersion::Version(value);
    }
    FileVersion::Invalid
}

/// Copy a cabinet stream selected from the read-only MSI into caller-owned storage.
pub(crate) fn read_stream(path: &Path, name: &str, mut writer: impl Write) -> Result<()> {
    let name = name.strip_prefix('#').unwrap_or(name);
    validate_name(name)?;
    let database = Database::open(path)?;
    let name_utf16 = wide(name.encode_utf16())?;
    // SAFETY: The function creates an independent one-field record.
    let parameter = unsafe { MsiCreateRecord(1) };
    ensure!(
        parameter != 0,
        "creating MSI cabinet query parameter failed"
    );
    let parameter = Handle(parameter);
    // SAFETY: The parameter record is live and the stream name is null terminated.
    check(
        unsafe { MsiRecordSetStringW(parameter.0, 1, name_utf16.as_ptr()) },
        "setting MSI cabinet query parameter",
    )?;
    let view = database.view(
        "SELECT `Name`, `Data` FROM `_Streams` WHERE `Name` = ?",
        Some(&parameter),
    )?;
    let record = view
        .fetch()?
        .context("MSI embedded cabinet stream is missing")?;
    ensure!(
        record.string(1)? == name,
        "MSI embedded cabinet name does not match exactly"
    );
    // SAFETY: The second field in the live record contains the stream.
    let mut remaining = unsafe { MsiRecordDataSize(record.0, 2) };
    ensure!(
        remaining > 0 && remaining <= CABINET_LIMIT,
        "invalid MSI embedded cabinet size"
    );
    let mut buffer = [0u8; 32 * 1024];
    while remaining != 0 {
        let mut size = remaining.min(buffer.len() as u32);
        // SAFETY: The buffer is writable through the requested size. MSI advances its stream cursor.
        check(
            unsafe { MsiRecordReadStream(record.0, 2, buffer.as_mut_ptr(), &mut size) },
            "reading MSI embedded cabinet bytes",
        )?;
        ensure!(
            size > 0 && size <= remaining && size as usize <= buffer.len(),
            "invalid MSI cabinet stream length"
        );
        writer.write_all(&buffer[..size as usize])?;
        remaining -= size;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installer_target_paths_use_long_destination_names() {
        let directories = BTreeMap::from([
            (
                "Application".into(),
                ("TARGETDIR".into(), "APP~1|Application Files:source".into()),
            ),
            ("Support".into(), ("Application".into(), ".:Bin".into())),
        ]);
        assert_eq!(
            install_path("Support", &directories, "example.lvlibp").unwrap(),
            "Application Files/example.lvlibp"
        );
        assert!(validate_name("..").is_err());
        assert!(validate_name("C:\\elsewhere").is_err());
        let external_root = BTreeMap::from([("Bin".into(), ("APPDIR".into(), "bin".into()))]);
        assert_eq!(
            install_path("Bin", &external_root, "example.lvlibp").unwrap(),
            "APPDIR/bin/example.lvlibp"
        );
        assert_eq!(
            target_path("Bin", &external_root, "example.lvlibp", false).unwrap(),
            None
        );
        assert_eq!(
            target_path("Support", &directories, "example.lvlibp", false)
                .unwrap()
                .as_deref(),
            Some("Application Files/example.lvlibp")
        );
        let cycle = BTreeMap::from([
            ("A".into(), ("B".into(), "a".into())),
            ("B".into(), ("A".into(), "b".into())),
        ]);
        assert!(install_path("A", &cycle, "example.lvlibp").is_err());
    }

    #[test]
    fn msi_file_versions_do_not_inherit_the_companion_version() {
        let row = FileDeclaration {
            name: "example.dll".into(),
            component: "Core".into(),
            version: "1.2.3.4".into(),
        };
        let files = BTreeMap::from([("FileA".into(), row)]);
        assert_eq!(
            version_declaration("1.2.3.4", "FileA", &files),
            FileVersion::Version("1.2.3.4")
        );
        assert_eq!(
            version_declaration("FileA", "FileB", &files),
            FileVersion::Companion("FileA")
        );
        assert_eq!(
            version_declaration("", "FileB", &files),
            FileVersion::Unversioned
        );
        assert_eq!(
            version_declaration("FileA", "FileA", &files),
            FileVersion::Invalid
        );
        assert_eq!(
            version_declaration("65536.1", "FileA", &files),
            FileVersion::Invalid
        );
        assert_eq!(
            version_declaration("1..2", "FileA", &files),
            FileVersion::Invalid
        );
        assert_eq!(
            version_declaration("MissingFile", "FileB", &files),
            FileVersion::Invalid
        );
    }
}
