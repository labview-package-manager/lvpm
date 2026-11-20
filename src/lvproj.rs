//! Read saved LabVIEW project items and build specifications.

use anyhow::{Context, Result, bail, ensure};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::collections::BTreeMap;
use std::path::Path;

pub const PACKED_LIBRARY_TYPE: &str = "Packed Library";
pub const NIPKG_TYPE: &str = "{E661DAE2-7517-431F-AC41-30807A3BDA38}";
const VERSION_FIELDS: &[(&str, &str)] = &[
    ("MinVersion", "min-version"),
    ("MinVersionType", "min-version-type"),
    ("MaxVersion", "max-version"),
    ("MaxVersionInclusive", "max-version-inclusive"),
];

pub struct NipkgBuild {
    pub name: String,
    pub version: Option<String>,
    pub auto_select: Option<String>,
    pub requirements: BTreeMap<String, Requirement>,
}

pub struct Requirement {
    pub relationship: String,
    pub version_fields: Vec<(&'static str, String)>,
}

pub struct Build {
    pub kind: String,
    pub properties: BTreeMap<String, String>,
    pub items: BTreeMap<String, Option<String>>,
}

pub(crate) struct ProjectItem {
    pub(crate) url: String,
    pub(crate) is_directory: bool,
}

impl Build {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.properties
            .get(key)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    }
}

pub fn nipkg_requirements(mut props: BTreeMap<String, String>) -> Result<NipkgBuild> {
    let name = props
        .remove("PKG_packageName")
        .filter(|name| !name.is_empty())
        .context("NIPKG build specification has no package name")?;
    let version = props
        .remove("PKG_version")
        .filter(|version| !version.is_empty());
    let auto_select = props.remove("PKG_autoSelectDeps");

    let count = match props.get("PKG_dependencies.Count") {
        Some(count) => count
            .parse::<usize>()
            .context("invalid PKG_dependencies.Count")?,
        None if props.keys().any(|key| key.starts_with("PKG_dependencies[")) => {
            bail!("NIPKG dependency entries have no PKG_dependencies.Count");
        }
        None => 0,
    };
    let mut requirements = BTreeMap::new();
    for index in 0..count {
        let field = |key| {
            props
                .get(&format!("PKG_dependencies[{index}].{key}"))
                .filter(|value| !value.is_empty())
        };
        let name = field("Package.Name")
            .with_context(|| format!("NIPKG dependency #{index} has no package name"))?;
        let relationship = field("Relationship")
            .with_context(|| format!("NIPKG dependency {name:?} has no relationship"))?;
        let version_fields = VERSION_FIELDS
            .iter()
            .filter(|(key, _)| *key != "MinVersionType" || field("MinVersion").is_some())
            .filter(|(key, _)| *key != "MaxVersionInclusive" || field("MaxVersion").is_some())
            .filter_map(|&(key, property)| Some((property, field(key)?.clone())))
            .collect();
        let requirement = Requirement {
            relationship: relationship.clone(),
            version_fields,
        };
        ensure!(
            requirements.insert(name.clone(), requirement).is_none(),
            "duplicate NIPKG dependency {name:?}"
        );
    }
    Ok(NipkgBuild {
        name,
        version,
        auto_select,
        requirements,
    })
}

/// Read URL-backed source items from all saved targets in the project.
pub(crate) fn read_project_items(path: &Path) -> Result<BTreeMap<String, ProjectItem>> {
    let xml = std::fs::read_to_string(path)?;
    let mut reader = Reader::from_str(&xml);
    let mut depth = 0usize;
    let mut project_found = false;
    let mut item_path = Vec::new();
    let mut items = BTreeMap::new();

    loop {
        let event = reader.read_event()?;
        let opens = matches!(&event, Event::Start(_));
        match event {
            Event::Start(element) | Event::Empty(element) => {
                let tag = element.name();
                if depth == 0 {
                    ensure!(
                        !project_found && tag.as_ref() == b"Project",
                        "expected a LabVIEW Project element"
                    );
                    project_found = true;
                } else if depth == item_path.len() + 1
                    && tag.as_ref() == b"Item"
                    && let Some(item) = source_item(&reader, &element)?
                {
                    item_path.push(item.name);
                    let reference = format!("/{}", item_path.join("/"));
                    // Top-level items name target trees rather than source files.
                    let file = item
                        .url
                        .filter(|url| depth > 1 && !url.is_empty())
                        .map(|url| ProjectItem {
                            url,
                            is_directory: item.is_directory,
                        });
                    ensure!(
                        items.insert(reference.clone(), file).is_none(),
                        "ambiguous LabVIEW project item {reference:?}"
                    );
                    if !opens {
                        item_path.pop();
                    }
                }
                if opens {
                    depth += 1;
                }
            }
            Event::End(_) => {
                depth = depth
                    .checked_sub(1)
                    .context("unmatched LabVIEW project XML end tag")?;
                item_path.truncate(depth.saturating_sub(1));
            }
            Event::Eof => break,
            _ => {}
        }
    }

    ensure!(project_found, "LabVIEW Project element not found");
    ensure!(depth == 0, "unclosed element in LabVIEW project XML");
    Ok(items
        .into_iter()
        .filter_map(|(reference, item)| Some((reference, item?)))
        .collect())
}

