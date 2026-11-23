//! Declare installer choices and record separately verified package contents.

use crate::nipkg::{self, PayloadInventory, PayloadReadKind};
use crate::sbom_artifact_document::{add_property, file_name, file_ref};
use crate::sbom_artifacts::{ArtifactFile, has_extension};
use crate::sbom_ci::FILE_SCAN_ISSUE_PROPERTY;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn add_configured_installers(build: &crate::lvproj::Build, components: &mut Vec<Value>) {
    if build.kind != "Installer" {
        return;
    }
    let mut declared = BTreeMap::new();
    for (key, name) in &build.properties {
        let Some(index) = key
            .strip_prefix("DistPart[")
            .and_then(|tail| tail.strip_suffix("].productName"))
            .and_then(|index| index.parse::<u32>().ok())
        else {
            continue;
        };
        if !name.is_empty() {
            declared.insert(index, name);
        }
    }
    for (index, name) in declared {
        let mut component = json!({
            "type": "application", "bom-ref": format!("lvpm:installer:configured:{index}"),
            "name": name, "properties": [
                {"name": "lvpm:installer:evidence", "value": "lvproj-additional-installer"},
                {"name": "lvpm:installer:installation", "value": "undetermined"}
            ]
        });
        if let Some(version) = build.get(&format!("DistPart[{index}].productVersion")) {
            component["version"] = json!(version);
        }
        components.push(component);
    }
}

pub(crate) fn add_archive_evidence(
    bom: &mut Value,
    files: &[ArtifactFile],
    primary: Option<&ArtifactFile>,
    mut primary_nipkg_inventory: Option<PayloadInventory>,
) -> Result<bool> {
    let mut file_inventory_complete = true;
    for file in files {
        let is_primary = primary.is_some_and(|primary| primary.key == file.key);
        if has_extension(&file.key, ".nipkg") {
            let inventory = if is_primary {
                primary_nipkg_inventory.take()
            } else {
                None
            };
            file_inventory_complete &= add_nipkg(bom, file, is_primary, inventory)?;
        } else if has_extension(&file.key, ".msi") {
            add_msi(bom, file, is_primary)?;
            let component = artifact_node(bom, file, is_primary)?;
            add_property(
                component.as_object_mut().expect("MSI component"),
                FILE_SCAN_ISSUE_PROPERTY,
                json!("MSI table declarations leave packaged file bytes unverified"),
            );
            file_inventory_complete = false;
        }
    }
    Ok(file_inventory_complete)
}

