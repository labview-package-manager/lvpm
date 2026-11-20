//! Walk the file payload of an NI package and inspect selected TAR members.

use super::{PackedPayload, PayloadFile, PayloadInventory};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{self, Read};

const TAR_BLOCK: usize = 512;
const TAR_ATTRIBUTES_LIMIT: u64 = 1024 * 1024;
pub(super) const PPL_FILE_LIMIT: u64 = 128 * 1024 * 1024;
pub(super) const PPL_TOTAL_LIMIT: u64 = 256 * 1024 * 1024;
pub(super) const PPL_COUNT_LIMIT: usize = 4096;

// Cap the work of traversing any payload, including skipped members and TAR metadata.
const LIMITS: Limits = Limits {
    expanded_bytes: 64 * 1024 * 1024 * 1024,
    members: 65_536,
    path_bytes: 16 * 1024 * 1024,
};

struct Limits {
    expanded_bytes: u64,
    members: usize,
    path_bytes: usize,
}

#[derive(Default)]
struct Attributes {
    path: Option<Vec<u8>>,
    size: Option<u64>,
}

#[derive(Clone, Copy)]
pub(crate) enum ReadKind {
    Packed,
    Inventory,
    Both,
}

pub(super) fn read(reader: impl Read, kind: ReadKind) -> Result<(PackedPayload, PayloadInventory)> {
    read_with_limits(reader, kind, LIMITS)
}

fn read_with_limits(
    reader: impl Read,
    kind: ReadKind,
    limits: Limits,
) -> Result<(PackedPayload, PayloadInventory)> {
    let mut packed = matches!(kind, ReadKind::Packed | ReadKind::Both).then(PackedScan::default);
    let collect_inventory = matches!(kind, ReadKind::Inventory | ReadKind::Both);
    let mut entries = BTreeMap::<String, Option<PayloadFile>>::new();
    let mut issues = Vec::new();
    members(reader, limits, |raw_path, kind, size, body| {
        if kind == b'5' || !collect_inventory && !regular(kind) {
            return Ok(());
        }

        let path = collect_inventory
            .then(|| normalize_path(raw_path))
            .transpose()?;
        if let Some(path) = &path {
            ensure!(
                !entries.contains_key(path),
                "duplicate direct .nipkg payload path {path:?}"
            );
        }
        if !regular(kind) {
            let path = path.expect("special members are visited for inventory only");
            let description = match kind {
                b'1' => "hard link".to_owned(),
                b'2' => "symbolic link".to_owned(),
                _ => format!("TAR member type {kind:#04x}"),
            };
            issues.push(format!("omitted {description}: {path}"));
            entries.insert(path, None);
            return Ok(());
        }

        let mut body = MemberReader {
            body,
            digest: collect_inventory.then(Sha256::new),
        };
        let mut prefix = [0; 4];
        let prefix_len = size.min(prefix.len() as u64) as usize;
        body.read_exact(&mut prefix[..prefix_len])
            .context("reading payload file signature")?;
        let is_nested_archive = |path: &[u8]| {
            extension(path, b".msi") || extension(path, b".cab") || &prefix == b"MSCF"
        };
        // Replay the signature only to the consumer. Its bytes were already hashed.
        let mut data = prefix[..prefix_len].chain(&mut body);
        let consumed = packed
            .as_mut()
            .map(|scan| scan.inspect(raw_path, size, is_nested_archive(raw_path), &mut data))
            .transpose()?
            .unwrap_or(false);

        if let Some(path) = path {
            if !consumed {
                let copied = io::copy(&mut data, &mut io::sink())
                    .with_context(|| format!("hashing .nipkg payload file {path:?}"))?;
                ensure!(copied == size, "truncated payload tar member {path:?}");
            }
            let digest = body.digest.expect("file inventory enables hashing");
            let sha256 = digest
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let nested_archive = is_nested_archive(path.as_bytes());
            entries.insert(
                path,
                Some(PayloadFile {
                    sha256,
                    size,
                    nested_archive,
                }),
            );
        }
        Ok(())
    })?;

    let packed = packed
        .map(PackedScan::finish)
        .transpose()?
        .unwrap_or_default();
    let files = entries
        .into_iter()
        .filter_map(|(path, entry)| entry.map(|file| (path, file)))
        .collect();
    issues.sort();
    Ok((packed, PayloadInventory { files, issues }))
}

#[derive(Default)]
struct PackedScan {
    packed: PackedPayload,
    #[cfg(windows)]
    nested: super::nested::Staged,
    #[cfg(not(windows))]
    unsupported_nested: bool,
}

