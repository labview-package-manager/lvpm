//! Read direct packed-library references from the LEIF metadata of a built PPL.

use crate::linkfix::Pth;
use anyhow::{Context, Result, ensure};
use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;

pub(crate) const ARTIFACT_NAME_PROPERTY: &str = "lvpm:ppl:artifact-name";

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Import {
    pub name: String,
    pub path: String,
    pub relative: bool,
    pub library_id: String,
}

pub(crate) struct Library {
    pub version: Option<String>,
    pub library_id: Option<String>,
    pub imports: BTreeSet<Import>,
}

pub(crate) struct Artifact {
    pub library: Library,
    pub sha256: String,
}

pub(crate) fn read_artifact(path: &Path) -> Result<Artifact> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading packed library {}", path.display()))?;
    let library =
        read(&bytes).with_context(|| format!("reading packed library {}", path.display()))?;
    Ok(Artifact {
        library,
        sha256: crate::cache::sha256_hex(&bytes),
    })
}

pub(crate) fn read(bytes: &[u8]) -> Result<Library> {
    let (leif, version) = if bytes.starts_with(b"LEIF") {
        (bytes, None)
    } else {
        let resources = crate::ppl_pe::read(bytes)?;
        (resources.leif, resources.version)
    };
    let (library_id, imports) = metadata(leif).context("reading LEIF packed library references")?;
    Ok(Library {
        version,
        library_id,
        imports,
    })
}

fn metadata(data: &[u8]) -> Result<(Option<String>, BTreeSet<Import>)> {
    // Observed in Windows PPLs (0x1209) and NI Linux RT PPLs (0x1309).
    // The first two length-prefixed blocks precede the reference table.
    ensure!(
        data.starts_with(b"LEIF\0\0\0\x22")
            && matches!(data.get(8..10), Some(b"\x12\x09" | b"\x13\x09")),
        "unsupported LEIF header or revision"
    );
    let mut size_field = &data[10..];
    ensure!(
        word(&mut size_field)? == data.len(),
        "LEIF size does not match its header"
    );
    let mut blocks = data.get(34..).context("truncated LEIF header")?;
    block(&mut blocks)?;
    let library_id = self_id(block(&mut blocks)?);
    let unpacked = unpack(block(&mut blocks)?)?;
    let mut records = unpacked.as_ref();
    let count = word(&mut records)?;
    ensure!(
        count <= records.len() / 16,
        "invalid LEIF packed library reference count"
    );
    let mut imports = BTreeSet::new();
    for _ in 0..count {
        let (path, end) = Pth::parse(records, 0)?;
        records = &records[end..];
        let library_id = std::str::from_utf8(block(&mut records)?)?;
        ensure!(
            valid_library_id(library_id),
            "invalid LEIF packed library identifier"
        );
        let relative = path.is_relative();
        let (name, path) = reference_path(&path)?;
        imports.insert(Import {
            name,
            path,
            relative,
            library_id: library_id.to_ascii_uppercase(),
        });
    }
    ensure!(
        records.is_empty(),
        "unexpected bytes after LEIF packed library references"
    );
    Ok((library_id, imports))
}

// The second block in observed LEIF files starts with a byte of flags and a
// length-prefixed library GUID. Its absence does not hide the declared imports.
fn self_id(data: &[u8]) -> Option<String> {
    let unpacked = unpack(data).ok()?;
    let mut fields = unpacked.get(1..)?;
    let text = std::str::from_utf8(block(&mut fields).ok()?).ok()?;
    valid_library_id(text).then(|| text.to_ascii_uppercase())
}

fn unpack(stored: &[u8]) -> Result<std::borrow::Cow<'_, [u8]>> {
    if let Ok((length, payload)) = unpacked_length(stored)
        && length == payload.len()
    {
        return Ok(std::borrow::Cow::Borrowed(payload));
    }
    let mut decoded = inflate(stored)?;
    let (length, payload) = unpacked_length(&decoded)?;
    ensure!(
        length == payload.len(),
        "invalid LEIF metadata block length"
    );
    let prefix_len = decoded.len() - length;
    decoded.drain(..prefix_len);
    Ok(std::borrow::Cow::Owned(decoded))
}

fn word(data: &mut &[u8]) -> Result<usize> {
    let (bytes, tail) = data
        .split_first_chunk::<4>()
        .context("truncated LEIF length")?;
    *data = tail;
    Ok(u32::from_be_bytes(*bytes) as usize)
}

fn block<'a>(data: &mut &'a [u8]) -> Result<&'a [u8]> {
    let length = word(data)?;
    let (bytes, tail) = data
        .split_at_checked(length)
        .context("truncated LEIF block")?;
    *data = tail;
    Ok(bytes)
}

