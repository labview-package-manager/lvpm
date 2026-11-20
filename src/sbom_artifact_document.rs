//! Add checked output membership to a build or standalone artifact inventory.

use crate::sbom_artifacts::{ArtifactFile, Inventory, has_extension};
use crate::sbom_ci::FILE_COVERAGE_REF_PREFIX;
use crate::sbom_pe::{self, Evidence};
use crate::{lvproj, nipkg::PayloadInventory};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct PostBuild<'a> {
    pub inventory: Inventory,
    pub primary_key: Option<&'a str>,
    pub product_name: Option<&'a str>,
    pub product_version: Option<&'a str>,
    pub inferred_name: Option<&'a str>,
}

impl PostBuild<'_> {
    pub(crate) fn primary(&self, build: Option<&lvproj::Build>) -> Result<Option<&ArtifactFile>> {
        let expected = build.and_then(expected_artifact);
        let primary = if let Some(key) = self.primary_key {
            let key = key.replace('\\', "/");
            let file = self
                .inventory
                .files
                .iter()
                .find(|file| file.key == key)
                .with_context(|| {
                    format!("primary artifact {key:?} is outside the selected artifact files")
                })?;
            Some(file)
        } else if let Some((name, extension)) = expected {
            let candidates = self.inventory.files.iter().filter(|file| {
                if let Some(name) = name {
                    same_filename(file_name(&file.key), name)
                } else {
                    has_extension(&file.key, extension)
                }
            });
            let mut candidates = candidates;
            let file = candidates.next().with_context(|| {
                format!(
                    "selected outputs do not contain the configured {}",
                    name.unwrap_or(extension)
                )
            })?;
            ensure!(
                candidates.next().is_none(),
                "several selected files match the configured {}, select one with --primary-artifact",
                name.unwrap_or(extension)
            );
            Some(file)
        } else if self.inventory.scopes.len() == 1 && !self.inventory.scopes[0].directory {
            self.inventory.files.first()
        } else {
            None
        };
        if let (Some(file), Some((name, extension))) = (primary, expected) {
            ensure!(
                has_extension(&file.key, extension),
                "selected primary artifact {:?} must be a {} file",
                file.key,
                extension
            );
            if let Some(name) = name {
                ensure!(
                    same_filename(file_name(&file.key), name),
                    "selected primary artifact {:?} does not match configured filename {name:?}",
                    file.key
                );
            }
        }
        Ok(primary)
    }

    pub(crate) fn standalone_name<'a>(
        &'a self,
        primary: Option<&'a ArtifactFile>,
    ) -> Result<&'a str> {
        self.product_name
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .or_else(|| primary.map(|file| file_name(&file.key)))
            .or(self.inferred_name)
            .context("cannot derive product name, use --product-name with multiple output roots")
    }

    pub(crate) fn attach(
        &self,
        bom: &mut Value,
        primary: Option<&ArtifactFile>,
        primary_nipkg_inventory: Option<PayloadInventory>,
    ) -> Result<()> {
        let metadata = bom["metadata"]
            .as_object_mut()
            .context("expected SBOM metadata")?;
        {
            let properties = metadata
                .entry("properties")
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .context("expected SBOM metadata properties")?;
            properties.extend(self.inventory.scopes.iter().map(|scope| json!({
                "name": if scope.directory { "lvpm:artifact:directory" } else { "lvpm:artifact:input-file" },
                "value": scope.key
            })));
        }
        let root = metadata
            .get_mut("component")
            .and_then(Value::as_object_mut)
            .context("expected SBOM product")?;
        let root_ref = root
            .get("bom-ref")
            .and_then(Value::as_str)
            .context("SBOM product has no bom-ref")?
            .to_owned();
        if let Some(name) = self.product_name {
            ensure!(!name.trim().is_empty(), "product name cannot be blank");
            root.insert("name".to_owned(), json!(name));
        }
        if let Some(version) = self.product_version {
            ensure!(
                !version.trim().is_empty(),
                "product version cannot be blank"
            );
            if let Some(previous) = root.insert("version".to_owned(), json!(version))
                && previous.as_str() != Some(version)
            {
                add_property(root, "lvpm:artifact:configured-version", previous);
            }
            add_property(
                root,
                "lvpm:artifact:version-evidence",
                json!("caller-supplied"),
            );
        }
        if let Some(file) = primary {
            root.insert("hashes".to_owned(), json!([sha256(file)]));
            record_file(root, file);
        }

        let output_files = self
            .inventory
            .files
            .iter()
            .filter(|file| primary.is_none_or(|primary| file.key != primary.key));
        let mut shipped = Vec::new();
        for file in output_files {
            let mut component = backed_component(file);
            record_file(
                component.as_object_mut().expect("file component object"),
                file,
            );
            shipped.push(component);
        }
        if !shipped.is_empty() {
            root.insert("components".to_owned(), json!(shipped));
        }
        add_native_evidence(bom, &self.inventory.files, NativeScope::Output(primary))?;
        let file_inventory_complete = crate::sbom_installers::add_archive_evidence(
            bom,
            &self.inventory.files,
            primary,
            primary_nipkg_inventory,
        )?;
        let compositions = bom
            .as_object_mut()
            .expect("SBOM object")
            .entry("compositions")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .context("expected SBOM compositions")?;
        compositions.extend([
            json!({"aggregate": "unknown", "dependencies": [root_ref]}),
            json!({
                "bom-ref": format!("{FILE_COVERAGE_REF_PREFIX}{root_ref}"),
                "aggregate": if file_inventory_complete { "complete" } else { "incomplete" },
                "assemblies": [root_ref]
            }),
        ]);
        Ok(())
    }
}

