//! Compose CycloneDX SBOMs while retaining inventory, scope and relationships.

use crate::ppl::ARTIFACT_NAME_PROPERTY as ARTIFACT_NAME;
use crate::sbom_paths::{OutputGuard, diagnostic_path};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};
use std::path::{Path, PathBuf};

type Edges = BTreeMap<String, Option<BTreeSet<String>>>;

const ARTIFACT_PATH: &str = "lvpm:ppl:artifact-path";
const LIBRARY_ID: &str = "lvpm:ppl:library-id";
const REFERENCE_PATH: &str = "lvpm:ppl:reference-path";
const MATCHED_FILES: &str = "lvpm:labview:matched-files";
const SOURCE_DOCUMENT: &str = "lvpm:sbom:source-document";
const SOURCE_REFERENCES: &str = "referenceMap";
pub(crate) const RELATION_EVIDENCE: &str = "lvpm:sbom:relationship-evidence";
const PRIMARY_ALIAS: &str = "primary";
const FILE_SUBJECT: &str = "lvpm:digest:subject";
const FILE_PATH: &str = "lvpm:artifact:path";

struct Bom {
    document: Value,
    root: String,
    nodes: BTreeMap<String, Value>,
    purls: BTreeMap<String, String>,
    edges: Edges,
    compositions: Vec<Value>,
    parents: BTreeMap<String, String>,
    source: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    children: BTreeMap<String, PathBuf>,
    relationships: Vec<Relationship>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Relationship {
    parent: String,
    child: String,
    kind: RelationKind,
    evidence: String,
}

// The plan's free-form evidence describes the declaration. Only a file
// already present in the parent's inventory can supply the checked digest.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RelationshipEvidence {
    parent: String,
    child: String,
    kind: RelationKind,
    evidence: String,
    #[serde(rename = "verifiedFileSha256", skip_serializing_if = "Option::is_none")]
    verified_file_sha256: Option<String>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum RelationKind {
    Contains,
    Requires,
}

impl Plan {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.relationships.is_empty(),
            "expected at least one relationship"
        );
        for alias in self.children.keys() {
            ensure!(
                !alias.trim().is_empty() && alias != PRIMARY_ALIAS,
                "invalid child alias {alias:?}"
            );
        }
        for relation in &self.relationships {
            for alias in [&relation.parent, &relation.child] {
                ensure!(
                    alias == PRIMARY_ALIAS || self.children.contains_key(alias),
                    "unknown document alias {alias:?}"
                );
            }
            ensure!(
                relation.parent != relation.child,
                "relationship endpoints must differ"
            );
            ensure!(
                !relation.evidence.trim().is_empty(),
                "relationship evidence must not be empty"
            );
        }
        Ok(())
    }

    /// Import a parent before its children when possible so an existing artifact
    /// node can absorb the separately supplied child BOM.
    fn order(&self) -> Vec<&str> {
        let mut counts: BTreeMap<&str, usize> =
            self.children.keys().map(|key| (key.as_str(), 0)).collect();
        let mut outgoing = BTreeMap::<&str, BTreeSet<&str>>::new();
        for relation in &self.relationships {
            let (parent, child) = (relation.parent.as_str(), relation.child.as_str());
            if parent != PRIMARY_ALIAS
                && child != PRIMARY_ALIAS
                && outgoing.entry(parent).or_default().insert(child)
            {
                *counts.get_mut(child).expect("validated child alias") += 1;
            }
        }
        let mut pending: BTreeSet<_> = counts
            .iter()
            .map(|(alias, count)| (*count, *alias))
            .collect();
        let mut order = Vec::with_capacity(pending.len());
        while let Some((_, alias)) = pending.pop_first() {
            order.push(alias);
            counts.remove(alias);
            if let Some(children) = outgoing.get(alias) {
                for child in children {
                    if let Some(count) = counts.get_mut(child) {
                        pending.remove(&(*count, child));
                        *count -= 1;
                        pending.insert((*count, child));
                    }
                }
            }
        }
        order
    }
}

/// Merge each child in order, reusing the matching node for a marked PPL product.
pub fn merge(primary: &Path, children: &[PathBuf]) -> Result<Value> {
    ensure!(!children.is_empty(), "expected at least one child SBOM");
    let mut primary = read_primary(primary)?;
    for path in children {
        let child =
            read(path).with_context(|| format!("reading child SBOM {}", diagnostic_path(path)))?;
        merge_boms(&mut primary, child)
            .map_err(|error| crate::sbom_ci::input_error(error, path))?;
    }
    render(primary)
}

/// Attach the VI dependency graphs of saved build specifications selected by
/// a package. There is no byte-level containment evidence for these inputs.
pub(crate) fn merge_configured_builds(primary: Value, children: Vec<Value>) -> Result<Value> {
    let mut primary = parse(primary)?;
    for child in children {
        merge_child(&mut primary, parse(child)?, ChildMerge::ConfiguredBuild)?;
    }
    render(primary)
}

/// Merge BOMs and apply the caller's direct containment and dependency evidence.
pub(crate) fn merge_with_relationships(
    primary: &Path,
    plan_path: &Path,
    guard: &OutputGuard,
) -> Result<Value> {
    guard.protect(plan_path)?;
    let plan = crate::sbom_ci::read_json(plan_path)?;
    let plan: Plan = serde_json::from_value(plan)
        .map_err(|error| crate::sbom_ci::input_error(error.into(), plan_path))?;
    plan.validate()
        .map_err(|error| crate::sbom_ci::input_error(error, plan_path))?;
    let base = plan_path.parent().unwrap_or(Path::new("."));
    let paths: BTreeMap<_, _> = plan
        .children
        .iter()
        .map(|(alias, path)| (alias.as_str(), base.join(path)))
        .collect();
    for path in paths.values() {
        guard.protect(path)?;
    }

    let mut primary = read_primary(primary)?;
    let mut roots = BTreeMap::from([(PRIMARY_ALIAS, primary.root.clone())]);
    for alias in plan.order() {
        let path = &paths[alias];
        let child =
            read(path).with_context(|| format!("reading child SBOM {}", diagnostic_path(path)))?;
        let root = merge_child(&mut primary, child, ChildMerge::Relationship)
            .map_err(|error| crate::sbom_ci::input_error(error, path))?;
        roots.insert(alias, root);
    }
    for relation in &plan.relationships {
        apply_relationship(&mut primary, relation, &roots)
            .map_err(|error| crate::sbom_ci::input_error(error, plan_path))?;
    }

    render(primary).map_err(|error| crate::sbom_ci::input_error(error, plan_path))
}

fn apply_relationship(
    bom: &mut Bom,
    relation: &Relationship,
    roots: &BTreeMap<&str, String>,
) -> Result<()> {
    let parent = &roots[relation.parent.as_str()];
    let child = &roots[relation.child.as_str()];
    ensure!(
        parent != child,
        "relationship endpoints refer to the same product {child:?}"
    );
    let verified_file_sha256 = match relation.kind {
        RelationKind::Contains => {
            let digest = verified_file_membership(bom, parent, child)?;
            record_parent(bom, child, parent)?;
            digest
        }
        RelationKind::Requires => {
            add_requirement(bom, parent, child);
            None
        }
    };
    record_evidence(
        bom,
        &RelationshipEvidence {
            parent: parent.clone(),
            child: child.clone(),
            kind: relation.kind,
            evidence: relation.evidence.clone(),
            verified_file_sha256,
        },
    )
}

fn verified_file_membership(bom: &Bom, parent: &str, child: &str) -> Result<Option<String>> {
    if bom.parents.get(child).map(String::as_str) != Some(parent) {
        return Ok(None);
    }
    let Some(digest) = artifact_digest(&bom.nodes[child])? else {
        return Ok(None);
    };
    // A prior declaration is not inventory evidence just because its child
    // is now nested. This also covers old merges without an evidence digest.
    for value in property_values(&bom.nodes[parent], RELATION_EVIDENCE) {
        let previous: RelationshipEvidence =
            serde_json::from_str(value).context("reading saved relationship evidence")?;
        if matches!(previous.kind, RelationKind::Contains)
            && previous.child == child
            && !previous
                .verified_file_sha256
                .as_deref()
                .is_some_and(|previous_digest| previous_digest.eq_ignore_ascii_case(digest))
        {
            return Ok(None);
        }
    }
    Ok(Some(digest.to_ascii_lowercase()))
}

fn record_evidence(bom: &mut Bom, evidence: &RelationshipEvidence) -> Result<()> {
    add_properties(
        bom.nodes
            .get_mut(&evidence.parent)
            .expect("relationship parent"),
        [document_property(RELATION_EVIDENCE, &json!(evidence))],
    )
}

pub(crate) fn unverified_file_membership(parent: &Value, encoded: &str) -> Option<String> {
    let Ok(evidence) = serde_json::from_str::<RelationshipEvidence>(encoded) else {
        return Some("unreadable relationship evidence in file inventory".into());
    };
    if !matches!(evidence.kind, RelationKind::Contains) {
        return None;
    }
    let digest = parent["components"]
        .as_array()
        .and_then(|children| {
            children
                .iter()
                .find(|child| child["bom-ref"] == evidence.child)
        })
        .and_then(|child| artifact_digest(child).ok().flatten());
    let verified = parent["bom-ref"] == evidence.parent
        && digest
            .zip(evidence.verified_file_sha256.as_deref())
            .is_some_and(|(actual, recorded)| actual.eq_ignore_ascii_case(recorded));
    (!verified).then(|| {
        format!(
            "file inventory has no verified containment of {:?} in {:?}",
            evidence.child, evidence.parent,
        )
    })
}

