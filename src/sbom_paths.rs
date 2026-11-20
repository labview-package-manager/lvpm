//! Display portable SBOM diagnostics and protect the files used to produce a BOM.

use anyhow::{Context, Result};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) fn diagnostic_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if path.is_absolute() || starts_absolute_path(&text) {
        return path.file_name().map_or_else(
            || path_basename(&text).to_owned(),
            |name| path_basename(&name.to_string_lossy()).to_owned(),
        );
    }
    text.replace('\\', "/")
}

/// Preserve useful names in portable references without exposing host directories.
pub(crate) fn portable_reference(reference: &str) -> String {
    if starts_absolute_path(reference) {
        format!("external/{}", path_basename(reference))
    } else {
        reference.to_owned()
    }
}

fn file_uri(text: &str) -> bool {
    text.get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:/"))
}

fn starts_absolute_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    file_uri(text)
        || text.starts_with('/')
        || text.starts_with('\\')
        || matches!(bytes, [drive, b':', b'/' | b'\\', ..] if drive.is_ascii_alphabetic())
}

fn path_basename(path: &str) -> &str {
    let path = if file_uri(path) {
        let address = &path[5..];
        address.split(['?', '#']).next().unwrap_or(address)
    } else {
        path
    };
    let basename = path
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
    if basename.is_empty()
        || matches!(basename, "." | ".." | "?" | "UNC")
        || matches!(basename.as_bytes(), [drive, b':'] if drive.is_ascii_alphabetic())
    {
        "<path>"
    } else {
        basename
    }
}

/// Redact paths in error chains and in evidence supplied as free-form text.
/// Typed paths should use diagnostic_path so punctuation in a real name is preserved.
pub(crate) fn redact_paths(message: &str) -> String {
    let mut result = String::with_capacity(message.len());
    let mut cursor = 0;
    let mut previous = None;
    while let Some(character) = message[cursor..].chars().next() {
        let text = &message[cursor..];
        let boundary = previous.is_none_or(|previous: char| {
            previous.is_whitespace()
                || "=(<{[;,\"'`".contains(previous)
                || previous == ':' && !text.starts_with("//")
        });
        if boundary && starts_absolute_path(text) && !diagnostic_pointer(text, previous) {
            let quote = previous.filter(|previous| matches!(previous, '\'' | '"' | '`'));
            let length = path_end(text, quote);
            if length > 1 {
                result.push_str(&portable_reference(&text[..length]));
                cursor += length;
                previous = text[..length].chars().next_back();
                continue;
            }
        }
        result.push(character);
        cursor += character.len_utf8();
        previous = Some(character);
    }
    result
}

fn diagnostic_pointer(text: &str, previous: Option<char>) -> bool {
    // Graph validation starts its messages with an RFC 6901 pointer.
    if !matches!(previous, None | Some('\n')) || !text.starts_with('/') {
        return false;
    }
    let Some((pointer, _)) = text.split_once(": ") else {
        return false;
    };
    let root = pointer[1..].split('/').next().unwrap_or_default();
    !pointer.contains(char::is_whitespace)
        && matches!(
            root,
            "metadata"
                | "components"
                | "services"
                | "dependencies"
                | "compositions"
                | "annotations"
                | "vulnerabilities"
                | "formulation"
                | "bom-ref"
                | "externalReferences"
                | "properties"
        )
}

fn path_end(text: &str, quote: Option<char>) -> usize {
    let mut parentheses = 0;
    let mut brackets = 0;
    for (index, character) in text.char_indices() {
        if matches!(character, '\r' | '\n' | '\t') {
            return index;
        }
        if let Some(quote) = quote {
            if character == quote {
                let backslashes = text.as_bytes()[..index]
                    .iter()
                    .rev()
                    .take_while(|&&byte| byte == b'\\')
                    .count();
                if backslashes % 2 == 0 {
                    return index;
                }
            }
            continue;
        }
        match character {
            '"' => return index,
            ';' if text[index + 1..].is_empty()
                || text[index + 1..].starts_with(char::is_whitespace) =>
            {
                return index;
            }
            '(' => parentheses += 1,
            '[' => brackets += 1,
            ')' if parentheses == 0 => return index,
            ')' => parentheses -= 1,
            ']' if brackets == 0 => return index,
            ']' => brackets -= 1,
            '}' | '>' => return index,
            ':' if text[index + 1..].starts_with(char::is_whitespace) => return index,
            _ => {}
        }
    }
    text.trim_end().len()
}

