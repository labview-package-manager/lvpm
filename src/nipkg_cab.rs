//! Use Windows SetupAPI to read selected cabinet files without installing anything.

use anyhow::{Context, Result, bail, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::c_void;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::os::windows::ffi::OsStrExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

const MAX_PATH: usize = 260;
const NOTIFY_FILE_IN_CABINET: u32 = 0x11;
const NOTIFY_NEED_NEW_CABINET: u32 = 0x12;
const NOTIFY_FILE_EXTRACTED: u32 = 0x13;
const FILEOP_ABORT: u32 = 0;
const FILEOP_DOIT: u32 = 1;
const FILEOP_SKIP: u32 = 2;
const ERROR_CANCELLED: u32 = 1223;

const FILE_LIMIT: u64 = 128 * 1024 * 1024;
const TOTAL_LIMIT: u64 = 256 * 1024 * 1024;
const ENTRY_LIMIT: usize = 4096;
const VOLUME_LIMIT: usize = 1024;
const SOURCE_LIMIT: u64 = 4 * 1024 * 1024 * 1024;

const PREVIOUS: u16 = 1;
const NEXT: u16 = 2;
const RESERVED: u16 = 4;

// These are the Windows SDK setupapi.h structures and callback ABI.
#[repr(C)]
struct FileInCabinet {
    name: *const u16,
    size: u32,
    win32_error: u32,
    dos_date: u16,
    dos_time: u16,
    dos_attribs: u16,
    target: [u16; MAX_PATH],
}

#[repr(C)]
struct FilePaths {
    target: *const u16,
    source: *const u16,
    win32_error: u32,
    flags: u32,
}

#[repr(C)]
struct CabinetInfo {
    directory: *const u16,
    file: *const u16,
    disk: *const u16,
    set_id: u16,
    index: u16,
}

#[link(name = "setupapi")]
unsafe extern "system" {
    fn SetupIterateCabinetW(
        cabinet: *const u16,
        reserved: u32,
        callback: extern "system" fn(*mut c_void, u32, usize, usize) -> u32,
        context: *mut c_void,
    ) -> i32;
}

#[derive(Debug)]
pub(crate) struct VolumeHeader {
    pub set_id: u16,
    pub index: u16,
    pub previous: Option<String>,
    pub next: Option<String>,
    pub from_previous: bool,
    pub to_next: bool,
}

/// Inspect cabinet linkage and file continuation flags without decompressing entries.
pub(crate) fn volume_header(path: &Path) -> Result<VolumeHeader> {
    let mut reader = BufReader::new(File::open(path)?);
    let length = reader.get_ref().metadata()?.len();
    let mut header = [0; 36];
    reader
        .read_exact(&mut header)
        .context("reading CAB header")?;
    ensure!(&header[..4] == b"MSCF", "invalid cabinet signature");

    let size = u64::from(u32::from_le_bytes(header[8..12].try_into().unwrap()));
    let files = u64::from(u32::from_le_bytes(header[16..20].try_into().unwrap()));
    let count = u16::from_le_bytes(header[28..30].try_into().unwrap());
    let flags = u16::from_le_bytes(header[30..32].try_into().unwrap());
    ensure!(
        36 <= files && files <= size && size <= length,
        "invalid CAB header offsets"
    );
    ensure!(
        flags & !(PREVIOUS | NEXT | RESERVED) == 0,
        "unsupported CAB header flags"
    );

    let mut fields = reader.by_ref().take(files - 36);
    if flags & RESERVED != 0 {
        let mut reserve = [0; 4];
        fields
            .read_exact(&mut reserve)
            .context("reading CAB reserve lengths")?;
        let count = u64::from(u16::from_le_bytes([reserve[0], reserve[1]]));
        ensure!(
            io::copy(&mut fields.by_ref().take(count), &mut io::sink())? == count,
            "truncated CAB header reserve"
        );
    }
    let previous = (flags & PREVIOUS != 0)
        .then(|| link_name(&mut fields))
        .transpose()?;
    let next = (flags & NEXT != 0)
        .then(|| link_name(&mut fields))
        .transpose()?;

    let mut from_previous = false;
    let mut to_next = false;
    if flags & (PREVIOUS | NEXT) != 0 {
        reader.seek(SeekFrom::Start(files))?;
        let mut table = reader.take(size - files);
        for _ in 0..count {
            let mut record = [0; 16];
            table
                .read_exact(&mut record)
                .context("reading CAB file table")?;
            let folder = u16::from_le_bytes(record[8..10].try_into().unwrap());
            from_previous |= matches!(folder, 0xfffd | 0xffff);
            to_next |= matches!(folder, 0xfffe | 0xffff);
            read_field(&mut table).context("reading CAB file name")?;
        }
    }
    Ok(VolumeHeader {
        set_id: u16::from_le_bytes(header[32..34].try_into().unwrap()),
        index: u16::from_le_bytes(header[34..36].try_into().unwrap()),
        previous,
        next,
        from_previous,
        to_next,
    })
}

fn read_field(reader: &mut impl Read) -> Result<Vec<u8>> {
    let mut value = Vec::new();
    for _ in 0..MAX_PATH {
        let mut byte = [0];
        reader.read_exact(&mut byte)?;
        if byte[0] == 0 {
            return Ok(value);
        }
        value.push(byte[0]);
    }
    bail!("CAB field exceeds the Windows filename limit")
}

fn link_name(reader: &mut impl Read) -> Result<String> {
    let name =
        String::from_utf8(read_field(reader)?).context("invalid CAB volume name encoding")?;
    read_field(reader).context("reading CAB volume disk label")?;
    Ok(name)
}

fn safe_volume_name(name: &str) -> bool {
    if name.is_empty()
        || name.len() >= MAX_PATH
        || name.ends_with(['.', ' '])
        || !name.bytes().all(|c| c.is_ascii_graphic() || c == b' ')
        || name.contains(['/', '\\', ':', '<', '>', '"', '|', '?', '*'])
    {
        return false;
    }
    let base = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    !base.is_empty()
        && !matches!(
            base.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        )
        && !((base.starts_with("COM") || base.starts_with("LPT"))
            && base.len() == 4
            && base.as_bytes()[3].is_ascii_digit())
}

/// Read a single cabinet. A continuation needs an explicitly supplied volume set.
pub(crate) fn read(
    path: &Path,
    select: impl FnMut(&str) -> bool,
) -> Result<BTreeMap<String, Vec<u8>>> {
    read_paths(std::slice::from_ref(&path.to_path_buf()), None, select)
}

/// The caller supplies the complete volume set in next-link order. Only
/// links within that set, and never a host path from a header, are followed.
pub(crate) fn read_volumes(
    volumes: &[(String, PathBuf)],
    select: impl FnMut(&str) -> bool,
) -> Result<BTreeMap<String, Vec<u8>>> {
    ensure!(
        !volumes.is_empty() && volumes.len() <= VOLUME_LIMIT,
        "invalid CAB volume count"
    );

    let mut distinct = BTreeSet::new();
    let mut headers: Vec<VolumeHeader> = Vec::new();
    let mut total = 0u64;
    for (index, (name, path)) in volumes.iter().enumerate() {
        ensure!(safe_volume_name(name), "unsafe CAB volume name {name:?}");
        ensure!(
            distinct.insert(name.to_ascii_lowercase()),
            "duplicate CAB volume name {name:?}"
        );
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_file(),
            "CAB volume {name:?} is not a regular file"
        );
        total = total
            .checked_add(metadata.len())
            .filter(|sum| *sum <= SOURCE_LIMIT)
            .context("CAB volumes exceed the combined source size limit")?;
        let current =
            volume_header(path).with_context(|| format!("reading CAB volume {name:?}"))?;
        for linked in current.previous.iter().chain(&current.next) {
            ensure!(
                safe_volume_name(linked),
                "unsafe linked CAB volume name {linked:?}"
            );
        }
        if let Some(previous) = headers.last() {
            ensure!(
                previous.set_id == current.set_id
                    && previous.index.checked_add(1) == Some(current.index)
                    && previous
                        .next
                        .as_ref()
                        .is_some_and(|next| next.eq_ignore_ascii_case(name)),
                "unexpected next CAB volume {name:?}"
            );
        }
        // A previous filename may name the first cabinet containing a split
        // file, rather than the cabinet with the preceding sequence number.
        if current.from_previous {
            let previous = current
                .previous
                .as_ref()
                .context("CAB continuation has no previous volume name")?;
            ensure!(
                volumes[..index]
                    .iter()
                    .any(|(earlier, _)| earlier.eq_ignore_ascii_case(previous)),
                "missing previous CAB volume {previous:?} for {name:?}"
            );
        }
        headers.push(current);
    }
    ensure!(
        !headers.last().is_some_and(|header| header.to_next),
        "missing next CAB volume for {:?}",
        volumes.last().expect("nonempty CAB set").0
    );

    let mut aliases = Scratch::create()?;
    let mut sources = Vec::new();
    for (name, path) in volumes {
        let alias = aliases.dir.join(name);
        aliases.volume_names.push(name.clone());
        if fs::hard_link(path, &alias).is_err() {
            let mut source = File::open(path)?;
            let mut destination = File::create_new(&alias)?;
            io::copy(&mut source, &mut destination)?;
        }
        sources.push(alias);
    }
    let mut directory = wide_path(&aliases.dir)?;
    directory.pop();
    directory.extend([u16::from(b'\\'), 0]);
    ensure!(
        directory.len() <= MAX_PATH,
        "CAB volume directory exceeds the Windows path limit"
    );
    let chain = Chain {
        names: &aliases.volume_names,
        directory,
        start: 0,
        current: 0,
    };
    read_paths(&sources, Some(chain), select)
}