impl PackedScan {
    /// Return whether this consumed the complete member.
    fn inspect(
        &mut self,
        path: &[u8],
        size: u64,
        nested: bool,
        data: &mut dyn Read,
    ) -> Result<bool> {
        if extension(path, b".lvlibp") {
            let path = normalize_path(path)?;
            ensure!(
                size <= PPL_FILE_LIMIT,
                "packed library {path:?} exceeds the payload file limit"
            );
            let mut bytes = vec![0; size as usize];
            data.read_exact(&mut bytes)
                .with_context(|| format!("reading packed library {path:?} from payload tar"))?;
            self.packed.add(None, &path, bytes)?;
            return Ok(true);
        }
        if nested {
            #[cfg(windows)]
            {
                self.nested.stage(normalize_path(path)?, size, data)?;
                return Ok(true);
            }
            #[cfg(not(windows))]
            {
                self.unsupported_nested = true;
            }
        }
        Ok(false)
    }

    fn finish(mut self) -> Result<PackedPayload> {
        #[cfg(windows)]
        self.nested.scan(&mut self.packed)?;
        #[cfg(not(windows))]
        if self.unsupported_nested {
            self.packed
                .issues
                .push("scanning MSI and CAB payload members requires Windows".to_owned());
        }
        Ok(self.packed)
    }
}

struct MemberReader<'a> {
    body: &'a mut dyn Read,
    digest: Option<Sha256>,
}

impl Read for MemberReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.body.read(buffer)?;
        if let Some(digest) = &mut self.digest {
            digest.update(&buffer[..count]);
        }
        Ok(count)
    }
}

fn regular(kind: u8) -> bool {
    matches!(kind, 0 | b'0')
}

fn members(
    reader: impl Read,
    limits: Limits,
    mut visit: impl FnMut(&[u8], u8, u64, &mut dyn Read) -> Result<()>,
) -> Result<()> {
    const EXPANDED_LIMIT_ERROR: &str =
        "expanded .nipkg data archive exceeds the payload size limit";
    let mut reader = reader.take(limits.expanded_bytes + 1);
    let mut remaining_members = limits.members;
    let mut remaining_path_bytes = limits.path_bytes;
    let mut attributes = Attributes::default();
    loop {
        let mut header = [0; TAR_BLOCK];
        let read = reader.read_exact(&mut header);
        ensure!(reader.limit() != 0, "{EXPANDED_LIMIT_ERROR}");
        read.context("reading payload tar header")?;
        if header.iter().all(|byte| *byte == 0) {
            ensure!(
                attributes.path.is_none() && attributes.size.is_none(),
                "orphaned payload tar attributes"
            );
            let result = finish_archive(&mut reader);
            ensure!(reader.limit() != 0, "{EXPANDED_LIMIT_ERROR}");
            return result;
        }
        remaining_members = remaining_members
            .checked_sub(1)
            .context("too many .nipkg payload TAR entries")?;
        let header_path = header_path(&header);
        remaining_path_bytes = remaining_path_bytes
            .checked_sub(header_path.len())
            .context("too many path bytes in the .nipkg payload")?;
        let size = tar_member_size(&header)?;
        let kind = header[156];
        let size = if matches!(kind, b'L' | b'x' | b'g' | b'K') {
            size
        } else {
            attributes.size.take().unwrap_or(size)
        };
        ensure!(
            size.checked_add(padding(size))
                .is_some_and(|bytes| bytes < reader.limit()),
            "{EXPANDED_LIMIT_ERROR}"
        );
        match kind {
            b'L' | b'x' | b'g' => {
                ensure!(
                    size <= TAR_ATTRIBUTES_LIMIT,
                    "payload tar attributes are too large"
                );
                let mut bytes = vec![0; size as usize];
                reader
                    .read_exact(&mut bytes)
                    .context("reading payload tar attributes")?;
                if kind == b'L' {
                    let name = field_bytes(&bytes);
                    ensure!(!name.is_empty(), "empty GNU tar long name");
                    attributes.path = Some(name.to_vec());
                } else if kind == b'x' {
                    pax_attributes(&bytes, &mut attributes)?;
                } else {
                    let mut global = Attributes::default();
                    pax_attributes(&bytes, &mut global)?;
                    ensure!(
                        global.path.is_none() && global.size.is_none(),
                        "global payload tar path and size overrides are unsupported"
                    );
                }
                skip_bytes(&mut reader, padding(size))?;
            }
            // GNU long symlink targets are not files and do not change
            // the following member's logical name or length.
            b'K' => skip_padded(&mut reader, size)?,
            _ => {
                let path = if let Some(path) = attributes.path.take() {
                    remaining_path_bytes = remaining_path_bytes
                        .checked_sub(path.len())
                        .context("too many path bytes in the .nipkg payload")?;
                    Cow::Owned(path)
                } else {
                    header_path
                };
                let mut body = reader.by_ref().take(size);
                visit(&path, kind, size, &mut body)?;
                let remaining = body.limit();
                skip_bytes(&mut body, remaining)?;
                skip_bytes(&mut reader, padding(size))?;
            }
        }
    }
}