/// Separate imported relationship evidence so its document-local endpoints
/// can be remapped alongside the actual dependency and containment links.
fn take_relationship_evidence(bom: &mut Bom) -> Result<Vec<RelationshipEvidence>> {
    let mut relationships = Vec::new();
    for (owner, node) in &mut bom.nodes {
        let Some(properties) = node.get_mut("properties").and_then(Value::as_array_mut) else {
            continue;
        };
        for property in properties
            .iter()
            .filter(|property| property["name"] == RELATION_EVIDENCE)
        {
            let encoded = string_field(property, "value")?;
            let relationship: RelationshipEvidence =
                serde_json::from_str(encoded).context("reading imported relationship evidence")?;
            ensure!(
                relationship.parent == *owner,
                "relationship evidence is attached to the wrong component {owner:?}"
            );
            relationships.push(relationship);
        }
        properties.retain(|property| property["name"] != RELATION_EVIDENCE);
        if properties.is_empty() {
            node.as_object_mut()
                .expect("validated component")
                .remove("properties");
        }
    }
    Ok(relationships)
}

fn record_parent(bom: &mut Bom, child: &str, parent: &str) -> Result<()> {
    ensure!(
        child != bom.root,
        "the primary product cannot be contained within a child"
    );
    if let Some(current) = bom.parents.get(child) {
        ensure!(
            current == parent,
            "component {child:?} already belongs to {current:?}, not {parent:?}"
        );
    } else {
        bom.parents.insert(child.to_owned(), parent.to_owned());
    }
    Ok(())
}

fn add_requirement(bom: &mut Bom, parent: &str, child: &str) {
    bom.edges
        .entry(parent.to_owned())
        .or_default()
        .get_or_insert_with(BTreeSet::new)
        .insert(child.to_owned());
}

fn merge_boms(primary: &mut Bom, child: Bom) -> Result<String> {
    merge_child(primary, child, ChildMerge::Dependency)
}

enum ChildMerge {
    Dependency,
    Relationship,
    ConfiguredBuild,
}

fn merge_child(primary: &mut Bom, mut child: Bom, mode: ChildMerge) -> Result<String> {
    let legacy = !matches!(mode, ChildMerge::Relationship);
    if let Some(existing) = verified_source_product(primary, &child)? {
        if legacy && artifact_name(&child.nodes[&child.root])?.is_none() && existing != primary.root
        {
            let parent = primary.root.clone();
            add_requirement(primary, &parent, &existing);
        }
        return Ok(existing);
    }

    // A child BOM's metadata properties (scan scope, diagnostics, and others)
    // apply to that product. Header values use independent provenance records.
    let mapped_sources = take_mapped_sources(&mut child)?;
    let properties = optional_array(&child.document["metadata"], "properties")?.to_vec();
    add_properties(
        child.nodes.get_mut(&child.root).expect("child root"),
        properties,
    )?;
    let relationships = take_relationship_evidence(&mut child)?;

    let product = &child.nodes[&child.root];
    let marked_ppl = artifact_name(product)?.is_some();
    let target = match find_artifact_target(primary, product)? {
        None if legacy && marked_ppl => find_ppl_node(primary, product)?,
        target => target,
    };
    if legacy && marked_ppl && target.is_none() {
        let name = artifact_name(product)?.expect("marked PPL");
        anyhow::bail!("no primary PPL component matches {name:?}");
    }
    let mut artifact_targets = BTreeMap::new();
    if let Some(target) = target {
        let target = target.to_owned();
        if marked_ppl {
            artifact_targets = match_direct_ppl_references(primary, &target, &child)?;
        }
        artifact_targets.insert(child.root.clone(), target);
        for (source, target) in &artifact_targets {
            graft_artifact(primary, target, &child.nodes[source])?;
        }
    }
    let matched_ppl = marked_ppl && artifact_targets.contains_key(&child.root);
    let configured_build = matches!(mode, ChildMerge::ConfiguredBuild);
    let shared_scan_counts = matched_ppl || configured_build;

    let child_refs: BTreeSet<_> = child.nodes.keys().cloned().collect();
    let in_use = reserved_refs(primary);
    let mut reserved = in_use.clone();
    reserved.extend(reserved_refs(&child));
    let mut next = 0;
    let mut remap = BTreeMap::new();
    for (original, mut component) in child.nodes {
        if let Some(reference) = artifact_targets.get(&original) {
            remap.insert(original, reference.clone());
            continue;
        }
        let purl = component.get("purl").and_then(Value::as_str);
        let shared = purl.and_then(|purl| primary.purls.get(purl));
        let reference = if let Some(reference) = shared {
            component["bom-ref"] = json!(reference);
            ensure!(
                original != child.root
                    && reference != &primary.root
                    && same_component(&primary.nodes[reference], &component, shared_scan_counts),
                "conflicting components with PURL {}",
                component["purl"]
            );
            reference.clone()
        } else if in_use.contains(&original) {
            if original != child.root
                && original != primary.root
                && primary
                    .nodes
                    .get(&original)
                    .is_some_and(|candidate| candidate == &component)
            {
                original.clone()
            } else {
                loop {
                    next += 1;
                    let candidate = format!("lvpm:merge:child:{next}");
                    if reserved.insert(candidate.clone()) {
                        break candidate;
                    }
                    // A prior graft may already have remapped this colliding
                    // reference. Reuse it when the component matches.
                    if matched_ppl
                        && !child_refs.contains(&candidate)
                        && !remap.values().any(|reference| reference == &candidate)
                        && let Some(existing) = primary.nodes.get(&candidate)
                    {
                        component["bom-ref"] = json!(candidate);
                        if existing == &component {
                            break candidate;
                        }
                    }
                }
            }
        } else {
            original.clone()
        };
        component["bom-ref"] = json!(reference);
        if let Some(purl) = component["purl"].as_str() {
            primary
                .purls
                .entry(purl.to_owned())
                .or_insert_with(|| reference.clone());
        }
        match primary.nodes.entry(reference.clone()) {
            Entry::Occupied(mut entry)
                if configured_build
                    && matched_file_count(&component) > matched_file_count(entry.get()) =>
            {
                entry.insert(component);
            }
            entry => {
                entry.or_insert(component);
            }
        }
        remap.insert(original, reference);
    }
    validate_unique_import_targets(&remap)?;

    for (reference, depends_on) in child.edges {
        let target = primary.edges.entry(remap[&reference].clone()).or_default();
        if let Some(depends_on) = depends_on {
            target.get_or_insert_with(BTreeSet::new).extend(
                depends_on
                    .into_iter()
                    .map(|dependency| remap[&dependency].clone()),
            );
        }
    }
    let merged_root = remap[&child.root].clone();
    if legacy && !matched_ppl {
        let parent = primary.root.clone();
        add_requirement(primary, &parent, &merged_root);
    }

    for (child, parent) in child.parents {
        record_parent(primary, &remap[&child], &remap[&parent])?;
    }
    for mut evidence in relationships {
        evidence.parent = remap[&evidence.parent].clone();
        evidence.child = remap
            .get(&evidence.child)
            .with_context(|| {
                format!(
                    "unknown child in relationship evidence {:?}",
                    evidence.child
                )
            })?
            .clone();
        record_evidence(primary, &evidence)?;
    }
    for mut composition in child.compositions {
        remap_composition(&mut composition, &remap);
        if let Some(reference) = composition["bom-ref"].as_str()
            && in_use.contains(reference)
        {
            let prefix = if crate::sbom_ci::is_file_inventory_composition(&composition) {
                crate::sbom_ci::FILE_COVERAGE_REF_PREFIX
            } else {
                "lvpm:merge:child:"
            };
            composition["bom-ref"] = json!(new_ref(&mut reserved, &mut next, prefix));
        }
        if !primary.compositions.contains(&composition) {
            primary.compositions.push(composition);
        }
    }
    for (owner, mut source) in mapped_sources {
        for target in source[SOURCE_REFERENCES]
            .as_object_mut()
            .context("invalid source BOM reference map")?
            .values_mut()
        {
            let reference = target.as_str().context("invalid mapped BOM reference")?;
            *target = json!(
                remap
                    .get(reference)
                    .with_context(|| format!("stale source BOM reference {reference:?}"))?
            );
        }
        add_properties(
            primary
                .nodes
                .get_mut(&remap[&owner])
                .expect("mapped provenance owner"),
            [document_property(SOURCE_DOCUMENT, &source)],
        )?;
    }
    if let Some(mut source) = child.source {
        source[SOURCE_REFERENCES] = json!(remap);
        add_properties(
            primary
                .nodes
                .get_mut(&merged_root)
                .expect("merged source product"),
            [document_property(SOURCE_DOCUMENT, &source)],
        )?;
    }
    Ok(merged_root)
}

fn remap_composition(composition: &mut Value, remap: &BTreeMap<String, String>) {
    for field in ["assemblies", "dependencies"] {
        if let Some(values) = composition.get_mut(field).and_then(Value::as_array_mut) {
            let mut seen = BTreeSet::new();
            values.retain_mut(|value| {
                let original = value.as_str().expect("validated composition reference");
                let reference = remap
                    .get(original)
                    .map_or(original, String::as_str)
                    .to_owned();
                *value = json!(reference);
                seen.insert(reference)
            });
        }
    }
}

fn new_ref(reserved: &mut BTreeSet<String>, next: &mut usize, prefix: &str) -> String {
    loop {
        *next += 1;
        let candidate = format!("{prefix}{next}");
        if reserved.insert(candidate.clone()) {
            return candidate;
        }
    }
}

fn reserved_refs(bom: &Bom) -> BTreeSet<String> {
    fn collect(value: &Value, refs: &mut BTreeSet<String>) {
        match value {
            Value::Object(fields) => {
                if let Some(reference) = value["bom-ref"].as_str() {
                    refs.insert(reference.to_owned());
                }
                for value in fields.values() {
                    collect(value, refs);
                }
            }
            Value::Array(items) => {
                for value in items {
                    collect(value, refs);
                }
            }
            _ => (),
        }
    }
    let mut refs = BTreeSet::new();
    collect(&bom.document["metadata"], &mut refs);
    for value in bom.nodes.values().chain(&bom.compositions) {
        collect(value, &mut refs);
    }
    refs
}

