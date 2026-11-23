//! Read import and version evidence from a Windows executable or DLL without loading it.
//!
//! Layout and RVA rules follow Microsoft's PE/COFF format:
//! https://learn.microsoft.com/en-us/windows/win32/debug/pe-format

use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const IMPORT_DIRECTORY: usize = 1;
const RESOURCE_DIRECTORY: usize = 2;
const DELAY_IMPORT_DIRECTORY: usize = 13;
const RESOURCE_VERSION: u32 = 16;
const RESOURCE_SUBDIRECTORY: u32 = 1 << 31;
const SECTION_LIMIT: usize = 96;
const IMPORT_LIMIT: usize = 4096;
const IMPORT_NAME_LIMIT: usize = 1024;
const RESOURCE_ENTRY_LIMIT: usize = 4096;
const VERSION_SIZE_LIMIT: usize = u16::MAX as usize;

#[derive(Debug)]
pub(crate) struct Evidence {
    pub machine: u16,
    pub file_version: Option<String>,
    pub imports: Vec<Import>,
    pub issues: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Import {
    pub name: String,
    pub delayed: bool,
}

pub(crate) fn scan(path: &Path) -> Result<Evidence> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    scan_image(file, len).context("reading PE evidence")
}

fn scan_image(source: impl Read + Seek, len: u64) -> Result<Evidence> {
    let mut image = Image::open(source, len)?;
    let mut imports = BTreeMap::new();
    let mut issues = Vec::new();
    for (name, delayed) in [("import table", false), ("delay-import table", true)] {
        if let Err(error) = read_imports(&mut image, delayed, &mut imports, &mut issues) {
            issues.push(format!("{name}: {error:#}"));
        }
    }
    let file_version = match read_version(&mut image) {
        Ok(version) => version,
        Err(error) => {
            issues.push(format!("version resource: {error:#}"));
            None
        }
    };
    Ok(Evidence {
        machine: image.machine,
        file_version,
        imports: imports
            .into_iter()
            .map(|((delayed, _), name)| Import { name, delayed })
            .collect(),
        issues,
    })
}

struct Image<S> {
    source: S,
    len: u64,
    machine: u16,
    image_base: u64,
    headers: u32,
    directories: Vec<u8>,
    directory_count: u32,
    sections: Vec<Section>,
}

struct Section {
    rva: u32,
    virtual_size: u32,
    raw_offset: u32,
    raw_size: u32,
}

#[derive(Clone, Copy)]
struct Directory {
    rva: u32,
    size: u32,
}

impl<S: Read + Seek> Image<S> {
    fn open(mut source: S, len: u64) -> Result<Self> {
        let read = |source: &mut S, offset, size| read_at(source, len, offset, size);
        let dos = read(&mut source, 0, 64).context("reading DOS header")?;
        ensure!(dos.starts_with(b"MZ"), "invalid DOS header signature");
        let pe_offset = u64::from(u32_at(&dos, 0x3c).expect("DOS header has fixed size"));
        ensure!(pe_offset >= 64, "invalid PE header offset");
        let coff =
            read(&mut source, pe_offset, 24).context("reading PE signature and COFF header")?;
        ensure!(coff.starts_with(b"PE\0\0"), "invalid PE signature");
        let machine = u16_at(&coff, 4).expect("COFF header has fixed size");
        let section_count = usize::from(u16_at(&coff, 6).expect("COFF header has fixed size"));
        ensure!(
            section_count <= SECTION_LIMIT,
            "PE image exceeds {SECTION_LIMIT} sections"
        );
        let optional_size = usize::from(u16_at(&coff, 20).expect("COFF header has fixed size"));
        let optional_at = pe_offset + 24;
        let optional =
            read(&mut source, optional_at, optional_size).context("reading PE optional header")?;
        let (directory_offset, image_base) = match u16_at(&optional, 0) {
            Some(0x10b) => (96, u32_at(&optional, 28).map(u64::from)),
            Some(0x20b) => (112, u64_at(&optional, 24)),
            _ => bail!("unsupported PE optional header"),
        };
        let image_base = image_base.context("missing PE image base")?;
        let headers = u32_at(&optional, 60).context("missing PE header size")?;
        let directory_count =
            u32_at(&optional, directory_offset - 4).context("missing PE data directory count")?;
        let directories = optional[directory_offset..].to_vec();
        let section_at = optional_at + optional_size as u64;
        let section_bytes = read(&mut source, section_at, section_count * 40)
            .context("reading PE section table")?;
        let sections = section_bytes
            .as_chunks::<40>()
            .0
            .iter()
            .map(|record| Section {
                virtual_size: u32_at(record, 8).expect("section has fixed size"),
                rva: u32_at(record, 12).expect("section has fixed size"),
                raw_size: u32_at(record, 16).expect("section has fixed size"),
                raw_offset: u32_at(record, 20).expect("section has fixed size"),
            })
            .collect();
        Ok(Self {
            source,
            len,
            machine,
            image_base,
            headers,
            directories,
            directory_count,
            sections,
        })
    }

