//! Semantic checks for the CycloneDX 1.5 inventory graph after schema validation.

use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(PartialEq, Eq, Clone, Copy)]
enum Kind {
    ComponentOrService,
    Vulnerability,
    Other,
}

type References<'a> = BTreeMap<&'a str, (Kind, String)>;

/// Check graph integrity without requiring a product, a closed graph, or a row
/// for every component. A missing row can represent unknown dependencies.
pub(crate) fn validate(document: &Value) -> Result<()> {
    let mut references = BTreeMap::new();
    collect_references(document, "", Kind::Other, &mut references)?;

    let mut sources = BTreeSet::new();
    for (index, row) in items(&document["dependencies"]).enumerate() {
        let path = format!("/dependencies/{index}");
        let source_path = format!("{path}/ref");
        let source = reference_at(&row["ref"], &source_path)?;
        resolve(
            &references,
            source,
            &source_path,
            Some(Kind::ComponentOrService),
            false,
        )?;
        ensure!(
            sources.insert(source),
            "{source_path}: duplicate dependency row for {source:?}"
        );

        check_list(
            &references,
            &row["dependsOn"],
            &format!("{path}/dependsOn"),
            Some(Kind::ComponentOrService),
            false,
        )?;
    }

    for (index, composition) in items(&document["compositions"]).enumerate() {
        let path = format!("/compositions/{index}");
        for (field, kind, allow_bom_link) in [
            ("assemblies", Kind::ComponentOrService, true),
            ("dependencies", Kind::ComponentOrService, false),
            ("vulnerabilities", Kind::Vulnerability, false),
        ] {
            check_list(
                &references,
                &composition[field],
                &format!("{path}/{field}"),
                Some(kind),
                allow_bom_link,
            )?;
        }
    }

    for (index, annotation) in items(&document["annotations"]).enumerate() {
        check_list(
            &references,
            &annotation["subjects"],
            &format!("/annotations/{index}/subjects"),
            None,
            true,
        )?;
    }

    for (index, vulnerability) in items(&document["vulnerabilities"]).enumerate() {
        for (affected_index, target) in items(&vulnerability["affects"]).enumerate() {
            let path = format!("/vulnerabilities/{index}/affects/{affected_index}/ref");
            resolve(
                &references,
                reference_at(&target["ref"], &path)?,
                &path,
                Some(Kind::ComponentOrService),
                true,
            )?;
        }
    }

    Ok(())
}

fn items(value: &Value) -> impl Iterator<Item = &Value> {
    value.as_array().into_iter().flatten()
}

