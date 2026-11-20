//! Unpacking a `.vip` into a target, and taking it back out again.
//!
//! Install is a file copy: expand the `Target Dir` token against the target's
//! roots, join the file's relative path, write it. No LabVIEW process is
//! involved. An install manifest records absolute paths so uninstall can
//! remove exactly what was added and nothing else.

use crate::spec::Spec;
use crate::target::{LvTarget, Roots, TokenClass};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};

/// A hook's state on an installed package. A hook is owed from the moment
/// its package's files are copied until it has run, whichever command runs
/// it: an install that stops part way leaves `Pending` for the next one.
/// `Skipped` is a hook the hook policy held back (a venv runs none unless the
/// manifest says `hooks = "run"`); it runs once the policy allows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum HookState {
    Pending,
    Ran { at: u64 },
    Failed { at: u64, error: String },
    Skipped { at: u64 },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub display_name: Option<String>,
    /// Metadata captured from the installed archive. Older records omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_metadata: Option<PackageMetadata>,
    pub installed_at: u64,
    /// Where it went: a scratch prefix or a LabVIEW install dir.
    pub target: String,
    /// Absolute paths, forward-slashed.
    pub files: Vec<String>,
    /// Palette files staged outside mounted package addons, by path within
    /// the package's original `menus/` tree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub palette_files: Vec<PaletteFile>,
    /// Direct package requirements, including an empty list when there are none.
    pub dependencies: Vec<String>,
    /// Hooks the package declares that were not run: every one in a venv
    /// (hooks act on a LabVIEW installation, and a venv is not one), and
    /// globally only a PostInstall that failed. Uninstall hooks are not
    /// skipped, they are pending — `lvpm uninstall` runs them from the
    /// extracted copies below. Manifests older than that extraction list them
    /// here, which is how uninstall knows it has nothing to run.
    pub skipped_hooks: Vec<String>,
    /// Whether the relink pass has run over these files since they were
    /// copied. Defaults to false so manifests written before relinking existed
    /// read as "not relinked", which is what they are.
    #[serde(default)]
    pub relinked: bool,
    /// The package's own folders, as the spec asked for them — what the relink
    /// pass walks. Recorded here because the file list alone cannot say where
    /// the package boundary was.
    #[serde(default)]
    pub relink_folders: Vec<String>,
    /// Where the package's `PostInstall.vi` was extracted to, when it ships
    /// one. The archive carries hook VIs at its root under the hook's own
    /// name — the path in `[Script VIs]` is only where the VI lived on the
    /// build machine. Recorded so the run can happen after the relink pass,
    /// and be retried.
    #[serde(default)]
    pub post_install_vi: Option<String>,
    /// Where that PostInstall hook stands. `None` on records written before
    /// this was tracked: whether it ran is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_install: Option<HookState>,
    /// Where `PreInstall.vi` was extracted to, when the package ships one. It
    /// has already run by the time this manifest exists — kept for the record
    /// and so uninstall can clean it up.
    #[serde(default)]
    pub pre_install_vi: Option<String>,
    /// Where `PreUninstall.vi` / `PostUninstall.vi` were extracted to, when
    /// the package ships them. Extracted at install time — the archive is
    /// long gone by the time uninstall needs them.
    #[serde(default)]
    pub pre_uninstall_vi: Option<String>,
    #[serde(default)]
    pub post_uninstall_vi: Option<String>,
    /// File groups a venv install left out because they aim at the machine
    /// rather than the LabVIEW tree, as `Target Dir (N files)`.
    #[serde(default)]
    pub skipped_groups: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PackageMetadata {
    pub vendor: Option<String>,
    pub license: Option<String>,
    pub archive_md5: String,
    pub archive_sha256: String,
}

/// Which tree of the merged palette addon a palette file belongs to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PaletteTree {
    /// `<menus>`: the Functions and Controls palette trees.
    #[default]
    #[serde(rename = "menus")]
    Menus,
    /// `.mnu` files under `<vi.lib>\addons`, the Addons palette. Several
    /// packages share a category folder there (`addons\HSE`), and LabVIEW
    /// shows only one addon's copy of a folder.
    #[serde(rename = "vi.lib/addons")]
    Addons,
}

