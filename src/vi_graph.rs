//! Read-only traversal of VI links from LabVIEW project sources.

use crate::sbom_metadata::portable_path_identity;
use crate::sbom_paths::redact_paths;
use crate::{linkfix, lvproj, ppl, target, venv};
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

pub struct Graph {
    pub roots: BTreeSet<String>,
    pub edges: BTreeMap<String, BTreeSet<String>>,
    pub issues: BTreeSet<String>,
    pub files: BTreeMap<String, PathBuf>,
    pub packed: BTreeMap<String, PackedComponent>,
    pub(crate) project_dlls: BTreeMap<String, PathBuf>,
    pub labview: Option<target::LvTarget>,
    pub venv: Option<venv::Venv>,
}

impl Graph {
    pub(crate) fn protect_inputs(&self, output: &crate::sbom_paths::OutputGuard) -> Result<()> {
        if let Some(venv) = &self.venv {
            output.protect(&venv.binding_path())?;
        }
        let files = self
            .files
            .values()
            .chain(self.project_dlls.values())
            .map(|path| llb_container(path).unwrap_or(path))
            .collect::<BTreeSet<_>>();
        for path in files {
            output.protect(path)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Default)]
pub struct Environment<'a> {
    pub project: Option<&'a Path>,
    pub global: bool,
    pub labview_version: Option<&'a str>,
}

