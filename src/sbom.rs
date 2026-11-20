//! CycloneDX export of installed packages and LabVIEW package requirements.

use crate::install::Manifest;
use crate::lvproj;
use crate::nipkg::{self, PayloadInventory, PayloadReadKind};
use crate::package_graph::ComponentId;
use crate::ppl::{self, ARTIFACT_NAME_PROPERTY};
use crate::project::Selection;
use crate::sbom_artifacts::{self, ArtifactFile, has_extension};
use crate::sbom_metadata::{
    self, ARTIFACT_FILE, DIGEST_SUBJECT, portable_path_identity, purl_segment, vipm_purl,
};
use crate::version::Version;
use crate::{
    sbom_artifact_document::{self, PostBuild},
    sbom_ci, sbom_paths,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

const PROJECT_REF: &str = "lvpm:project";

fn package_ref(key: &str) -> String {
    format!("lvpm:package:{key}")
}

fn scanned_package_ref(name: &str, version: &str) -> String {
    vipm_purl(name, version).unwrap_or_else(|| {
        format!(
            "lvpm:package:{}@{}",
            purl_segment(name),
            purl_segment(version)
        )
    })
}

fn package_component(
    name: &str,
    version: &str,
    reference: &str,
    metadata: Option<&crate::install::PackageMetadata>,
) -> Value {
    let mut component = json!({
        "type": "library",
        "bom-ref": reference,
        "name": name,
    });
    if !version.trim().is_empty() {
        component["version"] = json!(version);
    }
    if let Some(purl) = vipm_purl(name, version) {
        component["purl"] = json!(purl);
    }
    if let Some(metadata) = metadata {
        component["hashes"] = json!([
            {"alg": "MD5", "content": metadata.archive_md5},
            {"alg": "SHA-256", "content": metadata.archive_sha256}
        ]);
        let vendor = metadata
            .vendor
            .as_deref()
            .filter(|vendor| !vendor.trim().is_empty());
        let license = metadata
            .license
            .as_deref()
            .filter(|license| !license.trim().is_empty());
        if let Some(vendor) = vendor {
            component["supplier"] = json!({"name": vendor});
        }
        if let Some(license) = license {
            component["licenses"] = json!([{"license": {"name": license}}]);
        }
        component["properties"] = json!(
            sbom_metadata::archive_properties(vendor.is_some(), license.is_some())
                .map(|(name, value)| json!({"name": name, "value": value}))
                .collect::<Vec<_>>()
        );
    }
    component
}

pub fn document(
    name: &str,
    version: Option<&str>,
    selection: &Selection,
    installed: &[Manifest],
    include: Option<&Path>,
) -> Result<Value> {
    let mut records = BTreeMap::new();
    for record in installed {
        let key = record.name.to_ascii_lowercase();
        ensure!(
            records.insert(key, record).is_none(),
            "multiple installed records for package {:?}",
            record.name
        );
    }

    let mut roots = BTreeSet::new();
    for (package, constraint) in &selection.dependencies {
        let key = package.to_ascii_lowercase();
        let record = records.get(&key).with_context(|| {
            format!("project dependency {package:?} is not installed in this target")
        })?;
        ensure!(
            constraint.allows(&Version::parse(&record.version)),
            "installed {} {} does not satisfy project requirement {package}{constraint}",
            record.name,
            record.version
        );
        roots.insert(key);
    }

    let mut graph = BTreeMap::new();
    let mut pending: Vec<_> = roots.iter().cloned().collect();
    while let Some(key) = pending.pop() {
        if graph.contains_key(&key) {
            continue;
        }
        let record = records[&key];
        let mut direct = BTreeSet::new();
        for requirement in &record.dependencies {
            let dependency = requirement.to_ascii_lowercase();
            ensure!(
                records.contains_key(&dependency),
                "dependency {requirement:?} of {} is not installed in this target",
                record.name
            );
            direct.insert(dependency);
        }
        pending.extend(direct.iter().cloned());
        graph.insert(key, direct);
    }

    let mut subject = json!({"type": "application", "bom-ref": PROJECT_REF, "name": name});
    if let Some(version) = version {
        subject["version"] = json!(version);
    }

    let mut components = Vec::with_capacity(graph.len());
    let mut dependencies = Vec::with_capacity(graph.len() + 1);
    let project_dependencies = roots
        .iter()
        .map(|key| package_ref(key))
        .collect::<BTreeSet<_>>();
    for (key, direct) in &graph {
        let record = records[key];
        let reference = package_ref(key);
        components.push(package_component(
            &record.name,
            &record.version,
            &reference,
            record.package_metadata.as_ref(),
        ));
        dependencies.push(json!({
            "ref": reference,
            "dependsOn": direct.iter().map(|key| package_ref(key)).collect::<Vec<_>>()
        }));
    }

    assemble(
        json!({"component": subject}),
        components,
        dependencies,
        project_dependencies,
        include,
    )
}

pub fn packed_library_document(path: &Path, include: Option<&Path>) -> Result<Value> {
    let name = path
        .file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .context("packed library has no UTF-8 filename stem")?;
    let artifact = ppl::read_artifact(path)?;
    let library = artifact.library;
    let artifact_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("packed library has no UTF-8 filename")?;
    let mut subject = json!({
        "type": "library",
        "bom-ref": PROJECT_REF,
        "name": name,
        "hashes": [{"alg": "SHA-256", "content": artifact.sha256}],
        "properties": [
            {"name": ARTIFACT_NAME_PROPERTY, "value": artifact_name},
            {"name": DIGEST_SUBJECT, "value": ARTIFACT_FILE}
        ]
    });
    if let Some(library_id) = library.library_id {
        subject["properties"]
            .as_array_mut()
            .expect("packed library properties")
            .push(json!({"name": "lvpm:ppl:library-id", "value": library_id}));
    }
    if let Some(version) = library.version {
        subject["version"] = json!(version);
    }

    let mut direct = BTreeSet::new();
    let mut components = Vec::new();
    for import in library.imports {
        let path = portable_path_identity(&import.path);
        let reference = format!(
            "lvpm:ppl:{}:{}",
            purl_segment(&path),
            purl_segment(&import.library_id)
        );
        components.push(packed_import_component(&reference, &import));
        direct.insert(reference);
    }

    let metadata = json!({
        "component": subject,
        "properties": [
            {"name": "lvpm:ppl:dependency-source", "value": "leif-direct-packed-library-references"}
        ]
    });
    let mut document = assemble(metadata, components, Vec::new(), direct, include)?;
    document["compositions"] = json!([{"aggregate": "incomplete", "assemblies": [PROJECT_REF]}]);
    Ok(document)
}

pub(crate) fn artifact_document(
    post_build: &PostBuild<'_>,
    include: Option<&Path>,
) -> Result<Value> {
    let primary = post_build.primary(None)?;
    let name = post_build.standalone_name(primary)?;
    let subject = json!({"type": if primary.is_some() { "file" } else { "application" }, "bom-ref": PROJECT_REF, "name": name});
    let (mut document, nipkg_inventory) = match primary {
        Some(file) if has_extension(&file.key, ".nipkg") => {
            let (document, inventory) = artifact_nipkg_document(file, include)?;
            (document, Some(inventory))
        }
        Some(file) if has_extension(&file.key, ".lvlibp") => (
            packed_library_document(file.inspection_path(), include)?,
            None,
        ),
        _ => (
            assemble(
                json!({"component": subject}),
                Vec::new(),
                Vec::new(),
                BTreeSet::new(),
                include,
            )?,
            None,
        ),
    };
    if document["dependencies"][0]["dependsOn"]
        .as_array()
        .is_some_and(Vec::is_empty)
    {
        // The artifact-only mode did not discover an authoritative empty graph.
        document["dependencies"] = json!([]);
    }
    post_build.attach(&mut document, primary, nipkg_inventory)?;
    Ok(document)
}

pub(crate) struct ProjectBuildOptions<'a> {
    pub(crate) include: Option<&'a Path>,
    pub(crate) environment: crate::vi_graph::Environment<'a>,
    pub(crate) allow_incomplete: bool,
    pub(crate) vipm_database: Option<&'a Path>,
    pub(crate) output: Option<&'a sbom_paths::OutputGuard>,
}

