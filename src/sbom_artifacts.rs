//! Inventory explicitly selected distribution files and native project sources.

use crate::sbom_paths::{OutputGuard, diagnostic_path};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, Metadata};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug)]
pub(crate) struct Inventory {
    pub scopes: Vec<Scope>,
    pub files: Vec<ArtifactFile>,
    directories: Vec<(PathBuf, Vec<PathBuf>)>,
    _snapshots: Snapshots,
}

impl Inventory {
    /// Recheck the original selection immediately before replacing the SBOM.
    pub(crate) fn verify_sources(&self) -> Result<()> {
        for artifact in &self.files {
            let (digest, size) = hash_file(&artifact.path, None)
                .with_context(|| format!("rechecking inventoried file {:?}", artifact.key))?;
            ensure!(
                digest == artifact.sha256 && size == artifact.size,
                "file {:?} changed after it was inventoried",
                artifact.key
            );
        }
        for (path, entries) in &self.directories {
            ensure!(
                directory_entries(path)? == *entries,
                "artifact directory contents changed after they were inventoried: {}",
                diagnostic_path(path)
            );
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct Scope {
    pub key: String,
    pub directory: bool,
}

#[derive(Debug)]
pub(crate) struct ArtifactFile {
    pub key: String,
    path: PathBuf,
    inspection: Option<PathBuf>,
    pub sha256: String,
    pub size: u64,
}

impl ArtifactFile {
    /// Use the same bytes for the digest and all format-specific readers.
    pub(crate) fn inspection_path(&self) -> &Path {
        self.inspection
            .as_deref()
            .expect("inspected artifact has a snapshot")
    }
}

#[derive(Debug, Default)]
struct Snapshots {
    root: Option<PathBuf>,
    #[cfg(windows)]
    msi_root: Option<PathBuf>,
    #[cfg(windows)]
    selected_directories: Vec<PathBuf>,
}

impl Snapshots {
    fn create(&mut self, output: &Path, artifact: &Path, index: usize) -> Result<(PathBuf, File)> {
        if self.root.is_none() {
            let parent = output.parent().context("SBOM output has no parent")?;
            self.root = Some(create_snapshot_dir(parent, output)?);
        }
        let root = self.root.as_ref().expect("snapshot directory created");
        let directory = root.join(index.to_string());
        let name = artifact
            .file_name()
            .context("expected an artifact filename")?;
        let path = directory.join(name);
        #[cfg(windows)]
        if name
            .to_str()
            .is_some_and(|name| has_extension(name, ".msi"))
            && !msi_path_fits(&path)
        {
            let path = self.short_msi_path(output, index)?;
            let file = File::create_new(&path)?;
            return Ok((path, file));
        }
        fs::create_dir(&directory)?;
        let file = File::create_new(&path)?;
        Ok((path, file))
    }

    #[cfg(windows)]
    fn short_msi_path(&mut self, output: &Path, index: usize) -> Result<PathBuf> {
        // Windows Installer may reject a long database path even though Rust
        // can open that path. MSI table evidence does not use the basename.
        if self.msi_root.is_none() {
            let temp = canonicalize(&std::env::temp_dir())?;
            ensure!(
                !self
                    .selected_directories
                    .iter()
                    .any(|dir| temp.starts_with(dir)),
                "the Windows temporary directory for MSI snapshots is inside a selected artifact directory"
            );
            self.msi_root = Some(create_snapshot_dir(&temp, output)?);
        }
        let path = self
            .msi_root
            .as_ref()
            .expect("MSI snapshot directory created")
            .join(format!("{index}.msi"));
        ensure!(
            msi_path_fits(&path),
            "the temporary path is too long for Windows Installer"
        );
        Ok(path)
    }
}

#[cfg(windows)]
fn msi_path_fits(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().count() < 260
}

impl Drop for Snapshots {
    fn drop(&mut self) {
        if let Some(root) = &self.root {
            let _ = fs::remove_dir_all(root);
        }
        #[cfg(windows)]
        if let Some(root) = &self.msi_root {
            let _ = fs::remove_dir_all(root);
        }
    }
}

fn create_snapshot_dir(parent: &Path, output: &Path) -> Result<PathBuf> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    loop {
        let path = parent.join(format!(
            ".lvpm-sbom-snapshot-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        if path == output {
            continue;
        }
        match builder.create(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).context("creating a temporary artifact snapshot directory");
            }
        }
    }
}

fn requires_snapshot(key: &str) -> bool {
    [".exe", ".dll", ".lvlibp", ".nipkg", ".msi"]
        .into_iter()
        .any(|extension| has_extension(key, extension))
}

pub(crate) fn has_extension(name: &str, extension: &str) -> bool {
    name.get(name.len().saturating_sub(extension.len())..)
        .is_some_and(|actual| actual.eq_ignore_ascii_case(extension))
}

/// Paths are resolved against the working directory. The optional root only
/// controls their portable names in the inventory.
pub(crate) fn scan(
    paths: &[PathBuf],
    root: Option<&Path>,
    guard: &OutputGuard,
) -> Result<Inventory> {
    ensure!(!paths.is_empty(), "select at least one build artifact");
    let mut selected = BTreeMap::new();
    for path in paths {
        let metadata = artifact_metadata(path)?;
        let canonical = canonicalize(path)?;
        ensure!(
            selected.insert(canonical, metadata.is_dir()).is_none(),
            "build artifact selected more than once: {}",
            diagnostic_path(path)
        );
    }
    for path in selected.keys() {
        if let Some(parent) = path
            .ancestors()
            .skip(1)
            .find(|parent| selected.contains_key(*parent))
        {
            anyhow::bail!(
                "build artifact {} overlaps selected directory {}",
                diagnostic_path(path),
                diagnostic_path(parent)
            );
        }
    }
    let base = match root {
        Some(root) => {
            ensure!(
                artifact_metadata(root)?.is_dir(),
                "artifact root is not a directory: {}",
                diagnostic_path(root)
            );
            canonicalize(root)?
        }
        None => common_parent(selected.keys().map(PathBuf::as_path))?,
    };

    let output = guard.destination();
    let mut scopes = Vec::new();
    for (path, directory) in &selected {
        if *directory {
            ensure!(
                !output.starts_with(path),
                "SBOM output is inside selected artifact directory {}",
                diagnostic_path(path)
            );
        }
        scopes.push(Scope {
            key: logical_key(path, &base)?,
            directory: *directory,
        });
    }
    scopes.sort_by(|left, right| left.key.cmp(&right.key));

    let mut snapshots = Snapshots {
        root: None,
        #[cfg(windows)]
        msi_root: None,
        #[cfg(windows)]
        selected_directories: selected
            .iter()
            .filter(|(_, is_dir)| **is_dir)
            .map(|(path, _)| path.clone())
            .collect(),
    };
    let mut pending = selected.into_iter().rev().collect::<Vec<_>>();
    let mut files = Vec::new();
    let mut directories = Vec::new();
    while let Some((path, directory)) = pending.pop() {
        if directory {
            let children = directory_entries(&path)?;
            for child in children.iter().rev() {
                let metadata = artifact_metadata(child)?;
                pending.push((child.clone(), metadata.is_dir()));
            }
            directories.push((path, children));
        } else {
            guard.protect(&path)?;
            let key = logical_key(&path, &base)?;
            let file = inventory_file(path, key, &output, &mut snapshots, files.len())?;
            files.push(file);
        }
    }
    files.sort_by(|left, right| left.key.cmp(&right.key));
    Ok(Inventory {
        scopes,
        files,
        directories,
        _snapshots: snapshots,
    })
}

/// Capture native project inputs without inferring a shared artifact root or
/// claiming these files are part of a shipped output.
pub(crate) fn scan_project_dlls(
    paths: &BTreeMap<String, PathBuf>,
    guard: &OutputGuard,
) -> Result<Inventory> {
    let output = guard.destination();
    let mut snapshots = Snapshots::default();
    let mut files = Vec::with_capacity(paths.len());
    for (key, path) in paths {
        guard.protect(path)?;
        let file = inventory_file(
            path.clone(),
            key.clone(),
            &output,
            &mut snapshots,
            files.len(),
        )?;
        files.push(file);
    }

    Ok(Inventory {
        scopes: Vec::new(),
        files,
        directories: Vec::new(),
        _snapshots: snapshots,
    })
}

fn inventory_file(
    path: PathBuf,
    key: String,
    output: &Path,
    snapshots: &mut Snapshots,
    index: usize,
) -> Result<ArtifactFile> {
    let (sha256, size, inspection) = (|| {
        if requires_snapshot(&key) {
            let (inspection, mut file) = snapshots.create(output, &path, index)?;
            let (sha256, size) = hash_file(&path, Some(&mut file))?;
            Ok::<_, anyhow::Error>((sha256, size, Some(inspection)))
        } else {
            let (sha256, size) = hash_file(&path, None)?;
            Ok((sha256, size, None))
        }
    })()
    .with_context(|| format!("reading inventoried file {}", diagnostic_path(&path)))?;
    Ok(ArtifactFile {
        key,
        path,
        inspection,
        sha256,
        size,
    })
}

fn directory_entries(path: &Path) -> Result<Vec<PathBuf>> {
    ensure!(
        artifact_metadata(path)?.is_dir(),
        "artifact directory changed while being inventoried: {}",
        diagnostic_path(path)
    );
    let mut children = fs::read_dir(path)
        .with_context(|| format!("reading artifact directory {}", diagnostic_path(path)))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("listing artifact directory {}", diagnostic_path(path)))?;
    children.sort_unstable();
    Ok(children)
}