fn render(mut primary: Bom) -> Result<Value> {
    // Keep bounded nesting both for JSON readers and for rendering the tree.
    for child in primary.parents.keys() {
        let mut visited = BTreeSet::from([child]);
        let mut current = child;
        while let Some(parent) = primary.parents.get(current) {
            ensure!(
                visited.insert(parent),
                "containment cycle through component {parent:?}"
            );
            ensure!(visited.len() <= 48, "component containment is too deep");
            current = parent;
        }
    }
    let mut children = BTreeMap::<_, Vec<_>>::new();
    for (child, parent) in &primary.parents {
        children
            .entry(parent.clone())
            .or_default()
            .push(child.clone());
    }
    fn take_node(
        reference: &str,
        nodes: &mut BTreeMap<String, Value>,
        children: &BTreeMap<String, Vec<String>>,
    ) -> Value {
        let mut node = nodes.remove(reference).expect("validated containment node");
        if let Some(children_of_node) = children.get(reference) {
            node["components"] = json!(
                children_of_node
                    .iter()
                    .map(|child| take_node(child, nodes, children))
                    .collect::<Vec<_>>()
            );
        }
        node
    }
    let roots: Vec<_> = primary
        .nodes
        .keys()
        .filter(|reference| {
            *reference != &primary.root && !primary.parents.contains_key(*reference)
        })
        .cloned()
        .collect();
    primary.document["metadata"]["component"] =
        take_node(&primary.root, &mut primary.nodes, &children);
    primary.document["components"] = json!(
        roots
            .iter()
            .map(|reference| take_node(reference, &mut primary.nodes, &children))
            .collect::<Vec<_>>()
    );
    let root = primary.edges.remove(&primary.root).unwrap_or_default();
    let dependencies = std::iter::once((primary.root, root))
        .chain(primary.edges)
        .map(|(reference, depends_on)| match depends_on {
            Some(dependencies) => json!({"ref": reference, "dependsOn": dependencies}),
            None => json!({"ref": reference}),
        })
        .collect::<Vec<_>>();
    primary.document["dependencies"] = json!(dependencies);
    if !primary.compositions.is_empty() {
        primary.document["compositions"] = json!(primary.compositions);
    }
    if primary.document.get("serialNumber").is_some() {
        assign_derived_identity(&mut primary.document)?;
    }
    Ok(primary.document)
}

fn assign_derived_identity(document: &mut Value) -> Result<()> {
    use md5::{Digest, Md5};
    use std::fmt::Write;

    // A derived BOM is a separate revision-1 document. Use an RFC 4122
    // name-based UUID in the URL namespace, with a SHA-256 content digest as
    // the name suffix. The MD5 here is UUID formatting, not the content digest.
    const URL_NAMESPACE: [u8; 16] = 0x6ba7b8119dad11d180b400c04fd430c8u128.to_be_bytes();
    document
        .as_object_mut()
        .expect("BOM object")
        .remove("serialNumber");
    document["version"] = json!(1);
    let content = crate::cache::sha256_hex(&serde_json::to_vec(document)?);
    let mut name = Md5::new();
    name.update(URL_NAMESPACE);
    name.update(b"https://github.com/labview-package-manager/lvpm/sbom/sha256/");
    name.update(content.as_bytes());
    let mut bytes = name.finalize();
    bytes[6] = (bytes[6] & 0x0f) | 0x30;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut serial = String::from("urn:uuid:");
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            serial.push('-');
        }
        write!(&mut serial, "{byte:02x}").expect("writing a UUID to a string");
    }
    document["serialNumber"] = json!(serial);
    Ok(())
}

/// A reported archive or source hash alone is not product identity.
fn artifact_digest(component: &Value) -> Result<Option<&str>> {
    if property_values(component, FILE_SUBJECT).any(|subject| subject == "artifact-file")
        || artifact_name(component)?.is_some()
    {
        return sha256(component);
    }
    Ok(None)
}

fn find_artifact_target<'a>(primary: &'a Bom, product: &Value) -> Result<Option<&'a str>> {
    let Some(digest) = artifact_digest(product)? else {
        return Ok(None);
    };
    let paths: BTreeSet<_> = property_values(product, FILE_PATH).collect();
    let mut matches = Vec::new();
    let mut matching_paths = Vec::new();
    for (reference, candidate) in &primary.nodes {
        if reference == &primary.root
            || (candidate["type"] != "file" && candidate["type"] != product["type"])
        {
            continue;
        }
        if artifact_digest(candidate)?.is_some_and(|value| value.eq_ignore_ascii_case(digest)) {
            matches.push(reference.as_str());
            if property_values(candidate, FILE_PATH).any(|path| paths.contains(path)) {
                matching_paths.push(reference.as_str());
            }
        }
    }
    if !matching_paths.is_empty() {
        matches = matching_paths;
    }
    match matches.as_slice() {
        [reference] => Ok(Some(reference)),
        [] => Ok(None),
        _ => anyhow::bail!("multiple components match artifact SHA-256 {digest}"),
    }
}

fn find_ppl_node<'a>(primary: &'a Bom, product: &Value) -> Result<Option<&'a str>> {
    ensure!(
        product["type"] == "library",
        "the PPL SBOM product must be a library"
    );
    let name = artifact_name(product)?.context("the PPL SBOM product has no artifact name")?;
    let digest = sha256(product)?;
    let mut digest_matches = Vec::new();
    let mut name_matches = Vec::new();
    for (reference, component) in &primary.nodes {
        if reference == &primary.root || component["type"] != "library" {
            continue;
        }
        let Some(candidate) = artifact_name(component)? else {
            continue;
        };
        let candidate_digest = sha256(component)?;
        if digest.is_some_and(|digest| {
            candidate_digest.is_some_and(|candidate| digest.eq_ignore_ascii_case(candidate))
        }) {
            digest_matches.push(reference);
        }
        if name.eq_ignore_ascii_case(candidate) {
            name_matches.push((reference, candidate_digest));
        }
    }
    match digest_matches.as_slice() {
        [reference] => {
            ensure!(
                compatible_versions(&primary.nodes[*reference], product),
                "version mismatch for PPL {name:?}"
            );
            return Ok(Some(reference));
        }
        [] => {}
        _ => anyhow::bail!("multiple primary PPL components match the SHA-256 of {name:?}"),
    }
    match name_matches.as_slice() {
        [(reference, candidate)] => {
            ensure!(
                digest.is_none() || candidate.is_none(),
                "SHA-256 mismatch for PPL {name:?}"
            );
            ensure!(
                compatible_versions(&primary.nodes[*reference], product),
                "version mismatch for PPL {name:?}"
            );
            Ok(Some(reference))
        }
        [] => Ok(None),
        _ => anyhow::bail!("multiple primary PPL components match the artifact name {name:?}"),
    }
}

/// Reuse references already discovered from this PPL's binary. A library ID
/// alone is insufficient because copies can exist at different locations.
fn match_direct_ppl_references(
    primary: &Bom,
    target: &str,
    child: &Bom,
) -> Result<BTreeMap<String, String>> {
    let mut matches = BTreeMap::new();
    let Some(existing) = primary.edges.get(target).and_then(Option::as_ref) else {
        return Ok(matches);
    };
    let Some(incoming) = child.edges.get(&child.root).and_then(Option::as_ref) else {
        return Ok(matches);
    };

    for source in incoming {
        let component = &child.nodes[source];
        if source == &child.root
            || component["type"] != "library"
            || artifact_name(component)?.is_none()
        {
            continue;
        }
        let mut matched = None;
        for reference in existing {
            let candidate = &primary.nodes[reference];
            if reference == &primary.root
                || reference == target
                || candidate["type"] != "library"
                || artifact_name(candidate)?.is_none()
                || !same_ppl_reference(candidate, component)?
            {
                continue;
            }
            ensure!(
                matched.is_none(),
                "multiple direct PPL references match {source:?}"
            );
            matched = Some(reference);
        }
        if let Some(reference) = matched {
            matches.insert(source.clone(), reference.clone());
        }
    }
    Ok(matches)
}

fn same_ppl_reference(primary: &Value, incoming: &Value) -> Result<bool> {
    let primary_location = property_values(primary, ARTIFACT_PATH).next();
    let incoming_location = property_values(incoming, ARTIFACT_PATH).next();
    if matches!((primary_location, incoming_location), (Some(a), Some(b)) if a != b) {
        return Ok(false);
    }

    let primary_paths: Vec<_> = property_values(primary, REFERENCE_PATH).collect();
    let incoming_paths: Vec<_> = property_values(incoming, REFERENCE_PATH).collect();
    let same_path = primary_paths
        .iter()
        .any(|path| incoming_paths.contains(path));
    if !primary_paths.is_empty() && !incoming_paths.is_empty() && !same_path {
        return Ok(false);
    }
    if let (Some(a), Some(b)) = (sha256(primary)?, sha256(incoming)?) {
        if a.eq_ignore_ascii_case(b) {
            ensure!(
                compatible_versions(primary, incoming),
                "version mismatch for PPL reference"
            );
            return Ok(true);
        }
        ensure!(
            !(same_path
                && property_values(primary, LIBRARY_ID)
                    .next()
                    .zip(property_values(incoming, LIBRARY_ID).next())
                    .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))),
            "SHA-256 mismatch for PPL reference"
        );
        return Ok(false);
    }

    let primary_id = property_values(primary, LIBRARY_ID).next();
    let incoming_id = property_values(incoming, LIBRARY_ID).next();
    let same_id =
        matches!((primary_id, incoming_id), (Some(a), Some(b)) if a.eq_ignore_ascii_case(b));
    Ok(same_path && same_id && compatible_versions(primary, incoming))
}

