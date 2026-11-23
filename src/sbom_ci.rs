//! Validation and machine-readable outcomes for SBOM commands.

use crate::sbom_paths::{diagnostic_path, redact_paths};
use crate::{sbom_graph_validation, sbom_merge, sbom_paths, sbom_schema};
use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use std::fmt;
use std::io;
use std::path::Path;

pub(crate) const FILE_COVERAGE_REF_PREFIX: &str = "lvpm:sbom:file-inventory:";
pub(crate) const FILE_SCAN_ISSUE_PROPERTY: &str = "lvpm:artifact:inventory-scan-issue";

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CoverageScope {
    All,
    #[value(alias = "file-inventory")]
    Files,
    Runtime,
}

impl CoverageScope {
    fn description(self) -> &'static str {
        match self {
            Self::All => "SBOM",
            Self::Files => "file inventory",
            Self::Runtime => "runtime dependency",
        }
    }
}

/// Only lvpm's explicit file coverage uses these assembly compositions.
/// Older assembly compositions also represent partial source or PPL scans.
pub(crate) fn is_file_inventory_composition(composition: &Value) -> bool {
    composition["bom-ref"].as_str().is_some_and(|reference| {
        reference
            .strip_prefix(FILE_COVERAGE_REF_PREFIX)
            .is_some_and(|suffix| !suffix.is_empty())
    }) && items(composition, "assemblies").next().is_some()
        && items(composition, "dependencies").next().is_none()
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Failure {
    Processing,
    Usage,
    Input,
    Schema,
    Graph,
    OutputConflict,
    Incomplete,
    Changed,
    Io,
}

impl Failure {
    fn code(self) -> &'static str {
        match self {
            Self::Processing => "sbom.processing-failed",
            Self::Usage => "sbom.invalid-usage",
            Self::Input => "sbom.invalid-input",
            Self::Schema => "sbom.invalid-schema",
            Self::Graph => "sbom.invalid-graph",
            Self::OutputConflict => "sbom.output-conflict",
            Self::Incomplete => "sbom.incomplete",
            Self::Changed => "sbom.diff-changed",
            Self::Io => "sbom.io",
        }
    }

    fn exit(self) -> u8 {
        match self {
            Self::Processing => 1,
            Self::Usage => 2,
            Self::Input | Self::Schema | Self::Graph | Self::OutputConflict => 3,
            Self::Incomplete => 4,
            Self::Changed => 5,
            Self::Io => 6,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct Diagnostic {
    #[serde(skip)]
    failure: Option<Failure>,
    severity: &'static str,
    code: &'static str,
    location: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    coverage_scope: Option<CoverageScope>,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}: {}", self.severity, self.location, self.message)
    }
}

impl Diagnostic {
    fn emit(&self, json: bool) {
        let portable = Self {
            location: redact_paths(&self.location),
            message: redact_paths(&self.message),
            ..self.clone()
        };
        if json {
            eprintln!(
                "{}",
                serde_json::to_string(&portable).expect("serializing diagnostic strings")
            );
        } else {
            eprintln!("{portable}");
        }
    }
}

pub(crate) fn failure(
    kind: Failure,
    location: impl Into<String>,
    message: impl Into<String>,
) -> anyhow::Error {
    anyhow::Error::msg(Diagnostic {
        failure: Some(kind),
        severity: "error",
        code: kind.code(),
        location: location.into(),
        message: message.into(),
        coverage_scope: None,
    })
}

pub(crate) fn emit_error(error: &anyhow::Error, json: bool) -> u8 {
    if let Some(diagnostic) = error.downcast_ref::<Diagnostic>() {
        diagnostic.emit(json);
        return diagnostic.failure.expect("classified error").exit();
    }

    let (kind, location) = if error.is::<sbom_paths::OutputConflict>() {
        (Failure::OutputConflict, "--output")
    } else if error.is::<io::Error>()
        || error
            .downcast_ref::<serde_json::Error>()
            .is_some_and(serde_json::Error::is_io)
    {
        (Failure::Io, "sbom")
    } else {
        (Failure::Processing, "sbom")
    };
    Diagnostic {
        failure: Some(kind),
        severity: "error",
        code: kind.code(),
        location: location.to_owned(),
        message: format!("{error:#}"),
        coverage_scope: None,
    }
    .emit(json);
    kind.exit()
}

pub(crate) fn input_error(error: anyhow::Error, path: &Path) -> anyhow::Error {
    if error.is::<Diagnostic>() || error.is::<sbom_paths::OutputConflict>() {
        return error;
    }
    let kind = if error.is::<io::Error>()
        || error
            .downcast_ref::<serde_json::Error>()
            .is_some_and(serde_json::Error::is_io)
    {
        Failure::Io
    } else {
        Failure::Input
    };
    failure(kind, diagnostic_path(path), format!("{error:#}"))
}

pub(crate) fn read(path: &Path) -> Result<Value> {
    let bom = read_json(path)?;
    validate(&bom, path)?;
    Ok(bom)
}

pub(crate) fn read_json(path: &Path) -> Result<Value> {
    let file = std::fs::File::open(path).map_err(|error| input_error(error.into(), path))?;
    serde_json::from_reader(io::BufReader::new(file))
        .map_err(|error| input_error(error.into(), path))
}

pub(crate) fn validate(bom: &Value, path: &Path) -> Result<()> {
    match sbom_schema::validate(bom) {
        Ok(()) => {}
        Err(sbom_schema::SchemaError::Invalid { location, message }) => {
            return Err(failure(
                Failure::Schema,
                format!("{}#{location}", diagnostic_path(path)),
                message,
            ));
        }
        Err(error @ sbom_schema::SchemaError::Initialization(_)) => return Err(error.into()),
    }
    sbom_graph_validation::validate(bom)
        .map_err(|error| failure(Failure::Graph, diagnostic_path(path), format!("{error:#}")))
}

pub(crate) fn write(
    guard: &sbom_paths::OutputGuard,
    path: &Path,
    bom: &mut Value,
    coverage_policy: Option<CoverageScope>,
    json: bool,
    inventory: Option<&crate::sbom_artifacts::Inventory>,
) -> Result<()> {
    sanitize_scan_issues(bom);
    validate(bom, path)?;
    let notices = coverage_diagnostics(bom, path);
    if let Some(policy) = coverage_policy
        && let Some(message) = coverage_failure(bom, policy, &notices)
    {
        for diagnostic in &notices {
            diagnostic.emit(json);
        }
        return Err(anyhow::Error::msg(Diagnostic {
            failure: Some(Failure::Incomplete),
            severity: "error",
            code: Failure::Incomplete.code(),
            location: diagnostic_path(path),
            message,
            coverage_scope: Some(policy),
        }));
    }
    crate::sbom::write(path, bom, || {
        if let Some(inventory) = inventory {
            inventory.verify_sources()?;
        }
        guard.recheck()
    })
    .map_err(|error| input_error(error, path))?;
    for diagnostic in notices {
        diagnostic.emit(json);
    }
    Ok(())
}

fn coverage_failure(bom: &Value, policy: CoverageScope, notices: &[Diagnostic]) -> Option<String> {
    if notices.iter().any(|notice| {
        policy == CoverageScope::All
            || notice.coverage_scope == Some(policy)
            || notice.coverage_scope == Some(CoverageScope::All)
    }) {
        return Some(format!(
            "{} coverage is incomplete or unknown",
            policy.description()
        ));
    }

    // Preserve the original check-explicit-evidence policy for the bare flag.
    // The precise scopes require positive evidence for the product itself.
    let scope_field = match policy {
        CoverageScope::All => return None,
        CoverageScope::Files => "assemblies",
        CoverageScope::Runtime => "dependencies",
    };
    let covered = bom["metadata"]["component"]["bom-ref"]
        .as_str()
        .is_some_and(|root| {
            items(bom, "compositions").any(|composition| {
                composition["aggregate"] == "complete"
                    && (policy == CoverageScope::Files)
                        == is_file_inventory_composition(composition)
                    && items(composition, scope_field).any(|reference| reference == root)
            })
        });
    (!covered).then(|| {
        format!(
            "{} coverage has no complete evidence for the product",
            policy.description()
        )
    })
}

fn coverage_diagnostics(bom: &Value, path: &Path) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for (index, composition) in items(bom, "compositions").enumerate() {
        let Some(aggregate) = composition["aggregate"].as_str() else {
            continue;
        };
        if aggregate == "complete" {
            continue;
        }
        // A composition without an explicit file marker retains the
        // legacy dependency/scan semantics of earlier lvpm documents.
        let coverage_scope = if is_file_inventory_composition(composition) {
            CoverageScope::Files
        } else {
            CoverageScope::Runtime
        };
        let scope = ["assemblies", "dependencies"]
            .into_iter()
            .flat_map(|key| items(composition, key))
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        let message = if scope.is_empty() {
            format!("{} coverage is {aggregate}", coverage_scope.description())
        } else {
            format!(
                "{} coverage is {aggregate} for {}",
                coverage_scope.description(),
                scope.join(", ")
            )
        };
        diagnostics.push(Diagnostic {
            failure: None,
            severity: "warning",
            code: "sbom.coverage",
            location: format!("{}#/compositions/{index}", diagnostic_path(path)),
            message,
            coverage_scope: Some(coverage_scope),
        });
    }
    scan_properties(&bom["metadata"], "/metadata", path, &mut diagnostics);
    scan_component_tree(
        &bom["metadata"]["component"],
        "/metadata/component",
        true,
        path,
        &mut diagnostics,
    );
    for (index, component) in items(bom, "components").enumerate() {
        scan_component_tree(
            component,
            &format!("/components/{index}"),
            false,
            path,
            &mut diagnostics,
        );
    }
    diagnostics
}