impl PaletteTree {
    /// The tree's folder, relative to an addon root.
    pub fn dir(self) -> &'static str {
        match self {
            PaletteTree::Menus => "menus",
            PaletteTree::Addons => "vi.lib/addons",
        }
    }

    fn is_menus(&self) -> bool {
        *self == PaletteTree::Menus
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaletteFile {
    /// Relative to the tree's folder.
    pub relative: String,
    /// Absent in records from before `vi.lib/addons` palettes were merged.
    #[serde(default, skip_serializing_if = "PaletteTree::is_menus")]
    pub tree: PaletteTree,
}

/// What an install *would* do. Produced first so `--dry-run` and the real run
/// share one code path.
#[derive(Debug)]
pub struct Plan {
    pub writes: Vec<PlannedWrite>,
    /// Files the package ships that were already on disk and left alone
    /// (`Replace Mode = If Newer`). Still part of its footprint: they go in
    /// the manifest so uninstall takes them with everything else.
    pub kept: Vec<PathBuf>,
    pub missing_from_archive: Vec<String>,
    /// Groups not written because a venv holds LabVIEW-tree content only:
    /// the `Target Dir` and how many files it named.
    pub skipped_groups: Vec<(String, usize)>,
    pub palette_files: Vec<PaletteFile>,
}

#[derive(Debug)]
pub struct PlannedWrite {
    pub zip_index: usize,
    pub dest: PathBuf,
    pub overwrites: bool,
}

/// Reject absolute paths and `..` traversal before we join anything.
fn safe_relative(rel: &str) -> Result<PathBuf> {
    let rel = rel.replace('\\', "/");
    if rel.starts_with('/') || rel.contains(':') {
        bail!("refusing absolute path in package: {rel:?}");
    }
    let mut out = PathBuf::new();
    for part in rel.split('/') {
        match part {
            "" | "." => continue,
            ".." => bail!("refusing path traversal in package: {rel:?}"),
            p => out.push(p),
        }
    }
    Ok(out)
}

pub fn manifest_path(roots: &Roots, name: &str) -> PathBuf {
    roots.store_dir().join(format!("{name}.json"))
}

pub fn is_installed(roots: &Roots, name: &str) -> bool {
    manifest_path(roots, name).exists()
}

pub fn read_manifest(roots: &Roots, name: &str) -> Result<Manifest> {
    let p = manifest_path(roots, name);
    let text = std::fs::read_to_string(&p)
        .with_context(|| format!("{name} is not installed in this target ({})", p.display()))?;
    serde_json::from_str(&text).with_context(|| format!("reading installation record {} (dependency list required)", p.display()))
}

pub fn list_installed(roots: &Roots) -> Result<Vec<Manifest>> {
    list_installed_with(roots, |_| Ok(()))
}

pub(crate) fn list_installed_with(
    roots: &Roots, check_path: impl FnMut(&Path) -> Result<()>,
) -> Result<Vec<Manifest>> {
    let (manifests, mut unreadable) = list_installed_lenient_with(roots, check_path)?;
    match unreadable.is_empty() {
        true => Ok(manifests),
        false => Err(unreadable.remove(0).1),
    }
}

/// An install record that could not be read, and why.
pub type Unreadable = (PathBuf, anyhow::Error);

/// Every record that reads, and each one that does not with the reason. For
/// a command that only looks the records over (the install's sweep for owed
/// hooks): one record from an older lvpm, or a damaged one, belongs to some
/// other package and must not stop it. A command that acts on a record
/// (`uninstall`, `list`, `run-hooks --all`) uses [`list_installed`], which
/// refuses instead.
pub fn list_installed_lenient(roots: &Roots) -> Result<(Vec<Manifest>, Vec<Unreadable>)> {
    list_installed_lenient_with(roots, |_| Ok(()))
}

pub(crate) fn list_installed_lenient_with(
    roots: &Roots, mut check_path: impl FnMut(&Path) -> Result<()>,
) -> Result<(Vec<Manifest>, Vec<Unreadable>)> {
    let dir = roots.store_dir();
    if !dir.exists() {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut manifests = Vec::new();
    let mut unreadable = Vec::new();
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        paths.push(entry?.path());
    }
    paths.sort();
    for path in paths {
        if path.extension().is_some_and(|ext| ext == "json") {
            check_path(&path)?;
            let read = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display())).and_then(|text| {
                serde_json::from_str::<Manifest>(&text)
                    .with_context(|| format!("reading installation record {} (dependency list required)", path.display()))
            });
            match read {
                Ok(m) => manifests.push(m),
                Err(e) => unreadable.push((path, e)),
            }
        }
    }
    manifests.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((manifests, unreadable))
}