fn finish_archive(mut reader: impl Read) -> Result<()> {
    let mut buffer = [0; 8192];
    reader
        .read_exact(&mut buffer[..TAR_BLOCK])
        .context("reading payload tar end marker")?;
    ensure!(
        buffer[..TAR_BLOCK].iter().all(|byte| *byte == 0),
        "invalid payload tar end marker"
    );
    // Consume the entire bounded member so a decompressor checks its trailer.
    loop {
        let len = reader.read(&mut buffer).context("finishing payload tar")?;
        if len == 0 {
            return Ok(());
        }
        ensure!(
            buffer[..len].iter().all(|byte| *byte == 0),
            "unexpected data after payload tar end marker"
        );
    }
}

pub(super) fn tar_member_size(header: &[u8]) -> Result<u64> {
    let expected = tar_number(&header[148..156]).context("invalid tar checksum")?;
    let checksum: u64 = header
        .iter()
        .enumerate()
        .map(|(index, &byte)| {
            if (148..156).contains(&index) {
                u64::from(b' ')
            } else {
                u64::from(byte)
            }
        })
        .sum();
    ensure!(checksum == expected, "invalid tar header checksum");
    tar_number(&header[124..136]).context("invalid tar member size")
}

fn tar_number(bytes: &[u8]) -> Result<u64> {
    if bytes[0] & 0x80 != 0 {
        ensure!(bytes[0] == 0x80, "invalid GNU tar base-256 number");
        return bytes[1..].iter().try_fold(0u64, |value, &byte| {
            value
                .checked_mul(256)
                .and_then(|value| value.checked_add(u64::from(byte)))
                .context("tar number is too large")
        });
    }
    let text = std::str::from_utf8(bytes)?.trim_matches(['\0', ' ']);
    Ok(u64::from_str_radix(text, 8)?)
}

fn field_bytes(bytes: &[u8]) -> &[u8] {
    bytes.split(|byte| *byte == 0).next().unwrap_or_default()
}

fn header_path(header: &[u8; TAR_BLOCK]) -> Cow<'_, [u8]> {
    let name = field_bytes(&header[..100]);
    // With the GNU-specific magic this range also contains unrelated fields.
    if &header[257..263] == b"ustar\0" {
        let prefix = field_bytes(&header[345..500]);
        if !prefix.is_empty() {
            return Cow::Owned([prefix, b"/", name].concat());
        }
    }
    Cow::Borrowed(name)
}

fn pax_attributes(mut bytes: &[u8], attributes: &mut Attributes) -> Result<()> {
    while !bytes.is_empty() {
        let separator = bytes
            .iter()
            .position(|byte| *byte == b' ')
            .context("invalid PAX record length")?;
        let length: usize = std::str::from_utf8(&bytes[..separator])?
            .parse()
            .context("invalid PAX record length")?;
        let (entry, rest) = bytes
            .split_at_checked(length)
            .context("truncated PAX record")?;
        let fields = entry
            .get(separator + 1..)
            .and_then(|fields| fields.strip_suffix(b"\n"))
            .context("invalid PAX record")?;
        let delimiter = fields
            .iter()
            .position(|byte| *byte == b'=')
            .context("invalid PAX attribute")?;
        let value = &fields[delimiter + 1..];
        match &fields[..delimiter] {
            b"path" => attributes.path = (!value.is_empty()).then(|| value.to_vec()),
            b"size" => {
                attributes.size = if value.is_empty() {
                    None
                } else {
                    Some(
                        std::str::from_utf8(value)?
                            .parse()
                            .context("invalid PAX member size")?,
                    )
                };
            }
            field if field.starts_with(b"GNU.sparse.") => {
                anyhow::bail!("sparse payload tar members are unsupported")
            }
            _ => {}
        }
        bytes = rest;
    }
    Ok(())
}