fn scan_issue_scope(name: &str) -> Option<CoverageScope> {
    match name {
        FILE_SCAN_ISSUE_PROPERTY => Some(CoverageScope::Files),
        "lvpm:labview:scan-issue" | "lvpm:nipkg:payload-scan-issue" | "lvpm:pe:scan-issue" => {
            Some(CoverageScope::Runtime)
        }
        // Older installer issues do not distinguish skipped bytes
        // from unverified relationships. Keep them conservative.
        "lvpm:installer:scan-issue" => Some(CoverageScope::All),
        _ => None,
    }
}

/// Only scrub diagnostic properties written by lvpm. References, identifiers,
/// and caller-supplied metadata retain their original meaning.
pub(crate) fn sanitize_scan_issues(bom: &mut Value) {
    fn sanitize(node: &mut Value) {
        if let Some(properties) = node.get_mut("properties").and_then(Value::as_array_mut) {
            for property in properties {
                if property["name"]
                    .as_str()
                    .and_then(scan_issue_scope)
                    .is_some()
                    && let Some(message) = property["value"].as_str()
                {
                    property["value"] = Value::String(redact_paths(message));
                }
            }
        }
        if let Some(component) = node.get_mut("component") {
            sanitize(component);
        }
        for key in ["components", "services"] {
            if let Some(children) = node.get_mut(key).and_then(Value::as_array_mut) {
                for child in children {
                    sanitize(child);
                }
            }
        }
    }

    if let Some(metadata) = bom.get_mut("metadata") {
        sanitize(metadata);
    }
    for key in ["components", "services"] {
        if let Some(components) = bom.get_mut(key).and_then(Value::as_array_mut) {
            for component in components {
                sanitize(component);
            }
        }
    }
}