type Archive = zip::ZipArchive<std::io::Cursor<Vec<u8>>>;

pub fn open_archive(vip_bytes: Vec<u8>) -> Result<Archive> {
    Ok(zip::ZipArchive::new(std::io::Cursor::new(vip_bytes))?)
}

pub fn read_spec(zip: &mut Archive) -> Result<String> {
    // Almost always literally "spec" at the archive root.
    let idx = (0..zip.len()).find(|i| {
        zip.by_index(*i).is_ok_and(|f| f.name().is_ok_and(|n| n.eq_ignore_ascii_case("spec")))
    });
    let Some(idx) = idx else {
        bail!("no `spec` member in .vip archive");
    };
    let mut f = zip.by_index(idx)?;
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut f, &mut buf)?;
    // spec files are latin-1-ish; lossy is fine for the fields we read.
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Work out every file this package would write, without touching disk.
pub fn plan(roots: &Roots, spec: &Spec, zip: &mut Archive) -> Result<Plan> {
    // Zip entry names are case-sensitive; index once, tolerate case drift
    // between `spec` and the archive.
    let mut by_lower = std::collections::HashMap::new();
    for i in 0..zip.len() {
        let name = zip.by_index(i)?.name().with_context(|| format!("archive entry {i} has no usable name"))?.to_string();
        by_lower.insert(name.to_lowercase().replace('\\', "/"), i);
    }

    let mut writes = Vec::new();
    let mut kept = Vec::new();
    let mut missing = Vec::new();
    let mut skipped = Vec::new();
    let mut palette_files = Vec::new();

    for group in &spec.file_groups {
        if group.files.is_empty() {
            continue;
        }
        // A venv is an addon: LabVIEW-tree content only. What a package also
        // puts on the machine — a driver DLL into System32, say — is no part
        // of that, and a project's install must not scatter files across the
        // machine. Left out, and said so.
        if roots.venv().is_some() && Roots::classify(&group.target_dir)? != TokenClass::LabView {
            skipped.push((group.target_dir.clone(), group.files.len()));
            continue;
        }
        // In a venv, palette files go to the package's contribution and are
        // merged into the shared palette addon: all of a `<menus>` group, and
        // the `.mnu` files of a `<vi.lib>\addons` group (its VIs stay put).
        let palette_root = match roots.venv() {
            Some(_) => match crate::palette::menu_relative_root(&group.target_dir)? {
                Some(base) => Some((PaletteTree::Menus, base)),
                None => crate::palette::addons_relative_root(&group.target_dir)?
                    .map(|base| (PaletteTree::Addons, base)),
            },
            None => None,
        };
        if palette_root.is_some() {
            crate::palette::validate_package_name(&spec.name)?;
        }
        let if_newer = group.replace_mode.eq_ignore_ascii_case("If Newer");

        for rel in &group.files {
            let rel_path = safe_relative(rel)?;
            let palette = palette_root.as_ref().and_then(|(tree, base)| {
                (*tree == PaletteTree::Menus || crate::palette::is_mnu(&rel_path)).then(|| PaletteFile {
                    relative: base.join(&rel_path).to_string_lossy().replace('\\', "/"),
                    tree: *tree,
                })
            });
            let entry_name = format!("File Group {}/{}", group.index, rel.replace('\\', "/"));

            let Some(&zip_index) = by_lower.get(&entry_name.to_lowercase()) else {
                missing.push(entry_name);
                continue;
            };

            let dest = match &palette {
                Some(p) => crate::palette::contribution_tree(roots, &spec.name, p.tree).join(&p.relative),
                None => roots.expand(&group.target_dir)?.join(&rel_path),
            };
            roots.check_contained(&dest)?;

            let exists = dest.exists();
            palette_files.extend(palette);
            if exists && if_newer {
                // PoC simplification: `If Newer` only writes when absent.
                // Doing it properly means comparing the archive timestamp
                // against the file on disk.
                kept.push(dest);
                continue;
            }
            writes.push(PlannedWrite { zip_index, dest, overwrites: exists });
        }
    }

    Ok(Plan { writes, kept, missing_from_archive: missing, skipped_groups: skipped, palette_files })
}