fn extension(path: &[u8], extension: &[u8]) -> bool {
    path.len() > extension.len()
        && path[path.len() - extension.len()..].eq_ignore_ascii_case(extension)
}

pub(super) fn normalize_path(raw: &[u8]) -> Result<String> {
    ensure!(raw.len() <= 8192, "payload path is too long");
    let raw = std::str::from_utf8(raw).context("payload path is not UTF-8")?;
    ensure!(
        !raw.starts_with('/') && !raw.contains(['\\', ':']) && !raw.chars().any(char::is_control),
        "unsafe payload path {raw:?}"
    );
    let mut path = Vec::new();
    for part in raw.split('/') {
        ensure!(part != "..", "unsafe payload path {raw:?}");
        if !matches!(part, "" | ".") {
            path.push(part);
        }
    }
    ensure!(!path.is_empty(), "invalid payload path {raw:?}");
    Ok(path.join("/"))
}

fn padding(size: u64) -> u64 {
    (TAR_BLOCK as u64 - size % TAR_BLOCK as u64) % TAR_BLOCK as u64
}

fn skip_padded(reader: &mut impl Read, size: u64) -> Result<()> {
    skip_bytes(
        reader,
        size.checked_add(padding(size))
            .context("invalid payload tar member size")?,
    )
}