pub(crate) fn add_project_dlls(bom: &mut Value, inventory: &Inventory) -> Result<()> {
    let root_ref = bom["metadata"]["component"]["bom-ref"]
        .as_str()
        .context("SBOM product has no bom-ref")?
        .to_owned();
    let components = bom["components"]
        .as_array_mut()
        .context("expected SBOM components")?;
    let mut references = Vec::with_capacity(inventory.files.len());
    for file in &inventory.files {
        let reference = project_file_ref(file);
        components.push(json!({
            "type": "file",
            "bom-ref": reference,
            "name": file_name(&file.key),
            "hashes": [sha256(file)],
            "properties": [
                {"name": "lvpm:labview:project-path", "value": file.key},
                {"name": "lvpm:labview:byte-size", "value": file.size.to_string()},
                {"name": "lvpm:digest:subject", "value": "project-file"},
            ],
        }));
        references.push(Value::String(reference));
    }
    bom["dependencies"]
        .as_array_mut()
        .and_then(|rows| rows.iter_mut().find(|row| row["ref"] == root_ref))
        .and_then(|row| row["dependsOn"].as_array_mut())
        .context("expected SBOM product dependencies")?
        .extend(references);
    add_native_evidence(bom, &inventory.files, NativeScope::Project)
}

fn project_file_ref(file: &ArtifactFile) -> String {
    format!("lvpm:labview:project-file:{}", file.key)
}

enum NativeScope<'a> {
    Output(Option<&'a ArtifactFile>),
    Project,
}

// Record native import evidence only against the files explicitly selected.
fn add_native_evidence(
    bom: &mut Value,
    files: &[ArtifactFile],
    scope: NativeScope<'_>,
) -> Result<()> {
    let mut parsed = BTreeMap::new();
    for file in files
        .iter()
        .filter(|file| has_extension(&file.key, ".exe") || has_extension(&file.key, ".dll"))
    {
        let is_primary =
            matches!(scope, NativeScope::Output(Some(primary)) if primary.key == file.key);
        let component = if is_primary {
            &mut bom["metadata"]["component"]
        } else {
            let (nodes, reference) = match scope {
                NativeScope::Output(_) => (
                    &mut bom["metadata"]["component"]["components"],
                    file_ref(file),
                ),
                NativeScope::Project => (&mut bom["components"], project_file_ref(file)),
            };
            nodes
                .as_array_mut()
                .and_then(|nodes| nodes.iter_mut().find(|node| node["bom-ref"] == reference))
                .context("selected native file is missing from the SBOM inventory")?
        };
        let reference = component["bom-ref"]
            .as_str()
            .context("native file has no bom-ref")?
            .to_owned();
        match sbom_pe::scan(file.inspection_path()) {
            Ok(pe) => {
                let node = component.as_object_mut().expect("native component object");
                if node.get("type").and_then(Value::as_str) == Some("file") {
                    let kind = if has_extension(&file.key, ".dll") {
                        "library"
                    } else {
                        "application"
                    };
                    node.insert("type".to_owned(), json!(kind));
                }
                add_property(
                    node,
                    "lvpm:pe:machine",
                    json!(format!("0x{:04x}", pe.machine)),
                );
                if let Some(version) = &pe.file_version {
                    add_property(node, "lvpm:pe:file-version", json!(version));
                    if is_primary && !node.contains_key("version") {
                        node.insert("version".to_owned(), json!(version));
                        add_property(
                            node,
                            "lvpm:artifact:version-evidence",
                            json!("pe-file-version"),
                        );
                    }
                }
                for issue in &pe.issues {
                    add_property(node, "lvpm:pe:scan-issue", json!(issue));
                }
                parsed.insert(file.key.as_str(), (reference, pe));
            }
            Err(error) if error.is::<std::io::Error>() => return Err(error),
            Err(error) => {
                let node = component.as_object_mut().expect("native component object");
                add_property(node, "lvpm:pe:scan-issue", json!(format!("{error:#}")));
            }
        }
    }

    project_pe_imports(bom, &parsed);
    Ok(())
}