fn collect_references<'a>(
    value: &'a Value,
    path: &str,
    kind: Kind,
    references: &mut References<'a>,
) -> Result<()> {
    match value {
        Value::Object(fields) => {
            if let Some(reference) = fields.get("bom-ref") {
                let location = format!("{path}/bom-ref");
                let reference = reference_at(reference, &location)?;
                if let Some((_, previous)) = references.get(reference) {
                    anyhow::bail!(
                        "{location}: duplicate bom-ref {reference:?} (first declared at {previous})"
                    );
                }
                references.insert(reference, (kind, location));
            }
            for (name, child) in fields {
                // These members contain component or service definitions in
                // inventory, metadata tools, pedigree and formulation.
                let child_kind = match name.as_str() {
                    "component" | "components" | "service" | "services" | "ancestors"
                    | "descendants" | "variants" => Kind::ComponentOrService,
                    "vulnerabilities" => Kind::Vulnerability,
                    _ => Kind::Other,
                };
                let name = name.replace('~', "~0").replace('/', "~1");
                collect_references(child, &format!("{path}/{name}"), child_kind, references)?;
            }
        }
        Value::Array(children) => {
            for (index, child) in children.iter().enumerate() {
                collect_references(child, &format!("{path}/{index}"), kind, references)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn reference_at<'a>(value: &'a Value, path: &str) -> Result<&'a str> {
    value
        .as_str()
        .filter(|reference| !reference.is_empty())
        .with_context(|| format!("{path}: expected a nonempty BOM reference"))
}

fn check_list(
    references: &References<'_>,
    values: &Value,
    path: &str,
    expected: Option<Kind>,
    allow_bom_link: bool,
) -> Result<()> {
    let mut seen = BTreeSet::new();
    for (index, value) in items(values).enumerate() {
        let location = format!("{path}/{index}");
        let reference = reference_at(value, &location)?;
        ensure!(
            seen.insert(reference),
            "{location}: duplicate reference {reference:?}"
        );
        resolve(references, reference, &location, expected, allow_bom_link)?;
    }
    Ok(())
}

fn resolve(
    references: &References<'_>,
    reference: &str,
    path: &str,
    expected: Option<Kind>,
    allow_bom_link: bool,
) -> Result<()> {
    if let Some((kind, _)) = references.get(reference) {
        ensure!(
            expected.is_none_or(|expected| expected == *kind),
            "{path}: reference {reference:?} resolves to an unsupported object type"
        );
    } else {
        let relation = if path.starts_with("/dependencies/") || path.starts_with("/compositions/") {
            "dependency or composition"
        } else {
            "BOM"
        };
        ensure!(
            allow_bom_link && is_bom_link_element(reference),
            "{path}: unknown {relation} reference {reference:?}"
        );
    }
    Ok(())
}

fn is_bom_link_element(reference: &str) -> bool {
    // CycloneDX 1.5 permits external component links in selected fields.
    // refLinkType alone also accepts arbitrary nonempty strings, so check the
    // element syntax before allowing an unresolved reference.
    let Some((document, element)) = reference
        .strip_prefix("urn:cdx:")
        .and_then(|value| value.split_once('#'))
    else {
        return false;
    };
    let Some((uuid, version)) = document.split_once('/') else {
        return false;
    };
    !element.is_empty()
        && !version.is_empty()
        && !version.starts_with('0')
        && version.bytes().all(|byte| byte.is_ascii_digit())
        && uuid.len() == 36
        && uuid.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_nested_inventory_opaque_dependencies_and_permitted_external_links() {
        let bom = json!({
            "metadata":{"component":{"type":"application","name":"root","bom-ref":"root","components":[
                {"type":"library","name":"nested","bom-ref":"nested"}
            ]}},
            "services":[{"name":"host","services":[{"name":"API","bom-ref":"api"}]}],
            "dependencies":[
                {"ref":"root","dependsOn":["nested","api"]},
                {"ref":"nested","dependsOn":["root"]}
            ],
            "compositions":[{"aggregate":"unknown","assemblies":[
                "root","urn:cdx:01234567-89ab-cdef-1234-56789abcdef0/1#external"
            ]}]
        });
        validate(&bom).unwrap();
    }

    #[test]
    fn rejects_duplicate_definitions_and_ambiguous_dependency_rows() {
        let mut bom = json!({
            "metadata":{"component":{"type":"application","name":"root","bom-ref":"root"}},
            "components":[{"type":"library","name":"package","bom-ref":"lib"}],
            "dependencies":[{"ref":"root"},{"ref":"root","dependsOn":["lib"]}]
        });
        let error = validate(&bom).unwrap_err().to_string();
        assert!(error.contains("/dependencies/1/ref: duplicate dependency row"));

        bom["components"][0]["bom-ref"] = json!("root");
        let error = validate(&bom).unwrap_err().to_string();
        assert!(error.contains("duplicate bom-ref"));
        assert!(error.contains("/components/0/bom-ref"));
        assert!(error.contains("/metadata/component/bom-ref"));
    }

    #[test]
    fn rejects_dangling_or_wrong_kind_references_by_location() {
        let mut bom = json!({
            "components":[{"type":"library","name":"package","bom-ref":"lib"}],
            "dependencies":[{"ref":"lib","dependsOn":["missing"]}],
            "vulnerabilities":[{"bom-ref":"advisory"}]
        });
        let error = validate(&bom).unwrap_err().to_string();
        assert!(
            error.contains(
                "/dependencies/0/dependsOn/0: unknown dependency or composition reference"
            )
        );

        bom["dependencies"] = json!([]);
        bom["compositions"] = json!([{"aggregate":"incomplete","dependencies":["advisory"]}]);
        let error = validate(&bom).unwrap_err().to_string();
        assert!(error.contains("/compositions/0/dependencies/0: reference \"advisory\" resolves to an unsupported object type"));
    }
}