/// The `lvaddoninfo.json` LabVIEW reads to mount an addon (fields as NI's own
/// addons write them). The floor is the bound target itself, not what the
/// package claimed: the relink pass saves the payload in that version's
/// format, so nothing older could load it anyway.
pub fn addon_info(name: &str, target: &LvTarget) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "AddonName": name,
        "ApiVersion": "v1",
        "MinimumSupportedLVVersion": format!("{}.0", target.version.trunc() as u32),
        "SupportedBitness": target.bitness.to_string(),
    }))
    .expect("a fixed shape of strings serialises")
}

/// Execute a plan and record the manifest.
pub fn apply(
    roots: &Roots,
    spec: &Spec,
    mut zip: Archive,
    plan: &Plan,
    pre_install_vi: Option<&Path>,
    dependencies: Vec<String>,
) -> Result<Manifest> {
    let mut written = Vec::new();

    // A declared PostInstall hook ships as `PostInstall.vi` at the archive
    // root. Extract it next to the manifests so it survives until the relink
    // pass has made it runnable.
    let mut hook = |name: &str, member: &str| -> Result<Option<PathBuf>> {
        match spec.script_vis.iter().any(|(h, v)| h == name && !v.is_empty()) {
            true => extract_hook(roots, &spec.name, &mut zip, member),
            false => Ok(None),
        }
    };
    let post_install_vi = hook("PostInstall", "PostInstall.vi")?;
    let pre_uninstall_vi = hook("PreUninstall", "PreUninstall.vi")?;
    let post_uninstall_vi = hook("PostUninstall", "PostUninstall.vi")?;

    for w in &plan.writes {
        if let Some(parent) = w.dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut src = zip.by_index(w.zip_index)?;
        let mut buf = Vec::with_capacity(src.size() as usize);
        src.read_to_end(&mut buf)?;
        std::fs::write(&w.dest, &buf)
            .with_context(|| format!("writing {}", w.dest.display()))?;
        written.push(w.dest.to_string_lossy().replace('\\', "/"));
    }
    // The footprint is what the package ships, not what this run happened to
    // copy — a file left alone today is still gone when the package goes.
    for k in &plan.kept {
        written.push(k.to_string_lossy().replace('\\', "/"));
    }

    // An addon is what LVAddons mounts: `lvaddoninfo.json` beside the mirrored
    // tree names it and says which LabVIEW may load it. Only when something
    // landed — an addon holding nothing but its info file is noise, and a
    // package whose every group aimed at the machine leaves no addon at all.
    if roots.venv().is_some()
        && let Some(t) = &roots.target
        && written.iter().any(|file| Path::new(file).starts_with(&roots.application))
    {
        let info = roots.application.join("lvaddoninfo.json");
        std::fs::create_dir_all(&roots.application)?;
        std::fs::write(&info, addon_info(&spec.name, t))
            .with_context(|| format!("writing {}", info.display()))?;
        written.push(info.to_string_lossy().replace('\\', "/"));
    }

    let archive = zip.into_inner().into_inner();
    let manifest = Manifest {
        name: spec.name.clone(),
        version: spec.version.clone(),
        display_name: spec.display_name.clone(),
        package_metadata: Some(PackageMetadata {
            vendor: spec.vendor.clone(),
            license: spec.license.clone(),
            archive_md5: crate::index::md5_hex(&archive),
            archive_sha256: crate::cache::sha256_hex(&archive),
        }),
        installed_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        target: match &roots.target {
            Some(t) => t.label(),
            None => format!("scratch {}", roots.application.display()),
        },
        files: written,
        palette_files: plan.palette_files.clone(),
        dependencies,
        // PostInstall is not listed: its state is `post_install`. In a venv
        // PreInstall cannot run (its LabVIEW starts after the copy) and the
        // uninstall hooks do not run either; globally PreInstall has run by
        // now and the uninstall hooks wait in the copies extracted above.
        skipped_hooks: match roots.venv() {
            Some(_) => spec
                .script_vis
                .iter()
                .filter(|(h, _)| h != "PostInstall")
                .map(|(h, v)| format!("{h}={v}"))
                .collect(),
            None => Vec::new(),
        },
        relinked: false,
        post_install: post_install_vi.as_ref().map(|_| HookState::Pending),
        post_install_vi: post_install_vi.map(|p| p.to_string_lossy().replace('\\', "/")),
        pre_install_vi: pre_install_vi.map(|p| p.to_string_lossy().replace('\\', "/")),
        pre_uninstall_vi: pre_uninstall_vi.map(|p| p.to_string_lossy().replace('\\', "/")),
        post_uninstall_vi: post_uninstall_vi.map(|p| p.to_string_lossy().replace('\\', "/")),
        relink_folders: crate::relink::folders_for_spec(roots, spec)?
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect(),
        skipped_groups: plan
            .skipped_groups
            .iter()
            .map(|(dir, n)| format!("{dir} ({n} files)"))
            .collect(),
    };

    write_manifest(roots, &manifest)?;
    Ok(manifest)
}