fn project_pe_imports(bom: &mut Value, parsed: &BTreeMap<&str, (String, Evidence)>) {
    let mut candidates: BTreeMap<(u16, String), Vec<(&str, &str)>> = BTreeMap::new();
    for (&key, (reference, pe)) in parsed {
        if has_extension(key, ".dll") {
            candidates
                .entry((pe.machine, file_name(key).to_ascii_lowercase()))
                .or_default()
                .push((key, reference));
        }
    }
    let mut opaque = BTreeMap::new();
    let mut observations = BTreeMap::<String, Vec<Value>>::new();
    let mut edges = BTreeMap::<String, BTreeSet<_>>::new();
    for (importer, pe) in parsed.values() {
        for import in &pe.imports {
            let portable_name = crate::sbom_metadata::portable_path_identity(&import.name);
            let name = portable_name.to_ascii_lowercase();
            let key = (pe.machine, name.clone());
            // A PE import identifier can carry a path. Never use it to look
            // outside the selected outputs or to choose between two files.
            let found = (!name.contains(['/', '\\', ':']))
                .then(|| candidates.get(&key))
                .flatten();
            let (status, candidate) = match found.map(Vec::as_slice) {
                Some([(key, candidate)]) if *candidate != importer.as_str() => {
                    ("unique-local-candidate", Some(*key))
                }
                Some([_]) | None => ("unresolved", None),
                Some(_) => ("ambiguous", None),
            };
            let evidence = json!({
                "name": "lvpm:pe:import",
                "value": json!({"name": portable_name, "delay": import.delayed, "status": status, "candidatePath": candidate}).to_string()
            });
            observations
                .entry(importer.clone())
                .or_default()
                .push(evidence);
            if !import.delayed {
                // Matching a selected filename and machine only establishes a
                // candidate. The import table does not identify the loaded bytes.
                let reference = format!("lvpm:pe:import:{:04x}:{name}", pe.machine);
                opaque.entry(reference.clone()).or_insert_with(|| {
                    json!({
                        "type": "library", "bom-ref": reference, "name": name,
                        "properties": [
                            {"name": "lvpm:artifact:origin", "value": "unresolved"},
                            {"name": "lvpm:pe:evidence", "value": "import-name"},
                            {"name": "lvpm:pe:machine", "value": format!("0x{:04x}", pe.machine)}
                        ]
                    })
                });
                edges.entry(importer.clone()).or_default().insert(reference);
            }
        }
    }

    let root = bom["metadata"]["component"]
        .as_object_mut()
        .expect("SBOM product");
    add_import_observations(root, &mut observations);
    if let Some(nodes) = root.get_mut("components").and_then(Value::as_array_mut) {
        for node in nodes {
            add_import_observations(
                node.as_object_mut().expect("SBOM file component"),
                &mut observations,
            );
        }
    }
    let components = bom["components"]
        .as_array_mut()
        .expect("SBOM component array");
    for node in &mut *components {
        add_import_observations(
            node.as_object_mut().expect("SBOM component object"),
            &mut observations,
        );
    }
    components.extend(opaque.into_values());
    let rows = bom["dependencies"]
        .as_array_mut()
        .expect("SBOM dependency array");
    for (reference, additions) in edges {
        if let Some(row) = rows.iter_mut().find(|row| row["ref"] == reference) {
            let targets = row["dependsOn"]
                .as_array_mut()
                .expect("generated dependency array");
            let merged: BTreeSet<_> = targets
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .chain(additions)
                .collect();
            *targets = merged.into_iter().map(Value::String).collect();
        } else {
            rows.push(json!({"ref": reference, "dependsOn": additions}));
        }
    }
}

fn add_import_observations(
    node: &mut serde_json::Map<String, Value>,
    observations: &mut BTreeMap<String, Vec<Value>>,
) {
    if let Some(reference) = node.get("bom-ref").and_then(Value::as_str)
        && let Some(imports) = observations.remove(reference)
    {
        node.entry("properties")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .expect("SBOM component properties")
            .extend(imports);
    }
}