fn skip_bytes(reader: &mut impl Read, bytes: u64) -> Result<()> {
    let read = io::copy(&mut reader.take(bytes), &mut io::sink())
        .context("skipping payload tar member")?;
    ensure!(read == bytes, "truncated payload tar member or padding");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packed_libraries(reader: impl Read) -> Result<BTreeMap<String, Vec<u8>>> {
        Ok(read(reader, ReadKind::Packed)?
            .0
            .files
            .into_iter()
            .map(|(key, bytes)| (key.path, bytes))
            .collect())
    }

    fn entry(tar: &mut Vec<u8>, name: &[u8], prefix: &[u8], kind: u8, bytes: &[u8]) {
        let mut header = [0; TAR_BLOCK];
        header[..name.len()].copy_from_slice(name);
        header[124..135].copy_from_slice(format!("{:011o}", bytes.len()).as_bytes());
        header[148..156].fill(b' ');
        header[156] = kind;
        header[257..263].copy_from_slice(b"ustar\0");
        header[345..345 + prefix.len()].copy_from_slice(prefix);
        let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        header[148..154].copy_from_slice(format!("{checksum:06o}").as_bytes());
        header[154] = 0;
        tar.extend_from_slice(&header);
        tar.extend_from_slice(bytes);
        tar.resize(tar.len() + padding(bytes.len() as u64) as usize, 0);
    }

    #[test]
    fn reads_ustar_gnu_and_pax_paths_without_following_links() {
        let mut tar = Vec::new();
        entry(&mut tar, b"some.LVLIBP", b"./assets", b'0', b"ustar");
        entry(
            &mut tar,
            b"././@LongLink",
            b"",
            b'L',
            b"from/gnu/direct.lvlibp\0",
        );
        entry(&mut tar, b"short", b"", b'0', b"gnu");
        entry(
            &mut tar,
            b"PaxHeaders",
            b"",
            b'x',
            b"26 path=pax/member.lvlibp\n",
        );
        entry(&mut tar, b"other", b"", b'0', b"pax");
        entry(&mut tar, b"ignore.lvlibp", b"", b'2', b"");
        tar.resize(tar.len() + 2 * TAR_BLOCK, 0);

        let files = packed_libraries(tar.as_slice()).unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files["assets/some.LVLIBP"], b"ustar");
        assert_eq!(files["from/gnu/direct.lvlibp"], b"gnu");
        assert_eq!(files["pax/member.lvlibp"], b"pax");
    }

    #[test]
    fn inventories_regular_files_without_following_payload_links() {
        let mut tar = Vec::new();
        entry(&mut tar, b".", b"", b'5', b"");
        entry(&mut tar, b"./example.exe", b"./bin", b'0', b"abc");
        entry(&mut tar, b"./z/setup.MSI", b"", b'0', b"table");
        entry(&mut tar, b"./z/cabinet.c2", b"", b'0', b"MSCFarchive");
        entry(&mut tar, b"./z/shortcut", b"", b'2', b"");
        entry(&mut tar, b"./z/hardlink", b"", b'1', b"");
        tar.resize(tar.len() + 2 * TAR_BLOCK, 0);

        let (_, inventory) = read(tar.as_slice(), ReadKind::Inventory).unwrap();
        assert_eq!(
            inventory
                .files
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["bin/example.exe", "z/cabinet.c2", "z/setup.MSI"]
        );
        let file = &inventory.files["bin/example.exe"];
        assert_eq!(file.size, 3);
        assert_eq!(
            file.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(!file.nested_archive);
        assert!(inventory.files["z/cabinet.c2"].nested_archive);
        assert!(inventory.files["z/setup.MSI"].nested_archive);
        assert_eq!(
            inventory.issues,
            [
                "omitted hard link: z/hardlink",
                "omitted symbolic link: z/shortcut"
            ]
        );
    }

    #[test]
    fn combined_scan_hashes_the_same_member_bytes_it_collects() {
        let mut tar = Vec::new();
        entry(&mut tar, b"./plugins/core.lvlibp", b"", b'0', b"abc");
        entry(&mut tar, b"./bin/app.exe", b"", b'0', b"abcdef");
        tar.resize(tar.len() + 2 * TAR_BLOCK, 0);

        let (packed, inventory) = read(tar.as_slice(), ReadKind::Both).unwrap();
        assert_eq!(packed.files.values().next().unwrap(), b"abc");
        assert_eq!(
            inventory.files["plugins/core.lvlibp"].sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            inventory.files["bin/app.exe"].sha256,
            "bef57ec7f53a6d40beb640a780a639c83bc29ac8a9816f1fc6c5c6dcd93c4721"
        );
    }

    #[test]
    fn all_payload_modes_bound_bytes_and_count_directory_and_metadata_headers() {
        let mut tar = Vec::new();
        let directory = b".";
        let gnu_header = b"././@LongLink";
        let file = b"file";
        let long_path = b"folder/ignored.txt";
        entry(&mut tar, directory, b"", b'5', b"");
        entry(&mut tar, gnu_header, b"", b'L', long_path);
        entry(&mut tar, file, b"", b'0', b"ordinary file");
        tar.resize(tar.len() + 2 * TAR_BLOCK + 16, 0);

        let exact = Limits {
            expanded_bytes: tar.len() as u64,
            members: 3,
            path_bytes: directory.len() + gnu_header.len() + file.len() + long_path.len(),
        };
        for kind in [ReadKind::Packed, ReadKind::Inventory, ReadKind::Both] {
            assert!(read_with_limits(tar.as_slice(), kind, Limits { ..exact }).is_ok());
            for (limited, expected) in [
                (
                    Limits {
                        expanded_bytes: exact.expanded_bytes - 1,
                        ..exact
                    },
                    "payload size limit",
                ),
                (
                    Limits {
                        members: 2,
                        ..exact
                    },
                    "too many .nipkg payload TAR entries",
                ),
                (
                    Limits {
                        path_bytes: exact.path_bytes - 1,
                        ..exact
                    },
                    "too many path bytes",
                ),
            ] {
                let error = read_with_limits(tar.as_slice(), kind, limited)
                    .err()
                    .unwrap();
                assert!(error.to_string().contains(expected), "{error:#}");
            }
        }
    }

    #[test]
    fn rejects_ambiguous_payload_inventory_paths() {
        let mut tar = Vec::new();
        entry(&mut tar, b"bin/example.exe", b"", b'0', b"original");
        entry(&mut tar, b"./bin/./example.exe", b"", b'1', b"");
        assert!(
            read(tar.as_slice(), ReadKind::Inventory)
                .err()
                .unwrap()
                .to_string()
                .contains("duplicate")
        );
    }

    #[test]
    fn rejects_unsafe_and_duplicate_paths_and_truncated_tar() {
        for path in [
            b"app/../demo.lvlibp".as_slice(),
            b"/demo.lvlibp",
            b"C:\\demo.lvlibp",
        ] {
            let mut tar = Vec::new();
            entry(&mut tar, path, b"", b'0', b"a");
            assert!(
                packed_libraries(tar.as_slice())
                    .unwrap_err()
                    .to_string()
                    .contains("unsafe")
            );
        }

        let mut tar = Vec::new();
        entry(&mut tar, b"./app/demo.lvlibp", b"", b'0', b"a");
        assert!(
            packed_libraries(tar.as_slice())
                .unwrap_err()
                .to_string()
                .contains("tar header")
        );
        entry(&mut tar, b"app/./demo.lvlibp", b"", b'0', b"b");
        assert!(
            packed_libraries(tar.as_slice())
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );
    }
}