enum Sources<'a> {
    Build(&'a lvproj::Build),
    Project(&'a BTreeMap<String, lvproj::ProjectItem>),
}

pub fn scan(project: &Path, build: &lvproj::Build, environment: Environment<'_>) -> Result<Graph> {
    scan_sources(project, Sources::Build(build), environment)
}

pub(crate) fn scan_project(
    project: &Path,
    items: &BTreeMap<String, lvproj::ProjectItem>,
    environment: Environment<'_>,
) -> Result<Graph> {
    scan_sources(project, Sources::Project(items), environment)
}

fn scan_sources(
    project: &Path,
    sources: Sources<'_>,
    environment: Environment<'_>,
) -> Result<Graph> {
    let project = project.canonicalize()?;
    let project_dir = project
        .parent()
        .context("LabVIEW project has no parent directory")?;
    let venv = if environment.global {
        None
    } else {
        venv::find(environment.project, &std::env::current_dir()?)?
    };
    let labview = if let Some(venv) = &venv {
        Some(venv.target.clone())
    } else {
        let targets = target::detect()?;
        match environment.labview_version {
            Some(version) => Some(target::select(&targets, version)?),
            None => targets.into_iter().next(),
        }
    };
    let labview_dir = labview
        .as_ref()
        .map(|lv| lv.path.canonicalize())
        .transpose()?;
    let venv_dir = venv
        .as_ref()
        .map(|v| v.packages_dir().canonicalize())
        .transpose()?;
    let addons = venv_dir
        .as_deref()
        .map(linkfix::venv_roots)
        .transpose()?
        .unwrap_or_default();
    let system_addons = labview
        .as_ref()
        .map(SystemAddons::discover)
        .transpose()?
        .unwrap_or_default();
    let locations = Locations {
        project_dir,
        labview_dir: labview_dir.as_deref(),
        venv_dir: venv_dir.as_deref(),
        addons: &addons,
        system_addons: &system_addons,
    };
    let mut issues = BTreeSet::new();
    let (roots, project_dlls, expand_nested_members) = match sources {
        Sources::Build(build) => {
            let roots = build_roots(&project, build, &locations, &mut issues)?;
            ensure!(
                !roots.is_empty() || !issues.is_empty(),
                "build has no selected VI or library sources"
            );
            (
                roots,
                BTreeMap::new(),
                build.kind == lvproj::PACKED_LIBRARY_TYPE,
            )
        }
        Sources::Project(items) => {
            let (roots, dlls) = project_roots(&project, items, &locations, &mut issues);
            (roots, dlls, true)
        }
    };

    let mut edges = BTreeMap::new();
    let mut files = BTreeMap::new();
    let mut packed = PackedScan::default();
    let mut expanded = BTreeMap::new();
    let mut pending = roots
        .iter()
        .cloned()
        .map(|file| (file, Members::All))
        .collect::<Vec<_>>();
    while let Some((file, members)) = pending.pop() {
        let label = locations.label(&file);
        let members = if !linkfix::is_library_xml(&file) {
            Members::None
        } else if roots.contains(&file) {
            Members::All
        } else {
            members
        };
        if edges.contains_key(&label)
            && expanded.get(&label).copied().unwrap_or(Members::None) >= members
        {
            continue;
        }
        if extension_is(&file, "lvlibp") {
            let direct = packed.visit(&file, &label, &locations, &mut pending, &mut issues);
            files.insert(label.clone(), file);
            edges.insert(label, direct);
            continue;
        }
        if members != Members::None {
            expanded.insert(label.clone(), members);
        }
        let links = match read_links(&file, &locations, members, &mut issues) {
            Ok(links) => links,
            Err(error) => {
                issues.insert(format!("{label}: {error:#}"));
                Vec::new()
            }
        };
        let mut direct = BTreeSet::new();
        for link in links {
            if let Some(resolved) = resolve_file(&link.path) {
                let child = locations.label(&resolved);
                if child != label {
                    direct.insert(child);
                    let members = match link.kind {
                        LinkKind::ClassType => Members::Dispatch,
                        LinkKind::Member if expand_nested_members => Members::All,
                        _ => Members::None,
                    };
                    pending.push((resolved, members));
                }
            } else {
                issues.insert(format!("{label} -> {}", locations.label(&link.path)));
            }
        }
        files.insert(label.clone(), file);
        edges.insert(label, direct);
    }
    Ok(Graph {
        roots: roots.iter().map(|root| locations.label(root)).collect(),
        edges,
        issues: issues
            .into_iter()
            .map(|issue| redact_paths(&issue))
            .collect(),
        files,
        packed: packed.components,
        project_dlls,
        labview,
        venv,
    })
}

/// Read the explicit packed-library payload of a distribution build.
pub(crate) fn scan_packed_files(project: &Path, files: BTreeSet<PathBuf>) -> Result<Graph> {
    let project = project.canonicalize()?;
    let project_dir = project
        .parent()
        .context("LabVIEW project has no parent directory")?;
    let locations = Locations {
        project_dir,
        labview_dir: None,
        venv_dir: None,
        addons: &[],
        system_addons: &SystemAddons::default(),
    };

    let mut roots = BTreeSet::new();
    let mut issues = BTreeSet::new();
    let mut pending = Vec::new();
    for path in files {
        if let Some(resolved) = resolve_file(&path).filter(|file| extension_is(file, "lvlibp")) {
            roots.insert(locations.label(&resolved));
            pending.push((resolved, Members::None));
        } else {
            issues.insert(format!(
                "packed library payload file {} is missing",
                locations.label(&path)
            ));
        }
    }

    let mut packed = PackedScan::default();
    let mut edges = BTreeMap::new();
    let mut files = BTreeMap::new();
    while let Some((file, _)) = pending.pop() {
        let label = locations.label(&file);
        if let Entry::Vacant(edge) = edges.entry(label) {
            let direct = packed.visit(&file, edge.key(), &locations, &mut pending, &mut issues);
            files.insert(edge.key().clone(), file);
            edge.insert(direct);
        }
    }

    let issues = issues
        .into_iter()
        .map(|issue| redact_paths(&issue))
        .collect();
    Ok(Graph {
        roots,
        edges,
        issues,
        files,
        packed: packed.components,
        project_dlls: BTreeMap::new(),
        labview: None,
        venv: None,
    })
}

pub struct PackedComponent {
    pub name: String,
    pub artifact_path: Option<String>,
    pub reference_paths: BTreeSet<String>,
    pub sha256: Option<String>,
    pub version: Option<String>,
    pub library_id: Option<String>,
}

#[derive(Default)]
struct PackedScan {
    components: BTreeMap<String, PackedComponent>,
    files: BTreeMap<PathBuf, Result<Rc<ppl::Artifact>, String>>,
}

impl PackedScan {
    fn read(&mut self, path: &Path) -> Result<Rc<ppl::Artifact>, String> {
        self.files
            .entry(path.to_owned())
            .or_insert_with(|| {
                ppl::read_artifact(path)
                    .map(Rc::new)
                    .map_err(|error| format!("{error:#}"))
            })
            .clone()
    }

    fn component(
        &mut self,
        label: &str,
        file: &Path,
        artifact: Option<&ppl::Artifact>,
    ) -> &mut PackedComponent {
        self.components
            .entry(label.to_owned())
            .or_insert_with(|| PackedComponent {
                name: file
                    .file_name()
                    .expect("packed file has a filename")
                    .to_string_lossy()
                    .into_owned(),
                artifact_path: Some(label.to_owned()),
                reference_paths: BTreeSet::new(),
                sha256: artifact.map(|value| value.sha256.clone()),
                version: artifact.and_then(|value| value.library.version.clone()),
                library_id: artifact.and_then(|value| value.library.library_id.clone()),
            })
    }

    fn visit(
        &mut self,
        file: &Path,
        label: &str,
        locations: &Locations<'_>,
        pending: &mut Vec<(PathBuf, Members)>,
        issues: &mut BTreeSet<String>,
    ) -> BTreeSet<String> {
        let artifact = self.read(file);
        self.component(label, file, artifact.as_deref().ok());
        let artifact = match artifact {
            Ok(artifact) => artifact,
            Err(error) => {
                issues.insert(format!("{label}: {error}"));
                return BTreeSet::new();
            }
        };
        let mut direct = BTreeSet::new();
        for import in &artifact.library.imports {
            let resolved = locations.packed_import(file, import).and_then(|path| {
                let artifact = self.read(&path).ok()?;
                let id = artifact.library.library_id.as_deref();
                (id == Some(import.library_id.as_str())).then_some((path, artifact))
            });
            let reference_path = portable_path_identity(&import.path);
            if let Some((path, artifact)) = resolved {
                let child = locations.label(&path);
                if child != label {
                    self.component(&child, &path, Some(&artifact))
                        .reference_paths
                        .insert(reference_path);
                    direct.insert(child);
                    pending.push((path, Members::None));
                }
            } else {
                let child = format!(
                    "ppl-reference:{}:{label}:{reference_path}:{}",
                    label.len(),
                    import.library_id
                );
                self.components
                    .entry(child.clone())
                    .or_insert_with(|| PackedComponent {
                        name: import.name.clone(),
                        artifact_path: None,
                        reference_paths: BTreeSet::from([reference_path]),
                        sha256: None,
                        version: None,
                        library_id: Some(import.library_id.clone()),
                    });
                direct.insert(child);
            }
        }
        direct
    }
}

#[derive(Default)]
struct SystemAddons {
    store: Option<PathBuf>,
    roots: Vec<PathBuf>,
}

impl SystemAddons {
    fn discover(target: &target::LvTarget) -> Result<Self> {
        // Earlier LabVIEW installations cannot mount LVAddons.
        if target.version < 22.3 {
            return Ok(Self::default());
        }
        #[cfg(windows)]
        let store = std::env::var_os("ProgramW6432")
            .or_else(|| std::env::var_os("ProgramFiles"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
            .join(r"NI\LVAddons");
        #[cfg(target_os = "linux")]
        let store = PathBuf::from("/usr/local/natinst/share/lvaddons");
        Self::read(&store, target)
    }

    fn read(store: &Path, target: &target::LvTarget) -> Result<Self> {
        let store = match store.canonicalize() {
            Ok(store) => store,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.into()),
        };
        let mut roots = Vec::new();
        for addon in std::fs::read_dir(&store)? {
            let addon = addon?;
            if !addon.file_type()?.is_dir() {
                continue;
            }
            let mut compatible = Vec::new();
            for release in std::fs::read_dir(addon.path())? {
                let release = release?;
                if !release.file_type()?.is_dir() {
                    continue;
                }
                let Some(version) = numeric_addon_version(&release.file_name()) else {
                    continue;
                };
                let path = release.path();
                if AddonCompatibility::read(&path).is_some_and(|info| info.matches(target)) {
                    compatible.push((version, path));
                }
            }
            // The same physical add-on can retain older API versions.
            if let Some((_, path)) = compatible.into_iter().max() {
                roots.push(path);
            }
        }
        roots.sort();
        Ok(Self {
            store: Some(store),
            roots,
        })
    }
}

fn numeric_addon_version(name: &std::ffi::OsStr) -> Option<Vec<u32>> {
    name.to_str()?
        .split('.')
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .ok()
}

#[derive(serde::Deserialize)]
struct AddonCompatibility {
    #[serde(rename = "MinimumSupportedLVVersion")]
    minimum: String,
    #[serde(rename = "SupportedBitness")]
    bitness: Option<String>,
}

impl AddonCompatibility {
    fn read(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path.join("lvaddoninfo.json")).ok()?;
        serde_json::from_slice(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes)).ok()
    }

    fn matches(&self, target: &target::LvTarget) -> bool {
        self.minimum
            .parse::<f64>()
            .is_ok_and(|minimum| minimum.is_finite() && minimum <= target.version)
            && self
                .bitness
                .as_ref()
                .is_none_or(|bitness| bitness.parse::<u8>().ok() == Some(target.bitness))
    }
}