fn compatible_versions(primary: &Value, incoming: &Value) -> bool {
    match (primary["version"].as_str(), incoming["version"].as_str()) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

fn property_values<'a>(component: &'a Value, name: &'static str) -> impl Iterator<Item = &'a str> {
    component["properties"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(move |property| property["name"] == name)
        .filter_map(|property| property["value"].as_str())
}

fn artifact_name(component: &Value) -> Result<Option<&str>> {
    let mut names = optional_array(component, "properties")?
        .iter()
        .filter(|property| property["name"] == ARTIFACT_NAME);
    let name = names
        .next()
        .map(|property| string_field(property, "value"))
        .transpose()?;
    ensure!(
        names.next().is_none(),
        "multiple {ARTIFACT_NAME} properties on one component"
    );
    if let Some(name) = name {
        ensure!(
            !name.contains(['/', '\\'])
                && name
                    .rsplit_once('.')
                    .is_some_and(|(stem, extension)| !stem.is_empty()
                        && extension.eq_ignore_ascii_case("lvlibp")),
            "invalid PPL artifact filename {name:?}"
        );
    }
    Ok(name)
}

fn sha256(component: &Value) -> Result<Option<&str>> {
    let mut hashes = optional_array(component, "hashes")?.iter().filter(|hash| {
        hash["alg"]
            .as_str()
            .is_some_and(|alg| alg.eq_ignore_ascii_case("SHA-256"))
    });
    let Some(hash) = hashes.next() else {
        return Ok(None);
    };
    let hash = string_field(hash, "content")?;
    ensure!(
        hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid SHA-256 in PPL component"
    );
    for other in hashes {
        ensure!(
            hash.eq_ignore_ascii_case(string_field(other, "content")?),
            "conflicting SHA-256 hashes in PPL component"
        );
    }
    Ok(Some(hash))
}

fn graft_artifact(primary: &mut Bom, target: &str, product: &Value) -> Result<()> {
    let existing = &primary.nodes[target];
    ensure!(
        compatible_versions(existing, product),
        "version mismatch for artifact {target:?}"
    );
    if let (Some(known), Some(incoming)) = (artifact_digest(existing)?, artifact_digest(product)?) {
        ensure!(
            known.eq_ignore_ascii_case(incoming),
            "SHA-256 mismatch for artifact {target:?}"
        );
    }
    let hashes = grafted_hashes(existing, product, target)?;
    let supplier = grafted_supplier(existing, product, target)?;
    let licenses = grafted_licenses(existing, product, target)?;
    if let Some(purl) = product["purl"].as_str() {
        ensure!(
            primary
                .purls
                .get(purl)
                .is_none_or(|reference| reference == target),
            "artifact PURL {purl:?} belongs to a different primary component"
        );
        primary.purls.insert(purl.to_owned(), target.to_owned());
    }
    let artifact = primary
        .nodes
        .get_mut(target)
        .expect("matched packed library")
        .as_object_mut()
        .expect("validated packed library component");
    let product = product.as_object().expect("validated sidecar product");
    if let (Some(installed), Some(incoming)) = (artifact.get("purl"), product.get("purl")) {
        ensure!(
            installed == incoming,
            "conflicting PURLs for the matched artifact"
        );
    }
    if artifact.get("type").is_some_and(|value| value == "file") {
        artifact.insert("type".to_owned(), product["type"].clone());
        artifact.insert("name".to_owned(), product["name"].clone());
    }
    for (field, value) in product {
        if !matches!(
            field.as_str(),
            "properties" | "hashes" | "supplier" | "licenses"
        ) {
            artifact
                .entry(field.clone())
                .or_insert_with(|| value.clone());
        }
    }
    if let Some(hashes) = hashes {
        artifact.insert("hashes".to_owned(), hashes);
    }
    if let Some(supplier) = supplier {
        artifact.insert("supplier".to_owned(), supplier);
    }
    if let Some(licenses) = licenses {
        artifact.insert("licenses".to_owned(), licenses);
    }
    if let Some(properties) = product.get("properties").and_then(Value::as_array) {
        let target = artifact
            .entry("properties")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .context("PPL component properties must be an array")?;
        let marked = target
            .iter()
            .any(|property| property["name"] == ARTIFACT_NAME);
        for property in properties {
            if !(marked && property["name"] == ARTIFACT_NAME) && !target.contains(property) {
                target.push(property.clone());
            }
        }
    }
    Ok(())
}

fn grafted_hashes(existing: &Value, incoming: &Value, target: &str) -> Result<Option<Value>> {
    let installed = optional_array(existing, "hashes")?;
    let supplied = optional_array(incoming, "hashes")?;
    let mut digests = BTreeMap::<&str, &str>::new();
    let mut hashes = Vec::new();
    for hash in installed.iter().chain(supplied) {
        let algorithm = string_field(hash, "alg")?;
        let content = string_field(hash, "content")?;
        if let Some(known) = digests.insert(algorithm, content) {
            ensure!(
                known.eq_ignore_ascii_case(content),
                "conflicting {algorithm} hashes for artifact {target:?}"
            );
        } else {
            hashes.push(hash.clone());
        }
    }
    Ok((hashes.as_slice() != installed).then_some(Value::Array(hashes)))
}

fn grafted_supplier(existing: &Value, incoming: &Value, target: &str) -> Result<Option<Value>> {
    let Some(supplier) = incoming.get("supplier") else {
        return Ok(None);
    };
    let Some(installed) = existing.get("supplier") else {
        return Ok(Some(supplier.clone()));
    };
    ensure!(
        installed == supplier
            || supplier_declaration(installed)? == supplier_declaration(supplier)?,
        "conflicting supplier declarations for artifact {target:?}"
    );
    Ok(None)
}

/// Supplier and contact references are local to each document. Array order
/// does not change the declaration.
fn supplier_declaration(supplier: &Value) -> Result<Value> {
    let mut supplier = supplier.clone();
    let declaration = supplier
        .as_object_mut()
        .context("expected a supplier object")?;
    declaration.remove("bom-ref");
    if let Some(urls) = declaration.get_mut("url").and_then(Value::as_array_mut) {
        urls.sort_by_cached_key(Value::to_string);
    }
    if let Some(contacts) = declaration.get_mut("contact").and_then(Value::as_array_mut) {
        for contact in contacts.iter_mut() {
            contact
                .as_object_mut()
                .context("expected a supplier contact object")?
                .remove("bom-ref");
        }
        contacts.sort_by_cached_key(Value::to_string);
    }
    Ok(supplier)
}

fn grafted_licenses(existing: &Value, incoming: &Value, target: &str) -> Result<Option<Value>> {
    let installed = optional_array(existing, "licenses")?;
    let supplied = optional_array(incoming, "licenses")?;
    if installed.is_empty() {
        return Ok((!supplied.is_empty()).then(|| incoming["licenses"].clone()));
    }
    if !supplied.is_empty() {
        let installed = installed
            .iter()
            .map(license_declaration)
            .collect::<Result<BTreeSet<_>>>()?;
        let supplied = supplied
            .iter()
            .map(license_declaration)
            .collect::<Result<BTreeSet<_>>>()?;
        ensure!(
            installed == supplied,
            "conflicting license declarations for artifact {target:?}: primary {installed:?}, child {supplied:?}"
        );
    }
    Ok(None)
}

/// Compare the declared identifiers without conflating names and IDs or
/// making legal equivalence assumptions about different SPDX expressions.
fn license_declaration(entry: &Value) -> Result<(&'static str, &str)> {
    if let Some(expression) = entry.get("expression").and_then(Value::as_str) {
        return Ok(("SPDX expression", expression.trim()));
    }
    let license = &entry["license"];
    if let Some(id) = license.get("id").and_then(Value::as_str) {
        return Ok(("SPDX ID", id));
    }
    Ok(("name", string_field(license, "name")?.trim()))
}

fn same_component(primary: &Value, incoming: &Value, allow_scan_count_difference: bool) -> bool {
    if primary == incoming {
        return true;
    }
    if !allow_scan_count_difference {
        return false;
    }
    // The same package can contribute a different number of VIs to each
    // source scan. The count is evidence for that scan, not package identity.
    let without_scan_count = |component: &Value| {
        let mut component = component.as_object().expect("validated component").clone();
        if let Some(properties) = component
            .get_mut("properties")
            .and_then(Value::as_array_mut)
        {
            properties.retain(|property| property["name"] != MATCHED_FILES);
            if properties.is_empty() {
                component.remove("properties");
            }
        }
        component
    };
    without_scan_count(primary) == without_scan_count(incoming)
}

fn matched_file_count(component: &Value) -> Option<usize> {
    property_values(component, MATCHED_FILES)
        .next()
        .and_then(|value| value.parse().ok())
}

fn read_primary(path: &Path) -> Result<Bom> {
    let mut bom =
        read(path).with_context(|| format!("reading primary SBOM {}", diagnostic_path(path)))?;
    if property_values(&bom.nodes[&bom.root], SOURCE_DOCUMENT)
        .next()
        .is_none()
        && let Some(mut source) = bom.source.clone()
    {
        let references: BTreeMap<_, _> = bom
            .nodes
            .keys()
            .map(|reference| (reference, reference))
            .collect();
        source[SOURCE_REFERENCES] = json!(references);
        let root = bom.nodes.get_mut(&bom.root).expect("primary root");
        add_properties(root, [document_property(SOURCE_DOCUMENT, &source)])?;
    }
    Ok(bom)
}

fn read(path: &Path) -> Result<Bom> {
    let result = (|| {
        let contents = std::fs::read(path)?;
        let mut document: Value = serde_json::from_slice(&contents)?;
        crate::sbom_ci::validate(&document, path)?;
        crate::sbom_ci::sanitize_scan_issues(&mut document);
        let mut source = json!({
            "sha256": crate::cache::sha256_hex(&contents),
            "version": document.get("version").cloned().unwrap_or(json!(1)),
        });
        if let Some(serial) = document.get("serialNumber") {
            source["serialNumber"] = serial.clone();
        }
        let mut bom = parse(document)?;
        source["productRef"] = json!(bom.root);
        let mut metadata = bom.document["metadata"].clone();
        let fields = metadata.as_object_mut().expect("validated metadata");
        fields.remove("component");
        fields.remove("properties");
        if !fields.is_empty() {
            source["metadata"] = metadata;
        }
        bom.source = Some(source);
        Ok(bom)
    })();
    result.map_err(|error| crate::sbom_ci::input_error(error, path))
}

// Published provenance records include the references assigned at import.
// Move their reference maps with the child graph, not as opaque properties.
fn take_mapped_sources(bom: &mut Bom) -> Result<Vec<(String, Value)>> {
    let mut sources = Vec::new();
    for (owner, node) in &mut bom.nodes {
        let Some(properties) = node.get_mut("properties").and_then(Value::as_array_mut) else {
            continue;
        };
        let mut unmapped = Vec::new();
        for property in std::mem::take(properties) {
            if property["name"] == SOURCE_DOCUMENT {
                let source: Value = serde_json::from_str(string_field(&property, "value")?)
                    .context("reading source BOM provenance")?;
                if source.get(SOURCE_REFERENCES).is_some() {
                    sources.push((owner.clone(), source));
                    continue;
                }
            }
            unmapped.push(property);
        }
        *properties = unmapped;
        if properties.is_empty() {
            node.as_object_mut()
                .expect("validated component")
                .remove("properties");
        }
    }
    Ok(sources)
}

fn verified_source_product(primary: &Bom, child: &Bom) -> Result<Option<String>> {
    let Some(source) = &child.source else {
        return Ok(None);
    };
    if primary.source.as_ref() == Some(source) {
        return Ok(Some(primary.root.clone()));
    }
    let mut found = None;
    for (reference, component) in &primary.nodes {
        for mut record in property_values(component, SOURCE_DOCUMENT)
            .filter_map(|value| serde_json::from_str::<Value>(value).ok())
            .filter(|record| record["sha256"] == source["sha256"])
        {
            ensure!(
                found.is_none(),
                "source BOM SHA-256 appears in multiple import records"
            );
            let references = record
                .as_object_mut()
                .expect("source provenance object")
                .remove(SOURCE_REFERENCES);
            ensure!(
                record == *source,
                "source BOM provenance differs from the supplied document"
            );
            let references = references.context(
                "source BOM provenance has no reference map; merge the original inputs again",
            )?;
            let references: BTreeMap<String, String> =
                serde_json::from_value(references).context("invalid source BOM reference map")?;
            verify_imported_graph(primary, child, reference, &references)
                .context("stale source BOM provenance; merge the original inputs again")?;
            found = Some(reference.clone());
        }
    }
    Ok(found)
}

fn validate_unique_import_targets(references: &BTreeMap<String, String>) -> Result<()> {
    let mut targets = BTreeSet::new();
    for target in references.values() {
        ensure!(
            targets.insert(target),
            "multiple source references map to {target:?}"
        );
    }
    Ok(())
}

fn verify_imported_graph(
    primary: &Bom,
    child: &Bom,
    root: &str,
    references: &BTreeMap<String, String>,
) -> Result<()> {
    validate_unique_import_targets(references)?;
    ensure!(
        child.nodes.keys().eq(references.keys()) && references[&child.root] == root,
        "source BOM reference map does not match the supplied inventory"
    );
    for (original, expected) in &child.nodes {
        let target = &references[original];
        let existing = primary
            .nodes
            .get(target)
            .with_context(|| format!("missing mapped source component {target:?}"))?;
        verify_imported_component(existing, expected, target)?;
    }
    for (child_ref, parent_ref) in &child.parents {
        let target = &references[child_ref];
        ensure!(
            primary.parents.get(target) == Some(&references[parent_ref]),
            "missing source containment for {target:?}"
        );
    }
    for (original, expected) in &child.edges {
        let target = &references[original];
        let targets = primary
            .edges
            .get(target)
            .with_context(|| format!("missing source dependency row {target:?}"))?;
        if let Some(dependencies) = expected {
            ensure!(
                targets.as_ref().is_some_and(|targets| {
                    dependencies
                        .iter()
                        .all(|dependency| targets.contains(&references[dependency]))
                }),
                "missing source dependency for {target:?}"
            );
        }
    }
    for scope in &child.compositions {
        let file_inventory = crate::sbom_ci::is_file_inventory_composition(scope);
        let mut scope = scope.clone();
        remap_composition(&mut scope, references);
        let scope = scope.as_object_mut().expect("composition object");
        scope.remove("bom-ref");
        ensure!(
            primary.compositions.iter().any(|existing| {
                let same_scope =
                    crate::sbom_ci::is_file_inventory_composition(existing) == file_inventory;
                let existing = existing.as_object().expect("validated composition");
                same_scope
                    && existing.len() == scope.len() + usize::from(existing.contains_key("bom-ref"))
                    && scope
                        .iter()
                        .all(|(key, value)| existing.get(key) == Some(value))
            }),
            "missing source composition for {root:?}"
        );
    }
    Ok(())
}

fn verify_imported_component(existing: &Value, expected: &Value, reference: &str) -> Result<()> {
    let digest = artifact_digest(expected)?;
    let same_digest = digest
        .zip(artifact_digest(existing)?)
        .is_some_and(|(expected, actual)| expected.eq_ignore_ascii_case(actual));
    let same_artifact_name = artifact_name(expected)?
        .zip(artifact_name(existing)?)
        .is_some_and(|(expected, actual)| expected.eq_ignore_ascii_case(actual));
    ensure!(
        (existing["type"] == expected["type"] || same_digest && expected["type"] == "file")
            && (existing["name"] == expected["name"] || same_digest || same_artifact_name)
            && (digest.is_none() || same_digest),
        "mapped source component {reference:?} no longer has the declared identity"
    );
    for field in ["version", "purl"] {
        if let Some(value) = expected.get(field) {
            ensure!(
                existing.get(field) == Some(value),
                "mapped source component {reference:?} is missing {field}"
            );
        }
    }
    for expected_hash in optional_array(expected, "hashes")? {
        ensure!(
            optional_array(existing, "hashes")?.iter().any(|hash| {
                hash["alg"] == expected_hash["alg"]
                    && hash["content"]
                        .as_str()
                        .zip(expected_hash["content"].as_str())
                        .is_some_and(|(actual, expected)| actual.eq_ignore_ascii_case(expected))
            }),
            "mapped source component {reference:?} is missing a declared hash"
        );
    }
    ensure!(
        grafted_supplier(existing, expected, reference)?.is_none(),
        "mapped source component {reference:?} lost its supplier declaration"
    );
    ensure!(
        grafted_licenses(existing, expected, reference)?.is_none(),
        "mapped source component {reference:?} lost its license declaration"
    );
    Ok(())
}

fn add_properties(
    component: &mut Value,
    properties: impl IntoIterator<Item = Value>,
) -> Result<()> {
    let mut properties = properties.into_iter().peekable();
    if properties.peek().is_none() {
        return Ok(());
    }
    let target = component
        .as_object_mut()
        .expect("validated component")
        .entry("properties")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .context("component properties must be an array")?;
    for value in properties {
        if !target.contains(&value) {
            target.push(value);
        }
    }
    Ok(())
}

fn document_property(name: &str, data: &Value) -> Value {
    json!({"name": name, "value": data.to_string()})
}

fn parse(document: Value) -> Result<Bom> {
    let object = document
        .as_object()
        .context("expected a CycloneDX object")?;
    ensure!(
        document["bomFormat"] == "CycloneDX" && document["specVersion"] == "1.5",
        "expected a CycloneDX 1.5 JSON file"
    );
    for key in object.keys() {
        ensure!(
            matches!(
                key.as_str(),
                "$schema"
                    | "bomFormat"
                    | "specVersion"
                    | "version"
                    | "serialNumber"
                    | "metadata"
                    | "components"
                    | "dependencies"
                    | "compositions"
            ),
            "unsupported SBOM field {key:?}"
        );
    }
    let product = &document["metadata"]["component"];
    let root = string_field(product, "bom-ref")
        .context("reading metadata.component")?
        .to_owned();
    let mut nodes = BTreeMap::new();
    let mut purls = BTreeMap::new();
    let mut parents = BTreeMap::new();
    let mut pending = vec![(product, None::<&str>)];
    pending.extend(
        optional_array(&document, "components")?
            .iter()
            .map(|component| (component, None)),
    );
    while let Some((component, parent)) = pending.pop() {
        let reference = string_field(component, "bom-ref")?;
        string_field(component, "name")?;
        string_field(component, "type")?;
        ensure!(
            component.get("signature").is_none(),
            "signed component {reference:?} cannot be remapped"
        );
        let mut component_data = component.as_object().expect("validated component").clone();
        component_data.remove("components");
        if let Some(parent) = parent {
            parents.insert(reference.to_owned(), parent.to_owned());
        }
        ensure!(
            nodes
                .insert(reference.to_owned(), Value::Object(component_data))
                .is_none(),
            "duplicate component reference {reference:?}"
        );
        if component.get("purl").is_some() {
            let purl = string_field(component, "purl")?;
            ensure!(
                purls
                    .insert(purl.to_owned(), reference.to_owned())
                    .is_none(),
                "duplicate PURL {purl:?}"
            );
        }
        pending.extend(
            optional_array(component, "components")?
                .iter()
                .map(|child| (child, Some(reference))),
        );
    }

    let dependencies = optional_array(&document, "dependencies")?;
    let mut edges = BTreeMap::new();
    for dependency in dependencies {
        let fields = dependency
            .as_object()
            .context("invalid dependency object")?;
        for key in fields.keys() {
            ensure!(
                matches!(key.as_str(), "ref" | "dependsOn"),
                "unsupported dependency field {key:?}"
            );
        }
        let reference = string_field(dependency, "ref")?;
        ensure!(
            nodes.contains_key(reference),
            "unknown dependency reference {reference:?}"
        );
        let targets = dependency
            .get("dependsOn")
            .map(|_| validate_refs(&nodes, optional_array(dependency, "dependsOn")?))
            .transpose()?;
        ensure!(
            edges.insert(reference.to_owned(), targets).is_none(),
            "duplicate dependency row {reference:?}"
        );
    }

    let compositions = optional_array(&document, "compositions")?.to_vec();
    for composition in &compositions {
        let fields = composition
            .as_object()
            .context("invalid composition object")?;
        for key in fields.keys() {
            ensure!(
                matches!(
                    key.as_str(),
                    "bom-ref" | "aggregate" | "assemblies" | "dependencies"
                ),
                "unsupported composition field {key:?}"
            );
        }
        string_field(composition, "aggregate")?;
        for assembly in optional_array(composition, "assemblies")? {
            let reference = assembly
                .as_str()
                .context("expected an assembly reference")?;
            ensure!(
                nodes.contains_key(reference) || reference.starts_with("urn:cdx:"),
                "unknown dependency or composition reference {reference:?}"
            );
        }
        validate_refs(&nodes, optional_array(composition, "dependencies")?)?;
    }
    Ok(Bom {
        document,
        root,
        nodes,
        purls,
        edges,
        compositions,
        parents,
        source: None,
    })
}

fn string_field<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("expected a non-empty {field:?}"))
}