pub(crate) fn build_document(
    path: &Path,
    target: &str,
    spec: &str,
    options: ProjectBuildOptions<'_>,
    post_build: Option<&PostBuild<'_>>,
) -> Result<Value> {
    let build = lvproj::read_build(path, target, spec).with_context(|| {
        format!(
            "reading build specification {spec:?} on target {target:?} in {}",
            path.display()
        )
    })?;
    let primary = post_build
        .map(|artifacts| artifacts.primary(Some(&build)))
        .transpose()?
        .flatten();
    let (mut document, nipkg_inventory) = if let Some(file) = primary
        && build.kind.eq_ignore_ascii_case(lvproj::NIPKG_TYPE)
    {
        ensure!(
            options.vipm_database.is_none(),
            "--vipm-database is only used for VI dependency scans"
        );
        let configured = lvproj::nipkg_requirements(build.properties)?;
        let (mut built, inventory) = artifact_nipkg_document(file, options.include)?;
        ensure!(
            built["metadata"]["component"]["name"] == configured.name,
            "NI package {:?} does not match the configured package {:?}",
            file.key,
            configured.name
        );
        let properties = built["metadata"]["properties"]
            .as_array_mut()
            .expect("NI package properties");
        let configured_requirements = configured.requirements.into_iter().map(|(name, requirement)| json!({
            "name": name, "relationship": requirement.relationship, "constraints": requirement.version_fields,
        })).collect::<Vec<_>>();
        properties.extend([
            json!({"name": "lvpm:nipkg:target", "value": target}),
            json!({"name": "lvpm:nipkg:build-spec", "value": spec}),
            json!({"name": "lvpm:nipkg:configured-requirements", "value": json!(configured_requirements).to_string()}),
        ]);
        if let Some(version) = configured.version {
            properties.push(json!({"name": "lvpm:nipkg:configured-version", "value": version}));
        }
        if let Some(auto_select) = configured.auto_select {
            properties
                .push(json!({"name": "lvpm:nipkg:auto-select-dependencies", "value": auto_select}));
        }
        (built, Some(inventory))
    } else {
        let document =
            configured_build_document(build, path, target, spec, options, primary.is_some())?;
        (document, None)
    };
    if let Some(post_build) = post_build {
        post_build.attach(&mut document, primary, nipkg_inventory)?;
    }
    Ok(document)
}