struct Locations<'a> {
    project_dir: &'a Path,
    labview_dir: Option<&'a Path>,
    venv_dir: Option<&'a Path>,
    addons: &'a [PathBuf],
    system_addons: &'a SystemAddons,
}

impl Locations<'_> {
    fn label(&self, path: &Path) -> String {
        for (root, label) in [
            (self.venv_dir, "venv"),
            (Some(self.project_dir), "project"),
            (self.labview_dir, "labview"),
            (self.system_addons.store.as_deref(), "lvaddons"),
        ] {
            if let Some(relative) = root.and_then(|root| path.strip_prefix(root).ok())
                && !relative
                    .components()
                    .any(|part| part == Component::ParentDir)
            {
                return format!("{label}/{}", slash(relative));
            }
        }

        let text = path.to_string_lossy();
        let portable = portable_path_identity(&text);
        if portable == text {
            slash(path)
        } else {
            portable
        }
    }

    fn packed_import(&self, importing: &Path, import: &ppl::Import) -> Option<PathBuf> {
        let parent = importing.parent()?;
        let path = Path::new(&import.path);
        if !import.relative || !path.is_relative() || import.path.contains(':') {
            return None;
        }
        let candidate = parent.join(path).canonicalize().ok()?;
        let inside_scan_roots = [
            Some(parent),
            Some(self.project_dir),
            self.labview_dir,
            self.venv_dir,
        ]
        .into_iter()
        .flatten()
        .any(|root| candidate.starts_with(root));
        (inside_scan_roots && extension_is(&candidate, "lvlibp") && candidate.is_file())
            .then_some(candidate)
    }

    // LabVIEW checks the installation before its LVAddons overlays.
    fn logical(&self, logical: &Path, unresolved: PathBuf) -> Result<PathBuf> {
        let base = self
            .labview_dir
            .map(|root| root.join(logical))
            .unwrap_or(unresolved);
        if resolve_file(&base).is_some() || base.is_dir() {
            return Ok(base);
        }
        // Preserve the venv's lookup order before falling back to system add-ons.
        for (roots, kind) in [
            (self.addons, "venv"),
            (self.system_addons.roots.as_slice(), "system"),
        ] {
            let mut candidates = roots
                .iter()
                .map(|root| root.join(logical))
                .filter_map(|path| resolve_file(&path).or_else(|| path.is_dir().then_some(path)));
            let Some(first) = candidates.next() else {
                continue;
            };
            // Two add-ons in one search tier make attribution ambiguous.
            ensure!(
                candidates.next().is_none(),
                "multiple {kind} addons provide {}",
                slash(logical)
            );
            return Ok(first);
        }
        Ok(base)
    }

    fn url(&self, base: &Path, url: &str) -> Result<PathBuf> {
        let url = url.replace('\\', "/");
        if let Some(logical) = linkfix::token_to_logical(url.trim_start_matches('/')) {
            return self.logical(Path::new(&logical), PathBuf::from(&url));
        }
        let path = match url.strip_prefix('/') {
            Some(path)
                if path.as_bytes().get(1) == Some(&b'/')
                    && path.as_bytes()[0].is_ascii_alphabetic() =>
            {
                PathBuf::from(format!("{}:{}", &path[..1], &path[1..]))
            }
            _ => {
                // LabVIEW stores URLs relative to the referring file itself.
                // Consume leading parents before opening the path so POSIX
                // lookups do not try to traverse through that file.
                let mut anchor = base;
                let mut relative = Path::new(&url);
                while let Ok(tail) = relative.strip_prefix("..") {
                    anchor = anchor.parent().unwrap_or(anchor);
                    relative = tail;
                }
                anchor.join(relative)
            }
        };
        self.rebase(path)
    }

    fn link(&self, file: &Path, link: &linkfix::Pth) -> Result<Option<PathBuf>> {
        let mut parts = link.comps.iter().map(|c| String::from_utf8_lossy(c));
        let Some(first) = parts.next() else {
            return Ok(None);
        };
        if link.is_token()
            && let Some(logical) = linkfix::token_to_logical(&first)
        {
            let suffix: PathBuf = parts.map(|part| part.into_owned()).collect();
            return self
                .logical(
                    &Path::new(&logical).join(&suffix),
                    Path::new(first.as_ref()).join(suffix),
                )
                .map(Some);
        }
        let mut path = if link.is_token() {
            PathBuf::from(first.as_ref())
        } else if link.is_relative() {
            let Some(parent) = file.parent() else {
                return Ok(None);
            };
            let mut parent = parent.to_path_buf();
            if !first.is_empty() {
                parent.push(first.as_ref());
            }
            parent
        } else if link.is_drive() {
            PathBuf::from(format!("{first}:/"))
        } else if link.tp == 0 && first.is_empty() {
            PathBuf::from("/")
        } else {
            return Ok(None);
        };

        for part in parts {
            if part.is_empty() && link.is_relative() {
                path.pop();
            } else {
                path.push(part.as_ref());
            }
        }

        if link.is_drive() || (link.tp == 0 && first.is_empty()) {
            return self.rebase(path).map(Some);
        }
        Ok(Some(path))
    }

    fn rebase(&self, path: PathBuf) -> Result<PathBuf> {
        if resolve_file(&path).is_some() || path.is_dir() || !path.is_absolute() {
            return Ok(path);
        }

        // A link may still carry the absolute path from another installation.
        let folders = ["vi.lib", "user.lib", "instr.lib", "resource"];
        let mut parts = path.components();
        if let Some(folder) = parts.find(|part| {
            folders
                .iter()
                .any(|f| part.as_os_str().eq_ignore_ascii_case(f))
        }) {
            let logical = PathBuf::from(folder.as_os_str()).join(parts.as_path());
            let candidate = self.logical(&logical, path.clone())?;
            if resolve_file(&candidate).is_some() || candidate.is_dir() {
                return Ok(candidate);
            }
        }
        Ok(path)
    }
}