fn backed_component(file: &ArtifactFile) -> Value {
    json!({
        "type": "file",
        "bom-ref": file_ref(file),
        "name": file_name(&file.key),
        "hashes": [sha256(file)],
        "properties": [{"name": "lvpm:artifact:origin", "value": "unresolved"}],
    })
}

pub(crate) fn file_ref(file: &ArtifactFile) -> String {
    format!("lvpm:artifact:{}", file.key)
}

pub(crate) fn file_name(key: &str) -> &str {
    key.rsplit('/').next().expect("nonempty artifact key")
}

fn sha256(file: &ArtifactFile) -> Value {
    json!({"alg": "SHA-256", "content": file.sha256})
}

fn record_file(node: &mut serde_json::Map<String, Value>, file: &ArtifactFile) {
    add_property(node, "lvpm:artifact:path", json!(file.key));
    add_property(
        node,
        "lvpm:artifact:byte-size",
        json!(file.size.to_string()),
    );
    add_property(node, "lvpm:digest:subject", json!("artifact-file"));
}

pub(crate) fn add_property(node: &mut serde_json::Map<String, Value>, name: &str, value: Value) {
    let properties = node
        .entry("properties")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .expect("JSON component property array");
    let property = json!({"name": name, "value": value});
    if !properties.contains(&property) {
        properties.push(property);
    }
}

fn expected_artifact(build: &lvproj::Build) -> Option<(Option<&str>, &'static str)> {
    let extension = match build.kind.as_str() {
        "EXE" => ".exe",
        "DLL" => ".dll",
        lvproj::PACKED_LIBRARY_TYPE => ".lvlibp",
        kind if kind.eq_ignore_ascii_case(lvproj::NIPKG_TYPE) => return Some((None, ".nipkg")),
        _ => return None,
    };
    let name = build
        .get("TgtF_targetfileName")
        .or_else(|| build.get("Destination[0].destName"))
        .and_then(|path| path.rsplit(['/', '\\']).next())
        .filter(|name| has_extension(name, extension));
    Some((name, extension))
}

fn same_filename(actual: &str, expected: &str) -> bool {
    if cfg!(windows) {
        actual.eq_ignore_ascii_case(expected)
    } else {
        actual == expected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sbom_pe::Import;

    #[test]
    fn selected_dll_matches_only_annotate_imports() {
        let product = "project";
        let regular_dll = "lvpm:artifact:variant/KERNEL32.dll";
        let delay_dll = "lvpm:artifact:variant/delay.dll";
        let mut bom = json!({
            "metadata": {"component": {
                "bom-ref": product,
                "components": [{"bom-ref": regular_dll}, {"bom-ref": delay_dll}]
            }},
            "components": [],
            "dependencies": []
        });
        let binary = |imports| Evidence {
            machine: 0x8664,
            file_version: None,
            imports,
            issues: Vec::new(),
        };
        let parsed = BTreeMap::from([
            (
                "app.exe",
                (
                    product.to_owned(),
                    binary(vec![
                        Import {
                            name: "KERNEL32.dll".to_owned(),
                            delayed: false,
                        },
                        Import {
                            name: "delay.dll".to_owned(),
                            delayed: true,
                        },
                    ]),
                ),
            ),
            (
                "variant/KERNEL32.dll",
                (regular_dll.to_owned(), binary(Vec::new())),
            ),
            (
                "variant/delay.dll",
                (delay_dll.to_owned(), binary(Vec::new())),
            ),
        ]);

        project_pe_imports(&mut bom, &parsed);

        let opaque_ref = "lvpm:pe:import:8664:kernel32.dll";
        assert_eq!(
            bom["dependencies"],
            json!([{"ref": product, "dependsOn": [opaque_ref]}])
        );
        let libraries = bom["components"].as_array().unwrap();
        assert_eq!(libraries.len(), 1);
        assert_eq!(libraries[0]["bom-ref"], opaque_ref);
        assert_eq!(libraries[0]["type"], "library");
        assert!(libraries[0].get("hashes").is_none());
        assert!(libraries[0].get("version").is_none());

        let observations = bom["metadata"]["component"]["properties"]
            .as_array()
            .unwrap();
        let observations: Vec<Value> = observations
            .iter()
            .map(|property| {
                assert_eq!(property["name"], "lvpm:pe:import");
                serde_json::from_str(property["value"].as_str().unwrap()).unwrap()
            })
            .collect();
        assert_eq!(
            observations,
            [
                json!({"name": "KERNEL32.dll", "delay": false, "status": "unique-local-candidate", "candidatePath": "variant/KERNEL32.dll"}),
                json!({"name": "delay.dll", "delay": true, "status": "unique-local-candidate", "candidatePath": "variant/delay.dll"}),
            ]
        );
    }
}
