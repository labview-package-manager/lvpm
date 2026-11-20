//! Project VI links and declared requirements onto the installed package graph.

use crate::install::{self, PackageMetadata};
use crate::sbom_paths::{diagnostic_path, redact_paths};
use crate::version::{Version, is_version};
use crate::{cache, index, spec, target, vi_graph};
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub type PackageId = (String, String);

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct VersionConstraint {
    pub operator: &'static str,
    pub version: Version,
}

impl VersionConstraint {
    fn allows(&self, installed: &Version) -> bool {
        match self.operator {
            ">=" => installed >= &self.version,
            "<=" => installed <= &self.version,
            ">" => installed > &self.version,
            "<" => installed < &self.version,
            "=" | "==" => installed == &self.version,
            _ => unreachable!("validated version comparison"),
        }
    }
}

fn declared_requirements(
    entries: &[String],
) -> Result<BTreeMap<String, BTreeSet<VersionConstraint>>> {
    let mut requirements = BTreeMap::<String, BTreeSet<_>>::new();
    for entry in entries {
        let comparison = entry.find(['<', '>', '=']).unwrap_or(entry.len());
        let (name, expression) = entry.split_at(comparison);
        let name = name.trim();
        crate::palette::validate_package_name(name)
            .with_context(|| format!("invalid package requirement {entry:?}"))?;
        let constraints = requirements.entry(name.to_ascii_lowercase()).or_default();
        if !expression.is_empty() {
            // Match compound operators first so their final '=' is not read as
            // part of the version, and retain the exact comparison as evidence.
            let (operator, version) = [">=", "<=", "==", ">", "<", "="]
                .into_iter()
                .find_map(|op| {
                    expression
                        .strip_prefix(op)
                        .map(|version| (op, version.trim()))
                })
                .expect("expression begins with a comparison character");
            ensure!(
                is_version(version) && !version.split(['.', '-']).any(str::is_empty),
                "invalid package requirement {entry:?}: {version:?} is not a version"
            );
            constraints.insert(VersionConstraint {
                operator,
                version: Version::parse(version),
            });
        }
    }
    Ok(requirements)
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
pub enum ComponentId {
    Package(PackageId),
    Packed(String),
}

pub struct Package {
    pub name: String,
    pub metadata: Option<PackageMetadata>,
    pub matched_files: usize,
    pub required: BTreeMap<PackageId, BTreeSet<VersionConstraint>>,
}

pub struct PackageGraph {
    pub packages: BTreeMap<PackageId, Package>,
    pub roots: BTreeSet<ComponentId>,
    pub edges: BTreeMap<ComponentId, BTreeSet<ComponentId>>,
    pub issues: BTreeSet<String>,
    pub unowned_external: usize,
}

pub fn project(
    graph: &vi_graph::Graph,
    vipm_database: Option<&Path>,
    output: Option<&crate::sbom_paths::OutputGuard>,
) -> Result<PackageGraph> {
    let mut inventory = Inventory::default();
    let mut paths = BTreeMap::<_, BTreeSet<&str>>::new();
    for (label, file) in &graph.files {
        // A compiled PPL has its own dependency identity in the projected graph.
        if graph.packed.contains_key(label) {
            continue;
        }
        paths.entry(path_key(file)).or_default().insert(label);
        // The install record owns the LLB file, while the linker can name a member.
        for ancestor in file.ancestors().skip(1) {
            if ancestor
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("llb"))
            {
                paths.entry(path_key(ancestor)).or_default().insert(label);
                break;
            }
        }
    }

    let global_store = graph.labview.as_ref().map(target::Roots::labview);
    let venv_store = graph.venv.as_ref().map(crate::venv::Venv::store);
    // Keep attribution for links that actually resolve into the installation.
    // Ownership is matched on the physical path, never on a logical VI name.
    for (label, store) in [
        ("venv", venv_store.as_ref()),
        ("labview", global_store.as_ref()),
    ] {
        let Some(store) = store else { continue };
        let (records, unreadable) = install::list_installed_lenient_with(store, |path| {
            output.map_or(Ok(()), |guard| guard.protect(path))
        })?;
        for (path, error) in unreadable {
            let name = path
                .file_name()
                .map_or_else(|| "record".into(), |name| name.to_string_lossy());
            let reason = redact_paths(&format!("{error:#}"));
            inventory.issues.insert(format!(
                "unreadable lvpm ownership record {label}/installed/{name}: {reason}"
            ));
        }
        for record in records {
            let files = matched(&paths, record.files.iter().map(String::as_str));
            inventory.add(
                record.name,
                record.version,
                record.package_metadata,
                record.dependencies,
                files,
            );
        }
    }

    let default_database = graph.labview.as_ref().and_then(default_vipm_database);
    if let Some(database) = vipm_database.or(default_database.as_deref()) {
        inventory.read_vipm(database, &paths, output)?;
    }
    inventory.collapse(graph, output)
}