fn slash(path: &Path) -> String {
    path.to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('\\', "/")
}

fn build_roots(
    project: &Path,
    build: &lvproj::Build,
    locations: &Locations<'_>,
    issues: &mut BTreeSet<String>,
) -> Result<BTreeSet<PathBuf>> {
    let mut sources = BTreeMap::new();
    for (key, inclusion) in &build.properties {
        let Some((index, field)) = key
            .strip_prefix("Source[")
            .and_then(|key| key.split_once("]."))
        else {
            continue;
        };
        let inclusion = match field {
            "sourceInclusion" => inclusion.as_str(),
            "Library.LVLIBPtopLevel"
                if build.kind == lvproj::PACKED_LIBRARY_TYPE
                    && inclusion.eq_ignore_ascii_case("true")
                    && build
                        .get(&format!("Source[{index}].sourceInclusion"))
                        .is_none() =>
            {
                "TopLevel"
            }
            _ => continue,
        };
        let include = match inclusion {
            "TopLevel" | "Include" => true,
            "Exclude" => false,
            _ => continue,
        };
        let item = build
            .get(&format!("Source[{index}].itemID"))
            .with_context(|| format!("selected build source #{index} has no itemID"))?;
        // LabVIEW writes "/" as an empty container placeholder.
        if item != "/" {
            sources.insert(item, include);
        }
    }

    let mut roots = BTreeSet::new();
    for (&item, &include) in &sources {
        if include && !build.items.contains_key(item) {
            issues.insert(format!("build source {item:?} is absent from the project"));
        }
    }
    let included = |name: &str| {
        sources
            .iter()
            .filter(|(key, _)| is_under(name, key))
            .max_by_key(|(key, _)| key.len())
            .is_some_and(|(_, include)| *include)
    };
    for (name, url) in &build.items {
        let Some(url) = url else { continue };
        if !included(name) {
            continue;
        }
        let path = match locations.url(project, url) {
            Ok(path) => path,
            Err(error) => {
                issues.insert(format!("build source {name:?}: {error:#}"));
                continue;
            }
        };
        if path.is_dir() {
            for (file, relative) in linkfix::walk(&path)? {
                if included(&format!("{name}/{relative}")) {
                    add_root(file, &mut roots, locations, issues);
                }
            }
        } else {
            add_root(path, &mut roots, locations, issues);
        }
    }
    Ok(roots)
}