pub(crate) fn project_document(
    path: &Path,
    environment: crate::vi_graph::Environment<'_>,
    include: Option<&Path>,
    allow_incomplete: bool,
    vipm_database: Option<&Path>,
    output: &sbom_paths::OutputGuard,
) -> Result<(Value, Option<sbom_artifacts::Inventory>)> {
    let name = path
        .file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .context("the LabVIEW project filename has no usable name")?;
    let items = lvproj::read_project_items(path)?;
    let graph = crate::vi_graph::scan_project(path, &items, environment)?;
    graph.protect_inputs(output)?;
    let packages = crate::package_graph::project(&graph, vipm_database, Some(output))?;
    report_scan_issues(
        graph.issues.iter().chain(&packages.issues),
        allow_incomplete,
        path,
        "project",
    )?;
    let inventory = if graph.project_dlls.is_empty() {
        None
    } else {
        Some(sbom_artifacts::scan_project_dlls(
            &graph.project_dlls,
            output,
        )?)
    };
    let metadata = json!({
        "component": {"type": "application", "bom-ref": PROJECT_REF, "name": name},
        "properties": [
            {"name": "lvpm:labview:scan-scope", "value": "project"},
            {"name": "lvpm:labview:ownership-source", "value": "installed-file-ownership"}
        ]
    });
    let mut document = vi_document(metadata, include, graph, packages, true)?;
    if let Some(inventory) = &inventory {
        sbom_artifact_document::add_project_dlls(&mut document, inventory)?;
    }
    Ok((document, inventory))
}

fn configured_build_document(
    build: lvproj::Build,
    path: &Path,
    target: &str,
    spec: &str,
    options: ProjectBuildOptions<'_>,
    artifact_bound: bool,
) -> Result<Value> {
    let ProjectBuildOptions {
        include,
        environment,
        allow_incomplete,
        vipm_database,
        output,
    } = options;
    if matches!(
        build.kind.as_str(),
        "EXE" | "Source Distribution" | lvproj::PACKED_LIBRARY_TYPE
    ) {
        return scan_build_document(path, &build, build_metadata(target, spec, &build), options);
    }

    let crate::distribution::DistributionSources {
        files: packed_files,
        issues,
        vi_builds,
    } = crate::distribution::selected_sources(path, target, &build)?;
    let nipkg = build.kind.eq_ignore_ascii_case(lvproj::NIPKG_TYPE);
    ensure!(
        nipkg
            || build.kind == "Installer"
            || (artifact_bound && build.kind == "DLL")
            || !packed_files.is_empty()
            || !issues.is_empty(),
        "build specification {spec:?} has unsupported type {:?} and no recognized packed library selections",
        build.kind
    );
    ensure!(
        vipm_database.is_none() || !vi_builds.is_empty(),
        "--vipm-database is only used for VI dependency scans"
    );
    report_scan_issues(issues.iter(), allow_incomplete, path, "build")?;
    let mut document = if nipkg {
        nipkg_document(build, target, spec, include)?
    } else {
        distribution_document(&build, target, spec, include)?
    };
    add_packed_payload(&mut document, path, packed_files, allow_incomplete, output)?;
    if !issues.is_empty() {
        let properties = document["metadata"]["properties"]
            .as_array_mut()
            .expect("scan metadata properties");
        properties.extend(
            issues
                .into_iter()
                .map(|issue| json!({"name": "lvpm:labview:scan-issue", "value": issue})),
        );
        let compositions = document
            .as_object_mut()
            .expect("SBOM object")
            .entry("compositions")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .expect("SBOM compositions");
        let incomplete = json!({"aggregate": "incomplete", "assemblies": [PROJECT_REF]});
        if !compositions.contains(&incomplete) {
            compositions.push(incomplete);
        }
    }
    if vi_builds.is_empty() {
        return Ok(document);
    }

    let children = vi_builds
        .into_iter()
        .map(|(spec, child)| {
            let mut metadata = build_metadata(target, &spec, &child);
            metadata["properties"]
                .as_array_mut()
                .expect("build metadata properties")
                .push(json!({"name": "lvpm:nipkg:configured-source", "value": "true"}));
            let child_options = ProjectBuildOptions {
                include: None,
                environment,
                allow_incomplete,
                vipm_database,
                output,
            };
            scan_build_document(path, &child, metadata, child_options)
                .with_context(|| format!("scanning configured NI package source {spec:?}"))
        })
        .collect::<Result<Vec<_>>>()?;
    crate::sbom_merge::merge_configured_builds(document, children)
}