    fn directory(&self, index: usize) -> Result<Option<Directory>> {
        if index as u32 >= self.directory_count {
            return Ok(None);
        }
        let offset = index * 8;
        let rva = u32_at(&self.directories, offset).context("truncated data directory entry")?;
        let size =
            u32_at(&self.directories, offset + 4).context("truncated data directory entry")?;
        if rva == 0 && size == 0 {
            return Ok(None);
        }
        ensure!(rva != 0 && size != 0, "incomplete data directory entry");
        Ok(Some(Directory { rva, size }))
    }

    fn raw_range(&self, rva: u32) -> Result<(u64, u32)> {
        let mut mapping = (rva < self.headers).then(|| (u64::from(rva), self.headers - rva));
        for section in &self.sections {
            let virtual_size = if section.virtual_size == 0 {
                section.raw_size
            } else {
                section.virtual_size
            };
            if let Some(delta) = rva
                .checked_sub(section.rva)
                .filter(|&delta| delta < virtual_size)
            {
                ensure!(mapping.is_none(), "overlapping PE ranges at RVA {rva:#x}");
                let available = section.raw_size.min(virtual_size).saturating_sub(delta);
                mapping = Some((u64::from(section.raw_offset) + u64::from(delta), available));
            }
        }
        let (offset, size) = mapping.with_context(|| format!("unmapped PE address {rva:#x}"))?;
        let available = self.len.saturating_sub(offset).min(u64::from(size)) as u32;
        ensure!(
            available != 0,
            "PE address {rva:#x} has no file-backed bytes"
        );
        Ok((offset, available))
    }

    fn read_rva(&mut self, rva: u32, size: usize) -> Result<Vec<u8>> {
        let (offset, available) = self.raw_range(rva)?;
        ensure!(
            size <= available as usize,
            "truncated file-backed data at RVA {rva:#x}"
        );
        read_at(&mut self.source, self.len, offset, size)
    }
}

fn read_imports(
    image: &mut Image<impl Read + Seek>,
    delayed: bool,
    imports: &mut BTreeMap<(bool, String), String>,
    issues: &mut Vec<String>,
) -> Result<()> {
    let (directory, record_size, name_offset, label) = if delayed {
        (DELAY_IMPORT_DIRECTORY, 32, 4, "delay-import table")
    } else {
        (IMPORT_DIRECTORY, 20, 12, "import table")
    };
    let Some(directory) = image.directory(directory)? else {
        return Ok(());
    };
    let (_, available) = image.raw_range(directory.rva)?;
    let size = (directory.size.min(available) as usize).min((IMPORT_LIMIT + 1) * record_size);
    let data = image.read_rva(directory.rva, size)?;
    for (index, record) in data.chunks_exact(record_size).enumerate() {
        if record.iter().all(|&byte| byte == 0) {
            return Ok(());
        }
        ensure!(index < IMPORT_LIMIT, "table exceeds {IMPORT_LIMIT} records");
        let name_address = u32_at(record, name_offset).expect("complete import descriptor");
        match import_name(
            image,
            name_address,
            delayed.then(|| u32_at(record, 0).expect("complete delay descriptor")),
        ) {
            Ok(name) => {
                imports
                    .entry((delayed, name.to_ascii_lowercase()))
                    .or_insert(name);
            }
            Err(error) => issues.push(format!("{label} record {}: {error:#}", index + 1)),
        }
    }
    bail!("table has no complete null terminator in its file-backed range")
}