fn add_nipkg(
    bom: &mut Value,
    file: &ArtifactFile,
    primary: bool,
    primary_inventory: Option<PayloadInventory>,
) -> Result<bool> {
    // The primary package's control relationships and packed libraries are
    // already in the BOM when the caller supplies its file inventory.
    let (control, inventory) = match primary_inventory {
        Some(inventory) => (None, inventory),
        None => {
            let archive = nipkg::read(file.inspection_path(), PayloadReadKind::Inventory)
                .with_context(|| format!("reading NI package {:?}", file.key))?;
            (Some(archive.control), archive.inventory)
        }
    };
    let verified_ppls = if control.is_none() {
        take_shipped_ppls(bom)
    } else {
        BTreeMap::new()
    };
    let mut members = BTreeMap::new();
    let mut nested_archives = 0;
    for (path, member) in &inventory.files {
        nested_archives += usize::from(member.nested_archive);
        members.insert(path.clone(), json!({
            "type": "file", "bom-ref": format!("lvpm:artifact:nipkg:{}!/{path}", file.key),
            "name": file_name(path),
            "hashes": [{"alg": "SHA-256", "content": member.sha256}],
            "properties": [
                {"name": "lvpm:artifact:path", "value": format!("{}!/{path}", file.key)},
                {"name": "lvpm:artifact:byte-size", "value": member.size.to_string()},
                {"name": "lvpm:digest:subject", "value": "artifact-file"},
                {"name": "lvpm:installer:evidence", "value": "nipkg-tar-member"},
                {"name": "lvpm:installer:content", "value": if member.nested_archive {"nested-archive"} else {"file"}}
            ]
        }));
    }

    for (path, mut ppl) in verified_ppls {
        let node = ppl.as_object_mut().expect("PPL component object");
        add_property(
            node,
            "lvpm:artifact:path",
            json!(format!("{}!/{path}", file.key)),
        );
        add_property(node, "lvpm:digest:subject", json!("artifact-file"));
        add_property(
            node,
            "lvpm:installer:evidence",
            json!("ni-package-packed-library"),
        );
        if let Some(existing) = members.get(&path) {
            let same_bytes = existing["hashes"][0]["content"] == ppl["hashes"][0]["content"];
            anyhow::ensure!(
                same_bytes,
                "NI package member {path:?} changed while scanning"
            );
            members.insert(path, ppl);
        } else if let Some((archive, _)) = path.split_once("!/")
            && let Some(parent) = members.get_mut(archive)
        {
            child_components(parent).push(ppl);
        } else {
            members.insert(path, ppl);
        }
    }

    let file_inventory_complete = inventory.issues.is_empty() && nested_archives == 0;
    let component = artifact_node(bom, file, primary)?;
    let reference = component["bom-ref"]
        .as_str()
        .context("NI package has no bom-ref")?
        .to_owned();
    {
        let node = component.as_object_mut().expect("NI package component");
        node.insert("type".to_owned(), json!("application"));
        for issue in inventory.issues {
            add_property(node, FILE_SCAN_ISSUE_PROPERTY, json!(issue));
        }
        if nested_archives != 0 {
            add_property(
                node,
                FILE_SCAN_ISSUE_PROPERTY,
                json!(format!(
                    "{nested_archives} nested installer archives are present. Only supported packed-library contents were read in the dedicated package scan."
                )),
            );
        }
    }
    if !members.is_empty() {
        child_components(component).extend(members.into_values());
    }
    if let Some(control) = control {
        let node = component.as_object_mut().expect("NI package component");
        if !node.contains_key("version") {
            node.insert("version".to_owned(), json!(control.version));
        }
        add_property(node, "lvpm:installer:package-name", json!(control.name));
        if let Some(architecture) = control.architecture {
            add_property(node, "lvpm:nipkg:architecture", json!(architecture));
        }
        let mut required = BTreeSet::new();
        let flat = bom["components"]
            .as_array_mut()
            .expect("SBOM component array");
        for (name, requirement) in control.dependencies {
            let id = format!("lvpm:nipkg:artifact:{}!/{name}", file.key);
            let mut properties = requirement
                .properties
                .into_iter()
                .map(|(name, value)| json!({"name": format!("lvpm:nipkg:{name}"), "value": value}))
                .collect::<Vec<_>>();
            properties.push(
                json!({"name": "lvpm:installer:evidence", "value": "nipkg-control-requirement"}),
            );
            if requirement.required {
                required.insert(id.clone());
            }
            flat.push(
                json!({"type": "library", "bom-ref": id, "name": name, "properties": properties}),
            );
        }
        if !required.is_empty() {
            let rows = bom["dependencies"]
                .as_array_mut()
                .expect("SBOM dependency array");
            if let Some(row) = rows.iter_mut().find(|row| row["ref"] == reference) {
                required.extend(
                    row["dependsOn"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned),
                );
                row["dependsOn"] = json!(required);
            } else {
                rows.push(json!({"ref": reference, "dependsOn": required}));
            }
        }
    } else {
        for requirement in bom["components"]
            .as_array_mut()
            .expect("SBOM component array")
        {
            if requirement["bom-ref"]
                .as_str()
                .is_some_and(|id| id.starts_with("lvpm:nipkg:"))
            {
                add_property(
                    requirement.as_object_mut().expect("NI dependency object"),
                    "lvpm:installer:evidence",
                    json!("nipkg-control-requirement"),
                );
            }
        }
    }
    Ok(file_inventory_complete)
}