fn scan_build_document(
    path: &Path,
    build: &lvproj::Build,
    metadata: Value,
    options: ProjectBuildOptions<'_>,
) -> Result<Value> {
    let graph = crate::vi_graph::scan(path, build, options.environment)?;
    if let Some(output) = options.output {
        graph.protect_inputs(output)?;
    }
    let packages = crate::package_graph::project(&graph, options.vipm_database, options.output)?;
    report_scan_issues(
        graph.issues.iter().chain(&packages.issues),
        options.allow_incomplete,
        path,
        "build",
    )?;
    vi_document(metadata, options.include, graph, packages, false)
}

fn distribution_document(
    build: &lvproj::Build,
    target: &str,
    spec: &str,
    include: Option<&Path>,
) -> Result<Value> {
    let name = build
        .get("INST_productName")
        .or_else(|| build.get("TgtF_productName"))
        .unwrap_or(spec);
    let kind = if build.kind == "DLL" {
        "library"
    } else {
        "application"
    };
    let mut component = json!({"type": kind, "bom-ref": PROJECT_REF, "name": name});
    if let Some(version) = build.get("INST_productVersion") {
        component["version"] = json!(version);
    }
    let properties = json!([
        {"name": "lvpm:labview:target", "value": target},
        {"name": "lvpm:labview:build-spec", "value": spec},
        {"name": "lvpm:labview:build-type", "value": build.kind},
        {"name": "lvpm:labview:dependency-source", "value": "lvproj-configured-packed-library-payload"}
    ]);
    let mut components = Vec::new();
    crate::sbom_installers::add_configured_installers(build, &mut components);
    let mut document = assemble(
        json!({"component": component, "properties": properties}),
        components,
        Vec::new(),
        BTreeSet::new(),
        include,
    )?;
    document["compositions"] = json!([{"aggregate": "incomplete", "assemblies": [PROJECT_REF]}]);
    Ok(document)
}

fn nipkg_document(
    build: lvproj::Build,
    target: &str,
    spec: &str,
    include: Option<&Path>,
) -> Result<Value> {
    let build = lvproj::nipkg_requirements(build.properties)?;

    let mut subject = json!({"type": "application", "bom-ref": PROJECT_REF, "name": build.name});
    if let Some(version) = build.version {
        subject["version"] = json!(version);
    }
    let mut properties = vec![
        json!({"name": "lvpm:nipkg:target", "value": target}),
        json!({"name": "lvpm:nipkg:build-spec", "value": spec}),
        json!({"name": "lvpm:nipkg:dependency-source", "value": "lvproj-explicit"}),
    ];
    if let Some(auto_select) = build.auto_select {
        properties
            .push(json!({"name": "lvpm:nipkg:auto-select-dependencies", "value": auto_select}));
    }

    let mut components = Vec::new();
    let mut project_dependencies = BTreeSet::new();
    for (name, requirement) in build.requirements {
        let reference = format!("lvpm:nipkg:{}", purl_segment(&name));
        if requirement.relationship == "Required Dependency" {
            project_dependencies.insert(reference.clone());
        }
        let mut properties =
            vec![json!({"name": "lvpm:nipkg:relationship", "value": requirement.relationship})];
        for (key, value) in requirement.version_fields {
            properties.push(json!({"name": format!("lvpm:nipkg:{key}"), "value": value}));
        }
        components.push(json!({
            "type": "library", "bom-ref": reference, "name": name, "properties": properties
        }));
    }

    assemble(
        json!({"component": subject, "properties": properties}),
        components,
        Vec::new(),
        project_dependencies,
        include,
    )
}

pub fn nipkg_archive_document(path: &Path, include: Option<&Path>) -> Result<Value> {
    let archive = nipkg::read(path, PayloadReadKind::Packed)
        .with_context(|| format!("reading NI package {}", path.display()))?;
    nipkg_control_document(archive.control, archive.packed, include)
}

fn artifact_nipkg_document(
    file: &ArtifactFile,
    include: Option<&Path>,
) -> Result<(Value, PayloadInventory)> {
    let archive = nipkg::read(file.inspection_path(), PayloadReadKind::Both)
        .with_context(|| format!("reading NI package {:?}", file.key))?;
    let document = nipkg_control_document(archive.control, archive.packed, include)?;
    Ok((document, archive.inventory))
}