struct Chain<'a> {
    names: &'a [String],
    directory: Vec<u16>,
    start: usize,
    current: usize,
}

fn read_paths(
    paths: &[PathBuf],
    chain: Option<Chain<'_>>,
    mut select: impl FnMut(&str) -> bool,
) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut state = Selection {
        selected: BTreeMap::new(),
        scratch: Scratch::create()?,
        size: 0,
        extracted: 0,
        error: None,
        select: &mut select,
        chain,
    };
    // SetupAPI requests continuation only when an extracted file spans
    // volumes. Open every volume to also visit entries starting later.
    for (index, path) in paths.iter().enumerate() {
        if let Some(chain) = &mut state.chain {
            chain.start = index;
            chain.current = index;
        }
        let source = wide_path(&std::path::absolute(path)?)?;
        // The call is synchronous and the callback does not retain pointers.
        let status = unsafe {
            SetupIterateCabinetW(
                source.as_ptr(),
                0,
                notify,
                (&mut state as *mut Selection<'_>).cast(),
            )
        };
        let os_error = io::Error::last_os_error();
        if let Some(error) = state.error.take() {
            return Err(error).with_context(|| format!("reading cabinet {}", path.display()));
        }
        if status == 0 {
            return Err(os_error).with_context(|| format!("reading cabinet {}", path.display()));
        }
    }
    ensure!(
        state.extracted == state.selected.len(),
        "cabinet ended before all selected entries were extracted"
    );

    state
        .selected
        .into_iter()
        .map(|(name, (index, size))| {
            let file = File::open(state.scratch.path(index))
                .with_context(|| format!("reading extracted cabinet entry {name:?}"))?;
            let mut bytes = Vec::new();
            file.take(size + 1).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() as u64 == size,
                "cabinet entry {name:?} does not match its declared size"
            );
            Ok((name, bytes))
        })
        .collect()
}