fn take_shipped_ppls(bom: &mut Value) -> BTreeMap<String, Value> {
    let mut ppls = BTreeMap::new();
    let mut moved = BTreeSet::new();
    let components = bom["components"]
        .as_array_mut()
        .expect("SBOM component array");
    *components = std::mem::take(components)
        .into_iter()
        .filter_map(|component| {
            if property(&component, "lvpm:ppl:shipment") == Some("package-payload")
                && let Some(path) = property(&component, "lvpm:ppl:payload-path").map(str::to_owned)
            {
                moved.insert(
                    component["bom-ref"]
                        .as_str()
                        .expect("PPL reference")
                        .to_owned(),
                );
                ppls.insert(path, component);
                None
            } else {
                Some(component)
            }
        })
        .collect();
    // The previous package format put membership on the required-dependency
    // row. For this opt-in view, the shipped files are an assembly instead.
    let root = bom["metadata"]["component"]["bom-ref"]
        .as_str()
        .expect("NIPKG reference")
        .to_owned();
    if let Some(row) = bom["dependencies"]
        .as_array_mut()
        .and_then(|rows| rows.iter_mut().find(|row| row["ref"] == root))
        && let Some(targets) = row["dependsOn"].as_array_mut()
    {
        targets.retain(|target| {
            target
                .as_str()
                .is_none_or(|reference| !moved.contains(reference))
        });
    }
    ppls
}

#[cfg(windows)]
fn add_msi(bom: &mut Value, file: &ArtifactFile, primary: bool) -> Result<()> {
    let evidence = match crate::nipkg_msi::read_evidence(file.inspection_path()) {
        Ok(evidence) => evidence,
        Err(error) => {
            let component = artifact_node(bom, file, primary)?;
            add_property(
                component.as_object_mut().expect("MSI component"),
                "lvpm:installer:scan-issue",
                json!(format!("reading MSI tables: {error:#}")),
            );
            return Ok(());
        }
    };
    let component = artifact_node(bom, file, primary)?;
    let node = component.as_object_mut().expect("MSI component");
    node.insert("type".to_owned(), json!("application"));
    if let Some(name) = evidence.name {
        add_property(node, "lvpm:installer:msi-product-name", json!(name));
    }
    if let Some(version) = evidence.version {
        add_property(node, "lvpm:installer:msi-product-version", json!(version));
    }
    for issue in evidence.issues {
        add_property(node, "lvpm:installer:scan-issue", json!(issue));
    }
    let flat = bom["components"]
        .as_array_mut()
        .expect("SBOM component array");
    for entry in evidence.files {
        let mut properties = vec![
            json!({"name": "lvpm:installer:evidence", "value": "msi-file-table"}),
            json!({"name": "lvpm:installer:package-path", "value": file.key}),
            json!({"name": "lvpm:installer:installation", "value": entry.installation()}),
            json!({"name": "lvpm:installer:msi-component", "value": entry.component}),
        ];
        for (name, value) in [
            (
                "lvpm:installer:default-destination",
                entry.default_destination,
            ),
            ("lvpm:installer:condition", entry.condition),
            ("lvpm:installer:companion-file-id", entry.companion_file),
        ] {
            if let Some(value) = value {
                properties.push(json!({"name": name, "value": value}));
            }
        }
        properties.extend(
            entry
                .features
                .into_iter()
                .map(|feature| json!({"name":"lvpm:installer:feature", "value":feature})),
        );
        let mut component = json!({"type": "file", "bom-ref": format!("lvpm:artifact:msi:{}!/{}", file.key, entry.id),
            "name": entry.name, "properties": properties});
        if let Some(version) = entry.declared_version {
            component["version"] = json!(version);
        }
        flat.push(component);
    }
    Ok(())
}

#[cfg(not(windows))]
fn add_msi(bom: &mut Value, file: &ArtifactFile, primary: bool) -> Result<()> {
    let component = artifact_node(bom, file, primary)?;
    add_property(
        component.as_object_mut().expect("MSI component"),
        "lvpm:installer:scan-issue",
        json!("reading MSI table declarations requires Windows"),
    );
    Ok(())
}

fn child_components(component: &mut Value) -> &mut Vec<Value> {
    component
        .as_object_mut()
        .expect("SBOM component")
        .entry("components")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .expect("contained component array")
}

fn artifact_node<'a>(
    bom: &'a mut Value,
    file: &ArtifactFile,
    primary: bool,
) -> Result<&'a mut Value> {
    let component = &mut bom["metadata"]["component"];
    if primary {
        Ok(component)
    } else {
        let reference = file_ref(file);
        child_components(component)
            .iter_mut()
            .find(|node| node["bom-ref"] == reference)
            .context("archive is missing from the SBOM artifact inventory")
    }
}

fn property<'a>(value: &'a Value, name: &str) -> Option<&'a str> {
    value["properties"]
        .as_array()?
        .iter()
        .find(|property| property["name"] == name)?["value"]
        .as_str()
}