// Uncompressed blocks start with an unsigned variable-length payload size.
fn unpacked_length(data: &[u8]) -> Result<(usize, &[u8])> {
    let mut length = 0u64;
    for (index, &byte) in data.iter().take(5).enumerate() {
        length |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            ensure!(
                length <= u64::from(u32::MAX),
                "LEIF payload length is out of range"
            );
            return Ok((length as usize, &data[index + 1..]));
        }
    }
    anyhow::bail!("invalid LEIF variable-length size")
}

fn inflate(mut data: &[u8]) -> Result<Vec<u8>> {
    // Only this metadata block is inflated. Cap expansion even for malformed
    // PPLs, and require the zlib stream to match its advertised extent.
    const MAX_TABLE: usize = 16 * 1024 * 1024;
    let size = word(&mut data)?;
    ensure!(
        size <= MAX_TABLE,
        "LEIF reference table exceeds the supported size"
    );
    let mut decoder = flate2::bufread::ZlibDecoder::new(data);
    let mut bytes = Vec::new();
    decoder
        .by_ref()
        .take(size as u64 + 1)
        .read_to_end(&mut bytes)
        .context("invalid compressed LEIF reference table")?;
    ensure!(
        bytes.len() == size && decoder.total_in() == data.len() as u64,
        "LEIF reference table compression length mismatch"
    );
    Ok(bytes)
}

fn valid_library_id(id: &str) -> bool {
    id.len() == 38
        && id.starts_with('{')
        && id.ends_with('}')
        && id.as_bytes()[1..37]
            .iter()
            .enumerate()
            .all(|(i, byte)| match i {
                8 | 13 | 18 | 23 => *byte == b'-',
                _ => byte.is_ascii_hexdigit(),
            })
}

fn reference_path(path: &Pth) -> Result<(String, String)> {
    let parts = path
        .comps
        .iter()
        .map(|part| std::str::from_utf8(part))
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(
        parts
            .iter()
            .all(|part| !part.contains(['/', '\\']) && !part.chars().any(char::is_control)),
        "invalid LEIF packed library path component"
    );
    let name = parts
        .last()
        .filter(|name| {
            name.rsplit_once('.')
                .is_some_and(|(stem, ext)| !stem.is_empty() && ext.eq_ignore_ascii_case("lvlibp"))
        })
        .context("LEIF packed library reference has no .lvlibp filename")?;
    let reference = match (path.tp, parts.as_slice()) {
        (1, ["", tail @ ..]) | (1, tail) => tail
            .iter()
            .map(|part| if part.is_empty() { ".." } else { part })
            .collect::<Vec<_>>()
            .join("/"),
        (0, [drive, tail @ ..]) if path.is_drive() => format!("{drive}:/{}", tail.join("/")),
        (0, parts) => parts.join("/"),
        _ => anyhow::bail!("unsupported LEIF packed library path type {}", path.tp),
    };
    Ok(((*name).to_owned(), reference))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_a_compressed_raw_leif_table_and_rejects_trailing_data_in_its_stream() {
        let name = b"toolkit.lvlibp";
        let library_id = b"{01234567-89AB-CDEF-0123-456789ABCDEF}";
        let mut table = 1u32.to_be_bytes().to_vec();
        table.extend_from_slice(b"PTH0");
        table.extend_from_slice(&((name.len() + 7) as u32).to_be_bytes());
        table.extend_from_slice(&[0, 1, 0, 3, 0, 0, name.len() as u8]);
        table.extend_from_slice(name);
        table.extend_from_slice(&(library_id.len() as u32).to_be_bytes());
        table.extend_from_slice(library_id);
        table.insert(0, table.len().try_into().unwrap());

        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&table).unwrap();
        let mut compressed = (table.len() as u32).to_be_bytes().to_vec();
        compressed.extend(encoder.finish().unwrap());
        let wrap = |block: &[u8]| {
            let mut bytes = b"LEIF\0\0\0\x22\x12\x09".to_vec();
            bytes.resize(42, 0); // Header and two empty blocks.
            bytes.extend_from_slice(&(block.len() as u32).to_be_bytes());
            bytes.extend_from_slice(block);
            let size = (bytes.len() as u32).to_be_bytes();
            bytes[10..14].copy_from_slice(&size);
            bytes
        };

        let library = read(&wrap(&compressed)).unwrap();
        assert!(library.version.is_none());
        assert_eq!(library.imports.len(), 1);
        assert_eq!(library.imports.first().unwrap().path, "../toolkit.lvlibp");

        compressed.push(0);
        assert!(read(&wrap(&compressed)).is_err());
    }
}