/// Pull one hook VI out of the archive root into the target's `hooks` store.
/// A declared hook missing from the archive is a warning-level oddity, not an
/// error — the files themselves installed fine.
pub fn extract_hook(
    roots: &Roots,
    package: &str,
    zip: &mut Archive,
    member: &str,
) -> Result<Option<PathBuf>> {
    let Ok(mut src) = zip.by_name(member) else {
        return Ok(None);
    };
    let dir = roots.store_dir().parent().map(|p| p.join("hooks")).unwrap_or_default();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let dest = dir.join(format!("{package}-{member}"));
    let mut buf = Vec::with_capacity(src.size() as usize);
    src.read_to_end(&mut buf)?;
    std::fs::write(&dest, &buf).with_context(|| format!("writing {}", dest.display()))?;
    Ok(Some(dest))
}

pub(crate) fn write_manifest(roots: &Roots, m: &Manifest) -> Result<()> {
    let mp = manifest_path(roots, &m.name);
    std::fs::create_dir_all(mp.parent().unwrap())?;
    std::fs::write(&mp, serde_json::to_string_pretty(m)?)?;
    Ok(())
}

/// Record that the relink pass has run over this package.
///
/// Separate from `apply` because the two happen in different phases: every
/// package is copied before any is relinked, so the manifest is written once
/// with `relinked: false` and amended if and when the relink succeeds.
pub fn mark_relinked(roots: &Roots, name: &str) -> Result<()> {
    let mut m = read_manifest(roots, name)?;
    if m.relinked {
        return Ok(());
    }
    m.relinked = true;
    write_manifest(roots, &m)
}

/// Record how the package's PostInstall hook went.
pub fn set_post_install(roots: &Roots, name: &str, state: HookState) -> Result<()> {
    let mut m = read_manifest(roots, name)?;
    m.post_install = Some(state);
    write_manifest(roots, &m)
}

/// Now, as recorded in `HookState`.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Record a hook this install declared and did not run, so `lvpm list` says
/// so. `hook` is the `Name=path` form the manifest already uses.
pub fn mark_hook_skipped(roots: &Roots, name: &str, hook: &str) -> Result<()> {
    let mut m = read_manifest(roots, name)?;
    if m.skipped_hooks.iter().any(|h| h == hook) {
        return Ok(());
    }
    m.skipped_hooks.push(hook.to_string());
    write_manifest(roots, &m)
}

