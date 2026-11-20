//! Compare declared requirements from lvpm's two NIPKG SBOM export modes.

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

struct Inventory {
    name: String,
    version: Option<String>,
    packages: BTreeMap<String, Requirement>,
}

#[derive(PartialEq, Eq, Serialize)]
struct Requirement {
    required: bool,
    relationships: BTreeSet<String>,
    constraints: BTreeSet<String>,
    alternatives: BTreeSet<String>,
}

pub fn compare(buildspec: &Path, built: &Path) -> Result<Value> {
    let before = read(buildspec, "lvproj-explicit")
        .map_err(|error| crate::sbom_ci::input_error(error, buildspec))?;
    let after =
        read(built, "nipkg-control").map_err(|error| crate::sbom_ci::input_error(error, built))?;
    if before.name != after.name {
        return Err(crate::sbom_ci::failure(
            crate::sbom_ci::Failure::Input,
            crate::sbom_paths::diagnostic_path(built),
            format!(
                "cannot compare different packages: buildspec names {:?}, artifact names {:?}",
                before.name, after.name
            ),
        ));
    }

    let mut added = Vec::new();
    let mut missing = Vec::new();
    let mut changed = Vec::new();
    for (name, requirement) in &before.packages {
        match after.packages.get(name) {
            None => missing.push(json!({"package": name, "requirement": requirement})),
            Some(built) if built != requirement => {
                changed.push(json!({"package": name, "before": requirement, "after": built}));
            }
            Some(_) => {}
        }
    }
    for (name, requirement) in &after.packages {
        if !before.packages.contains_key(name) {
            added.push(json!({"package": name, "requirement": requirement}));
        }
    }

    Ok(json!({
        "package": before.name,
        "buildspec_version": before.version,
        "built_version": after.version,
        "added": added,
        "missing": missing,
        "changed": changed
    }))
}

pub(crate) fn has_changes(diff: &Value) -> bool {
    ["added", "missing", "changed"]
        .into_iter()
        .any(|key| diff[key].as_array().is_some_and(|items| !items.is_empty()))
}

fn read(path: &Path, source: &str) -> Result<Inventory> {
    let document = crate::sbom_ci::read(path)?;
    ensure!(
        document["bomFormat"] == "CycloneDX" && document["specVersion"] == "1.5",
        "expected CycloneDX 1.5 JSON"
    );
    let metadata = &document["metadata"];
    ensure!(
        properties(metadata, "dependency-source")? == [source],
        "expected an lvpm NIPKG SBOM from {source:?}"
    );
    let product = &metadata["component"];
    let name = required_string(product, "name")?.to_owned();
    let version = product
        .get("version")
        .map(|_| required_string(product, "version").map(str::to_owned))
        .transpose()?;
    let root = required_string(product, "bom-ref")?;
    let mut root_rows = array(&document, "dependencies")?
        .iter()
        .filter(|row| row["ref"] == root);
    let root_row = root_rows.next().context("missing product dependency row")?;
    ensure!(
        root_rows.next().is_none(),
        "duplicate product dependency row"
    );
    let required = array(root_row, "dependsOn")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .context("expected a string dependency reference")
        })
        .collect::<Result<BTreeSet<_>>>()?;

    let mut packages = BTreeMap::new();
    let mut refs = BTreeSet::new();
    for component in array(&document, "components")? {
        let reference = required_string(component, "bom-ref")?;
        ensure!(
            reference != root && refs.insert(reference),
            "duplicate component reference {reference:?}"
        );
        if !reference.starts_with("lvpm:nipkg:") {
            continue;
        }
        let package = required_string(component, "name")?;
        let requirement = read_requirement(component, required.contains(reference))
            .with_context(|| format!("reading requirement for {package:?}"))?;
        ensure!(
            packages.insert(package.to_owned(), requirement).is_none(),
            "duplicate NI package {package:?}"
        );
    }
    for reference in required {
        ensure!(
            refs.contains(reference),
            "unknown product dependency reference {reference:?}"
        );
    }
    Ok(Inventory {
        name,
        version,
        packages,
    })
}

fn read_requirement(component: &Value, required: bool) -> Result<Requirement> {
    let relationships = properties(component, "relationship")?;
    ensure!(!relationships.is_empty(), "missing relationship");
    let mut constraints = BTreeSet::new();
    let version_requirements = properties(component, "version-requirement")?;
    if version_requirements.is_empty() {
        if let Some(version) = single_property(component, "min-version")? {
            let operator = match single_property(component, "min-version-type")? {
                Some("Inclusive") => ">=",
                Some("Exclusive") => ">>",
                Some("Exact" | "Equal") => "=",
                mode => bail!("unsupported minimum version mode {mode:?}"),
            };
            constraints.insert(format!("{operator} {version}"));
        }
        if let Some(version) = single_property(component, "max-version")? {
            let operator = match single_property(component, "max-version-inclusive")? {
                Some("true") => "<=",
                Some("false") => "<<",
                mode => bail!("unsupported maximum version mode {mode:?}"),
            };
            constraints.insert(format!("{operator} {version}"));
        }
    } else {
        for raw in version_requirements {
            let choice = format!("package ({raw})");
            let (_, requirement) = crate::nipkg::parse_choice(&choice)?;
            let (operator, version) = requirement.context("expected a version requirement")?;
            constraints.insert(format!("{operator} {version}"));
        }
    }
    let alternatives = properties(component, "alternative")?
        .into_iter()
        .map(normalize_alternative)
        .collect::<Result<BTreeSet<_>>>()?;
    Ok(Requirement {
        required,
        relationships: relationships.into_iter().map(str::to_owned).collect(),
        constraints,
        alternatives,
    })
}

fn normalize_alternative(raw: &str) -> Result<String> {
    let choices = raw
        .split('|')
        .map(|choice| {
            let (name, requirement) = crate::nipkg::parse_choice(choice.trim())?;
            Ok(match requirement {
                Some((operator, version)) => format!("{name} ({operator} {version})"),
                None => name.to_owned(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(choices.join(" | "))
}

fn array<'a>(object: &'a Value, field: &str) -> Result<&'a [Value]> {
    object[field]
        .as_array()
        .map(Vec::as_slice)
        .with_context(|| format!("expected array {field:?}"))
}

fn required_string<'a>(object: &'a Value, field: &str) -> Result<&'a str> {
    object[field]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("expected string {field:?}"))
}

fn properties<'a>(object: &'a Value, name: &str) -> Result<Vec<&'a str>> {
    let name = format!("lvpm:nipkg:{name}");
    array(object, "properties")?
        .iter()
        .filter(|property| property["name"] == name)
        .map(|property| required_string(property, "value"))
        .collect()
}

fn single_property<'a>(object: &'a Value, name: &str) -> Result<Option<&'a str>> {
    let values = properties(object, name)?;
    ensure!(values.len() <= 1, "duplicate property {name:?}");
    Ok(values.first().copied())
}