fn nipkg_control_document(
    control: nipkg::Control,
    packed: nipkg::PackedPayload,
    include: Option<&Path>,
) -> Result<Value> {
    let subject = json!({
        "type": "application", "bom-ref": PROJECT_REF, "name": control.name, "version": control.version
    });
    let mut properties =
        vec![json!({"name": "lvpm:nipkg:dependency-source", "value": "nipkg-control"})];
    if let Some(architecture) = control.architecture {
        properties.push(json!({"name": "lvpm:nipkg:architecture", "value": architecture}));
    }
    for (name, value) in control.relationships {
        properties.push(json!({"name": format!("lvpm:nipkg:{name}"), "value": value}));
    }

    let mut components = Vec::new();
    let mut project_dependencies = BTreeSet::new();
    for (name, dependency) in control.dependencies {
        let reference = format!("lvpm:nipkg:{}", purl_segment(&name));
        if dependency.required {
            project_dependencies.insert(reference.clone());
        }
        let properties = dependency
            .properties
            .into_iter()
            .map(|(name, value)| json!({"name": format!("lvpm:nipkg:{name}"), "value": value}))
            .collect::<Vec<_>>();
        components.push(json!({
            "type": "library", "bom-ref": reference, "name": name, "properties": properties
        }));
    }
    let payload = nipkg_packed_payload(packed);
    project_dependencies.extend(payload.roots);
    let mut incomplete: Vec<_> = payload.components.keys().cloned().collect();
    components.extend(payload.components.into_values());
    if !payload.issues.is_empty() {
        properties.extend(payload.issues.into_iter().map(|issue| {
            json!({
                "name": "lvpm:nipkg:payload-scan-issue", "value": issue
            })
        }));
        incomplete.push(PROJECT_REF.to_owned());
    }
    let mut document = assemble(
        json!({"component": subject, "properties": properties}),
        components,
        payload.dependencies,
        project_dependencies,
        include,
    )?;
    if !incomplete.is_empty() {
        document["compositions"] = json!([{"aggregate": "incomplete", "assemblies": incomplete}]);
    }
    Ok(document)
}

#[derive(Default)]
struct NipkgPackedPayload {
    components: BTreeMap<String, Value>,
    dependencies: Vec<Value>,
    roots: BTreeSet<String>,
    issues: Vec<String>,
}

fn nipkg_packed_payload(packed: crate::nipkg::PackedPayload) -> NipkgPackedPayload {
    let mut payload = NipkgPackedPayload {
        issues: packed.issues,
        ..Default::default()
    };
    let mut libraries = BTreeMap::new();
    for (key, bytes) in packed.files {
        let path = key.display();
        let library = ppl::read(&bytes)
            .inspect_err(|error| {
                payload.issues.push(format!("{path}: {error:#}"));
            })
            .ok();
        let artifact = crate::vi_graph::PackedComponent {
            name: key
                .path
                .rsplit('/')
                .next()
                .expect("normalized archive path")
                .to_owned(),
            artifact_path: None,
            reference_paths: BTreeSet::new(),
            sha256: Some(crate::cache::sha256_hex(&bytes)),
            version: library.as_ref().and_then(|library| library.version.clone()),
            library_id: library
                .as_ref()
                .and_then(|library| library.library_id.clone()),
        };
        let reference = match &key.container {
            Some(container) => format!(
                "lvpm:ppl:nipkg-nested:{}:{}",
                purl_segment(container),
                purl_segment(&key.path)
            ),
            None => packed_ref(&format!("nipkg/{path}")),
        };
        let mut component = packed_component(&reference, &artifact, false);
        component["properties"]
            .as_array_mut()
            .expect("PPL properties")
            .extend([
                json!({"name": "lvpm:ppl:shipment", "value": "package-payload"}),
                json!({"name": "lvpm:ppl:payload-path", "value": path}),
            ]);
        payload.roots.insert(reference.clone());
        payload.components.insert(reference.clone(), component);
        libraries.insert(key, (reference, library));
    }

    for (key, (reference, library)) in &libraries {
        let Some(library) = library else { continue };
        let mut direct = BTreeSet::new();
        for import in &library.imports {
            let candidate =
                nipkg_import_path(&key.path, import).map(|path| crate::nipkg::PackedPath {
                    container: key.container.clone(),
                    path,
                });
            let shipped = candidate
                .as_ref()
                .and_then(|path| libraries.get(path))
                .filter(|(_, library)| {
                    library
                        .as_ref()
                        .and_then(|library| library.library_id.as_deref())
                        == Some(import.library_id.as_str())
                });
            let path = portable_path_identity(&import.path);
            let target = if let Some((reference, _)) = shipped {
                let component = payload
                    .components
                    .get_mut(reference)
                    .expect("payload PPL component");
                let properties = component["properties"]
                    .as_array_mut()
                    .expect("PPL properties");
                let link = json!({"name": "lvpm:ppl:reference-path", "value": path});
                if !properties.contains(&link) {
                    properties.push(link);
                }
                reference.clone()
            } else {
                let target = format!(
                    "lvpm:ppl:nipkg-reference:{}:{}:{}",
                    purl_segment(reference),
                    purl_segment(&path),
                    purl_segment(&import.library_id),
                );
                payload
                    .components
                    .entry(target.clone())
                    .or_insert_with(|| packed_import_component(&target, import));
                target
            };
            if target != *reference {
                direct.insert(target);
            }
        }
        payload
            .dependencies
            .push(json!({"ref": reference, "dependsOn": direct}));
    }
    payload
}