fn scan_properties(node: &Value, pointer: &str, path: &Path, diagnostics: &mut Vec<Diagnostic>) {
    for (index, property) in items(node, "properties").enumerate() {
        if let Some(scope) = property["name"].as_str().and_then(scan_issue_scope)
            && let Some(message) = property["value"].as_str()
        {
            diagnostics.push(Diagnostic {
                failure: None,
                severity: "warning",
                code: "sbom.scan-issue",
                location: format!("{}#{pointer}/properties/{index}", diagnostic_path(path)),
                message: message.to_owned(),
                coverage_scope: Some(scope),
            });
        }
    }
}

fn scan_component_tree(
    node: &Value,
    pointer: &str,
    in_primary_assembly: bool,
    path: &Path,
    diagnostics: &mut Vec<Diagnostic>,
) {
    scan_properties(node, pointer, path, diagnostics);
    if in_primary_assembly {
        for (index, property) in items(node, "properties").enumerate() {
            if property["name"] == sbom_merge::RELATION_EVIDENCE
                && let Some(encoded) = property["value"].as_str()
                && let Some(message) = sbom_merge::unverified_file_membership(node, encoded)
            {
                diagnostics.push(Diagnostic {
                    failure: None,
                    severity: "warning",
                    code: "sbom.coverage",
                    location: format!("{}#{pointer}/properties/{index}", diagnostic_path(path)),
                    message,
                    coverage_scope: Some(CoverageScope::Files),
                });
            }
        }
    }
    for (index, child) in items(node, "components").enumerate() {
        scan_component_tree(
            child,
            &format!("{pointer}/components/{index}"),
            in_primary_assembly,
            path,
            diagnostics,
        );
    }
}

fn items<'a>(object: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    object[key].as_array().into_iter().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scoped_policies_require_positive_evidence_for_their_own_scope() {
        let mut bom = json!({
            "metadata": {"component": {"bom-ref": "main"}},
            "compositions": [
                {"aggregate": "complete", "dependencies": ["main"]},
                {"aggregate": "incomplete", "assemblies": ["main"],
                 "bom-ref": format!("{FILE_COVERAGE_REF_PREFIX}main")}
            ]
        });
        let path = Path::new("test.cdx.json");
        let notices = coverage_diagnostics(&bom, path);
        assert!(coverage_failure(&bom, CoverageScope::Runtime, &notices).is_none());
        assert!(coverage_failure(&bom, CoverageScope::All, &notices).is_some());
        assert!(coverage_failure(&bom, CoverageScope::Files, &notices).is_some());

        bom["compositions"] = json!([{"aggregate": "complete", "assemblies": ["main"]}]);
        let notices = coverage_diagnostics(&bom, path);
        assert!(coverage_failure(&bom, CoverageScope::All, &notices).is_none());
        assert!(coverage_failure(&bom, CoverageScope::Files, &notices).is_some());
        assert!(coverage_failure(&bom, CoverageScope::Runtime, &notices).is_some());
    }
}