fn project_roots(
    project: &Path,
    items: &BTreeMap<String, lvproj::ProjectItem>,
    locations: &Locations<'_>,
    issues: &mut BTreeSet<String>,
) -> (BTreeSet<PathBuf>, BTreeMap<String, PathBuf>) {
    let mut selected = BTreeSet::new();
    for (name, item) in items {
        let filename = Path::new(item.url.rsplit(['/', '\\']).next().unwrap_or_default());
        if !item.is_directory && !is_project_source(filename) {
            continue;
        }
        let path = match locations.url(project, &item.url) {
            Ok(path) => path,
            Err(error) => {
                issues.insert(format!("project item {name:?}: {error:#}"));
                continue;
            }
        };
        if item.is_directory {
            match linkfix::walk(&path) {
                Ok(files) => selected.extend(files.into_iter().map(|(path, _)| path)),
                Err(error) => {
                    issues.insert(format!("project folder {name:?}: {error:#}"));
                }
            }
        } else {
            selected.insert(path);
        }
    }

    let mut roots = BTreeSet::new();
    let mut dlls = BTreeMap::new();
    for path in selected {
        if !is_project_source(&path) {
            continue;
        }
        let dll = extension_is(&path, "dll");
        let resolved = if dll {
            path.canonicalize().ok().filter(|file| file.is_file())
        } else {
            resolve_file(&path)
        };
        match resolved {
            Some(file) if dll => {
                dlls.insert(locations.label(&file), file);
            }
            Some(file) => {
                roots.insert(file);
            }
            None => {
                issues.insert(format!(
                    "project source {} is missing or unreadable",
                    locations.label(&path)
                ));
            }
        }
    }
    (roots, dlls)
}

fn is_project_source(path: &Path) -> bool {
    is_labview_file(path) || extension_is(path, "dll")
}

fn is_under(item: &str, parent: &str) -> bool {
    item == parent
        || item
            .strip_prefix(parent)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn add_root(
    file: PathBuf,
    roots: &mut BTreeSet<PathBuf>,
    locations: &Locations<'_>,
    issues: &mut BTreeSet<String>,
) {
    if is_labview_file(&file) {
        match resolve_file(&file) {
            Some(resolved) => {
                roots.insert(resolved);
            }
            None => {
                issues.insert(format!(
                    "build source {} is missing",
                    locations.label(&file)
                ));
            }
        }
    }
}

fn extension_is(path: &Path, name: &str) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case(name))
}