fn import_name(
    image: &mut Image<impl Read + Seek>,
    address: u32,
    attributes: Option<u32>,
) -> Result<String> {
    let rva = match attributes {
        None | Some(1) => address,
        Some(0) => {
            // Older delay-load descriptors store a VA, not an RVA.
            u64::from(address)
                .checked_sub(image.image_base)
                .and_then(|rva| u32::try_from(rva).ok())
                .context("invalid delay-load virtual address")?
        }
        Some(flags) => bail!("unsupported delay-load attributes {flags:#x}"),
    };
    ensure!(rva != 0, "missing import name address");
    let (_, available) = image.raw_range(rva)?;
    let data = image.read_rva(rva, (available as usize).min(IMPORT_NAME_LIMIT))?;
    let end = data
        .iter()
        .position(|&byte| byte == 0)
        .context("unterminated PE import name")?;
    let name = &data[..end];
    ensure!(
        !name.is_empty() && name.iter().all(|byte| (0x20..=0x7e).contains(byte)),
        "invalid ASCII PE import name"
    );
    Ok(String::from_utf8(name.to_vec()).expect("validated ASCII import name"))
}

fn read_version(image: &mut Image<impl Read + Seek>) -> Result<Option<String>> {
    let Some(directory) = image.directory(RESOURCE_DIRECTORY)? else {
        return Ok(None);
    };
    let (_, available) = image.raw_range(directory.rva)?;
    let directory = Directory {
        size: directory.size.min(available),
        ..directory
    };
    let types = resource_entries(image, directory, 0)?;
    let Some(kind) = types.iter().find(|(name, _)| *name == RESOURCE_VERSION) else {
        return Ok(None);
    };
    let names = resource_entries(image, directory, subdirectory(kind.1)?)?;
    // Prefer the conventional VERSIONINFO resource identifier, then the
    // lowest numeric identifier. Named entries have no numeric resource ID.
    let Some((_, name)) = names
        .into_iter()
        .filter(|(id, _)| id & RESOURCE_SUBDIRECTORY == 0)
        .min_by_key(|(id, _)| (*id != 1, *id))
    else {
        return Ok(None);
    };
    let Some((_, language)) = resource_entries(image, directory, subdirectory(name)?)?
        .into_iter()
        .filter(|(id, _)| id & RESOURCE_SUBDIRECTORY == 0)
        .min_by_key(|(id, _)| *id)
    else {
        bail!("version resource has no numeric language entry")
    };
    ensure!(
        language & RESOURCE_SUBDIRECTORY == 0,
        "invalid PE resource data entry"
    );
    let data = resource_data(image, directory, language, 16)?;
    let rva = u32_at(&data, 0).expect("complete resource data entry");
    let size = u32_at(&data, 4).expect("complete resource data entry") as usize;
    ensure!(
        size <= VERSION_SIZE_LIMIT,
        "PE version resource exceeds {VERSION_SIZE_LIMIT} bytes"
    );
    crate::ppl_pe::file_version(&image.read_rva(rva, size)?)
}

fn subdirectory(offset: u32) -> Result<u32> {
    ensure!(
        offset & RESOURCE_SUBDIRECTORY != 0,
        "expected PE resource subdirectory"
    );
    Ok(offset & !RESOURCE_SUBDIRECTORY)
}