fn optional_array<'a>(value: &'a Value, field: &str) -> Result<&'a [Value]> {
    match value.get(field) {
        Some(value) => value
            .as_array()
            .map(Vec::as_slice)
            .with_context(|| format!("expected an array in {field:?}")),
        None => Ok(&[]),
    }
}

fn validate_refs(nodes: &BTreeMap<String, Value>, values: &[Value]) -> Result<BTreeSet<String>> {
    let mut refs = BTreeSet::new();
    for value in values {
        let reference = value.as_str().context("expected a string reference")?;
        ensure!(
            nodes.contains_key(reference),
            "unknown dependency or composition reference {reference:?}"
        );
        refs.insert(reference.to_owned());
    }
    Ok(refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(reference: &str, name: &str, digest: Option<&str>) -> Value {
        let mut artifact = json!({
            "type": "library",
            "bom-ref": reference,
            "name": name,
            "properties": [{"name": ARTIFACT_NAME, "value": name}]
        });
        if let Some(digest) = digest {
            artifact["hashes"] = json!([{"alg": "SHA-256", "content": digest}]);
        }
        artifact
    }

    #[test]
    fn configured_builds_keep_the_largest_match_count_for_a_shared_package() {
        let primary = json!({
            "bomFormat": "CycloneDX", "specVersion": "1.5", "version": 1,
            "metadata": {"component": {"type": "application", "bom-ref": "installer", "name": "Installer"}},
            "components": [{"type": "library", "bom-ref": "ni-runtime", "name": "NI runtime"}],
            "dependencies": [{"ref": "installer", "dependsOn": ["ni-runtime"]}]
        });
        let digest = "ab".repeat(32);
        let package = |reference: &str, count: &str| {
            json!({
                "type": "library", "bom-ref": reference, "name": "driver", "version": "1.2.3",
                "purl": "pkg:generic/driver@1.2.3",
                "hashes": [{"alg": "SHA-256", "content": digest}],
                "properties": [
                    {"name": FILE_SUBJECT, "value": "package-archive"},
                    {"name": MATCHED_FILES, "value": count}
                ]
            })
        };
        let declared = json!({
            "bomFormat": "CycloneDX", "specVersion": "1.5", "version": 1,
            "metadata": {"component": {"type": "application", "bom-ref": "exe", "name": "EXE"}},
            "components": [
                {"type": "library", "bom-ref": "library", "name": "Library"},
                package("driver", "0")
            ],
            "dependencies": [
                {"ref": "exe", "dependsOn": ["library"]},
                {"ref": "library", "dependsOn": ["driver"]}
            ]
        });
        let observed = json!({
            "bomFormat": "CycloneDX", "specVersion": "1.5", "version": 1,
            "metadata": {"component": {"type": "library", "bom-ref": "source", "name": "Source"}},
            "components": [package("driver-observed", "7")],
            "dependencies": [{"ref": "source", "dependsOn": ["driver-observed"]}]
        });

        for (children, reference) in [
            (vec![declared.clone(), observed.clone()], "driver"),
            (vec![observed, declared], "driver-observed"),
        ] {
            let combined =
                parse(merge_configured_builds(primary.clone(), children).unwrap()).unwrap();
            assert_eq!(combined.purls["pkg:generic/driver@1.2.3"], reference);
            assert_eq!(combined.nodes[reference], package(reference, "7"));
            assert_eq!(
                combined.edges["installer"],
                Some(BTreeSet::from([
                    "exe".into(),
                    "ni-runtime".into(),
                    "source".into()
                ]))
            );
            assert_eq!(
                combined.edges["exe"],
                Some(BTreeSet::from(["library".into()]))
            );
            for from in ["library", "source"] {
                assert_eq!(
                    combined.edges[from],
                    Some(BTreeSet::from([reference.into()]))
                );
            }
        }
    }

    #[test]
    fn artifact_graft_preserves_hash_and_supplier_evidence_on_reimport() {
        let digest = "ab".repeat(32);
        let sha512 = "cd".repeat(64);
        let supplier = json!({"name": "Example Supply", "url": ["https://example.org"]});
        let mut primary = parse(json!({
            "bomFormat": "CycloneDX", "specVersion": "1.5",
            "metadata": {"component": {
                "type": "application", "bom-ref": "package", "name": "Package",
                "components": [file_subject("shipped", "file", "app.exe", &digest)]
            }}
        }))
        .unwrap();
        let mut app = file_subject(
            "sidecar",
            "application",
            "Application",
            &digest.to_ascii_uppercase(),
        );
        app["hashes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"alg": "SHA-512", "content": sha512}));
        app["supplier"] = supplier.clone();
        let child = || {
            let mut child = parse(json!({
                "bomFormat": "CycloneDX", "specVersion": "1.5",
                "metadata": {"component": app},
            }))
            .unwrap();
            child.source = Some(json!({"sha256": "ef".repeat(32), "version": 1}));
            child
        };

        assert_eq!(
            merge_child(&mut primary, child(), ChildMerge::Relationship).unwrap(),
            "shipped"
        );
        assert_eq!(primary.nodes["shipped"]["supplier"], supplier);
        assert_eq!(
            primary.nodes["shipped"]["hashes"],
            json!([
                {"alg": "SHA-256", "content": digest},
                {"alg": "SHA-512", "content": sha512},
            ])
        );
        let first = render(primary).unwrap();
        let mut repeated = parse(first.clone()).unwrap();
        assert_eq!(
            merge_child(&mut repeated, child(), ChildMerge::Relationship).unwrap(),
            "shipped"
        );
        assert_eq!(render(repeated).unwrap(), first);

        let mut stale = first;
        stale["metadata"]["component"]["components"][0]
            .as_object_mut()
            .unwrap()
            .remove("supplier");
        let error = merge_child(
            &mut parse(stale).unwrap(),
            child(),
            ChildMerge::Relationship,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("lost its supplier declaration"));
    }

    #[test]
    fn artifact_graft_checks_conflicting_hashes_and_supplier_declarations() {
        let digest = "ab".repeat(32);
        let mut shipped = artifact("shipped", "library.lvlibp", Some(&digest));
        shipped["hashes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"alg": "SHA-512", "content": "cd".repeat(64)}));
        let supplier = json!({
            "name": "Example Supply", "bom-ref": "primary-supplier",
            "url": ["https://example.org", "https://example.com"],
            "contact": [{"name": "Alice", "bom-ref": "primary-contact"}, {"name": "Bob"}]
        });
        shipped["supplier"] = supplier.clone();
        let mut primary = parse(json!({
            "bomFormat": "CycloneDX", "specVersion": "1.5",
            "metadata": {"component": {"type": "application", "bom-ref": "host", "name": "Host"}},
            "components": [shipped],
        }))
        .unwrap();
        let mut sidecar = artifact("sidecar", "library.lvlibp", Some(&digest));
        sidecar["supplier"] = json!({
            "name": "Example Supply", "bom-ref": "sidecar-supplier",
            "url": ["https://example.com", "https://example.org"],
            "contact": [{"name": "Bob"}, {"name": "Alice", "bom-ref": "sidecar-contact"}]
        });
        graft_artifact(&mut primary, "shipped", &sidecar).unwrap();
        let expected = primary.nodes["shipped"].clone();
        assert_eq!(expected["supplier"], supplier);

        sidecar["hashes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"alg": "SHA-512", "content": "ef".repeat(64)}));
        let error = graft_artifact(&mut primary, "shipped", &sidecar).unwrap_err();
        assert!(error.to_string().contains("conflicting SHA-512 hashes"));
        assert_eq!(primary.nodes["shipped"], expected);

        sidecar["hashes"].as_array_mut().unwrap().pop();
        sidecar["supplier"]["name"] = json!("Different Supplier");
        let error = graft_artifact(&mut primary, "shipped", &sidecar).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("conflicting supplier declarations")
        );
        assert_eq!(primary.nodes["shipped"], expected);
    }

    #[test]
    fn graft_checks_license_sets_and_enriches_unknown_licenses() {
        let digest = "ab".repeat(32);
        let mut sidecar = artifact("sidecar", "helper.lvlibp", Some(&digest));
        let known = json!([{"license": {"id": "MIT"}}, {"license": {"id": "Apache-2.0"}}]);
        for initial in [None, Some(json!([]))] {
            let mut shipped = artifact("shipped", "helper.lvlibp", Some(&digest));
            if let Some(initial) = initial {
                shipped["licenses"] = initial;
            }
            let mut primary = parse(json!({
                "bomFormat": "CycloneDX", "specVersion": "1.5",
                "metadata": {"component": {"type": "application", "bom-ref": "host", "name": "host"}},
                "components": [shipped]
            })).unwrap();
            sidecar["licenses"] = known.clone();
            graft_artifact(&mut primary, "shipped", &sidecar).unwrap();
            assert_eq!(primary.nodes["shipped"]["licenses"], known);

            sidecar["licenses"] = json!([
                {"license": {"id": "Apache-2.0", "bom-ref": "sidecar-license"}},
                {"license": {"id": "MIT"}}
            ]);
            graft_artifact(&mut primary, "shipped", &sidecar).unwrap();

            sidecar["licenses"] = json!([]);
            graft_artifact(&mut primary, "shipped", &sidecar).unwrap();
            assert_eq!(primary.nodes["shipped"]["licenses"], known);

            for conflicting in [
                json!([{"license": {"id": "GPL-3.0-only"}}]),
                json!([{"expression": "MIT OR Apache-2.0"}]),
            ] {
                sidecar["licenses"] = conflicting;
                let error = graft_artifact(&mut primary, "shipped", &sidecar)
                    .unwrap_err()
                    .to_string();
                assert!(
                    error.contains("conflicting license declarations for artifact \"shipped\""),
                    "{error}"
                );
                assert!(error.contains("Apache-2.0"), "{error}");
                assert_eq!(primary.nodes["shipped"]["licenses"], known);
            }
        }
    }

    #[test]
    fn grafts_by_hash_without_reparenting_and_repeats_idempotently() {
        let digest = "ab".repeat(32);
        let mut packed = artifact("packed", "built.lvlibp", Some(&digest));
        packed["version"] = json!("1.0.0.7");
        let primary = json!({
            "bomFormat": "CycloneDX",
            "specVersion": "1.5",
            "metadata": {"component": {"type": "application", "bom-ref": "exe", "name": "exe"}},
            "components": [
                packed,
                {"type": "library", "bom-ref": "package", "name": "package"},
                {"type": "library", "bom-ref": "legacy", "name": "existing dependency"},
                {"type": "library", "bom-ref": "shared", "name": "shared", "purl": "pkg:generic/shared@1.0",
                 "properties": [{"name": MATCHED_FILES, "value": "8"}]}
            ],
            "dependencies": [
                {"ref": "exe", "dependsOn": ["package"]},
                {"ref": "package", "dependsOn": ["packed"]},
                {"ref": "packed", "dependsOn": ["legacy"]}
            ],
            "compositions": [{"aggregate": "incomplete", "assemblies": ["packed"]}]
        });
        let mut product = artifact(
            "child",
            "renamed.lvlibp",
            Some(&digest.to_ascii_uppercase()),
        );
        product["version"] = json!("1.0.0.7");
        let sidecar = json!({
            "bomFormat": "CycloneDX",
            "specVersion": "1.5",
            "metadata": {
                "component": product,
                "properties": [{"name": "description", "value": "PPL build specification"}]
            },
            "components": [
                {"type": "library", "bom-ref": "legacy", "name": "sidecar dependency"},
                {"type": "library", "bom-ref": "shared-child", "name": "shared", "purl": "pkg:generic/shared@1.0",
                 "properties": [{"name": MATCHED_FILES, "value": "2"}]}
            ],
            "dependencies": [
                {"ref": "child", "dependsOn": ["legacy", "shared-child"]}
            ],
            "compositions": [{"aggregate": "incomplete", "assemblies": ["child"]}]
        });

        let graft = |primary, child| {
            let mut primary = parse(primary).unwrap();
            merge_boms(&mut primary, parse(child).unwrap()).unwrap();
            render(primary).unwrap()
        };
        let mut stale = sidecar.clone();
        stale["metadata"]["component"]["version"] = json!("1.0.0.0");
        let error =
            merge_boms(&mut parse(primary.clone()).unwrap(), parse(stale).unwrap()).unwrap_err();
        assert!(error.to_string().contains("version mismatch"));
        let result = graft(primary, sidecar.clone());
        assert_eq!(result["dependencies"][0]["dependsOn"], json!(["package"]));
        let graph = parse(result.clone()).unwrap();
        assert_eq!(
            graph.edges["package"],
            Some(BTreeSet::from(["packed".to_owned()]))
        );
        assert_eq!(
            graph.edges["packed"],
            Some(BTreeSet::from([
                "legacy".to_owned(),
                "lvpm:merge:child:1".to_owned(),
                "shared".to_owned()
            ]))
        );
        assert_eq!(graph.nodes["packed"]["version"], "1.0.0.7");
        assert_eq!(graph.nodes["packed"]["hashes"][0]["content"], digest);
        assert_eq!(
            artifact_name(&graph.nodes["packed"]).unwrap(),
            Some("built.lvlibp")
        );
        assert_eq!(
            graph.nodes["packed"]["properties"][1]["value"],
            "PPL build specification"
        );
        assert_eq!(graph.compositions.len(), 1);
        assert_eq!(graft(result.clone(), sidecar), result);
    }

    #[test]
    fn reuses_an_existing_binary_import_with_the_same_path_and_library_id() {
        let binary_import = |reference, path, location| {
            let mut component = artifact(reference, "dependency.lvlibp", None);
            let properties = component["properties"].as_array_mut().unwrap();
            properties.push(json!({"name": REFERENCE_PATH, "value": path}));
            properties.push(
                json!({"name": LIBRARY_ID, "value": "{00000000-0000-0000-0000-000000000001}"}),
            );
            if let Some(location) = location {
                properties.push(json!({"name": ARTIFACT_PATH, "value": location}));
            }
            component
        };
        let mut primary = parse(json!({
            "bomFormat": "CycloneDX",
            "specVersion": "1.5",
            "metadata": {"component": {"type": "application", "bom-ref": "exe", "name": "exe"}},
            "components": [
                artifact("packed", "built.lvlibp", None),
                binary_import("same", "support/dependency.lvlibp", Some("project/support/dependency.lvlibp")),
                binary_import("elsewhere", "backup/dependency.lvlibp", Some("project/backup/dependency.lvlibp"))
            ],
            "dependencies": [
                {"ref": "exe", "dependsOn": ["packed"]},
                {"ref": "packed", "dependsOn": ["same", "elsewhere"]}
            ]
        })).unwrap();
        let sidecar = parse(json!({
            "bomFormat": "CycloneDX",
            "specVersion": "1.5",
            "metadata": {"component": artifact("root", "built.lvlibp", None)},
            "components": [binary_import("sidecar-import", "support/dependency.lvlibp", None)],
            "dependencies": [{"ref": "root", "dependsOn": ["sidecar-import"]}]
        }))
        .unwrap();

        merge_boms(&mut primary, sidecar).unwrap();
        let result = parse(render(primary).unwrap()).unwrap();
        assert_eq!(result.nodes.len(), 4);
        assert_eq!(
            result.edges["packed"],
            Some(BTreeSet::from(["same".to_owned(), "elsewhere".to_owned()]))
        );
        assert_eq!(
            property_values(&result.nodes["same"], ARTIFACT_PATH).next(),
            Some("project/support/dependency.lvlibp")
        );
    }

    #[test]
    fn applies_sidecars_to_the_growing_graph_and_shares_package_purls() {
        let bom = |component, components, dependencies| {
            parse(json!({
                "bomFormat": "CycloneDX",
                "specVersion": "1.5",
                "metadata": {"component": component},
                "components": components,
                "dependencies": dependencies
            }))
            .unwrap()
        };
        let mut primary = bom(
            json!({"type": "application", "bom-ref": "exe", "name": "exe"}),
            json!([artifact("packed", "main.lvlibp", None)]),
            json!([{"ref": "exe", "dependsOn": ["packed"]}]),
        );
        let first = bom(
            artifact("first", "main.lvlibp", None),
            json!([
                artifact("core", "core.lvlibp", None),
                {"type": "library", "bom-ref": "shared", "name": "shared", "purl": "pkg:generic/shared@1.0"}
            ]),
            json!([{"ref": "first", "dependsOn": ["core", "shared"]}]),
        );
        let second = bom(
            artifact("second", "core.lvlibp", None),
            json!([{"type": "library", "bom-ref": "other-ref", "name": "shared", "purl": "pkg:generic/shared@1.0"}]),
            json!([{"ref": "second", "dependsOn": ["other-ref"]}]),
        );

        merge_boms(&mut primary, first).unwrap();
        merge_boms(&mut primary, second).unwrap();
        let result = parse(render(primary).unwrap()).unwrap();
        assert_eq!(result.nodes.len(), 4);
        assert_eq!(
            result.edges["packed"],
            Some(BTreeSet::from(["core".to_owned(), "shared".to_owned()]))
        );
        assert_eq!(
            result.edges["core"],
            Some(BTreeSet::from(["shared".to_owned()]))
        );
        assert_eq!(
            result.edges["exe"],
            Some(BTreeSet::from(["packed".to_owned()]))
        );
    }

    #[test]
    fn routes_by_the_artifact_marker_and_rejects_an_unmatched_ppl() {
        let bom = |component| {
            parse(json!({
                "bomFormat": "CycloneDX",
                "specVersion": "1.5",
                "metadata": {"component": component},
                "components": []
            }))
            .unwrap()
        };
        let mut primary = bom(json!({"type": "application", "bom-ref": "exe", "name": "exe"}));
        let named_like_ppl =
            bom(json!({"type": "library", "bom-ref": "child", "name": "missing.lvlibp"}));
        merge_boms(&mut primary, named_like_ppl).unwrap();
        assert_eq!(
            primary.edges["exe"],
            Some(BTreeSet::from(["child".to_owned()]))
        );

        let marked_ppl = bom(artifact("ppl", "missing.lvlibp", None));
        let error = merge_boms(&mut primary, marked_ppl).unwrap_err();
        assert!(error.to_string().contains("no primary PPL component"));
    }

    fn file_subject(reference: &str, kind: &str, name: &str, digest: &str) -> Value {
        json!({
            "type": kind, "bom-ref": reference, "name": name,
            "hashes": [{"alg": "SHA-256", "content": digest}],
            "properties": [
                {"name": FILE_SUBJECT, "value": "artifact-file"},
                {"name": FILE_PATH, "value": "bin/app.exe"}
            ],
        })
    }

    #[test]
    fn explicit_composition_matches_shipped_bytes_and_keeps_requirements_distinct() {
        let digest = "ab".repeat(32);
        let primary = json!({
            "bomFormat": "CycloneDX", "specVersion": "1.5",
            "metadata": {"component": {
                "type": "application", "bom-ref": "installer", "name": "Installer",
                "components": [file_subject("installed-app", "file", "app.exe", &digest)]
            }}
        });
        let app = json!({
            "bomFormat": "CycloneDX", "specVersion": "1.5",
            "metadata": {"component": file_subject("installer", "application", "Application", &digest)},
            "compositions": [{"aggregate": "incomplete", "assemblies": ["installer"]}]
        });
        let ppl = json!({
            "bomFormat": "CycloneDX", "specVersion": "1.5",
            "metadata": {"component": artifact("ppl", "plugin.lvlibp", None)},
            "components": [{"type": "library", "bom-ref": "runtime", "name": "Runtime"}],
            "dependencies": [{"ref": "ppl", "dependsOn": ["runtime"]}]
        });

        let mut combined = parse(primary).unwrap();
        let mut child = parse(app).unwrap();
        child.source = Some(json!({
            "sha256": "ef".repeat(32), "version": 2,
            "serialNumber": "urn:uuid:7874bd20-29a1-4140-81d1-c81480b1a623",
            "metadata": {"timestamp": "2026-11-20T12:00:00Z"}
        }));
        let app_ref = merge_child(&mut combined, child, ChildMerge::Relationship).unwrap();
        assert_eq!(app_ref, "installed-app");
        let ppl_ref =
            merge_child(&mut combined, parse(ppl).unwrap(), ChildMerge::Relationship).unwrap();
        let relations: Plan = serde_json::from_value(json!({
            "children": {"app": "app.cdx.json", "add-on": "ppl.cdx.json"},
            "relationships": [
                {"parent": "primary", "child": "app", "kind": "contains", "evidence": "installer payload"},
                {"parent": "app", "child": "add-on", "kind": "requires", "evidence": "binary import"}
            ],
        })).unwrap();
        relations.validate().unwrap();
        assert_eq!(relations.order(), ["app", "add-on"]);
        let roots = BTreeMap::from([
            ("primary", combined.root.clone()),
            ("app", app_ref),
            ("add-on", ppl_ref),
        ]);
        for relationship in &relations.relationships {
            apply_relationship(&mut combined, relationship, &roots).unwrap();
        }
        let result = render(combined).unwrap();
        crate::sbom_ci::validate(&result, Path::new("merged.cdx.json")).unwrap();
        let contained = result["metadata"]["component"]["components"]
            .as_array()
            .unwrap();
        assert_eq!(contained.len(), 1);
        assert_eq!(contained[0]["type"], "application");
        assert_eq!(contained[0]["name"], "Application");
        let source = property_values(&contained[0], SOURCE_DOCUMENT)
            .next()
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(source).unwrap()["version"], 2);
        let graph = parse(result).unwrap();
        assert_eq!(graph.edges["installer"], None);
        assert_eq!(
            graph.edges["installed-app"],
            Some(BTreeSet::from(["ppl".to_owned()]))
        );
        assert_eq!(
            graph.edges["ppl"],
            Some(BTreeSet::from(["runtime".to_owned()]))
        );
        assert_eq!(
            graph.compositions[0]["assemblies"],
            json!(["installed-app"])
        );
    }

    #[test]
    fn repeats_ordinary_children_and_remaps_imported_relationship_evidence() {
        let document = |root: &str, name: &str, components: Value| {
            parse(json!({
                "bomFormat": "CycloneDX", "specVersion": "1.5",
                "metadata": {"component": {"type": "application", "bom-ref": root, "name": name}},
                "components": components,
            }))
            .unwrap()
        };
        let mut main = document(
            "main",
            "Main",
            json!([
                {"type": "library", "bom-ref": "child-dependency", "name": "Original dependency"}
            ]),
        );
        let child = || {
            let mut child = document(
                "main",
                "Child",
                json!([
                    {"type": "library", "bom-ref": "child-dependency", "name": "Child dependency"}
                ]),
            );
            child.source = Some(json!({"sha256": "aa".repeat(32), "version": 1}));
            let relation = Relationship {
                parent: "main".into(),
                child: "child-dependency".into(),
                kind: RelationKind::Requires,
                evidence: "link table".into(),
            };
            apply_relationship(
                &mut child,
                &relation,
                &BTreeMap::from([
                    ("main", "main".to_owned()),
                    ("child-dependency", "child-dependency".to_owned()),
                ]),
            )
            .unwrap();
            child
        };
        let product = merge_boms(&mut main, child()).unwrap();
        assert_eq!(merge_boms(&mut main, child()).unwrap(), product);
        let child_edges = main.edges[&product].as_ref().unwrap();
        let remapped = child_edges.first().unwrap();
        assert_ne!(remapped, "child-dependency");
        let evidence = property_values(&main.nodes[&product], RELATION_EVIDENCE)
            .next()
            .unwrap();
        let evidence: Value = serde_json::from_str(evidence).unwrap();
        assert_eq!(evidence["parent"], product);
        assert_eq!(evidence["child"], *remapped);
        assert_eq!(main.nodes.len(), 4);

        let properties = main.nodes[&product]["properties"].as_array().unwrap();
        let index = properties
            .iter()
            .position(|property| property["name"] == SOURCE_DOCUMENT)
            .unwrap();
        let original = properties[index].clone();
        let mut modified: Value =
            serde_json::from_str(original["value"].as_str().unwrap()).unwrap();
        modified[SOURCE_REFERENCES]["child-dependency"] = json!(product);
        main.nodes.get_mut(&product).unwrap()["properties"][index] =
            document_property(SOURCE_DOCUMENT, &modified);
        let error = merge_boms(&mut main, child()).unwrap_err();
        assert!(format!("{error:#}").contains("multiple source references"));
        main.nodes.get_mut(&product).unwrap()["properties"][index] = original;

        main.edges
            .get_mut(&product)
            .unwrap()
            .as_mut()
            .unwrap()
            .clear();
        let error = merge_boms(&mut main, child()).unwrap_err();
        assert!(error.to_string().contains("stale source BOM provenance"));
        assert!(format!("{error:#}").contains("missing source dependency"));
    }

    #[test]
    fn merged_document_identity_depends_on_content() {
        let original = "urn:uuid:7874bd20-29a1-4140-81d1-c81480b1a623";
        let combined = |name: &str| {
            let mut primary = parse(json!({
                "bomFormat": "CycloneDX",
                "specVersion": "1.5",
                "serialNumber": original,
                "version": 7,
                "metadata": {"component": {"type": "application", "bom-ref": "main", "name": "Main"}},
            })).unwrap();
            let child = parse(json!({
                "bomFormat": "CycloneDX",
                "specVersion": "1.5",
                "metadata": {"component": {"type": "library", "bom-ref": "child", "name": name}},
            }))
            .unwrap();
            merge_boms(&mut primary, child).unwrap();
            render(primary).unwrap()
        };
        let sqlite = combined("SQLite");
        assert_eq!(sqlite["version"], 1);
        assert_ne!(sqlite["serialNumber"], original);
        assert_ne!(sqlite["serialNumber"], combined("zlib")["serialNumber"]);
        assert_eq!(sqlite, combined("SQLite"));
    }

    #[test]
    fn explicit_ppl_sources_with_equal_names_and_distinct_digests_stay_separate() {
        let mut installed = artifact("original", "plugin.lvlibp", Some(&"aa".repeat(32)));
        installed["version"] = json!("1.0.0");
        let mut distinct = artifact("input", "plugin.lvlibp", Some(&"bb".repeat(32)));
        distinct["version"] = json!("2.0.0");
        let mut primary = parse(json!({
            "bomFormat": "CycloneDX", "specVersion": "1.5",
            "metadata": {"component": {"type": "application", "bom-ref": "host", "name": "Host"}},
            "components": [installed]
        }))
        .unwrap();
        let child = parse(json!({
            "bomFormat": "CycloneDX", "specVersion": "1.5",
            "metadata": {"component": distinct}
        }))
        .unwrap();
        let incoming = merge_child(&mut primary, child, ChildMerge::Relationship).unwrap();
        assert_eq!(incoming, "input");
        assert_eq!(primary.nodes["input"]["version"], "2.0.0");
        assert_eq!(primary.nodes["original"]["version"], "1.0.0");
    }

    #[test]
    fn falls_back_to_an_unambiguous_filename_when_a_digest_is_unavailable() {
        let first = "aa".repeat(32);
        let second = "bb".repeat(32);
        let bom = |components| {
            parse(json!({
                "bomFormat": "CycloneDX",
                "specVersion": "1.5",
                "metadata": {"component": {"type": "application", "bom-ref": "exe", "name": "exe"}},
                "components": components
            }))
            .unwrap()
        };
        let mut built = artifact("first", "helper.lvlibp", Some(&first));
        built["version"] = json!("1.1.0.4");
        let source_only = artifact("sidecar", "HELPER.lvlibp", None);
        let primary = bom(vec![built.clone()]);
        assert_eq!(
            find_ppl_node(&primary, &source_only).unwrap(),
            Some("first")
        );
        let mut version_conflict = source_only.clone();
        version_conflict["version"] = json!("1.1.0.3");
        assert!(
            find_ppl_node(&primary, &version_conflict)
                .unwrap_err()
                .to_string()
                .contains("version mismatch")
        );

        let stale = artifact("sidecar", "helper.lvlibp", Some(&second));
        assert!(
            find_ppl_node(&primary, &stale)
                .unwrap_err()
                .to_string()
                .contains("SHA-256 mismatch")
        );
        let multiple = bom(vec![
            built,
            artifact("second", "helper.lvlibp", Some(&second)),
        ]);
        assert!(
            find_ppl_node(&multiple, &source_only)
                .unwrap_err()
                .to_string()
                .contains("multiple")
        );
        assert_eq!(find_ppl_node(&multiple, &stale).unwrap(), Some("second"));
    }
}