fn canonicalize(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("resolving artifact path {}", diagnostic_path(path)))
}

fn common_parent<'a>(mut paths: impl Iterator<Item = &'a Path>) -> Result<PathBuf> {
    let first = paths.next().context("select at least one build artifact")?;
    // A directory is named in the inventory just like a selected file.
    let mut base = first.parent().context("an artifact root is required")?;
    for path in paths {
        while path == base || !path.starts_with(base) {
            base = base.parent().context(
                "artifact paths must share a directory; stage them together under --artifact-root",
            )?;
        }
    }
    // Inferring an entire volume for unrelated outputs can expose machine paths.
    ensure!(
        base.parent().is_some(),
        "artifact paths share only a filesystem root; supply --artifact-root explicitly"
    );
    Ok(base.to_path_buf())
}

fn logical_key(path: &Path, root: &Path) -> Result<String> {
    let relative = path.strip_prefix(root).with_context(|| {
        format!(
            "artifact path {} is outside root {}",
            diagnostic_path(path),
            diagnostic_path(root)
        )
    })?;
    if relative.as_os_str().is_empty() {
        return Ok(".".into());
    }
    let mut parts = Vec::new();
    for part in relative {
        let part = part
            .to_str()
            .with_context(|| format!("artifact path is not UTF-8: {}", diagnostic_path(path)))?;
        ensure!(
            !part.contains('\\'),
            "artifact path has a non-portable backslash: {}",
            diagnostic_path(path)
        );
        parts.push(part);
    }
    Ok(parts.join("/"))
}