fn default_vipm_database(labview: &target::LvTarget) -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    let program_data = PathBuf::from(std::env::var_os("ProgramData")?);
    let path = program_data.join("JKI/VIPM/databases").join(format!(
        "LV {:.1} ({}-bit)",
        labview.version, labview.bitness
    ));
    path.is_dir().then_some(path)
}

fn path_key(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        let normalized = text.replace('\\', "/");
        let normalized = normalized.strip_prefix("//?/").unwrap_or(&normalized);
        normalized.to_lowercase()
    } else {
        text.into_owned()
    }
}

fn matched<'a, 'b>(
    paths: &BTreeMap<String, BTreeSet<&'a str>>,
    records: impl Iterator<Item = &'b str>,
) -> BTreeSet<&'a str> {
    records
        .flat_map(|line| {
            let path = Path::new(line.trim_start_matches('\u{feff}'));
            paths.get(&path_key(path)).into_iter().flatten().copied()
        })
        .collect()
}

struct InstalledPackage {
    package: Package,
    requirements: Vec<String>,
    archive_conflict: bool,
}

#[derive(Default)]
struct Inventory<'a> {
    claims: BTreeMap<&'a str, BTreeSet<PackageId>>,
    packages: BTreeMap<PackageId, InstalledPackage>,
    unused_vipm_records: BTreeMap<String, BTreeSet<PathBuf>>,
    issues: BTreeSet<String>,
}

impl<'a> Inventory<'a> {
    fn add(
        &mut self,
        name: String,
        version: String,
        metadata: Option<PackageMetadata>,
        requirements: Vec<String>,
        files: BTreeSet<&'a str>,
    ) {
        let id = (name.to_ascii_lowercase(), version);
        let record = self
            .packages
            .entry(id.clone())
            .or_insert_with(|| InstalledPackage {
                package: Package {
                    name,
                    metadata: None,
                    matched_files: 0,
                    required: BTreeMap::new(),
                },
                requirements: Vec::new(),
                archive_conflict: false,
            });
        if let (Some(existing), Some(incoming)) = (&record.package.metadata, &metadata) {
            record.archive_conflict |= existing.archive_sha256 != incoming.archive_sha256;
        }
        if record.package.metadata.is_none() {
            record.package.metadata = metadata;
        }
        record.requirements.extend(requirements);
        for file in files {
            self.claims.entry(file).or_default().insert(id.clone());
        }
    }