fn is_labview_file(path: &Path) -> bool {
    linkfix::is_vi_like(path)
        || linkfix::is_library_xml(path)
        || ["llb", "lvlibp", "xnode", "xctl"]
            .iter()
            .any(|ext| extension_is(path, ext))
}

fn resolve_file(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return path.canonicalize().ok();
    }
    for ancestor in path.ancestors().skip(1) {
        if extension_is(ancestor, "llb") && ancestor.is_file() {
            return Some(
                ancestor
                    .canonicalize()
                    .ok()?
                    .join(path.strip_prefix(ancestor).ok()?),
            );
        }
        if (extension_is(ancestor, "lvclass") || extension_is(ancestor, "lvlibp"))
            && ancestor.is_file()
        {
            return ancestor.canonicalize().ok();
        }
    }
    None
}

fn llb_container(file: &Path) -> Option<&Path> {
    file.ancestors()
        .skip(1)
        .find(|path| extension_is(path, "llb") && path.is_file())
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum Members {
    None,
    Dispatch,
    All,
}

enum LinkKind {
    Reference,
    Member,
    ClassType,
}

struct ScanLink {
    path: PathBuf,
    kind: LinkKind,
}

fn read_links(
    file: &Path,
    locations: &Locations<'_>,
    members: Members,
    issues: &mut BTreeSet<String>,
) -> Result<Vec<ScanLink>> {
    if linkfix::is_library_xml(file) {
        return xml_links(file, locations, members);
    }
    if extension_is(file, "xnode") || extension_is(file, "xctl") {
        return xml_links(file, locations, Members::All);
    }
    if extension_is(file, "llb") {
        let bytes = std::fs::read(file)?;
        let members = linkfix::llb_members(&bytes);
        ensure!(!members.is_empty(), "no readable members in LLB");
        return Ok(members
            .into_iter()
            .map(|(name, _)| file.join(name))
            .filter(|path| is_labview_file(path))
            .map(|path| ScanLink {
                path,
                kind: LinkKind::Reference,
            })
            .collect());
    }
    let container = llb_container(file);
    let bytes = std::fs::read(container.unwrap_or(file))?;
    let data = match container {
        Some(_) => {
            let name = file.file_name().context("LLB member has no name")?;
            linkfix::llb_members(&bytes)
                .into_iter()
                .find(|(member, _)| name.eq_ignore_ascii_case(member))
                .map(|(_, data)| data)
                .context("LLB member not found")?
        }
        None => &bytes,
    };
    let resource = linkfix::Rsrc::parse(data)?;
    let mut paths = Vec::new();
    let mut scanned_instances = BTreeSet::new();
    read_resource_links(
        &resource,
        file,
        file,
        locations,
        issues,
        &mut paths,
        &mut scanned_instances,
    )?;
    Ok(paths)
}

fn read_resource_links(
    resource: &linkfix::Rsrc,
    file: &Path,
    link_source: &Path,
    locations: &Locations<'_>,
    issues: &mut BTreeSet<String>,
    paths: &mut Vec<ScanLink>,
    scanned_instances: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let version = resource.version()?;
    for section in &resource.sections {
        if !matches!(&section.ident, b"LIvi" | b"LIbd" | b"LIfp" | b"LIds") {
            continue;
        }
        let links = match linkfix::LinkSection::parse(&section.data, version) {
            Ok(links) => links,
            Err(error) => {
                issues.insert(format!(
                    "{}: reading {}: {error:#}",
                    locations.label(file),
                    String::from_utf8_lossy(&section.ident)
                ));
                continue;
            }
        };
        for link in links.objs {
            // The enclosing VINS resource stays part of the same physical VI.
            if link_source != file && link.is_embedded_owner() {
                continue;
            }
            if let Some(instance_path) = link.embedded_instance_path() {
                // VINS paths start inside the enclosing VI's virtual directory.
                let instance_source = instance_path.iter().try_fold(
                    link_source.to_path_buf(),
                    |mut source, name| {
                        let name = String::from_utf8_lossy(name);
                        let entry = Path::new(name.as_ref());
                        if entry.file_name() != Some(entry.as_os_str()) {
                            return None;
                        }
                        source.push(entry);
                        Some(source)
                    },
                );
                let Some(instance_source) = instance_source else {
                    issues.insert(format!(
                        "{}: invalid embedded VI basename",
                        locations.label(file)
                    ));
                    continue;
                };
                if scanned_instances.contains(&instance_source) {
                    continue;
                }
                let Some(embedded) = resource.embedded_vi(instance_path) else {
                    issues.insert(format!(
                        "{} -> {}",
                        locations.label(file),
                        locations.label(&instance_source)
                    ));
                    continue;
                };
                scanned_instances.insert(instance_source.clone());
                if let Err(error) = read_resource_links(
                    &embedded,
                    file,
                    &instance_source,
                    locations,
                    issues,
                    paths,
                    scanned_instances,
                ) {
                    issues.insert(format!(
                        "{}: reading embedded VI {}: {error:#}",
                        locations.label(file),
                        locations.label(&instance_source)
                    ));
                }
                continue;
            }
            match locations.link(link_source, &link.path) {
                Ok(Some(path)) if is_labview_file(&path) => {
                    // A saved class type link can dispatch to that class's
                    // instance methods even without a direct method link.
                    let kind = if link.ident.ends_with(b"PI") && extension_is(&path, "lvclass") {
                        LinkKind::ClassType
                    } else {
                        LinkKind::Reference
                    };
                    paths.push(ScanLink { path, kind });
                }
                Ok(_) => {}
                Err(error) => {
                    issues.insert(format!("{}: {error:#}", locations.label(file)));
                }
            }
        }
    }
    Ok(())
}

fn xml_links(file: &Path, locations: &Locations<'_>, members: Members) -> Result<Vec<ScanLink>> {
    use quick_xml::events::Event;

    let class = extension_is(file, "lvclass");
    if members == Members::None && !class {
        return Ok(Vec::new());
    }
    let xml = std::fs::read_to_string(file)?;
    let mut reader = quick_xml::Reader::from_str(&xml);
    let mut paths = Vec::new();
    loop {
        let event = reader.read_event()?;
        let start = matches!(&event, Event::Start(_));
        match event {
            Event::Start(item) | Event::Empty(item) if item.name().as_ref() == b"Item" => {
                let Some(kind) = item.try_get_attribute("Type")? else {
                    continue;
                };
                let kind = kind.decode_and_unescape_value(reader.decoder())?;
                let member = matches!(
                    kind.as_ref(),
                    "VI" | "LVClass"
                        | "Library"
                        | "LVLibp"
                        | "Packed Library"
                        | "Class Private Data"
                        | "Control"
                        | "Ability VI"
                );
                let include_member = match members {
                    Members::All => member,
                    Members::Dispatch if class && kind == "VI" && start => {
                        is_instance_method(&mut reader)?
                    }
                    _ => false,
                };
                if (kind == "Parent" || include_member)
                    && let Some(url) = item.try_get_attribute("URL")?
                {
                    let url = url.decode_and_unescape_value(reader.decoder())?;
                    let path = locations.url(file, &url)?;
                    if is_labview_file(&path) {
                        paths.push(ScanLink {
                            path,
                            kind: if include_member {
                                LinkKind::Member
                            } else {
                                LinkKind::Reference
                            },
                        });
                    }
                }
            }
            Event::Eof => return Ok(paths),
            _ => {}
        }
    }
}

/// Read the saved dispatch flag from the immediate properties of a method item.
fn is_instance_method(reader: &mut quick_xml::Reader<&[u8]>) -> Result<bool> {
    use quick_xml::events::Event;
    use quick_xml::name::QName;

    let mut depth = 0;
    loop {
        match reader.read_event()? {
            Event::Start(property)
                if depth == 0
                    && property.name().as_ref() == b"Property"
                    && property.try_get_attribute("Name")?.is_some_and(|name| {
                        name.value.as_ref() == b"NI.ClassItem.IsStaticMethod"
                    }) =>
            {
                let value = reader.read_text(QName(b"Property"))?;
                reader.read_to_end(QName(b"Item"))?;
                return Ok(value.trim().eq_ignore_ascii_case("false"));
            }
            Event::Start(_) => depth += 1,
            Event::End(_) if depth == 0 => return Ok(false),
            Event::End(_) => depth -= 1,
            Event::Eof => anyhow::bail!("unclosed class method item"),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_known_roots_and_keeps_external_graph_files_distinct() {
        let base = std::env::temp_dir().join("lvpm-scan-labels");
        let project = base.join("project");
        let labview = base.join("labview");
        let venv = base.join("packages");
        let locations = Locations {
            project_dir: &project,
            labview_dir: Some(&labview),
            venv_dir: Some(&venv),
            addons: &[],
            system_addons: &SystemAddons::default(),
        };
        assert_eq!(
            locations.label(&project.join("src/main.vi")),
            "project/src/main.vi"
        );
        assert_eq!(
            locations.label(&labview.join("vi.lib/helper.vi")),
            "labview/vi.lib/helper.vi"
        );
        assert_eq!(
            locations.label(&venv.join("addon/helper.vi")),
            "venv/addon/helper.vi"
        );
        let first = locations.label(&project.join("../outside-first/helper.vi"));
        let second = locations.label(&base.join("outside-second/helper.vi"));
        for label in [&first, &second] {
            assert!(label.starts_with("external/") && label.ends_with("/helper.vi"));
            assert!(!label.contains("outside-"));
        }
        assert_ne!(first, second);
    }

    #[test]
    fn resolves_system_addons_for_the_selected_version_and_bitness() {
        let root = std::env::temp_dir().join(format!("lvpm-system-addons-{}", std::process::id()));
        let store = root.join("LVAddons");
        let project = root.join("project");
        let installation = root.join("LabVIEW");
        let logical = Path::new("vi.lib/api.llb/Member.vi");
        let make_addon = |release: &str, minimum: &str, bitness: Option<&str>, file: &str| {
            let folder = store.join(release);
            std::fs::create_dir_all(folder.join("vi.lib")).unwrap();
            let info = serde_json::json!({
                "AddonName": "driver",
                "MinimumSupportedLVVersion": minimum,
                "SupportedBitness": bitness,
            });
            std::fs::write(folder.join("lvaddoninfo.json"), info.to_string()).unwrap();
            std::fs::write(folder.join(file), []).unwrap();
        };

        make_addon("common/1", "22.0", None, "vi.lib/api.llb");
        make_addon("common/26.2", "26.0", None, "vi.lib/api.llb");
        make_addon("common/26.10", "26.3", None, "vi.lib/api.llb");
        make_addon("driver32/1", "22.0", Some("32"), "vi.lib/native.vi");
        make_addon("driver64/1", "22.0", Some("64"), "vi.lib/native.vi");
        let target = target::LvTarget {
            version: 25.3,
            bitness: 64,
            path: installation.clone(),
        };

        let system_addons = SystemAddons::read(&store, &target).unwrap();
        let locations = Locations {
            project_dir: &project,
            labview_dir: Some(&installation),
            venv_dir: None,
            addons: &[],
            system_addons: &system_addons,
        };
        let mounted = system_addons.store.as_ref().unwrap();
        let vi = locations.url(&root, "/<vilib>/api.llb/Member.vi").unwrap();
        assert_eq!(vi, mounted.join("common/1").join(logical));
        assert_eq!(
            locations.label(&vi),
            "lvaddons/common/1/vi.lib/api.llb/Member.vi"
        );
        assert_eq!(
            locations.url(&root, "/<vilib>/native.vi").unwrap(),
            mounted.join("driver64/1/vi.lib/native.vi")
        );
        assert_eq!(
            locations.rebase(root.join("old/vi.lib").join("api.llb/Member.vi")).unwrap(),
            vi
        );

        std::fs::create_dir_all(installation.join("vi.lib")).unwrap();
        std::fs::write(installation.join("vi.lib/api.llb"), []).unwrap();
        assert_eq!(
            locations.url(&root, "/<vilib>/api.llb/Member.vi").unwrap(),
            installation.join(logical)
        );
        let newer = target::LvTarget { version: 26.3, ..target };
        assert_eq!(
            SystemAddons::read(&store, &newer).unwrap().roots,
            [mounted.join("common/26.10"), mounted.join("driver64/1")]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolves_venv_addons_without_shadowing_the_base_installation() {
        let root = std::env::temp_dir().join(format!("lvpm-vi-overlay-{}", std::process::id()));
        let installation = root.join("LabVIEW");
        let addons = [
            root.join("packages/first/1"),
            root.join("packages/second/1"),
        ];
        let logical = Path::new("vi.lib/overlay.vi");

        for dir in [&installation, &addons[0], &addons[1]] {
            std::fs::create_dir_all(dir.join("vi.lib")).unwrap();
        }
        let locations = Locations {
            project_dir: &root,
            labview_dir: Some(&installation),
            venv_dir: None,
            addons: &addons,
            system_addons: &SystemAddons::default(),
        };
        let base = installation.join(logical);
        let first = addons[0].join(logical);
        let second = addons[1].join(logical);
        let resolve = || locations.url(&root, "/<vilib>/overlay.vi");

        std::fs::write(&first, []).unwrap();
        assert_eq!(resolve().unwrap(), first.canonicalize().unwrap());
        std::fs::write(&second, []).unwrap();
        assert!(
            resolve()
                .unwrap_err()
                .to_string()
                .contains("multiple venv addons")
        );
        std::fs::write(&base, []).unwrap();
        assert_eq!(resolve().unwrap(), base);
        let global = Locations {
            addons: &[],
            system_addons: &SystemAddons::default(),
            ..locations
        };
        std::fs::remove_file(&base).unwrap();
        assert_eq!(global.url(&root, "/<vilib>/overlay.vi").unwrap(), base);
        std::fs::remove_dir_all(root).unwrap();
    }
}