fn resource_entries(
    image: &mut Image<impl Read + Seek>,
    directory: Directory,
    offset: u32,
) -> Result<Vec<(u32, u32)>> {
    let header = resource_data(image, directory, offset, 16)?;
    let named = u16_at(&header, 12).expect("complete resource directory header");
    let numeric = u16_at(&header, 14).expect("complete resource directory header");
    let count = usize::from(named) + usize::from(numeric);
    ensure!(
        count <= RESOURCE_ENTRY_LIMIT,
        "resource directory exceeds {RESOURCE_ENTRY_LIMIT} entries"
    );
    let offset = offset
        .checked_add(16)
        .context("PE resource entry offset overflow")?;
    let data = resource_data(image, directory, offset, count * 8)?;
    Ok(data
        .as_chunks::<8>()
        .0
        .iter()
        .map(|entry| {
            (
                u32_at(entry, 0).expect("complete resource directory entry"),
                u32_at(entry, 4).expect("complete resource directory entry"),
            )
        })
        .collect())
}

fn resource_data(
    image: &mut Image<impl Read + Seek>,
    directory: Directory,
    offset: u32,
    size: usize,
) -> Result<Vec<u8>> {
    ensure!(
        u64::from(offset) + size as u64 <= u64::from(directory.size),
        "truncated PE resource directory"
    );
    if size == 0 {
        return Ok(Vec::new());
    }
    let rva = directory
        .rva
        .checked_add(offset)
        .context("PE resource address overflow")?;
    image.read_rva(rva, size)
}