    fn read_vipm(
        &mut self,
        database: &Path,
        paths: &BTreeMap<String, BTreeSet<&'a str>>,
        output: Option<&crate::sbom_paths::OutputGuard>,
    ) -> Result<()> {
        let entries = std::fs::read_dir(database).with_context(|| {
            format!(
                "reading VIPM ownership database {}",
                diagnostic_path(database)
            )
        })?;
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let dir = entry.path();
            let inventory = dir.join("files-installed");
            if let Some(output) = output {
                output.protect(&inventory)?;
            }
            let installed_files = match std::fs::read(inventory) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    self.issues.insert(format!(
                        "reading VIPM files-installed in {}: {error}",
                        entry.file_name().to_string_lossy()
                    ));
                    continue;
                }
            };
            // The 2026 database stores one absolute path per line. Decode
            // per line so an unrelated legacy path cannot hide valid claims.
            let files = matched(
                paths,
                installed_files
                    .split(|byte| *byte == b'\n')
                    .filter_map(|line| {
                        std::str::from_utf8(line.strip_suffix(b"\r").unwrap_or(line)).ok()
                    }),
            );
            if files.is_empty() {
                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                self.unused_vipm_records
                    .entry(name)
                    .or_default()
                    .insert(dir);
            } else {
                self.load_vipm_record(&dir, files, output)?;
            }
        }
        Ok(())
    }

    fn load_vipm_record(
        &mut self,
        dir: &Path,
        files: BTreeSet<&'a str>,
        output: Option<&crate::sbom_paths::OutputGuard>,
    ) -> Result<()> {
        let archive = vipm_archive_path(dir);
        if let (Some(output), Ok(path)) = (output, &archive) {
            output.protect(path)?;
        }
        let record = archive.and_then(|archive| read_vipm_archive(dir, &archive));
        match record {
            Ok((name, version, metadata, requires)) => {
                self.add(name, version, Some(metadata), requires, files);
            }
            Err(error) => {
                let name = dir
                    .file_name()
                    .expect("package directory")
                    .to_string_lossy();
                let reason = redact_paths(&format!("{error:#}"));
                self.issues.insert(format!("VIPM record {name}: {reason}"));
            }
        }
        Ok(())
    }

    fn load_available_versions(
        &mut self,
        name: &str,
        output: Option<&crate::sbom_paths::OutputGuard>,
    ) -> Result<()> {
        if let Some(dirs) = self.unused_vipm_records.remove(name) {
            for dir in dirs {
                self.load_vipm_record(&dir, BTreeSet::new(), output)?;
            }
        }
        Ok(())
    }

    fn resolve_requirement(
        &mut self,
        source: &PackageId,
        name: &str,
        constraints: &BTreeSet<VersionConstraint>,
        output: Option<&crate::sbom_paths::OutputGuard>,
    ) -> Result<Option<PackageId>> {
        self.load_available_versions(name, output)?;

        // If a VI resolved to an installed version, its physical owner is
        // stronger evidence than a different copy listed in another store.
        let mut candidates: Vec<_> = self
            .packages
            .range((name.to_owned(), String::new())..)
            .take_while(|(id, _)| id.0 == name)
            .collect();
        if candidates
            .iter()
            .any(|(_, installed)| installed.package.matched_files != 0)
        {
            candidates.retain(|(_, installed)| installed.package.matched_files != 0);
        }
        let matches: Vec<_> = candidates
            .iter()
            .filter(|(id, _)| {
                if constraints.is_empty() {
                    return true;
                }
                if !is_version(&id.1) {
                    return false;
                }
                let installed = Version::parse(&id.1);
                constraints
                    .iter()
                    .all(|constraint| constraint.allows(&installed))
            })
            .map(|(id, _)| *id)
            .collect();
        if let [id] = matches.as_slice() {
            return Ok(Some((*id).clone()));
        }

        let requested = if constraints.is_empty() {
            name.to_owned()
        } else {
            constraints
                .iter()
                .map(|constraint| format!("{name}{}{}", constraint.operator, constraint.version))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let versions = candidates
            .iter()
            .map(|(id, _)| id.1.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let issue = if candidates.is_empty() {
            "not installed".to_owned()
        } else if matches.is_empty() {
            format!("installed versions do not satisfy the requirement ({versions})")
        } else {
            format!("ambiguous installed versions ({versions})")
        };
        self.issues.insert(format!(
            "{}@{} requires {requested}: {issue}",
            source.0, source.1
        ));
        Ok(None)
    }

    fn follow_requirements(
        &mut self,
        roots: impl Iterator<Item = PackageId>,
        edges: &mut BTreeMap<ComponentId, BTreeSet<ComponentId>>,
        output: Option<&crate::sbom_paths::OutputGuard>,
    ) -> Result<BTreeSet<PackageId>> {
        let mut seen = BTreeSet::new();
        let mut pending: Vec<_> = roots.collect();
        while let Some(source) = pending.pop() {
            if !seen.insert(source.clone()) {
                continue;
            }
            // Load any additional record for this name before closing over
            // its requirements. Future dependency lookups cannot change it.
            self.load_available_versions(&source.0, output)?;
            let record = &self.packages[&source];
            ensure!(
                !record.archive_conflict,
                "different archives claim package {}@{}",
                source.0,
                source.1
            );
            let from = ComponentId::Package(source.clone());
            let direct = edges.entry(from).or_default();
            let requirements = match declared_requirements(&record.requirements) {
                Ok(requirements) => requirements,
                Err(error) => {
                    self.issues.insert(format!(
                        "{}@{} has invalid Requires: {error:#}",
                        source.0, source.1
                    ));
                    continue;
                }
            };
            for (name, constraints) in requirements {
                let Some(dependency) =
                    self.resolve_requirement(&source, &name, &constraints, output)?
                else {
                    continue;
                };
                if dependency == source {
                    // A self-requirement can constrain the installed version,
                    // but CycloneDX forbids a dependency on the same bom-ref.
                    continue;
                }
                direct.insert(ComponentId::Package(dependency.clone()));
                self.packages
                    .get_mut(&source)
                    .expect("installed package")
                    .package
                    .required
                    .insert(dependency.clone(), constraints);
                pending.push(dependency);
            }
        }
        Ok(seen)
    }

    fn collapse(
        mut self,
        graph: &vi_graph::Graph,
        output: Option<&crate::sbom_paths::OutputGuard>,
    ) -> Result<PackageGraph> {
        let mut owners = BTreeMap::new();
        let mut package_files = BTreeMap::<PackageId, Vec<&str>>::new();
        for (file, claims) in std::mem::take(&mut self.claims) {
            if claims.len() != 1 {
                self.issues.insert(format!(
                    "{file} belongs to multiple packages: {}",
                    claims
                        .iter()
                        .map(|(name, version)| format!("{name}@{version}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                continue;
            }
            let owner = claims.into_iter().next().expect("one owner");
            package_files.entry(owner.clone()).or_default().push(file);
            owners.insert(file, owner);
        }
        for (id, files) in &package_files {
            self.packages
                .get_mut(id)
                .expect("matched package")
                .package
                .matched_files = files.len();
        }
        let unowned_external = graph
            .files
            .keys()
            .filter(|label| {
                !label.starts_with("project/")
                    && !owners.contains_key(label.as_str())
                    && !graph.packed.contains_key(*label)
            })
            .count();
        let roots = first_components(
            graph,
            &owners,
            graph.roots.iter().map(String::as_str).collect(),
            None,
        );
        let mut edges: BTreeMap<_, _> = package_files
            .iter()
            .map(|(id, files)| {
                let direct = first_components(graph, &owners, files.clone(), Some(id));
                (ComponentId::Package(id.clone()), direct)
            })
            .collect();
        let included = self.follow_requirements(package_files.into_keys(), &mut edges, output)?;
        for label in graph.packed.keys() {
            if let Some(direct) = graph.edges.get(label) {
                edges.insert(
                    ComponentId::Packed(label.clone()),
                    direct.iter().cloned().map(ComponentId::Packed).collect(),
                );
            }
        }
        Ok(PackageGraph {
            packages: self
                .packages
                .into_iter()
                .filter(|(id, _)| included.contains(id))
                .map(|(id, installed)| (id, installed.package))
                .collect(),
            roots,
            edges,
            issues: self
                .issues
                .into_iter()
                .map(|issue| redact_paths(&issue))
                .collect(),
            unowned_external,
        })
    }
}

/// Walk through unowned files and files owned by the current package.
/// Stop each path at a different package or a compiled PPL.
fn first_components<'a>(
    graph: &'a vi_graph::Graph,
    owners: &BTreeMap<&str, PackageId>,
    mut pending: Vec<&'a str>,
    from: Option<&PackageId>,
) -> BTreeSet<ComponentId> {
    let mut visited = BTreeSet::new();
    let mut found = BTreeSet::new();
    while let Some(file) = pending.pop() {
        if !visited.insert(file) {
            continue;
        }
        if graph.packed.contains_key(file) {
            found.insert(ComponentId::Packed(file.to_owned()));
            continue;
        }
        if let Some(owner) = owners.get(file)
            && Some(owner) != from
        {
            found.insert(ComponentId::Package(owner.clone()));
            continue;
        }
        pending.extend(graph.edges[file].iter().map(String::as_str));
    }
    found
}

fn vipm_archive_path(dir: &Path) -> Result<PathBuf> {
    let mut archives = Vec::new();
    for file in std::fs::read_dir(dir)? {
        let file = file?;
        let path = file.path();
        if file.file_type()?.is_file()
            && path.extension().is_some_and(|ext| {
                ext.eq_ignore_ascii_case("vip") || ext.eq_ignore_ascii_case("ogp")
            })
        {
            archives.push(path);
        }
    }
    ensure!(
        archives.len() == 1,
        "expected one installed package archive, found {}",
        archives.len()
    );
    Ok(archives.pop().expect("one archive"))
}

fn read_vipm_archive(
    dir: &Path,
    archive: &Path,
) -> Result<(String, String, PackageMetadata, Vec<String>)> {
    let bytes = std::fs::read(archive)?;
    let md5 = index::md5_hex(&bytes);
    let sha256 = cache::sha256_hex(&bytes);
    let mut zip = install::open_archive(bytes)?;
    let spec = spec::parse(&install::read_spec(&mut zip)?)?;
    ensure!(
        !spec.version.is_empty()
            && dir
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case(&spec.name)),
        "package name or version does not match the installation record"
    );
    let metadata = PackageMetadata {
        vendor: spec.vendor,
        license: spec.license,
        archive_md5: md5,
        archive_sha256: sha256,
    };
    let requirements = spec
        .requires
        .as_deref()
        .unwrap_or_default()
        .split([',', ';'])
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect();
    Ok((spec.name, spec.version, metadata, requirements))
}
