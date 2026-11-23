//! Discover selected LabVIEW sources in file-based build specifications.

use crate::{linkfix, lvproj};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

/// Selected PPL files, referenced VI builds, and unresolved-source diagnostics.
#[derive(Default)]
pub(crate) struct DistributionSources {
    pub(crate) files: BTreeSet<PathBuf>,
    pub(crate) issues: BTreeSet<String>,
    pub(crate) vi_builds: BTreeMap<String, lvproj::Build>,
}

pub(crate) fn selected_sources(
    project: &Path,
    target: &str,
    build: &lvproj::Build,
) -> Result<DistributionSources> {
    let mut sources = Sources {
        project: project.canonicalize()?,
        result: DistributionSources::default(),
        visited: BTreeSet::new(),
    };
    sources.build(target, build)?;
    Ok(sources.result)
}

struct Sources {
    project: PathBuf,
    result: DistributionSources,
    visited: BTreeSet<(String, String)>,
}

impl Sources {
    fn build(&mut self, target: &str, build: &lvproj::Build) -> Result<()> {
        if build.kind.eq_ignore_ascii_case(lvproj::NIPKG_TYPE) {
            self.nipkg(target, build)
        } else if build.kind == "Installer" {
            self.installer(target, build)
        } else {
            self.included(build)
        }
    }