#[derive(Debug)]
pub(crate) enum OutputConflict {
    InputAlias { input: PathBuf, output: PathBuf },
    InvalidDestination { path: PathBuf, reason: &'static str },
}

impl fmt::Display for OutputConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputAlias { input, output } => write!(
                f,
                "SBOM output {} would overwrite input {}",
                diagnostic_path(output),
                diagnostic_path(input)
            ),
            Self::InvalidDestination { path, reason } => {
                write!(f, "invalid SBOM output {}: {reason}", diagnostic_path(path))
            }
        }
    }
}

impl std::error::Error for OutputConflict {}

pub(crate) struct OutputGuard {
    output: PathBuf,
    parent: PathBuf,
    identity: Option<FileIdentity>,
}

impl OutputGuard {
    pub(crate) fn new(output: &Path) -> Result<Self> {
        let invalid = |reason| OutputConflict::InvalidDestination {
            path: output.to_path_buf(),
            reason,
        };
        let name = output
            .file_name()
            .ok_or_else(|| invalid("the output must name a file"))?;
        #[cfg(windows)]
        if invalid_windows_name(name) {
            return Err(invalid("unsupported Windows filename or file stream").into());
        }
        #[cfg(not(windows))]
        let _ = name;

        let parent = output
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .canonicalize()
            .with_context(|| {
                format!(
                    "resolving parent directory of SBOM output {}",
                    output.display()
                )
            })?;
        if !parent.is_dir() {
            return Err(invalid("the parent is not a directory").into());
        }
        let entry = fs::symlink_metadata(output);
        let identity = match entry {
            Ok(metadata) => {
                let valid_file = if metadata.is_symlink() {
                    // A dangling link is still an existing destination.
                    match fs::metadata(output) {
                        Ok(target) => target.is_file(),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                        Err(error) => {
                            return Err(error).with_context(|| {
                                format!("inspecting SBOM output {}", output.display())
                            });
                        }
                    }
                } else {
                    metadata.is_file()
                };
                if !valid_file {
                    return Err(invalid("the destination is not a regular file").into());
                }
                Some(file_identity(output).with_context(|| {
                    format!(
                        "determining file identity of SBOM output {}",
                        output.display()
                    )
                })?)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspecting SBOM output {}", output.display()));
            }
        };
        Ok(Self {
            output: output.to_path_buf(),
            parent,
            identity,
        })
    }

    pub(crate) fn destination(&self) -> PathBuf {
        self.parent.join(
            self.output
                .file_name()
                .expect("validated SBOM output filename"),
        )
    }

    pub(crate) fn protect(&self, input: &Path) -> Result<()> {
        let Some(output) = self.identity else {
            return Ok(());
        };
        let input_identity = match file_identity(input) {
            Ok(identity) => identity,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                // Optional or virtual inputs may lack a physical file.
                return Ok(());
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "determining file identity of SBOM input {}",
                        input.display()
                    )
                });
            }
        };
        if input_identity == output {
            return Err(OutputConflict::InputAlias {
                input: input.to_path_buf(),
                output: self.output.clone(),
            }
            .into());
        }
        Ok(())
    }

    /// Check that the destination still names the same file, if any, before writing.
    pub(crate) fn recheck(&self) -> Result<()> {
        let current = Self::new(&self.output)?;
        if current.identity != self.identity || current.parent != self.parent {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "SBOM output {} changed while building the document",
                    self.output.display()
                ),
            )
            .into());
        }
        Ok(())
    }
}