// Resolve in the package's virtual path space without consulting the host.
// A parent step is permitted only while it stays inside that path space.
fn nipkg_import_path(importer: &str, import: &ppl::Import) -> Option<String> {
    if !import.relative || import.path.starts_with('/') || import.path.contains(':') {
        return None;
    }
    let mut parts = importer.split('/').collect::<Vec<_>>();
    parts.pop();
    for part in import.path.split('/') {
        match part {
            "" => return None,
            "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

fn build_metadata(target: &str, spec: &str, build: &lvproj::Build) -> Value {
    let name = build.get("TgtF_productName").unwrap_or(spec);
    let mut subject = json!({
        "type": if build.kind == "EXE" { "application" } else { "library" },
        "bom-ref": PROJECT_REF,
        "name": name
    });
    if build.get("Bld_version.major").is_some() {
        let version = ["major", "minor", "patch", "build"]
            .map(|part| {
                build
                    .get(&format!("Bld_version.{part}"))
                    .unwrap_or("0")
                    .to_owned()
            })
            .join(".");
        subject["version"] = json!(version);
    }
    if build.kind == lvproj::PACKED_LIBRARY_TYPE
        && let Some(name) = build
            .get("TgtF_targetfileName")
            .or_else(|| build.get("Destination[0].destName"))
            .or_else(|| build.get("Destination[0].path"))
            .and_then(|value| value.rsplit(['/', '\\']).next())
            .filter(|name| {
                name.rsplit_once('.').is_some_and(|(stem, extension)| {
                    !stem.is_empty() && extension.eq_ignore_ascii_case("lvlibp")
                })
            })
    {
        subject["properties"] = json!([{"name": ARTIFACT_NAME_PROPERTY, "value": name}]);
    }
    json!({
        "component": subject,
        "properties": [
            {"name": "lvpm:labview:target", "value": target},
            {"name": "lvpm:labview:build-spec", "value": spec},
            {"name": "lvpm:labview:build-type", "value": build.kind}
        ]
    })
}

fn vi_document(
    mut metadata: Value,
    include: Option<&Path>,
    mut graph: crate::vi_graph::Graph,
    packages: crate::package_graph::PackageGraph,
    project_scope: bool,
) -> Result<Value> {
    graph.issues.extend(packages.issues);
    let properties = metadata["properties"]
        .as_array_mut()
        .expect("VI scan metadata properties");
    properties.extend([
        json!({"name": "lvpm:labview:dependency-source", "value": "static-vi-links-and-installed-file-ownership"}),
        json!({"name": "lvpm:labview:scan-environment", "value": if graph.venv.is_some() { "venv" } else { "labview-installation" }}),
        json!({"name": "lvpm:labview:scanned-files", "value": graph.files.len().to_string()}),
        json!({"name": "lvpm:labview:unowned-external-files", "value": packages.unowned_external.to_string()}),
    ]);
    for issue in &graph.issues {
        properties.push(json!({"name": "lvpm:labview:scan-issue", "value": issue}));
    }
    let reference = |id: &ComponentId| match id {
        ComponentId::Package((name, version)) => scanned_package_ref(name, version),
        ComponentId::Packed(label) => packed_ref(label),
    };
    let mut components: Vec<_> = packages
        .packages
        .iter()
        .map(|(id, package)| {
            let mut component = package_component(
                &package.name,
                &id.1,
                &scanned_package_ref(&id.0, &id.1),
                package.metadata.as_ref(),
            );
            let fields = component.as_object_mut().expect("package component");
            crate::sbom_artifact_document::add_property(
                fields,
                "lvpm:labview:matched-files",
                json!(package.matched_files.to_string()),
            );
            for ((name, version), constraints) in &package.required {
                let mut requirement = json!({"name": name, "version": version});
                if let Some(minimum) = constraints.first()
                    && constraints.len() == 1
                    && minimum.operator == ">="
                {
                    requirement["minimum"] = json!(minimum.version.raw);
                } else if !constraints.is_empty() {
                    requirement["constraints"] = json!(
                        constraints
                            .iter()
                            .map(|constraint| json!({
                                "operator": constraint.operator,
                                "version": constraint.version.raw,
                            }))
                            .collect::<Vec<_>>()
                    );
                }
                crate::sbom_artifact_document::add_property(
                    fields,
                    "lvpm:package:requires",
                    json!(requirement.to_string()),
                );
            }
            component
        })
        .collect();
    let mut partial_assemblies = Vec::new();
    for (label, packed) in &graph.packed {
        let reference = packed_ref(label);
        components.push(packed_component(
            &reference,
            packed,
            !project_scope && graph.roots.contains(label),
        ));
        partial_assemblies.push(reference);
    }
    let dependencies = packages
        .edges
        .iter()
        .map(|(id, direct)| {
            json!({
                "ref": reference(id),
                "dependsOn": direct.iter().map(reference).collect::<Vec<_>>()
            })
        })
        .collect();
    let roots = packages.roots.iter().map(reference).collect();
    let mut document = assemble(metadata, components, dependencies, roots, include)?;
    if !graph.issues.is_empty() {
        partial_assemblies.push(PROJECT_REF.to_owned());
    }
    let mut compositions = Vec::new();
    if !partial_assemblies.is_empty() {
        compositions.push(json!({"aggregate": "incomplete", "assemblies": partial_assemblies}));
    }
    if project_scope && graph.issues.is_empty() {
        compositions.push(json!({"aggregate": "unknown", "assemblies": [PROJECT_REF]}));
    }
    if !compositions.is_empty() {
        document["compositions"] = json!(compositions);
    }
    Ok(document)
}

fn report_scan_issues<'a>(
    issues: impl Iterator<Item = &'a String>,
    allow_incomplete: bool,
    project: &Path,
    scope: &str,
) -> Result<()> {
    if allow_incomplete {
        return Ok(());
    }
    let issues = issues.collect::<BTreeSet<_>>();
    if issues.is_empty() {
        return Ok(());
    }
    let first = issues
        .iter()
        .take(3)
        .map(|issue| issue.as_str())
        .collect::<Vec<_>>()
        .join("\n  ");
    Err(sbom_ci::failure(
        sbom_ci::Failure::Incomplete,
        sbom_paths::diagnostic_path(project),
        format!(
            "{} unresolved {scope} scan issue(s); pass --allow-incomplete to export a partial graph:\n  {first}",
            issues.len()
        ),
    ))
}

fn add_packed_payload(
    document: &mut Value,
    project: &Path,
    files: BTreeSet<PathBuf>,
    allow_incomplete: bool,
    output: Option<&sbom_paths::OutputGuard>,
) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let graph = crate::vi_graph::scan_packed_files(project, files)?;
    if let Some(output) = output {
        graph.protect_inputs(output)?;
    }
    report_scan_issues(graph.issues.iter(), allow_incomplete, project, "build")?;

    let components = document["components"]
        .as_array_mut()
        .expect("SBOM component array");
    let existing: BTreeSet<_> = components
        .iter()
        .filter_map(|item| item["bom-ref"].as_str().map(str::to_owned))
        .collect();
    let mut assemblies = Vec::new();
    for (label, packed) in &graph.packed {
        let reference = packed_ref(label);
        ensure!(
            !existing.contains(&reference),
            "manual component uses packed library bom-ref {reference:?}"
        );
        components.push(packed_component(
            &reference,
            packed,
            graph.roots.contains(label),
        ));
        assemblies.push(reference);
    }

    let dependencies = document["dependencies"]
        .as_array_mut()
        .expect("SBOM dependency array");
    let roots = dependencies[0]["dependsOn"]
        .as_array_mut()
        .expect("SBOM product dependencies");
    roots.extend(graph.roots.iter().map(|label| json!(packed_ref(label))));
    for (label, edges) in &graph.edges {
        dependencies.push(json!({
            "ref": packed_ref(label), "dependsOn": edges.iter().map(|target| packed_ref(target)).collect::<Vec<_>>()
        }));
    }

    if !graph.issues.is_empty() {
        let metadata = document["metadata"].as_object_mut().expect("SBOM metadata");
        let properties = metadata
            .entry("properties")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .expect("SBOM metadata properties");
        properties.extend(
            graph
                .issues
                .iter()
                .map(|issue| json!({"name": "lvpm:labview:scan-issue", "value": issue})),
        );
        assemblies.push(PROJECT_REF.to_owned());
    }
    let compositions = document
        .as_object_mut()
        .expect("SBOM object")
        .entry("compositions")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .expect("SBOM compositions");
    compositions.push(json!({"aggregate": "incomplete", "assemblies": assemblies}));
    Ok(())
}

fn packed_ref(label: &str) -> String {
    format!("lvpm:ppl:artifact:{}", purl_segment(label))
}

fn packed_import_component(reference: &str, import: &ppl::Import) -> Value {
    json!({
        "type": "library",
        "bom-ref": reference,
        "name": import.name,
        "properties": [
            {"name": ARTIFACT_NAME_PROPERTY, "value": import.name},
            {"name": "lvpm:ppl:reference-path", "value": portable_path_identity(&import.path)},
            {"name": "lvpm:ppl:library-id", "value": import.library_id}
        ]
    })
}

fn packed_component(
    reference: &str,
    artifact: &crate::vi_graph::PackedComponent,
    configured_source: bool,
) -> Value {
    let mut properties = vec![json!({"name": ARTIFACT_NAME_PROPERTY, "value": artifact.name})];
    if configured_source {
        properties.push(json!({"name": "lvpm:ppl:shipment", "value": "configured-source"}));
    }
    if let Some(path) = &artifact.artifact_path {
        properties.push(json!({"name": "lvpm:ppl:artifact-path", "value": path}));
    }
    if let Some(id) = &artifact.library_id {
        properties.push(json!({"name": "lvpm:ppl:library-id", "value": id}));
    }
    properties.extend(artifact.reference_paths.iter().map(|path| {
        json!({
            "name": "lvpm:ppl:reference-path", "value": path
        })
    }));
    let mut component = json!({
        "type": "library", "bom-ref": reference, "name": artifact.name, "properties": properties
    });
    if let Some(sha256) = &artifact.sha256 {
        component["hashes"] = json!([{"alg": "SHA-256", "content": sha256}]);
        crate::sbom_artifact_document::add_property(
            component.as_object_mut().expect("PPL component"),
            DIGEST_SUBJECT,
            json!(ARTIFACT_FILE),
        );
    }
    if let Some(version) = &artifact.version {
        component["version"] = json!(version);
    }
    component
}

fn assemble(
    metadata: Value,
    mut components: Vec<Value>,
    mut dependencies: Vec<Value>,
    mut project_dependencies: BTreeSet<String>,
    include: Option<&Path>,
) -> Result<Value> {
    if let Some(path) = include {
        include_components(path, &mut components, &mut project_dependencies)
            .map_err(|error| sbom_ci::input_error(error, path))?;
    }
    dependencies.insert(
        0,
        json!({"ref": PROJECT_REF, "dependsOn": project_dependencies}),
    );

    Ok(json!({
        "$schema": "http://cyclonedx.org/schema/bom-1.5.schema.json",
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": metadata,
        "components": components,
        "dependencies": dependencies
    }))
}

/// Include a component inventory whose entries are direct product dependencies.
fn include_components(
    path: &Path,
    components: &mut Vec<Value>,
    project_dependencies: &mut BTreeSet<String>,
) -> Result<()> {
    let mut input = sbom_ci::read_json(path)?;
    // Manual lists have always accepted identical entries. Normalize those
    // before validating the list, but leave conflicting entries for rejection.
    if let Some(components) = input.get_mut("components").and_then(Value::as_array_mut) {
        let mut seen = BTreeSet::new();
        components.retain(|component| seen.insert(component.to_string()));
    }
    sbom_ci::validate(&input, path)?;
    ensure!(
        input["bomFormat"] == "CycloneDX" && input["specVersion"] == "1.5",
        "expected a CycloneDX 1.5 JSON file"
    );
    let header = input.as_object().context("expected a JSON object")?;
    for key in header.keys() {
        ensure!(
            matches!(
                key.as_str(),
                "$schema" | "bomFormat" | "specVersion" | "version" | "serialNumber" | "components"
            ),
            "manual SBOM must contain only components and the document header (found {key:?})"
        );
    }

    let manual = input["components"]
        .as_array()
        .context("expected a components array")?;
    for (index, component) in manual.iter().enumerate() {
        let reference = required_string(component, "bom-ref", index)?;
        required_string(component, "name", index)?;
        required_string(component, "type", index)?;
        let purl = component
            .get("purl")
            .map(|_| required_string(component, "purl", index))
            .transpose()?;
        ensure!(
            reference != PROJECT_REF,
            "manual component uses the product's bom-ref {reference:?}"
        );
        ensure!(
            component.get("components").is_none(),
            "manual component {reference:?} has nested components"
        );

        let existing = components.iter().find(|other| {
            other["bom-ref"] == reference || purl.is_some_and(|purl| other["purl"] == purl)
        });
        if let Some(existing) = existing {
            ensure!(
                existing == component,
                "conflicting component for bom-ref {reference:?} or PURL {purl:?}"
            );
        } else {
            components.push(component.clone());
        }
        project_dependencies.insert(reference.to_owned());
    }
    Ok(())
}

fn required_string<'a>(component: &'a Value, key: &str, index: usize) -> Result<&'a str> {
    component[key]
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .with_context(|| format!("manual component #{} needs a non-empty {key:?}", index + 1))
}

pub(crate) fn write(
    path: &Path,
    document: &Value,
    before_replace: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(document)?;
    bytes.push(b'\n');

    // Create the file beside its destination so a failed write cannot truncate
    // an existing SBOM. create_new also keeps concurrent writers independent.
    let mut temp = path.as_os_str().to_owned();
    temp.push(format!(".tmp-{}", std::process::id()));
    let temp = PathBuf::from(temp);
    let mut file = std::fs::File::create_new(&temp)
        .with_context(|| format!("creating temporary SBOM {}", temp.display()))?;
    let result = file.write_all(&bytes);
    drop(file);
    result
        .map_err(anyhow::Error::from)
        .and_then(|_| before_replace())
        .and_then(|_| std::fs::rename(&temp, path).map_err(anyhow::Error::from))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temp);
        })
        .with_context(|| format!("writing SBOM {}", path.display()))
}