    fn nipkg(&mut self, target: &str, build: &lvproj::Build) -> Result<()> {
        let source_count = count(build, "PKG_sources.Count")?;
        for (index, field, reference) in indexed(build, "PKG_sources", source_count) {
            if field != "ID" || reference == "/" {
                continue;
            }
            match build.get(&format!("PKG_sources[{index}].Type")) {
                Some("File") => self.project_file(build, reference),
                Some("Folder") => self.project_selection(build, |item| under(item, reference))?,
                Some(kind) if kind == "Build" || kind.ends_with(" Build") => {
                    self.referenced_build(target, reference, None)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn installer(&mut self, target: &str, build: &lvproj::Build) -> Result<()> {
        let source_count = count(build, "SourceCount")?;
        for (index, field, reference) in indexed(build, "Source", source_count) {
            if field != "tag" || reference == "/" {
                continue;
            }
            let prefix = format!("Source[{index}].File");
            let file_count = count(build, &format!("{prefix}Count"))?;
            let selected = indexed(build, &prefix, file_count)
                .filter(|(_, field, _)| *field == "name")
                .map(|(_, _, name)| name)
                .collect::<Vec<_>>();
            match build.get(&format!("Source[{index}].type")) {
                Some("File") => self.project_file(build, reference),
                Some("Folder") if selected.is_empty() => {
                    self.project_selection(build, |item| under(item, reference))?;
                }
                Some("Folder") => {
                    for name in selected.into_iter().filter(|name| is_ppl(name)) {
                        self.project_file(build, &format!("{reference}/{name}"));
                    }
                }
                Some(_) if !build.items.contains_key(reference) => {
                    self.referenced_build(target, reference, Some(&selected))?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn referenced_build(
        &mut self,
        target: &str,
        reference: &str,
        selected: Option<&[&str]>,
    ) -> Result<()> {
        // The build container name can be localized. Its position in the reference is stable.
        let Some((_, spec)) = reference
            .strip_prefix(&format!("/{target}/"))
            .and_then(|path| path.split_once('/'))
            .filter(|(group, spec)| !group.is_empty() && !spec.is_empty() && !spec.contains('/'))
        else {
            return Ok(());
        };
        if selected.is_none() && self.result.vi_builds.contains_key(spec) {
            return Ok(());
        }
        let build = lvproj::read_build(&self.project, target, spec)
            .with_context(|| format!("reading referenced build {reference:?}"))?;
        if let Some(selected) = selected
            && !selected.is_empty()
        {
            for name in selected.iter().copied().filter(|name| is_ppl(name)) {
                self.selected_output(&build, reference, name);
            }
        } else if build.kind == lvproj::PACKED_LIBRARY_TYPE {
            self.packed_output(&build, reference);
        } else if selected.is_none() && matches!(build.kind.as_str(), "EXE" | "Source Distribution")
        {
            // The VI scanner handles the full graph, including selected PPLs.
            self.result.vi_builds.insert(spec.to_owned(), build);
        } else if selected.is_none() && self.visited.insert((target.to_owned(), spec.to_owned())) {
            self.build(target, &build)?;
        }
        Ok(())
    }

    fn project_file(&mut self, build: &lvproj::Build, reference: &str) {
        let path = build
            .items
            .get(reference)
            .and_then(Option::as_deref)
            .and_then(|url| labview_path(&self.project, url));
        if is_ppl(reference) || path.as_ref().is_some_and(is_ppl) {
            self.add_ppl(path, reference);
        }
    }

    fn project_selection(
        &mut self,
        build: &lvproj::Build,
        includes: impl Fn(&str) -> bool,
    ) -> Result<()> {
        for (reference, url) in &build.items {
            if !includes(reference) {
                continue;
            }
            let path = url
                .as_deref()
                .and_then(|url| labview_path(&self.project, url));
            if is_ppl(reference) || path.as_ref().is_some_and(is_ppl) {
                self.add_ppl(path, reference);
            } else if let Some(path) = path.filter(|path| path.is_dir()) {
                for (file, relative) in linkfix::walk(&path)? {
                    let item = format!("{reference}/{relative}");
                    if is_ppl(&file) && includes(&item) {
                        self.add_ppl(Some(file), &item);
                    }
                }
            }
        }
        Ok(())
    }

    fn included(&mut self, build: &lvproj::Build) -> Result<()> {
        let count = count(build, "SourceCount")?;
        let selection = indexed(build, "Source", count)
            .filter(|(_, field, _)| *field == "itemID")
            .filter_map(|(index, _, item)| {
                let inclusion = build.get(&format!("Source[{index}].sourceInclusion"))?;
                match inclusion {
                    "Include" | "TopLevel" => Some((item, true)),
                    "Exclude" => Some((item, false)),
                    _ => None,
                }
            })
            .collect::<BTreeMap<_, _>>();
        self.project_selection(build, |item| {
            selection
                .iter()
                .filter(|(selected, _)| under(item, selected))
                .max_by_key(|(selected, _)| selected.len())
                .is_some_and(|(_, include)| *include)
        })?;
        for (item, include) in selection {
            if include && is_ppl(item) && !build.items.contains_key(item) {
                self.add_ppl(None, item);
            }
        }
        Ok(())
    }

    fn packed_output(&mut self, build: &lvproj::Build, reference: &str) {
        let path = self.output_path(build, 0);
        let path = path.filter(|path| is_ppl(path)).or_else(|| {
            let name = build
                .get("TgtF_targetfileName")
                .or_else(|| build.get("Destination[0].destName"))?;
            let directory = self.output_path(build, 0)?;
            let name = relative_filename(name)?;
            is_ppl(&name).then(|| directory.join(name))
        });
        self.add_ppl(path, reference);
    }

    fn selected_output(&mut self, build: &lvproj::Build, reference: &str, name: &str) {
        let mut files = BTreeSet::new();
        if let Some(name) = relative_filename(name) {
            for (index, field, _) in indexed(build, "Destination", None) {
                if field != "path" {
                    continue;
                }
                let Some(path) = self.output_path(build, index) else {
                    continue;
                };
                let file_destination = build.get(&format!("Destination[{index}].type"))
                    == Some("App")
                    || (index == 0
                        && matches!(build.kind.as_str(), "EXE" | lvproj::PACKED_LIBRARY_TYPE));
                let mut location = path.as_path();
                if file_destination {
                    let Some(parent) = path.parent() else {
                        continue;
                    };
                    location = parent;
                }
                let candidate = location.join(&name);
                if candidate.is_file()
                    && let Ok(candidate) = candidate.canonicalize()
                {
                    files.insert(candidate);
                }
            }
        }
        let label = format!("{reference}/{name}");
        match files.len() {
            0 => self.add_ppl(None, &label),
            1 => self.result.files.extend(files),
            _ => {
                self.result.issues.insert(format!(
                    "packed project library source {label:?} matches several build outputs"
                ));
            }
        }
    }

    fn output_path(&self, build: &lvproj::Build, index: usize) -> Option<PathBuf> {
        let key = format!("Destination[{index}].path");
        let path = build.get(&key)?;
        let relative_to = build
            .get(&format!("{key}.type"))
            .or_else(|| build.get(&format!("{key}Type")))
            .or_else(|| build.get("Bld_localDestDirType"));
        if relative_to == Some("relativeToAppDir") && Path::new(path).is_relative() {
            return None;
        }
        let project_name = self.project.file_stem()?.to_str()?;
        labview_path(
            self.project.parent()?,
            &path.replace("NI_AB_PROJECTNAME", project_name),
        )
    }

    fn add_ppl(&mut self, path: Option<PathBuf>, label: &str) {
        if let Some(path) = path
            .filter(|path| is_ppl(path) && path.is_file())
            .and_then(|path| path.canonicalize().ok())
        {
            self.result.files.insert(path);
        } else {
            self.result.issues.insert(format!(
                "packed project library source {label:?} is missing"
            ));
        }
    }
}

fn under(item: &str, folder: &str) -> bool {
    item == folder
        || item
            .strip_prefix(folder)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn is_ppl(path: impl AsRef<Path>) -> bool {
    path.as_ref()
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("lvlibp"))
}

fn labview_path(base: &Path, value: &str) -> Option<PathBuf> {
    let value = value.replace('\\', "/");
    if value.trim_start_matches('/').starts_with('<') {
        return None;
    }
    if cfg!(windows)
        && let Some(path) = value.strip_prefix('/')
        && path.as_bytes().get(1) == Some(&b'/')
        && path.as_bytes()[0].is_ascii_alphabetic()
    {
        return Some(PathBuf::from(format!("{}:{}", &path[..1], &path[1..])));
    }
    Some(base.join(value))
}

fn relative_filename(name: &str) -> Option<PathBuf> {
    let name = name.replace('\\', "/");
    let path = Path::new(&name);
    (!name.is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_))))
    .then(|| path.to_path_buf())
}

fn count(build: &lvproj::Build, key: &str) -> Result<Option<usize>> {
    build
        .get(key)
        .map(|value| value.parse().with_context(|| format!("invalid {key}")))
        .transpose()
}

fn indexed<'a, 'b>(
    build: &'a lvproj::Build,
    prefix: &'b str,
    count: Option<usize>,
) -> impl Iterator<Item = (usize, &'a str, &'a str)> + use<'a, 'b> {
    build.properties.iter().filter_map(move |(key, value)| {
        let (index, field) = key
            .strip_prefix(prefix)?
            .strip_prefix('[')?
            .split_once("].")?;
        let index = index.parse().ok()?;
        (count.is_none_or(|count| index < count) && !value.is_empty()).then_some((
            index,
            field,
            value.as_str(),
        ))
    })
}