/// Remove every file the manifest recorded, then prune directories this
/// leaves empty. Directories that still hold anything are left alone.
pub fn uninstall(roots: &Roots, name: &str) -> Result<(usize, Manifest)> {
    let manifest = read_manifest(roots, name)?;
    let mut removed = 0;
    let mut dirs: Vec<PathBuf> = Vec::new();

    let stop_at = roots.application.clone();

    for f in &manifest.files {
        let p = PathBuf::from(f);
        if p.exists() {
            std::fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
            removed += 1;
        }
        let mut d = p.parent().map(Path::to_path_buf);
        while let Some(dir) = d {
            // Never prune the LabVIEW install dir itself or anything above it,
            // nor the venv: palette contributions live outside the package's
            // addon, so `stop_at` is not among their ancestors.
            if dir == stop_at || Some(dir.as_path()) == roots.venv() || dir.parent().is_none() {
                break;
            }
            dirs.push(dir.clone());
            d = dir.parent().map(Path::to_path_buf);
        }
    }

    // Deepest first, so parents get a chance once children are gone.
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    dirs.dedup();
    for d in dirs {
        if d.exists() && std::fs::read_dir(&d)?.next().is_none() {
            let _ = std::fs::remove_dir(&d);
        }
    }

    // The extracted hook VIs go with the package they belonged to.
    for hook in [
        &manifest.post_install_vi,
        &manifest.pre_install_vi,
        &manifest.pre_uninstall_vi,
        &manifest.post_uninstall_vi,
    ]
    .into_iter()
    .flatten()
    {
        let _ = std::fs::remove_file(hook);
    }
    std::fs::remove_file(manifest_path(roots, name))?;
    Ok((removed, manifest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_state_is_recorded_and_older_records_read_as_unknown() {
        let old = r#"{"name":"p","version":"1","display_name":null,"installed_at":0,"target":"t",
            "files":[],"dependencies":[],"skipped_hooks":["PostInstall=x.vi"],"post_install_vi":"h.vi"}"#;
        let m: Manifest = serde_json::from_str(old).unwrap();
        assert_eq!(m.post_install, None);

        for state in [
            HookState::Pending,
            HookState::Ran { at: 5 },
            HookState::Failed { at: 6, error: "boom".into() },
        ] {
            let mut m: Manifest = serde_json::from_str(old).unwrap();
            m.post_install = Some(state.clone());
            let text = serde_json::to_string(&m).unwrap();
            let back: Manifest = serde_json::from_str(&text).unwrap();
            assert_eq!(back.post_install, Some(state));
        }
        let text = serde_json::to_string(&HookState::Ran { at: 5 }).unwrap();
        assert_eq!(text, r#"{"state":"ran","at":5}"#);
    }

    #[test]
    fn uninstall_prunes_package_but_preserves_packages_directory_and_state() {
        let dir = std::env::temp_dir().join(format!("lvpm-uninstall-layout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = LvTarget { version: 26.3, bitness: 64, path: dir.join("LabVIEW") };
        let package = Roots::project(&dir, &target, "example");
        let store = Roots::project_store(&dir, &target);
        let payload = package.application.join("vi.lib").join("test.vi");
        std::fs::create_dir_all(payload.parent().unwrap()).unwrap();
        std::fs::write(&payload, b"test").unwrap();
        let manifest = Manifest {
            name: "example".into(), version: "1".into(), display_name: None, package_metadata: None, installed_at: 0,
            target: "test".into(), files: vec![payload.to_string_lossy().into_owned()], dependencies: vec![],
            palette_files: vec![],
            skipped_hooks: vec![], relinked: false, relink_folders: vec![],
            post_install_vi: None, post_install: None, pre_install_vi: None, pre_uninstall_vi: None,
            post_uninstall_vi: None, skipped_groups: vec![],
        };
        write_manifest(&package, &manifest).unwrap();
        assert_eq!(uninstall(&store, "example").unwrap().0, 1);
        assert!(!dir.join("packages").join("example").exists());
        assert!(dir.join("packages").is_dir());
        assert!(dir.join("installed").is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_record_fails_the_strict_listing_and_is_set_aside_by_the_lenient_one() {
        let dir = std::env::temp_dir().join(format!("lvpm-lenient-records-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Roots::scratch(&dir);
        std::fs::create_dir_all(store.store_dir()).unwrap();
        let good = r#"{"name":"good","version":"1","display_name":null,"installed_at":0,"target":"t",
            "files":[],"dependencies":[],"skipped_hooks":[],"post_install_vi":"h.vi"}"#;
        // Written by an lvpm from before records listed dependencies.
        let legacy = r#"{"name":"legacy","version":"1","display_name":null,"installed_at":0,"target":"t",
            "files":[],"skipped_hooks":[]}"#;
        std::fs::write(manifest_path(&store, "good"), good).unwrap();
        std::fs::write(manifest_path(&store, "legacy"), legacy).unwrap();
        std::fs::write(manifest_path(&store, "broken"), "{ not json").unwrap();

        let (read, unreadable) = list_installed_lenient(&store).unwrap();
        assert_eq!(read.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["good"]);
        let names: Vec<String> =
            unreadable.iter().map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["broken.json", "legacy.json"]);
        assert!(format!("{:#}", unreadable[1].1).contains("dependency list required"));

        let err = list_installed(&store).unwrap_err();
        assert!(format!("{err:#}").contains("broken.json"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        assert!(safe_relative("../../etc/passwd").is_err());
        assert!(safe_relative("/etc/passwd").is_err());
        assert!(safe_relative(r"C:\windows\system32\evil.dll").is_err());
        assert!(safe_relative("vi.lib/addons/ok.llb").is_ok());
    }

    #[test]
    fn addon_info_has_the_fields_ni_writes_and_floors_at_the_bound_target() {
        let t = LvTarget { version: 26.3, bitness: 64, path: PathBuf::from(r"C:\LV") };
        let v: serde_json::Value = serde_json::from_str(&addon_info("oglib_error", &t)).unwrap();
        assert_eq!(v["AddonName"], "oglib_error");
        assert_eq!(v["ApiVersion"], "v1");
        assert_eq!(v["MinimumSupportedLVVersion"], "26.0");
        assert_eq!(v["SupportedBitness"], "64");
    }

    #[test]
    fn venv_menu_files_are_recorded_in_the_private_contribution_store() {
        use std::io::Write;

        let dir = std::env::temp_dir().join(format!("lvpm-palette-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let target = LvTarget { version: 26.3, bitness: 64, path: dir.join("LabVIEW") };
        let roots = Roots::project(&dir, &target, "example");
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer.start_file("File Group 0/functions.mnu", zip::write::SimpleFileOptions::default()).unwrap();
        writer.write_all(b"menu").unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        let mut zip = open_archive(bytes).unwrap();
        let spec = crate::spec::parse(
            r#"[Package]
Name="example"
Version="1"

[File Group 0]
Target Dir="<menus>/Categories"
Num Files=1
File 0="functions.mnu"
"#,
        )
        .unwrap();

        let plan = plan(&roots, &spec, &mut zip).unwrap();
        let source = crate::palette::contribution_root(&roots, "example").join("Categories/functions.mnu");
        assert_eq!(plan.writes[0].dest, source);
        assert_eq!(plan.palette_files, [PaletteFile { relative: "Categories/functions.mnu".into(), tree: PaletteTree::Menus }]);
        let manifest = apply(&roots, &spec, zip, &plan, None, Vec::new()).unwrap();

        assert!(source.is_file());
        assert!(!roots.application.join("lvaddoninfo.json").exists());
        assert!(manifest.files.iter().any(|file| PathBuf::from(file) == source));
        assert_eq!(manifest.palette_files, plan.palette_files);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn venv_addons_mnu_files_go_to_the_contribution_store_and_vis_stay_in_the_addon() {
        use std::io::Write;

        let dir = std::env::temp_dir().join(format!("lvpm-addons-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let target = LvTarget { version: 26.3, bitness: 64, path: dir.join("LabVIEW") };
        let roots = Roots::project(&dir, &target, "hse_lib_hse_logger");
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for name in ["dir.mnu", "functions_hse_logger.mnu", "Helper.vi"] {
            writer.start_file(format!("File Group 0/{name}"), zip::write::SimpleFileOptions::default()).unwrap();
            writer.write_all(name.as_bytes()).unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();
        let mut zip = open_archive(bytes).unwrap();
        let spec = crate::spec::parse(
            r#"[Package]
Name="hse_lib_hse_logger"
Version="1"

[File Group 0]
Target Dir="<vi.lib>\addons\HSE"
Num Files=3
File 0="dir.mnu"
File 1="functions_hse_logger.mnu"
File 2="Helper.vi"
"#,
        )
        .unwrap();

        let plan = plan(&roots, &spec, &mut zip).unwrap();
        let store = crate::palette::contribution_tree(&roots, "hse_lib_hse_logger", PaletteTree::Addons);
        assert_eq!(plan.writes[0].dest, store.join("HSE/dir.mnu"));
        assert_eq!(plan.writes[1].dest, store.join("HSE/functions_hse_logger.mnu"));
        assert_eq!(plan.writes[2].dest, roots.expand(r"<vi.lib>\addons\HSE").unwrap().join("Helper.vi"));
        assert_eq!(
            plan.palette_files,
            [
                PaletteFile { relative: "HSE/dir.mnu".into(), tree: PaletteTree::Addons },
                PaletteFile { relative: "HSE/functions_hse_logger.mnu".into(), tree: PaletteTree::Addons },
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_palette_record_from_before_addons_merging_reads_as_menus() {
        let file: PaletteFile = serde_json::from_str(r#"{"relative":"Categories/a.mnu"}"#).unwrap();
        assert_eq!(file.tree, PaletteTree::Menus);
        assert_eq!(serde_json::to_string(&file).unwrap(), r#"{"relative":"Categories/a.mnu"}"#);
        let addons = PaletteFile { relative: "HSE/a.mnu".into(), tree: PaletteTree::Addons };
        assert_eq!(serde_json::to_string(&addons).unwrap(), r#"{"relative":"HSE/a.mnu","tree":"vi.lib/addons"}"#);
    }
}