struct Selection<'a> {
    selected: BTreeMap<String, (usize, u64)>,
    scratch: Scratch,
    size: u64,
    extracted: usize,
    error: Option<anyhow::Error>,
    select: &'a mut dyn FnMut(&str) -> bool,
    chain: Option<Chain<'a>>,
}

extern "system" fn notify(context: *mut c_void, message: u32, param1: usize, param2: usize) -> u32 {
    // SetupAPI passes back the exact context supplied by read.
    let state = unsafe { &mut *context.cast::<Selection<'_>>() };
    let result = catch_unwind(AssertUnwindSafe(|| state.handle(message, param1, param2)));
    let error = match result {
        Ok(Ok(status)) => return status,
        Ok(Err(error)) => error,
        Err(_) => anyhow::anyhow!("cabinet callback panicked"),
    };
    state.error = Some(error);
    if message == NOTIFY_FILE_IN_CABINET {
        FILEOP_ABORT
    } else {
        ERROR_CANCELLED
    }
}

impl Selection<'_> {
    fn handle(&mut self, message: u32, param1: usize, param2: usize) -> Result<u32> {
        match message {
            NOTIFY_FILE_IN_CABINET => {
                ensure!(param1 != 0, "cabinet file notification is missing metadata");
                // For this notification, Param1 points to writable FILE_IN_CABINET_INFO_W.
                let entry = unsafe { &mut *(param1 as *mut FileInCabinet) };
                let name = read_name(entry.name)?;
                if let Some(chain) = &mut self.chain {
                    let volume = read_name(param2 as *const u16)?;
                    let name = volume.rsplit(['/', '\\']).next().unwrap_or_default();
                    if !chain.names[chain.start].eq_ignore_ascii_case(name) {
                        return Ok(FILEOP_SKIP);
                    }
                    chain.current = chain.start;
                }
                if !(self.select)(&name) {
                    return Ok(FILEOP_SKIP);
                }

                let size = u64::from(entry.size);
                ensure!(
                    size <= FILE_LIMIT,
                    "cabinet entry {name:?} exceeds the file size limit"
                );
                ensure!(
                    self.selected.len() < ENTRY_LIMIT,
                    "too many selected cabinet entries"
                );
                ensure!(
                    self.size + size <= TOTAL_LIMIT,
                    "selected cabinet entries exceed the total size limit"
                );
                ensure!(
                    !self.selected.contains_key(&name),
                    "duplicate cabinet entry {name:?}"
                );

                let index = self.selected.len();
                let target = wide_path(&self.scratch.path(index))?;
                ensure!(
                    target.len() <= entry.target.len(),
                    "cabinet scratch path exceeds the Windows target limit"
                );
                entry.target.fill(0);
                entry.target[..target.len()].copy_from_slice(&target);
                self.selected.insert(name, (index, size));
                self.scratch.count += 1;
                self.size += size;
                Ok(FILEOP_DOIT)
            }
            NOTIFY_FILE_EXTRACTED => {
                ensure!(
                    param1 != 0,
                    "cabinet extraction notification is missing status"
                );
                // This notification supplies a readable FILEPATHS_W until the callback returns.
                let entry = unsafe { &*(param1 as *const FilePaths) };
                if entry.win32_error != 0 {
                    return Err(io::Error::from_raw_os_error(entry.win32_error as i32))
                        .context("extracting a selected cabinet entry");
                }
                self.extracted += 1;
                Ok(0)
            }
            NOTIFY_NEED_NEW_CABINET => {
                let chain = self
                    .chain
                    .as_mut()
                    .context("CAB continuation requires an explicit volume set")?;
                ensure!(
                    param1 != 0 && param2 != 0,
                    "CAB continuation notification is missing metadata"
                );
                // The SDK supplies CABINET_INFO_W and a writable MAX_PATH
                // WCHAR new-source buffer for this notification.
                let info = unsafe { &*(param1 as *const CabinetInfo) };
                let requested = read_name(info.file)?;
                let next = chain.current + 1;
                ensure!(
                    chain
                        .names
                        .get(next)
                        .is_some_and(|name| name.eq_ignore_ascii_case(&requested)),
                    "CAB requested unapproved continuation volume {requested:?}"
                );
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        chain.directory.as_ptr(),
                        param2 as *mut u16,
                        chain.directory.len(),
                    );
                }
                chain.current = next;
                Ok(0)
            }
            _ => Ok(0),
        }
    }
}

