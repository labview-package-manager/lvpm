//! Read the PE resources of a Windows packed project library without loading it.

use anyhow::{Context, Result, bail, ensure};

const RC_DATA: u32 = 10;
const RC_VERSION: u32 = 16;
const DIRECTORY_FLAG: u32 = 1 << 31;

pub(crate) struct Resources<'a> {
    pub leif: &'a [u8],
    pub version: Option<String>,
}

pub(crate) fn read(data: &[u8]) -> Result<Resources<'_>> {
    ensure!(
        data.starts_with(b"MZ"),
        "unsupported .lvlibp container: expected a Windows PE image"
    );
    let pe_start = u32_at(data, 0x3c).context("truncated DOS header")? as usize;
    let pe = data.get(pe_start..).context("invalid PE header offset")?;
    ensure!(pe.starts_with(b"PE\0\0"), "invalid PE signature");
    let coff = bytes_at(pe, 4, 20).context("truncated PE COFF header")?;
    let sections = u16_at(coff, 2).unwrap() as usize;
    let optional_size = u16_at(coff, 16).unwrap() as usize;
    let optional = bytes_at(pe, 24, optional_size).context("truncated PE optional header")?;
    let directory_offset = match u16_at(optional, 0) {
        Some(0x10b) => 96,
        Some(0x20b) => 112,
        _ => bail!("unsupported PE optional header"),
    };
    let directory_count = u32_at(optional, directory_offset - 4)
        .context("truncated PE data directory count")? as usize;
    ensure!(directory_count > 2, "PE image has no resource directory");
    let resource_entry = bytes_at(optional, directory_offset + 2 * 8, 8)
        .context("truncated PE resource data directory")?;
    let resource_rva = u32_at(resource_entry, 0).unwrap();
    let resource_size = u32_at(resource_entry, 4).unwrap() as usize;
    ensure!(
        resource_rva != 0 && resource_size != 0,
        "PE image has no resource directory"
    );

    let sections =
        bytes_at(pe, 24 + optional_size, sections * 40).context("truncated PE section table")?;
    let image = Image { data, sections };
    let directory = image
        .at_rva(resource_rva, resource_size)
        .context("invalid PE resource directory range")?;
    let leif =
        resource(&image, directory, RC_DATA, 2)?.context("packed library has no LEIF resource")?;
    ensure!(
        leif.starts_with(b"LEIF"),
        "invalid packed library LEIF signature"
    );
    let version = resource(&image, directory, RC_VERSION, 1)?
        .map(file_version)
        .transpose()?
        .flatten();
    Ok(Resources { leif, version })
}

struct Image<'a> {
    data: &'a [u8],
    sections: &'a [u8],
}

impl<'a> Image<'a> {
    fn at_rva(&self, rva: u32, len: usize) -> Option<&'a [u8]> {
        self.sections
            .as_chunks::<40>()
            .0
            .iter()
            .find_map(|section| {
                let address = u32_at(section, 12)?;
                let raw_size = u32_at(section, 16)? as usize;
                let raw_offset = u32_at(section, 20)? as usize;
                let delta = rva.checked_sub(address)? as usize;
                bytes_at(bytes_at(self.data, raw_offset, raw_size)?, delta, len)
            })
    }
}

/// Numeric PE resource entries use three levels: type, name, and language.
fn resource<'a>(
    image: &Image<'a>,
    directory: &[u8],
    kind: u32,
    name: u32,
) -> Result<Option<&'a [u8]>> {
    let Some(kind) = find_entry(directory, 0, kind)? else {
        return Ok(None);
    };
    ensure!(
        kind & DIRECTORY_FLAG != 0,
        "invalid PE resource type directory"
    );
    let Some(name) = find_entry(directory, kind & !DIRECTORY_FLAG, name)? else {
        return Ok(None);
    };
    ensure!(
        name & DIRECTORY_FLAG != 0,
        "invalid PE resource name directory"
    );

    // Numeric language IDs are ordered, so the neutral ID is preferred when
    // present. Fixed version fields have the same meaning in every language.
    let language = entries(directory, name & !DIRECTORY_FLAG)?
        .map(|entry| (u32_at(entry, 0).unwrap(), u32_at(entry, 4).unwrap()))
        .filter(|(id, _)| *id & DIRECTORY_FLAG == 0)
        .min_by_key(|(id, _)| *id)
        .map(|(_, entry)| entry)
        .context("PE resource has no numeric language entry")?;
    ensure!(
        language & DIRECTORY_FLAG == 0,
        "invalid PE resource data entry"
    );
    let entry =
        bytes_at(directory, language as usize, 16).context("truncated PE resource data entry")?;
    let address = u32_at(entry, 0).unwrap();
    let size = u32_at(entry, 4).unwrap() as usize;
    Ok(Some(
        image
            .at_rva(address, size)
            .context("invalid PE resource data range")?,
    ))
}

fn find_entry(directory: &[u8], offset: u32, id: u32) -> Result<Option<u32>> {
    let mut matches = entries(directory, offset)?.filter(|entry| u32_at(*entry, 0) == Some(id));
    let found = matches.next().and_then(|entry| u32_at(entry, 4));
    ensure!(matches.next().is_none(), "duplicate PE resource entry {id}");
    Ok(found)
}

fn entries(directory: &[u8], offset: u32) -> Result<std::slice::Iter<'_, [u8; 8]>> {
    let table = directory
        .get(offset as usize..)
        .context("invalid PE resource directory offset")?;
    let named = u16_at(table, 12).context("truncated PE resource directory header")? as usize;
    let numeric = u16_at(table, 14).context("truncated PE resource directory header")? as usize;
    let items = bytes_at(table, 16, (named + numeric) * 8)
        .context("truncated PE resource directory entries")?;
    Ok(items.as_chunks::<8>().0.iter())
}

pub(crate) fn file_version(resource: &[u8]) -> Result<Option<String>> {
    let length = u16_at(resource, 0).context("missing PE version length")? as usize;
    let resource = resource
        .get(..length)
        .context("truncated PE version resource")?;
    let value_len = u16_at(resource, 2).context("missing PE version value length")? as usize;
    ensure!(
        u16_at(resource, 4) == Some(0),
        "PE version resource has a nonbinary value"
    );
    let key = bytes_at(resource, 6, 32).context("missing PE version key")?;
    let key = key
        .as_chunks::<2>()
        .0
        .iter()
        .copied()
        .map(u16::from_le_bytes);
    ensure!(
        key.eq("VS_VERSION_INFO\0".encode_utf16()),
        "invalid PE version key"
    );
    if value_len == 0 {
        return Ok(None);
    }
    // The key ends at byte 38; VS_FIXEDFILEINFO begins on a four-byte boundary.
    let value =
        bytes_at(resource, 40, value_len).context("truncated PE fixed version information")?;
    ensure!(
        value.len() == 52 && u32_at(value, 0) == Some(0xfeef04bd),
        "invalid PE fixed version information"
    );
    let high = u32_at(value, 8).unwrap();
    let low = u32_at(value, 12).unwrap();
    Ok(Some(format!(
        "{}.{}.{}.{}",
        high >> 16,
        high & 0xffff,
        low >> 16,
        low & 0xffff
    )))
}

fn bytes_at(data: &[u8], offset: usize, size: usize) -> Option<&[u8]> {
    data.get(offset..offset.checked_add(size)?)
}

fn u16_at(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes_at(data, offset, 2)?.try_into().ok()?,
    ))
}

fn u32_at(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes_at(data, offset, 4)?.try_into().ok()?,
    ))
}