#[cfg(windows)]
fn invalid_windows_name(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    if name.ends_with(['.', ' ']) || name.contains(':') {
        return true;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
}

#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity(u64, u64);

#[cfg(unix)]
fn file_identity(path: &Path) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::metadata(path)?;
    Ok(FileIdentity(metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    volume_serial_number: u64,
    file_id: [u8; 16],
}

#[cfg(windows)]
fn file_identity(path: &Path) -> io::Result<FileIdentity> {
    use std::os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, RawHandle},
    };

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandleEx(
            file: RawHandle,
            class: i32,
            information: *mut FileIdentity,
            size: u32,
        ) -> i32;
    }

    const FILE_ID_INFO: i32 = 18;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x02000000;
    // Zero desired access retrieves metadata without reading file contents.
    // BACKUP_SEMANTICS allows directory handles as well.
    let file = fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let mut identity = FileIdentity {
        volume_serial_number: 0,
        file_id: [0; 16],
    };
    // SAFETY: The handle is live for this call and the buffer has the layout
    // and length of the Windows FILE_ID_INFO structure.
    let status = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FILE_ID_INFO,
            &mut identity,
            std::mem::size_of::<FileIdentity>() as u32,
        )
    };
    if status == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("lvpm-sbom-paths-{name}-{}", std::process::id()));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn detects_normalized_and_hardlinked_inputs() {
        let dir = scratch("aliases");
        let input = dir.join("package.nipkg");
        let alias = dir.join("result with spaces.json");
        fs::write(&input, b"original").unwrap();
        fs::hard_link(&input, &alias).unwrap();

        let guard = OutputGuard::new(&alias).unwrap();
        assert!(
            guard
                .protect(&dir.join(".").join("package.nipkg"))
                .unwrap_err()
                .is::<OutputConflict>()
        );
        let guard = OutputGuard::new(&input).unwrap();
        assert!(guard.protect(&input).unwrap_err().is::<OutputConflict>());
        assert_eq!(fs::read(&input).unwrap(), b"original");
        assert_eq!(fs::read(&alias).unwrap(), b"original");

        fs::remove_file(alias).unwrap();
        fs::remove_file(input).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn rejects_non_file_destination_and_detects_changes() {
        let dir = scratch("destination");
        let output = dir.join("result.json");
        let input = dir.join("package.nipkg");
        fs::write(&input, b"original").unwrap();

        assert!(matches!(OutputGuard::new(&dir), Err(error) if error.is::<OutputConflict>()));
        assert!(OutputGuard::new(&dir.join("missing").join("result.json")).is_err());
        let guard = OutputGuard::new(&output).unwrap();
        guard.protect(&input).unwrap();
        guard.recheck().unwrap();

        fs::write(&output, b"another writer").unwrap();
        assert!(guard.recheck().is_err());
        let guard = OutputGuard::new(&output).unwrap();
        guard.protect(&input).unwrap();
        guard.protect(&dir.join("missing")).unwrap();

        fs::remove_file(output).unwrap();
        fs::remove_file(input).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn detects_symlinked_inputs_and_rejects_dangling_destinations() {
        use std::os::unix::fs::symlink;

        let dir = scratch("symlinks");
        let input = dir.join("package.nipkg");
        let alias = dir.join("result.json");
        fs::write(&input, b"original").unwrap();
        symlink(&input, &alias).unwrap();

        let guard = OutputGuard::new(&alias).unwrap();
        assert!(guard.protect(&input).unwrap_err().is::<OutputConflict>());
        fs::remove_file(input).unwrap();
        assert!(matches!(OutputGuard::new(&alias), Err(error) if error.is::<OutputConflict>()));

        fs::remove_file(alias).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn diagnostics_keep_names_and_pointers_without_absolute_paths() {
        let message = concat!(
            r#"/dependencies/0/ref: unknown "C:\\Users\\Private Space\\driver.vi"; "#,
            r#"archive=\\private-server\private-share\setup.cab; "#,
            "file:/home/private/manual.cdx.json; ",
            "path:/opt/private/data.vi; path:D:\\Private\\document.vi; ",
            r"reading D:\Private's QA\part;two\source.vi: denied; ",
            "see https://example.org/packages/readme and project/main.vi"
        );
        let portable = redact_paths(message);
        for visible in [
            "/dependencies/0/ref:",
            "external/driver.vi",
            "external/setup.cab",
            "external/manual.cdx.json",
            "path:external/data.vi",
            "path:external/document.vi",
            "external/source.vi: denied",
            "https://example.org/packages/readme",
            "project/main.vi",
        ] {
            assert!(portable.contains(visible), "{portable}");
        }
        for hidden in ["Private", "private", r"C:\\", r"D:\", "/home/", "/opt/"] {
            assert!(!portable.contains(hidden), "{portable}");
        }
        assert_eq!(redact_paths(&portable), portable);
        assert_eq!(
            diagnostic_path(Path::new("/tmp/private/output.cdx.json")),
            "output.cdx.json"
        );
        assert_eq!(
            diagnostic_path(Path::new("bom/relative.cdx.json")),
            "bom/relative.cdx.json"
        );
    }

    #[cfg(windows)]
    #[test]
    fn rejects_windows_device_names_and_streams() {
        let dir = scratch("windows");
        for name in ["AUX.cdx.json", "COM¹", "result.json:stream", "result.json."] {
            assert!(
                matches!(OutputGuard::new(&dir.join(name)), Err(error) if error.is::<OutputConflict>())
            );
        }
        fs::remove_dir(dir).unwrap();
    }
}