fn read_at(source: &mut (impl Read + Seek), len: u64, offset: u64, size: usize) -> Result<Vec<u8>> {
    ensure!(
        offset
            .checked_add(size as u64)
            .is_some_and(|end| end <= len),
        "truncated PE file"
    );
    let mut bytes = vec![0; size];
    source.seek(SeekFrom::Start(offset))?;
    source.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const PE: usize = 0x80;
    const OPT: usize = PE + 24;
    const RAW: usize = 0x200;
    const VA: u32 = 0x1000;
    const IMAGE_BASE: u32 = 0x400000;
    const RT: usize = 0x500;

    fn put16(bytes: &mut [u8], offset: usize, n: u16) {
        bytes[offset..offset + 2].copy_from_slice(&n.to_le_bytes());
    }

    fn put32(bytes: &mut [u8], offset: usize, n: u32) {
        bytes[offset..offset + 4].copy_from_slice(&n.to_le_bytes());
    }

    fn rva(raw_offset: usize) -> u32 {
        VA + (raw_offset - RAW) as u32
    }

    fn fixture(wide: bool) -> Vec<u8> {
        let mut bytes = vec![0; 0x1000];
        let directory_offset = if wide { 112 } else { 96 };
        let optional_size = directory_offset + 16 * 8;
        let sections = OPT + optional_size;
        bytes[..2].copy_from_slice(b"MZ");
        put32(&mut bytes, 0x3c, PE as u32);
        bytes[PE..PE + 4].copy_from_slice(b"PE\0\0");
        put16(&mut bytes, PE + 4, if wide { 0x8664 } else { 0x14c });
        put16(&mut bytes, PE + 6, 1);
        put16(&mut bytes, PE + 20, optional_size as u16);
        put16(&mut bytes, OPT, if wide { 0x20b } else { 0x10b });
        put32(&mut bytes, OPT + if wide { 24 } else { 28 }, IMAGE_BASE);
        put32(&mut bytes, OPT + 60, RAW as u32);
        put32(&mut bytes, OPT + directory_offset - 4, 16);
        for (index, raw, size) in [(1, 0x300, 60), (2, RT, 0x100), (13, 0x400, 96)] {
            put32(&mut bytes, OPT + directory_offset + index * 8, rva(raw));
            put32(&mut bytes, OPT + directory_offset + index * 8 + 4, size);
        }
        put32(&mut bytes, sections + 8, 0xe00);
        put32(&mut bytes, sections + 12, VA);
        put32(&mut bytes, sections + 16, 0xe00);
        put32(&mut bytes, sections + 20, RAW as u32);

        put32(&mut bytes, 0x300 + 12, rva(0x680));
        put32(&mut bytes, 0x314 + 12, rva(0x690));
        bytes[0x680..0x68a].copy_from_slice(b"Kernel.dll");
        bytes[0x690..0x69a].copy_from_slice(b"KERNEL.DLL");
        put32(&mut bytes, 0x400, 1);
        put32(&mut bytes, 0x404, rva(0x6a0));
        bytes[0x6a0..0x6a9].copy_from_slice(b"Delay.dll");
        put32(&mut bytes, 0x420, u32::from(wide));
        put32(
            &mut bytes,
            0x424,
            rva(0x6b0) + if wide { 0 } else { IMAGE_BASE },
        );
        bytes[0x6b0..0x6ba].copy_from_slice(b"Legacy.dll");

        put16(&mut bytes, RT + 14, 1);
        put32(&mut bytes, RT + 16, RESOURCE_VERSION);
        put32(&mut bytes, RT + 20, RESOURCE_SUBDIRECTORY | 0x18);
        put16(&mut bytes, RT + 0x26, 1);
        put32(&mut bytes, RT + 0x28, 1);
        put32(&mut bytes, RT + 0x2c, RESOURCE_SUBDIRECTORY | 0x30);
        put16(&mut bytes, RT + 0x3e, 1);
        put32(&mut bytes, RT + 0x40, 1033);
        put32(&mut bytes, RT + 0x44, 0x48);
        put32(&mut bytes, RT + 0x48, rva(0x700));
        put32(&mut bytes, RT + 0x4c, 92);
        put16(&mut bytes, 0x700, 92);
        put16(&mut bytes, 0x702, 52);
        for (i, word) in "VS_VERSION_INFO\0".encode_utf16().enumerate() {
            put16(&mut bytes, 0x706 + i * 2, word);
        }
        put32(&mut bytes, 0x728, 0xfeef04bd);
        put32(&mut bytes, 0x730, 0x0001_0002);
        put32(&mut bytes, 0x734, 0x0003_0004);
        bytes
    }

    fn parse(bytes: &[u8]) -> Result<Evidence> {
        scan_image(Cursor::new(bytes), bytes.len() as u64)
    }

    #[test]
    fn reads_pe32_and_pe32_plus_imports_and_version() {
        for (wide, machine) in [(false, 0x14c), (true, 0x8664)] {
            let evidence = parse(&fixture(wide)).unwrap();
            assert_eq!(evidence.machine, machine);
            assert_eq!(evidence.file_version.as_deref(), Some("1.2.3.4"));
            assert_eq!(
                evidence
                    .imports
                    .iter()
                    .map(|entry| (&*entry.name, entry.delayed))
                    .collect::<Vec<_>>(),
                [
                    ("Kernel.dll", false),
                    ("Delay.dll", true),
                    ("Legacy.dll", true),
                ]
            );
            assert!(evidence.issues.is_empty());
        }
    }

    #[test]
    fn bad_optional_evidence_keeps_the_other_imports() {
        let mut bytes = fixture(false);
        put32(&mut bytes, RT + 0x48, 0xffff_fffc);
        put32(&mut bytes, 0x400, 3);
        put32(&mut bytes, OPT + 96 + 8 + 4, 20);
        let evidence = parse(&bytes).unwrap();
        assert_eq!(evidence.file_version, None);
        assert_eq!(
            evidence
                .imports
                .iter()
                .map(|entry| (&*entry.name, entry.delayed))
                .collect::<Vec<_>>(),
            [("Kernel.dll", false), ("Legacy.dll", true),]
        );
        assert_eq!(evidence.issues.len(), 3);
        assert!(evidence.issues[0].contains("no complete null terminator"));
        assert!(evidence.issues[1].contains("unsupported delay-load attributes"));
        assert!(evidence.issues[2].starts_with("version resource:"));
    }

    #[test]
    fn absent_resources_are_optional_and_truncated_headers_fail() {
        let mut bytes = fixture(true);
        put32(&mut bytes, OPT + 112 + 2 * 8, 0);
        put32(&mut bytes, OPT + 112 + 2 * 8 + 4, 0);
        let evidence = parse(&bytes).unwrap();
        assert_eq!(evidence.file_version, None);
        assert_eq!(evidence.imports.len(), 3);
        assert!(evidence.issues.is_empty());
        assert!(
            parse(&bytes[..OPT + 10])
                .unwrap_err()
                .to_string()
                .contains("PE optional header")
        );
    }
}