fn artifact_metadata(path: &Path) -> Result<Metadata> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting build artifact {}", diagnostic_path(path)))?;
    ensure!(
        !is_link(&metadata) && (metadata.is_file() || metadata.is_dir()),
        "artifact must be a regular file or directory without redirects: {}",
        diagnostic_path(path)
    );
    Ok(metadata)
}

#[cfg(windows)]
fn is_link(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn hash_file(path: &Path, mut snapshot: Option<&mut File>) -> Result<(String, u64)> {
    let before = artifact_metadata(path)?;
    ensure!(before.is_file(), "expected a regular artifact file");

    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut size = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let bytes = &buffer[..count];
        if let Some(file) = &mut snapshot {
            file.write_all(bytes)?;
        }
        digest.update(bytes);
        size += count as u64;
    }
    let after = artifact_metadata(path)?;
    ensure!(
        after.is_file()
            && size == before.len()
            && size == after.len()
            && before.modified().ok() == after.modified().ok(),
        "artifact changed while it was being read"
    );
    let hex = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok((hex, size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspected_bytes_stay_bound_to_the_digest_after_the_source_changes() {
        let root = scratch("changed-content");
        let artifact = root.join("app.dll");
        fs::write(&artifact, b"first").unwrap();
        let modified = fs::metadata(&artifact).unwrap().modified().unwrap();
        let guard = OutputGuard::new(&root.join("result.cdx.json")).unwrap();
        let inventory = scan(std::slice::from_ref(&artifact), None, &guard).unwrap();
        let file = &inventory.files[0];
        let snapshot = file.inspection_path().to_owned();
        assert_eq!(snapshot.file_name(), artifact.file_name());
        assert_eq!(fs::read(&snapshot).unwrap(), b"first");
        assert_eq!(file.sha256, crate::cache::sha256_hex(b"first"));
        inventory.verify_sources().unwrap();

        // A build can rewrite a file and preserve its timestamps and length.
        fs::write(&artifact, b"other").unwrap();
        File::options()
            .write(true)
            .open(&artifact)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert_eq!(fs::read(&snapshot).unwrap(), b"first");
        let error = inventory.verify_sources().unwrap_err().to_string();
        assert!(
            error.contains("changed after it was inventoried"),
            "{error}"
        );
        drop(inventory);
        assert!(!snapshot.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recheck_rejects_new_members_in_an_inventoried_directory() {
        let root = scratch("new-member");
        let delivery = root.join("delivery");
        fs::create_dir(&delivery).unwrap();
        fs::write(delivery.join("original.dat"), b"original").unwrap();
        let guard = OutputGuard::new(&root.join("result.cdx.json")).unwrap();
        let inventory = scan(std::slice::from_ref(&delivery), None, &guard).unwrap();
        inventory.verify_sources().unwrap();
        fs::write(delivery.join("extra.dat"), b"extra").unwrap();
        let error = inventory.verify_sources().unwrap_err().to_string();
        assert!(error.contains("directory contents changed"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn msi_snapshot_uses_a_short_path_when_the_output_directory_is_deep() {
        use std::os::windows::ffi::OsStrExt;

        let root = scratch("long-msi-output");
        let artifact = root.join("installer-with-a-realistic-filename.msi");
        fs::write(&artifact, b"fixture bytes").unwrap();
        let padding = 225usize
            .saturating_sub(root.as_os_str().encode_wide().count())
            .max(1);
        let output_directory = root.join("d".repeat(padding));
        fs::create_dir(&output_directory).unwrap();
        let guard = OutputGuard::new(&output_directory.join("result.cdx.json")).unwrap();
        let inventory = scan(std::slice::from_ref(&artifact), None, &guard).unwrap();
        let staged = inventory.files[0].inspection_path().to_owned();
        assert!(msi_path_fits(&staged));
        assert!(!staged.starts_with(canonicalize(&output_directory).unwrap()));
        assert_eq!(fs::read(&staged).unwrap(), b"fixture bytes");
        inventory.verify_sources().unwrap();
        drop(inventory);
        assert!(!staged.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inventories_explicit_files_and_directory_members_in_key_order() {
        let root = scratch("paths");
        let left = root.join("folder one");
        let right = root.join("folder two");
        fs::create_dir(&left).unwrap();
        fs::create_dir(&right).unwrap();
        let left_file = left.join("example.exe");
        let right_file = right.join("example.exe");
        let hidden = root.join("excluded.vi");
        fs::write(&left_file, b"abc").unwrap();
        fs::write(&right_file, b"abcd").unwrap();
        fs::write(&hidden, b"not selected").unwrap();
        let guard = OutputGuard::new(&root.join("result.cdx.json")).unwrap();

        let inventory = scan(&[right_file.clone(), left.clone()], None, &guard).unwrap();
        let keys = inventory
            .files
            .iter()
            .map(|file| &*file.key)
            .collect::<Vec<_>>();
        assert_eq!(keys, ["folder one/example.exe", "folder two/example.exe"]);
        assert_eq!(inventory.scopes[0].key, "folder one");
        assert!(inventory.scopes[0].directory);
        assert_eq!(inventory.scopes[1].key, "folder two/example.exe");
        assert!(!inventory.scopes[1].directory);
        assert_eq!(inventory.files[0].path, left_file.canonicalize().unwrap());
        assert_eq!(inventory.files[0].size, 3);
        assert_eq!(
            inventory.files[0].sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_ne!(inventory.files[0].sha256, inventory.files[1].sha256);

        drop(inventory);
        fs::remove_file(hidden).unwrap();
        fs::remove_file(left_file).unwrap();
        fs::remove_file(right_file).unwrap();
        fs::remove_dir(left).unwrap();
        fs::remove_dir(right).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn rejects_missing_overlapping_unscoped_and_output_paths() {
        let root = scratch("scope");
        let payload = root.join("payload");
        fs::create_dir(&payload).unwrap();
        let file = payload.join("file");
        fs::write(&file, b"selected").unwrap();
        let guard = OutputGuard::new(&root.join("result.cdx.json")).unwrap();

        let inventory = scan(std::slice::from_ref(&payload), Some(&payload), &guard).unwrap();
        assert_eq!(inventory.scopes[0].key, ".");
        assert_eq!(inventory.files[0].key, "file");
        assert!(scan(&[root.join("missing")], None, &guard).is_err());
        assert!(scan(&[payload.clone(), file.clone()], Some(&root), &guard).is_err());
        let outside = root.join("outside.txt");
        fs::write(&outside, b"outside").unwrap();
        assert!(scan(std::slice::from_ref(&outside), Some(&payload), &guard).is_err());

        let inside = OutputGuard::new(&payload.join("result.cdx.json")).unwrap();
        assert!(scan(std::slice::from_ref(&payload), None, &inside).is_err());
        let alias = root.join("alias.cdx.json");
        fs::hard_link(&file, &alias).unwrap();
        let alias_guard = OutputGuard::new(&alias).unwrap();
        assert!(
            scan(std::slice::from_ref(&file), None, &alias_guard)
                .unwrap_err()
                .is::<crate::sbom_paths::OutputConflict>()
        );

        fs::remove_file(alias).unwrap();
        fs::remove_file(outside).unwrap();
        fs::remove_file(file).unwrap();
        fs::remove_dir(payload).unwrap();
        fs::remove_dir(root).unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lvpm-sbom-artifact-{name}-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        dir
    }
}