pub fn read_build(path: &Path, target: &str, spec: &str) -> Result<Build> {
    let xml = std::fs::read_to_string(path)?;
    let mut reader = Reader::from_str(&xml);
    let mut depth = 0usize;
    let mut selected = 0usize;
    let mut target_found = false;
    let mut kind = None;
    let mut properties = BTreeMap::new();
    let mut items = BTreeMap::new();
    let mut item_path: Vec<String> = Vec::new();

    loop {
        let event = reader.read_event()?;
        let opens = matches!(&event, Event::Start(_));
        match event {
            Event::Start(element) | Event::Empty(element) => {
                let tag = element.name();
                if depth == 4 && selected == 4 && tag.as_ref() == b"Property" {
                    let name = attribute(&reader, &element, b"Name")?;
                    let value = if opens {
                        let raw = reader.read_text(tag)?;
                        quick_xml::escape::unescape(&raw)?.trim().to_owned()
                    } else {
                        String::new()
                    };
                    if let Some(name) = name {
                        ensure!(
                            properties.insert(name.clone(), value).is_none(),
                            "duplicate build property {name:?}"
                        );
                    }
                    continue;
                }
                if selected == 2
                    && depth >= 2
                    && depth - 2 == item_path.len()
                    && tag.as_ref() == b"Item"
                    && let Some(item) = source_item(&reader, &element)?
                {
                    item_path.push(item.name);
                    let reference = format!("/{target}/{}", item_path.join("/"));
                    ensure!(
                        items.insert(reference.clone(), item.url).is_none(),
                        "ambiguous LabVIEW project item {reference:?}"
                    );
                    if !opens {
                        item_path.pop();
                    }
                }
                if depth == selected {
                    let matched = match (depth, tag.as_ref()) {
                        (0, b"Project") => true,
                        (1, b"Item")
                            if attribute(&reader, &element, b"Name")?.as_deref()
                                == Some(target) =>
                        {
                            ensure!(!target_found, "ambiguous LabVIEW target {target:?}");
                            target_found = true;
                            true
                        }
                        (2, b"Item") => {
                            attribute(&reader, &element, b"Type")?.as_deref() == Some("Build")
                        }
                        (3, b"Item")
                            if attribute(&reader, &element, b"Name")?.as_deref() == Some(spec) =>
                        {
                            ensure!(
                                kind.is_none(),
                                "ambiguous build specification {spec:?} on target {target:?}"
                            );
                            kind = Some(
                                attribute(&reader, &element, b"Type")?
                                    .context("build specification has no type")?,
                            );
                            true
                        }
                        _ => false,
                    };
                    if matched && opens {
                        selected += 1;
                    }
                }
                if opens {
                    depth += 1;
                }
            }
            Event::End(_) => {
                depth -= 1;
                selected = selected.min(depth);
                item_path.truncate(depth.saturating_sub(2));
            }
            Event::Eof => break,
            _ => {}
        }
    }

    ensure!(depth == 0, "unclosed element in LabVIEW project XML");
    ensure!(target_found, "LabVIEW target {target:?} not found");
    let kind = kind
        .with_context(|| format!("build specification {spec:?} not found on target {target:?}"))?;
    Ok(Build {
        kind,
        properties,
        items,
    })
}

struct SourceItem {
    name: String,
    url: Option<String>,
    is_directory: bool,
}

fn source_item(reader: &Reader<&[u8]>, element: &BytesStart<'_>) -> Result<Option<SourceItem>> {
    let kind = attribute(reader, element, b"Type")?;
    if matches!(kind.as_deref(), Some("Build" | "Dependencies")) {
        return Ok(None);
    }
    Ok(Some(SourceItem {
        name: attribute(reader, element, b"Name")?.context("unnamed LabVIEW project item")?,
        url: attribute(reader, element, b"URL")?,
        is_directory: kind.as_deref() == Some("Folder"),
    }))
}

fn attribute(
    reader: &Reader<&[u8]>,
    element: &BytesStart<'_>,
    name: &[u8],
) -> Result<Option<String>> {
    element
        .try_get_attribute(name)?
        .map(|attribute| {
            Ok(attribute
                .decode_and_unescape_value(reader.decoder())?
                .into_owned())
        })
        .transpose()
}