fn read_name(name: *const u16) -> Result<String> {
    ensure!(!name.is_null(), "cabinet entry has no name");
    // The SDK provides a NUL-terminated UTF-16 name for this notification.
    let len = (0..MAX_PATH)
        .find(|&offset| unsafe { *name.add(offset) == 0 })
        .context("cabinet entry name exceeds the Windows filename limit")?;
    ensure!(len != 0, "cabinet entry has an empty name");
    String::from_utf16(unsafe { std::slice::from_raw_parts(name, len) })
        .context("cabinet entry name is not UTF-16")
}

fn wide_path(path: &Path) -> Result<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    ensure!(!wide.contains(&0), "cabinet path contains a NUL");
    wide.push(0);
    Ok(wide)
}

struct Scratch {
    dir: PathBuf,
    count: usize,
    volume_names: Vec<String>,
}

impl Scratch {
    fn create() -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let temp = std::path::absolute(std::env::temp_dir())?;
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_nanos();
        for _ in 0..32 {
            let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = temp.join(format!(
                "lvpm-cab-{}-{stamp:x}-{suffix:x}",
                std::process::id()
            ));
            match fs::create_dir(&dir) {
                Ok(()) => {
                    return Ok(Self {
                        dir,
                        count: 0,
                        volume_names: Vec::new(),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("creating cabinet scratch directory"),
            }
        }
        bail!("could not create a unique cabinet scratch directory")
    }

    fn path(&self, index: usize) -> PathBuf {
        self.dir.join(format!("{index:x}.bin"))
    }
}

impl Drop for Scratch {
    #[expect(
        clippy::permissions_set_readonly_false,
        reason = "the Windows-only cleanup clears a DOS attribute on extracted scratch files"
    )]
    fn drop(&mut self) {
        // Windows preserves DOS attributes when extracting. Clear read-only
        // on the files created here before removing these known paths.
        for index in 0..self.count {
            let path = self.path(index);
            if let Ok(metadata) = fs::symlink_metadata(&path)
                && metadata.is_file()
                && metadata.permissions().readonly()
            {
                let mut permissions = metadata.permissions();
                permissions.set_readonly(false);
                let _ = fs::set_permissions(&path, permissions);
            }
            let _ = fs::remove_file(&path);
        }
        for name in &self.volume_names {
            let _ = fs::remove_file(self.dir.join(name));
        }
        let _ = fs::remove_dir(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_missing_and_unsafe_continuation_volumes() {
        let mut fixture = Scratch::create().unwrap();
        fixture.count = 1;
        let path = fixture.path(0);
        let next = b"part2.cab\0disk\0";
        let files = 36 + next.len();
        let mut data = vec![0; files + 16];
        data[..4].copy_from_slice(b"MSCF");
        data[16..20].copy_from_slice(&(files as u32).to_le_bytes());
        data[28..30].copy_from_slice(&1u16.to_le_bytes());
        data[30..32].copy_from_slice(&NEXT.to_le_bytes());
        data[36..files].copy_from_slice(next);
        data[files + 8..files + 10].copy_from_slice(&0xfffeu16.to_le_bytes());
        data.extend_from_slice(b"split.lvlibp\0");
        let size = data.len() as u32;
        data[8..12].copy_from_slice(&size.to_le_bytes());
        fs::write(&path, data).unwrap();

        let header = volume_header(&path).unwrap();
        assert!(header.to_next);
        assert_eq!(header.next.as_deref(), Some("part2.cab"));
        let error = read_volumes(&[("part1.cab".to_owned(), path.clone())], |_| true).unwrap_err();
        assert!(error.to_string().contains("missing next CAB volume"));
        for name in ["../part1.cab", "part1.cab.", "C:\\part1.cab", "NUL.cab"] {
            let error = read_volumes(&[(name.to_owned(), path.clone())], |_| true).unwrap_err();
            assert!(error.to_string().contains("unsafe CAB volume name"));
        }
    }

    #[test]
    fn extracts_only_selected_entries_and_preserves_names() {
        let mut fixture = Scratch::create().unwrap();
        fixture.count = 1;
        let path = fixture.path(0);
        let entries: [(&str, &[u8]); 2] = [("FileID1", b"PPL bytes"), ("other.dll", b"DLL bytes")];

        // A cabinet with one folder and one uncompressed block. Construct it
        // locally so the test needs neither makecab.exe nor a binary fixture.
        let mut file_table = Vec::new();
        let mut data = Vec::new();
        for (name, bytes) in entries {
            file_table.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            file_table.extend_from_slice(&(data.len() as u32).to_le_bytes());
            file_table.extend_from_slice(&[0, 0, 0x21, 0x28, 0, 0, 0x20, 0]);
            file_table.extend_from_slice(name.as_bytes());
            file_table.push(0);
            data.extend_from_slice(bytes);
        }
        let data_offset = 44 + file_table.len() as u32;
        let mut cab = [0; 44].to_vec();
        cab[..4].copy_from_slice(b"MSCF");
        cab[8..12].copy_from_slice(&(data_offset + 8 + data.len() as u32).to_le_bytes());
        cab[16..20].copy_from_slice(&44u32.to_le_bytes());
        cab[24..30].copy_from_slice(&[3, 1, 1, 0, 2, 0]);
        cab[36..40].copy_from_slice(&data_offset.to_le_bytes());
        cab[40..42].copy_from_slice(&1u16.to_le_bytes());
        cab.extend(file_table);
        cab.extend_from_slice(&[0; 4]);
        cab.extend_from_slice(&(data.len() as u16).to_le_bytes());
        cab.extend_from_slice(&(data.len() as u16).to_le_bytes());
        cab.extend(data);
        fs::write(&path, cab).unwrap();

        assert_eq!(
            read(&path, |name| name == "FileID1").unwrap(),
            BTreeMap::from([("FileID1".to_owned(), b"PPL bytes".to_vec())])
        );
        assert!(read(&path, |_| false).unwrap().is_empty());
    }
}
