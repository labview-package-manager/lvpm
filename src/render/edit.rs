//! The wording of a named `lvpm install` and `lvpm uninstall` in a project (and
//! so of `add` and `remove`): what they did to `lvpm.toml`, and the errors they
//! end in. The lock and the sync they run say what they do
//! in their own words (`lock`, `sync`).

use super::lock::error_for as lock_error;
use super::sync::{error as sync_error, incomplete_body};
use super::{Renderer, Stream};
use lvpm_core::errors::{EditError, LockError, SyncError};
use lvpm_core::events::{Event, ManifestEdit};
use lvpm_core::manifest_edit::DependencyTable;
use lvpm_core::ops::env::shell_word;
use lvpm_core::project::{self, PackageArg};

pub(super) fn render(r: &mut Renderer, event: &Event) {
    match event {
        Event::ManifestEdited {
            edit,
            dev,
            group,
            created_table,
            dry_run,
        } => {
            let table = DependencyTable::from_flags(*dev, group.as_deref());
            r.line(Stream::Out, &edit_line(edit, &table, *dry_run));
            if *created_table {
                let verb = if *dry_run { "would create" } else { "created" };
                r.line(
                    Stream::Out,
                    &format!("{verb} [{}], a new group", table.key()),
                );
            }
        }
        Event::InstallAddsToProject {
            package,
            destination,
            dry_run,
        } => {
            let said = match dry_run {
                false => {
                    format!("adding {package} to lvpm.toml and syncing the project to lvpm.lock")
                }
                true => {
                    format!("would add {package} to lvpm.toml and sync the project to lvpm.lock")
                }
            };
            r.line(Stream::Out, &said);
            if let Some(flag) = destination {
                let gets = if *dry_run { "would get" } else { "gets" };
                let note = destination_note(flag, &format!("{gets} {package}"), "install");
                r.line(Stream::Out, &note);
            }
        }
        Event::UninstallRemovesFromProject {
            package,
            destination,
            dry_run,
        } => {
            let said = match dry_run {
                false => format!(
                    "removing {package} from lvpm.toml and syncing the project to lvpm.lock"
                ),
                true => format!(
                    "would remove {package} from lvpm.toml and sync the project to lvpm.lock"
                ),
            };
            r.line(Stream::Out, &said);
            if let Some(flag) = destination {
                let loses = if *dry_run { "would lose" } else { "loses" };
                let note = destination_note(flag, &format!("{loses} {package}"), "uninstall");
                r.line(Stream::Out, &note);
            }
        }
        Event::UninstalledStillListed {
            name,
            dev,
            group,
            destination,
        } => {
            let table = DependencyTable::from_flags(*dev, group.as_deref());
            r.line(
                Stream::Out,
                &format!(
                    "note: {name} is still in lvpm.toml; `{}` removes it for good (a sync \
                     would install it again)",
                    command_for("uninstall", name, &table, destination.as_deref())
                ),
            )
        }
        _ => unreachable!("routed here by Renderer::render: {event:?}"),
    }
}

/// What a named `install` or `uninstall` tells a user who gave a destination
/// flag: it only picks where the project syncs. `change` is what `lvpm.toml`
/// undergoes ("gets x", "would get x" on a dry run); `verb` is the command,
/// which `--no-project` makes direct.
fn destination_note(flag: &str, change: &str, verb: &str) -> String {
    format!(
        "note: in a project, {flag} picks where the project syncs; lvpm.toml {change}. \
         Pass --no-project to {verb} without changing the project."
    )
}

/// The flag that names `table` to `install` and `uninstall`: none for `[dependencies]`.
fn flag_for(table: &DependencyTable) -> String {
    match table {
        DependencyTable::Dependencies => String::new(),
        DependencyTable::DevDependencies => "--dev".to_string(),
        DependencyTable::Group(group) => format!("--group {}", shell_word(group)),
    }
}

/// The `lvpm <verb>` (`install` or `uninstall`) that puts `name` in `table` or
/// takes it out, on the target the user picked. Every word a shell may read is
/// quoted here, once: callers pass the words as they are.
fn command_for(
    verb: &str,
    name: &str,
    table: &DependencyTable,
    destination: Option<&str>,
) -> String {
    let name = shell_word(name);
    let command = match flag_for(table).as_str() {
        "" => format!("lvpm {verb} {name}"),
        flag => format!("lvpm {verb} {name} {flag}"),
    };
    on_the_target(command, destination)
}

/// `command_for`, with the source `index` pins the package to, before the target.
fn command_with_pin(
    verb: &str,
    package: &str,
    table: &DependencyTable,
    index: Option<&str>,
    destination: Option<&str>,
) -> String {
    let pin = index.map_or_else(String::new, |source| {
        format!(" --index {}", shell_word(source))
    });
    on_the_target(
        format!("{}{pin}", command_for(verb, package, table, None)),
        destination,
    )
}

/// `lvpm <verb> <package> --no-project` on `target`: the direct command that
/// leaves the project alone.
fn no_project_command(verb: &str, package: &str, target: Option<&str>) -> String {
    on_the_target(
        format!("lvpm {verb} {} --no-project", shell_word(package)),
        target,
    )
}

/// `command` with the destination flags the user gave, as typed with their
/// values: without them it could act on another target.
fn on_the_target(command: String, destination: Option<&str>) -> String {
    match destination {
        Some(flags) => format!("{command} {flags}"),
        None => command,
    }
}

/// What happened to one manifest entry, or on a dry run what would.
fn edit_line(edit: &ManifestEdit, table: &DependencyTable, dry_run: bool) -> String {
    let header = format!("[{}]", table.key());
    let table = match table {
        DependencyTable::Dependencies => String::new(),
        _ => format!(" {header}"),
    };
    // A package pinned to a source says where it comes from, last.
    let source_pin = (edit.source_pin()).map_or_else(String::new, |name| format!(" (from {name})"));
    match (edit, dry_run) {
        (
            ManifestEdit::Added {
                name, constraint, ..
            },
            false,
        ) => {
            format!("lvpm.toml: added {name} = \"{constraint}\" to {header}{source_pin}")
        }
        (
            ManifestEdit::Added {
                name, constraint, ..
            },
            true,
        ) => {
            format!("lvpm.toml would get: {name} = \"{constraint}\"{table}{source_pin}")
        }
        (ManifestEdit::Updated { name, from, to, .. }, false) => {
            format!("lvpm.toml was updated: {name} \"{from}\" -> \"{to}\"{table}{source_pin}")
        }
        (ManifestEdit::Updated { name, from, to, .. }, true) => {
            format!("lvpm.toml would be updated: {name} \"{from}\" -> \"{to}\"{table}{source_pin}")
        }
        (ManifestEdit::Removed { name, constraint }, false) => {
            format!("lvpm.toml: removed {name} = \"{constraint}\"{table}")
        }
        (ManifestEdit::Removed { name, constraint }, true) => {
            format!("lvpm.toml would lose: {name} = \"{constraint}\"{table}")
        }
    }
}

/// A named `lvpm install` or `lvpm uninstall` as the user typed it, for the
/// commands a hint suggests: they repeat all of it, so they act on the same
/// package, table, source pin and target.
pub struct Typed<'a> {
    /// `install` or `uninstall`.
    pub verb: &'a str,
    /// The package, as typed (`gamma@>=0.5`).
    pub package: &'a str,
    /// Where the command saved the package, or took it from.
    pub table: &'a DependencyTable,
    /// The source it pinned the package to (`--index`).
    pub index: Option<&'a str>,
    /// The destination flags given, as typed with their values, and the
    /// `--project` that picked the project.
    pub destination: Option<&'a str>,
    /// `destination` without `--project`: for a command with `--no-project`,
    /// which clap refuses beside it.
    pub target: Option<&'a str>,
    /// The directory `--project` named, if it was given.
    pub project: Option<&'a str>,
}

impl Typed<'_> {
    /// The command to run again, as `run {rerun} again` reads: the one typed,
    /// without its `--index` when `keep_pin` is false.
    fn rerun(&self, keep_pin: bool) -> String {
        let index = self.index.filter(|_| keep_pin);
        format!(
            "`{}`",
            command_with_pin(self.verb, self.package, self.table, index, self.destination)
        )
    }

    /// The package the user typed is `name`, whatever constraint came with it.
    fn is_the_package(&self, name: &str) -> bool {
        let typed =
            PackageArg::parse(self.package).map_or_else(|_| self.package.to_string(), |p| p.name);
        project::same_name(&typed, name)
    }

    /// The command that does the same to the package without the project, on
    /// the same target: it keeps no table, pin or `--project`.
    fn without_the_project(&self) -> String {
        no_project_command(self.verb, self.package, self.target)
    }

    /// `in <dir>, ` when `--project` alone picked the target: the command
    /// without it would act on the project the shell is in, so it says where to
    /// run it. Empty when another flag picked the target.
    fn in_the_project(&self) -> String {
        match (self.target, self.project) {
            (None, Some(dir)) => format!("in {}, ", shell_word(dir)),
            _ => String::new(),
        }
    }
}

/// What a named `lvpm install` and `lvpm uninstall` in a project print when they
/// fail: what happened, then the next thing to do.
pub fn error(error: &EditError, typed: &Typed) -> String {
    let Typed {
        table,
        index,
        destination,
        target,
        ..
    } = *typed;
    let rerun = typed.rerun(true);
    match error {
        EditError::Lock(LockError::NeedsNewerLabview {
            name, project_year, ..
        }) => {
            let package = format!("{name}@VERSION");
            // The package that needs the newer LabVIEW may be a dependency of
            // the one typed: its table and pin are not that one's.
            let command = match typed.is_the_package(name) {
                true => command_with_pin("install", &package, table, index, destination),
                false => on_the_target(
                    format!("lvpm install {}", shell_word(&package)),
                    destination,
                ),
            };
            format!(
                "{error}\n\
                 next: pick a version that runs on LabVIEW {project_year} with \
                 `{command}`, or raise labview in lvpm.toml"
            )
        }
        // lvpm.toml was put back, so the pin may only be on the command line:
        // the rerun leaves it out, for the pin is what the user is to change.
        EditError::Lock(LockError::PinnedNotListed { package, .. }) => format!(
            "{error}\n\
             next: pin {package} to a source that lists it, or list it in that source, \
             then run {} again",
            typed.rerun(false)
        ),
        EditError::Lock(cause) => lock_error(cause, &rerun),
        EditError::Sync(SyncError::Incomplete { done, left, cause }) => format!(
            "{}\nlvpm.toml and lvpm.lock were put back as they were\n\
             next: fix what is wrong and run {rerun} again; `lvpm sync` puts the destination \
             back in line with lvpm.lock",
            incomplete_body(done, left, cause)
        ),
        EditError::Sync(cause) => sync_error(cause),
        EditError::Cancelled => {
            format!("{error}; lvpm.toml and lvpm.lock are unchanged\nnext: run {rerun} again")
        }
        EditError::EmptyConstraint { name } => {
            let with =
                |package: &str| command_with_pin("install", package, table, index, destination);
            format!(
                "{error}\n\
                 quote it, e.g. \"{name}@>=0.5\": an unquoted > sends the output to a file\n\
                 the shell may have created a file named \"=0.5\" here; it can be deleted\n\
                 next: run `{}` with the constraint you mean, or `{}` for the newest version",
                with(&format!("{name}@>=0.5")),
                with(name)
            )
        }
        EditError::NotADependency {
            name,
            installed: true,
        } => format!(
            "{error}\n\
             next: {}`{}` uninstalls it from the target without changing the project",
            typed.in_the_project(),
            no_project_command("uninstall", name, target)
        ),
        // Temporary until #55 asks before a sync removes.
        EditError::ForeignInInstallation { .. } => format!(
            "{error}\n\
             next: {}run `{}` to {} it without syncing the project",
            typed.in_the_project(),
            typed.without_the_project(),
            typed.verb
        ),
        EditError::NotADependency {
            installed: false, ..
        } => format!("{error}\nnext: check the name"),
        EditError::NotInGroup { name, group } => format!(
            "{error}\n\
             next: name a package that [dependency-groups.{group}] lists, or add it with `{}`",
            command_for(
                "install",
                name,
                &DependencyTable::Group(group.clone()),
                destination
            )
        ),
        EditError::Required { roots, .. } => {
            // One command for each table that lists a root: `uninstall` takes
            // a package out of the table its flag names, and refuses another.
            let commands = (roots.iter())
                .flat_map(|(root, tables)| match tables.as_slice() {
                    [] => vec![command_for(
                        "uninstall",
                        root,
                        &DependencyTable::Dependencies,
                        destination,
                    )],
                    tables => (tables.iter())
                        .map(|table| command_for("uninstall", root, table, destination))
                        .collect(),
                })
                .collect();
            required(error, commands, None)
        }
        EditError::InOtherTable { name, found_in } => {
            let hints = other_table_hints("uninstall", name, found_in, destination);
            format!("{error}\n{hints}")
        }
        EditError::Other(cause) => format!("{cause:#}"),
    }
}

/// What a direct `uninstall` prints when other installed packages require its
/// package: the same words as a project's, and the way past them. `no_project`
/// is whether the user typed it, which the commands it suggests need then too.
pub fn direct_error(error: &EditError, destination: Option<&str>, no_project: bool) -> String {
    let EditError::Required { name, roots, .. } = error else {
        return error.to_string();
    };
    let flag = if no_project { " --no-project" } else { "" };
    let commands = (roots.iter())
        .map(|(root, _)| {
            on_the_target(
                format!("lvpm uninstall {}{flag}", shell_word(root)),
                destination,
            )
        })
        .collect();
    required(error, commands, Some(name))
}

/// A package other packages require cannot go: the error, then one `next:` line
/// that names the `commands` that take every root out first. `force` is the
/// package a direct uninstall may remove anyway.
fn required(error: &EditError, commands: Vec<String>, force: Option<&str>) -> String {
    let commands: Vec<String> = commands
        .iter()
        .map(|command| format!("`{command}`"))
        .collect();
    let list = match commands.split_last() {
        Some((last, first)) if !first.is_empty() => format!("{} and {last}", first.join(", ")),
        _ => commands.join(""),
    };
    let or_force = force.map_or_else(String::new, |name| {
        format!(", or pass --force to remove {name} anyway")
    });
    format!("{error}\nnext: run {list} first{or_force}")
}

/// The way out of naming a table that does not hold the package: one `next:`
/// for the one table that does, one line for each of several.
fn other_table_hints(
    verb: &str,
    name: &str,
    found_in: &[DependencyTable],
    destination: Option<&str>,
) -> String {
    match found_in {
        [only] => {
            let command = command_for(verb, name, only, destination);
            match flag_for(only).as_str() {
                "" => format!("next: `{command}` removes it from [dependencies]"),
                flag => format!("next: use {flag}: `{command}`"),
            }
        }
        several => (several.iter())
            .map(|table| {
                format!(
                    "next: `{}` removes it from [{}]",
                    command_for(verb, name, table, destination),
                    table.key()
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::renderer;
    use super::*;
    use lvpm_core::events::SyncStep;

    fn added() -> ManifestEdit {
        ManifestEdit::Added {
            name: "gamma".into(),
            constraint: ">=1.0".into(),
            index: None,
        }
    }

    #[test]
    fn an_edit_is_a_line_on_stdout_and_a_dry_run_says_it_is_only_a_plan() {
        let (mut renderer, out, err) = renderer();
        for (edit, dev, dry_run) in [
            (added(), false, false),
            (added(), true, false),
            (added(), false, true),
            (
                ManifestEdit::Updated {
                    name: "beta".into(),
                    from: ">=1.0".into(),
                    to: ">=0.5".into(),
                    index: None,
                },
                false,
                false,
            ),
            (
                ManifestEdit::Removed {
                    name: "gamma".into(),
                    constraint: ">=1.0".into(),
                },
                true,
                true,
            ),
        ] {
            renderer.render(&Event::ManifestEdited {
                edit,
                dev,
                group: None,
                created_table: false,
                dry_run,
            });
        }
        assert_eq!(
            out.text(),
            "lvpm.toml: added gamma = \">=1.0\" to [dependencies]\n\
             lvpm.toml: added gamma = \">=1.0\" to [dev-dependencies]\n\
             lvpm.toml would get: gamma = \">=1.0\"\n\
             lvpm.toml was updated: beta \">=1.0\" -> \">=0.5\"\n\
             lvpm.toml would lose: gamma = \">=1.0\" [dev-dependencies]\n"
        );
        assert_eq!(err.text(), "");
    }

    #[test]
    fn a_package_pinned_to_a_source_says_where_it_comes_from_at_the_end_of_the_line() {
        let (mut renderer, out, _) = renderer();
        let pinned = |edit: ManifestEdit, dry_run: bool| Event::ManifestEdited {
            edit,
            dev: false,
            group: Some("ci".into()),
            created_table: false,
            dry_run,
        };
        let added = ManifestEdit::Added {
            name: "gamma".into(),
            constraint: ">=1.0".into(),
            index: Some("second".into()),
        };
        let updated = ManifestEdit::Updated {
            name: "gamma".into(),
            from: "*".into(),
            to: ">=1.0".into(),
            index: Some("second".into()),
        };
        for (edit, dry_run) in [
            (added.clone(), false),
            (added, true),
            (updated.clone(), false),
            (updated, true),
        ] {
            renderer.render(&pinned(edit, dry_run));
        }
        assert_eq!(
            out.text(),
            "lvpm.toml: added gamma = \">=1.0\" to [dependency-groups.ci] (from second)\n\
             lvpm.toml would get: gamma = \">=1.0\" [dependency-groups.ci] (from second)\n\
             lvpm.toml was updated: gamma \"*\" -> \">=1.0\" [dependency-groups.ci] (from second)\n\
             lvpm.toml would be updated: gamma \"*\" -> \">=1.0\" [dependency-groups.ci] (from second)\n"
        );
    }

    #[test]
    fn the_json_of_an_edit_has_an_index_only_when_the_package_is_pinned() {
        let json = |index: Option<&str>| {
            serde_json::to_value(ManifestEdit::Added {
                name: "gamma".into(),
                constraint: ">=1.0".into(),
                index: index.map(str::to_string),
            })
            .unwrap()
        };
        assert_eq!(
            json(None),
            serde_json::json!({"edit": "added", "name": "gamma", "constraint": ">=1.0"})
        );
        assert_eq!(
            json(Some("second")),
            serde_json::json!({"edit": "added", "name": "gamma", "constraint": ">=1.0", "index": "second"})
        );
    }

    #[test]
    fn a_plain_named_install_in_a_project_says_it_is_an_add_and_nothing_of_an_opt_out() {
        let (mut renderer, out, _) = renderer();
        renderer.render(&Event::InstallAddsToProject {
            package: "gamma@1.0".into(),
            destination: None,
            dry_run: false,
        });
        assert_eq!(
            out.text(),
            "adding gamma@1.0 to lvpm.toml and syncing the project to lvpm.lock\n"
        );
        assert!(!out.text().contains("--no-project"), "{}", out.text());
    }

    #[test]
    fn a_named_install_given_a_destination_flag_says_it_only_picks_where_the_project_syncs() {
        let (mut renderer, out, _) = renderer();
        renderer.render(&Event::InstallAddsToProject {
            package: "gamma".into(),
            destination: Some("--prefix".into()),
            dry_run: false,
        });
        assert_eq!(
            out.text(),
            "adding gamma to lvpm.toml and syncing the project to lvpm.lock\n\
             note: in a project, --prefix picks where the project syncs; lvpm.toml gets gamma. \
             Pass --no-project to install without changing the project.\n"
        );
    }

    #[test]
    fn a_named_uninstall_in_a_project_says_it_edits_the_project_and_a_destination_flag_only_picks_where_it_syncs()
     {
        let (mut renderer, out, _) = renderer();
        for destination in [None, Some("--prefix".to_string())] {
            renderer.render(&Event::UninstallRemovesFromProject {
                package: "gamma".into(),
                destination,
                dry_run: false,
            });
        }
        assert_eq!(
            out.text(),
            "removing gamma from lvpm.toml and syncing the project to lvpm.lock\n\
             removing gamma from lvpm.toml and syncing the project to lvpm.lock\n\
             note: in a project, --prefix picks where the project syncs; lvpm.toml loses gamma. \
             Pass --no-project to uninstall without changing the project.\n"
        );
    }

    #[test]
    fn a_dry_run_of_an_install_or_uninstall_in_a_project_says_what_it_would_do_not_what_it_does() {
        let (mut renderer, out, _) = renderer();
        renderer.render(&Event::InstallAddsToProject {
            package: "gamma".into(),
            destination: None,
            dry_run: true,
        });
        renderer.render(&Event::UninstallRemovesFromProject {
            package: "gamma".into(),
            destination: None,
            dry_run: true,
        });
        assert_eq!(
            out.text(),
            "would add gamma to lvpm.toml and sync the project to lvpm.lock\n\
             would remove gamma from lvpm.toml and sync the project to lvpm.lock\n"
        );
    }

    #[test]
    fn the_json_of_the_project_events_has_dry_run_and_destination_only_when_they_are_set() {
        let json = |event: Event| serde_json::to_value(event).unwrap();
        assert_eq!(
            json(Event::UninstallRemovesFromProject {
                package: "gamma".into(),
                destination: None,
                dry_run: false,
            }),
            serde_json::json!({"event": "uninstall_removes_from_project", "data": {"package": "gamma"}})
        );
        assert_eq!(
            json(Event::InstallAddsToProject {
                package: "gamma".into(),
                destination: Some("--prefix".into()),
                dry_run: true,
            }),
            serde_json::json!({"event": "install_adds_to_project", "data": {
                "package": "gamma", "destination": "--prefix", "dry_run": true
            }})
        );
    }

    #[test]
    fn a_name_that_is_not_a_dependency_says_to_uninstall_it_directly_only_when_the_target_holds_it()
    {
        let said = |installed, destination| {
            written_on(
                &EditError::NotADependency {
                    name: "gamma".into(),
                    installed,
                },
                destination,
            )
        };
        assert_eq!(
            said(true, None),
            "gamma is not a dependency of this project\n\
             next: `lvpm uninstall gamma --no-project` uninstalls it from the target without \
             changing the project"
        );
        // The target it was asked about is the one to uninstall from.
        for flags in ["--prefix dest", "--global", "--labview-version 2026"] {
            assert_eq!(
                said(true, Some(flags)),
                format!(
                    "gamma is not a dependency of this project\n\
                     next: `lvpm uninstall gamma --no-project {flags}` uninstalls it from the \
                     target without changing the project"
                )
            );
        }
        assert_eq!(
            said(false, Some("--prefix dest")),
            "gamma is not a dependency of this project\nnext: check the name"
        );
    }

    #[test]
    fn an_uninstalled_package_that_the_manifest_still_lists_says_how_to_remove_it_for_good() {
        let (mut renderer, out, err) = renderer();
        for (dev, destination) in [
            (false, None),
            (true, None),
            (true, Some("--prefix dest".to_string())),
            (false, Some("--global".to_string())),
        ] {
            renderer.render(&Event::UninstalledStillListed {
                name: "caraya".into(),
                dev,
                group: None,
                destination,
            });
        }
        assert_eq!(
            out.text(),
            "note: caraya is still in lvpm.toml; `lvpm uninstall caraya` removes it for good (a sync \
             would install it again)\n\
             note: caraya is still in lvpm.toml; `lvpm uninstall caraya --dev` removes it for good \
             (a sync would install it again)\n\
             note: caraya is still in lvpm.toml; `lvpm uninstall caraya --dev --prefix dest` removes \
             it for good (a sync would install it again)\n\
             note: caraya is still in lvpm.toml; `lvpm uninstall caraya --global` removes it for \
             good (a sync would install it again)\n"
        );
        assert_eq!(err.text(), "");
    }

    fn group(name: &str) -> DependencyTable {
        DependencyTable::Group(name.into())
    }

    #[test]
    fn an_edit_of_a_group_names_its_table_and_a_new_one_says_it_was_made_on_a_line_of_its_own() {
        let (mut renderer, out, err) = renderer();
        for (created_table, dry_run) in [(true, false), (false, false), (true, true)] {
            renderer.render(&Event::ManifestEdited {
                edit: added(),
                dev: false,
                group: Some("ci".into()),
                created_table,
                dry_run,
            });
        }
        renderer.render(&Event::ManifestEdited {
            edit: ManifestEdit::Removed {
                name: "gamma".into(),
                constraint: ">=1.0".into(),
            },
            dev: false,
            group: Some("ci".into()),
            created_table: false,
            dry_run: false,
        });
        assert_eq!(
            out.text(),
            "lvpm.toml: added gamma = \">=1.0\" to [dependency-groups.ci]\n\
             created [dependency-groups.ci], a new group\n\
             lvpm.toml: added gamma = \">=1.0\" to [dependency-groups.ci]\n\
             lvpm.toml would get: gamma = \">=1.0\" [dependency-groups.ci]\n\
             would create [dependency-groups.ci], a new group\n\
             lvpm.toml: removed gamma = \">=1.0\" [dependency-groups.ci]\n"
        );
        assert_eq!(err.text(), "");
    }

    #[test]
    fn an_uninstalled_package_listed_in_a_group_says_to_remove_it_with_that_group() {
        let (mut renderer, out, _) = renderer();
        renderer.render(&Event::UninstalledStillListed {
            name: "ci_tools".into(),
            dev: false,
            group: Some("ci".into()),
            destination: None,
        });
        assert_eq!(
            out.text(),
            "note: ci_tools is still in lvpm.toml; `lvpm uninstall ci_tools --group ci` removes it \
             for good (a sync would install it again)\n"
        );
    }

    #[test]
    fn a_name_in_a_group_is_told_to_use_that_group_and_in_several_tables_gets_a_line_for_each() {
        let in_group = written(&EditError::InOtherTable {
            name: "beta".into(),
            found_in: vec![group("ci")],
        });
        assert_eq!(
            in_group,
            "beta is in [dependency-groups.ci]\nnext: use --group ci: `lvpm uninstall beta --group ci`"
        );
        let in_several = written(&EditError::InOtherTable {
            name: "beta".into(),
            found_in: vec![
                DependencyTable::Dependencies,
                DependencyTable::DevDependencies,
                group("ci"),
            ],
        });
        assert_eq!(
            in_several,
            "beta is in [dependencies], [dev-dependencies] and [dependency-groups.ci]\n\
             next: `lvpm uninstall beta` removes it from [dependencies]\n\
             next: `lvpm uninstall beta --dev` removes it from [dev-dependencies]\n\
             next: `lvpm uninstall beta --group ci` removes it from [dependency-groups.ci]"
        );
    }

    #[test]
    fn a_name_that_is_not_in_the_group_says_which_group_and_how_to_add_it() {
        let text = written(&EditError::NotInGroup {
            name: "beta".into(),
            group: "ci".into(),
        });
        assert_eq!(
            text,
            "beta is not in [dependency-groups.ci]\n\
             next: name a package that [dependency-groups.ci] lists, or add it with \
             `lvpm install beta --group ci`"
        );
    }

    fn written_as(
        error: &EditError,
        verb: &str,
        package: &str,
        table: &DependencyTable,
        index: Option<&str>,
        destination: Option<&str>,
    ) -> String {
        let typed = Typed {
            verb,
            package,
            table,
            index,
            destination,
            target: destination,
            project: None,
        };
        super::error(error, &typed)
    }

    /// What `lvpm install gamma` printed.
    fn written(error: &EditError) -> String {
        written_as(
            error,
            "install",
            "gamma",
            &DependencyTable::Dependencies,
            None,
            None,
        )
    }

    /// What `lvpm uninstall alpha`, given `destination`, printed.
    fn written_on(error: &EditError, destination: Option<&str>) -> String {
        let table = DependencyTable::Dependencies;
        written_as(error, "uninstall", "alpha", &table, None, destination)
    }

    #[test]
    fn the_newer_labview_hint_is_an_install_with_its_table_pin_and_destination() {
        let error = EditError::Lock(LockError::NeedsNewerLabview {
            name: "omega".into(),
            version: "1.0".into(),
            needs_year: 2026,
            project_year: 2024,
        });
        let next = |table: &DependencyTable, index, destination| {
            written_as(&error, "install", "omega", table, index, destination)
                .lines()
                .last()
                .unwrap()
                .to_string()
        };
        assert_eq!(
            next(&DependencyTable::Dependencies, None, None),
            "next: pick a version that runs on LabVIEW 2024 with `lvpm install omega@VERSION`, \
             or raise labview in lvpm.toml"
        );
        assert_eq!(
            next(
                &DependencyTable::DevDependencies,
                Some("second"),
                Some("--prefix dest")
            ),
            "next: pick a version that runs on LabVIEW 2024 with \
             `lvpm install omega@VERSION --dev --index second --prefix dest`, \
             or raise labview in lvpm.toml"
        );
        assert_eq!(
            next(&group("ci"), None, Some("--global")),
            "next: pick a version that runs on LabVIEW 2024 with \
             `lvpm install omega@VERSION --group ci --global`, or raise labview in lvpm.toml"
        );
    }

    #[test]
    fn an_empty_constraint_says_to_quote_it_and_what_the_shell_may_have_left_behind() {
        let text = written(&EditError::EmptyConstraint {
            name: "gamma".into(),
        });
        for said in [
            "a constraint is expected after \"@\" in \"gamma@\"",
            "quote it, e.g. \"gamma@>=0.5\"",
            "the shell may have created a file named \"=0.5\"",
            "can be deleted",
        ] {
            assert!(text.contains(said), "{said:?} is not in:\n{text}");
        }
    }

    #[test]
    fn an_empty_constraint_hint_keeps_the_table_pin_and_destination_of_the_command() {
        let error = EditError::EmptyConstraint {
            name: "gamma".into(),
        };
        let said = written_as(
            &error,
            "install",
            "gamma@",
            &DependencyTable::DevDependencies,
            Some("second"),
            Some("--prefix dest"),
        );
        assert_eq!(
            said.lines().last().unwrap(),
            "next: run `lvpm install 'gamma@>=0.5' --dev --index second --prefix dest` with the \
             constraint you mean, or `lvpm install gamma --dev --index second --prefix dest` \
             for the newest version"
        );
    }

    #[test]
    fn a_name_in_the_other_table_is_told_which_flag_to_change() {
        let in_dev = written(&EditError::InOtherTable {
            name: "caraya".into(),
            found_in: vec![DependencyTable::DevDependencies],
        });
        assert_eq!(
            in_dev,
            "caraya is in [dev-dependencies]\nnext: use --dev: `lvpm uninstall caraya --dev`"
        );
        let in_normal = written(&EditError::InOtherTable {
            name: "beta".into(),
            found_in: vec![DependencyTable::Dependencies],
        });
        assert_eq!(
            in_normal,
            "beta is in [dependencies]\nnext: `lvpm uninstall beta` removes it from [dependencies]"
        );
    }

    #[test]
    fn the_hint_for_another_table_is_always_an_uninstall_on_the_target_the_user_picked() {
        let in_dev = EditError::InOtherTable {
            name: "caraya".into(),
            found_in: vec![DependencyTable::DevDependencies],
        };
        let said = |destination| written_on(&in_dev, destination);
        assert_eq!(
            said(Some("--prefix dest")),
            "caraya is in [dev-dependencies]\n\
             next: use --dev: `lvpm uninstall caraya --dev --prefix dest`"
        );
        assert_eq!(
            said(Some("--labview-version 2026")),
            "caraya is in [dev-dependencies]\n\
             next: use --dev: `lvpm uninstall caraya --dev --labview-version 2026`"
        );
        let in_normal = EditError::InOtherTable {
            name: "beta".into(),
            found_in: vec![DependencyTable::Dependencies, group("ci")],
        };
        assert_eq!(
            written_on(&in_normal, Some("--global")),
            "beta is in [dependencies] and [dependency-groups.ci]\n\
             next: `lvpm uninstall beta --global` removes it from [dependencies]\n\
             next: `lvpm uninstall beta --group ci --global` removes it from [dependency-groups.ci]"
        );
    }

    #[test]
    fn a_sync_that_stopped_says_both_files_were_put_back_and_how_to_bring_the_destination_in_line()
    {
        let text = written(&EditError::Sync(SyncError::Incomplete {
            done: Vec::new(),
            left: vec![SyncStep::Install {
                name: "gamma".into(),
                version: "1.0".into(),
            }],
            cause: "disk full".into(),
        }));
        assert_eq!(
            text,
            "sync stopped before it was done: disk full\n\
             not done:\n  install gamma 1.0\n\
             lvpm.toml and lvpm.lock were put back as they were\n\
             next: fix what is wrong and run `lvpm install gamma` again; `lvpm sync` puts the destination \
             back in line with lvpm.lock"
        );
    }

    #[test]
    fn every_error_but_a_foreign_one_ends_with_the_next_thing_to_run_and_a_lock_error_reruns_the_command()
     {
        for error in [
            EditError::EmptyConstraint { name: "g".into() },
            EditError::NotADependency {
                name: "g".into(),
                installed: false,
            },
            EditError::NotADependency {
                name: "g".into(),
                installed: true,
            },
            EditError::NotInGroup {
                name: "g".into(),
                group: "ci".into(),
            },
            EditError::InOtherTable {
                name: "g".into(),
                found_in: vec![DependencyTable::Dependencies, group("ci")],
            },
            required_by_others(&["app"]),
            foreign(),
            EditError::Lock(LockError::NoLabview),
            EditError::Lock(LockError::NeedsNewerLabview {
                name: "omega".into(),
                version: "1.0".into(),
                needs_year: 2026,
                project_year: 2024,
            }),
            EditError::Sync(SyncError::Incomplete {
                done: Vec::new(),
                left: Vec::new(),
                cause: "x".into(),
            }),
        ] {
            let text = written(&error);
            let last = text.lines().last().unwrap();
            assert!(last.starts_with("next: "), "{text}");
        }
        let text = written(&EditError::Lock(LockError::PinnedNotListed {
            package: "alpha".into(),
            source_name: "second".into(),
            feed: "http://second.example/index.vipr".into(),
        }));
        assert!(
            text.ends_with(
                "\nnext: pin alpha to a source that lists it, or list it in that source, \
                 then run `lvpm install gamma` again"
            ),
            "{text}"
        );
        assert!(!text.contains("index key"), "{text}");
        let text = written(&EditError::Lock(LockError::NoLabview));
        assert!(
            text.ends_with("then run `lvpm install gamma` again"),
            "{text}"
        );
        assert!(!text.contains("`lvpm lock`"), "{text}");
    }

    #[test]
    fn a_bare_rerun_would_be_a_sync_so_every_hint_repeats_the_whole_command_as_typed() {
        let pinned = EditError::Lock(LockError::PinnedNotListed {
            package: "alpha".into(),
            source_name: "second".into(),
            feed: "http://second.example/index.vipr".into(),
        });
        let rerun = |error: &EditError| {
            let typed = Typed {
                verb: "install",
                package: "alpha@>=1.0",
                table: &group("ci"),
                index: Some("second"),
                destination: Some("--prefix dest"),
                target: Some("--prefix dest"),
                project: None,
            };
            super::error(error, &typed)
                .lines()
                .last()
                .unwrap()
                .to_string()
        };
        // The pin is what the user is told to change, so it is not repeated.
        assert_eq!(
            rerun(&pinned),
            "next: pin alpha to a source that lists it, or list it in that source, \
             then run `lvpm install 'alpha@>=1.0' --group ci --prefix dest` again"
        );
        // Anything else repeats it with its pin.
        let expected = "`lvpm install 'alpha@>=1.0' --group ci --index second --prefix dest`";
        for error in [
            EditError::Cancelled,
            EditError::Lock(LockError::NoLabview),
            EditError::Sync(SyncError::Incomplete {
                done: Vec::new(),
                left: Vec::new(),
                cause: "x".into(),
            }),
        ] {
            let last = rerun(&error);
            assert!(last.contains(expected), "{last}");
        }
    }

    #[test]
    fn an_uninstall_hint_to_rerun_names_the_package_table_and_destination_and_no_pin() {
        let text = written_as(
            &EditError::Cancelled,
            "uninstall",
            "caraya",
            &DependencyTable::DevDependencies,
            None,
            Some("--prefix dest"),
        );
        assert_eq!(
            text,
            "cancelled; lvpm.toml and lvpm.lock are unchanged\n\
             next: run `lvpm uninstall caraya --dev --prefix dest` again"
        );
    }

    #[test]
    fn a_cancelled_edit_says_cancelled_and_that_both_files_are_unchanged() {
        let text = written(&EditError::Cancelled);
        assert_eq!(
            text,
            "cancelled; lvpm.toml and lvpm.lock are unchanged\nnext: run `lvpm install gamma` again"
        );
    }

    fn required_by_others(roots: &[&str]) -> EditError {
        EditError::Required {
            name: "lib".into(),
            required_by: vec!["mid".into(), "app2".into()],
            roots: roots
                .iter()
                .map(|root| (root.to_string(), Vec::new()))
                .collect(),
        }
    }

    #[test]
    fn a_package_others_require_says_who_and_one_next_line_names_every_root_to_remove_first() {
        assert_eq!(
            written(&required_by_others(&["app"])),
            "lib is required by installed packages: mid, app2\n\
             next: run `lvpm uninstall app` first"
        );
        assert_eq!(
            written(&required_by_others(&["app", "app2"])),
            "lib is required by installed packages: mid, app2\n\
             next: run `lvpm uninstall app` and `lvpm uninstall app2` first"
        );
        assert_eq!(
            written_on(&required_by_others(&["a", "b", "c"]), Some("--prefix dest")),
            "lib is required by installed packages: mid, app2\n\
             next: run `lvpm uninstall a --prefix dest`, `lvpm uninstall b --prefix dest` \
             and `lvpm uninstall c --prefix dest` first"
        );
    }

    #[test]
    fn a_root_is_named_with_the_flag_of_each_table_that_lists_it() {
        let error = EditError::Required {
            name: "lib".into(),
            required_by: vec!["tool".into(), "tests".into(), "app".into()],
            roots: vec![
                ("app".into(), vec![DependencyTable::Dependencies]),
                ("tool".into(), vec![DependencyTable::DevDependencies]),
                ("tests".into(), vec![DependencyTable::Group("ci".into())]),
                (
                    "both".into(),
                    vec![
                        DependencyTable::DevDependencies,
                        DependencyTable::Group("ci".into()),
                    ],
                ),
            ],
        };
        assert_eq!(
            written_on(&error, Some("--prefix dest")),
            "lib is required by installed packages: tool, tests, app\n\
             next: run `lvpm uninstall app --prefix dest`, \
             `lvpm uninstall tool --dev --prefix dest`, \
             `lvpm uninstall tests --group ci --prefix dest`, \
             `lvpm uninstall both --dev --prefix dest` \
             and `lvpm uninstall both --group ci --prefix dest` first"
        );
    }

    #[test]
    fn a_direct_uninstall_says_it_the_same_way_and_how_to_force_it() {
        let error = required_by_others(&["app"]);
        assert_eq!(
            super::direct_error(&error, Some("--prefix dest"), true),
            "lib is required by installed packages: mid, app2\n\
             next: run `lvpm uninstall app --no-project --prefix dest` first, \
             or pass --force to remove lib anyway"
        );
        assert_eq!(
            super::direct_error(&error, None, false),
            "lib is required by installed packages: mid, app2\n\
             next: run `lvpm uninstall app` first, or pass --force to remove lib anyway"
        );
    }

    #[test]
    fn a_dry_run_note_says_what_lvpm_toml_would_get_or_lose() {
        let (mut renderer, out, _) = renderer();
        renderer.render(&Event::InstallAddsToProject {
            package: "gamma".into(),
            destination: Some("--prefix".into()),
            dry_run: true,
        });
        renderer.render(&Event::UninstallRemovesFromProject {
            package: "gamma".into(),
            destination: Some("--prefix".into()),
            dry_run: true,
        });
        let text = out.text();
        assert!(text.contains("lvpm.toml would get gamma."), "{text}");
        assert!(text.contains("lvpm.toml would lose gamma."), "{text}");
        assert!(!text.contains("gets") && !text.contains("loses"), "{text}");
    }

    #[test]
    fn a_name_that_is_not_in_the_group_carries_the_destination_in_the_command_it_suggests() {
        let text = written_on(
            &EditError::NotInGroup {
                name: "beta".into(),
                group: "ci".into(),
            },
            Some("--prefix dest"),
        );
        assert!(
            text.ends_with("or add it with `lvpm install beta --group ci --prefix dest`"),
            "{text}"
        );
    }

    /// What `lvpm uninstall alpha --project P --prefix dest` printed.
    fn written_in_project(error: &EditError, verb: &str, package: &str) -> String {
        let typed = Typed {
            verb,
            package,
            table: &DependencyTable::Dependencies,
            index: None,
            destination: Some("--prefix dest --project P"),
            target: Some("--prefix dest"),
            project: None,
        };
        super::error(error, &typed)
    }

    #[test]
    fn the_hint_to_uninstall_it_directly_leaves_out_project_which_no_project_refuses() {
        let said = written_in_project(
            &EditError::NotADependency {
                name: "gamma".into(),
                installed: true,
            },
            "uninstall",
            "gamma",
        );
        assert_eq!(
            said,
            "gamma is not a dependency of this project\n\
             next: `lvpm uninstall gamma --no-project --prefix dest` uninstalls it from the \
             target without changing the project"
        );
    }

    #[test]
    fn the_newer_labview_hint_keeps_the_table_and_pin_only_for_the_package_that_was_typed() {
        let error = |name: &str| {
            EditError::Lock(LockError::NeedsNewerLabview {
                name: name.into(),
                version: "1.0".into(),
                needs_year: 2026,
                project_year: 2024,
            })
        };
        let typed = |package, error: &EditError| {
            let typed = Typed {
                verb: "install",
                package,
                table: &DependencyTable::DevDependencies,
                index: Some("second"),
                destination: Some("--prefix dest"),
                target: Some("--prefix dest"),
                project: None,
            };
            super::error(error, &typed)
                .lines()
                .last()
                .unwrap()
                .to_string()
        };
        // Typed with a constraint and in another case, it is still the package.
        assert!(
            typed("Omega@>=1.0", &error("omega"))
                .contains("`lvpm install omega@VERSION --dev --index second --prefix dest`")
        );
        // A dependency of what was typed is not saved where that was.
        let said = typed("omega", &error("lib"));
        assert!(
            said.contains("`lvpm install lib@VERSION --prefix dest`"),
            "{said}"
        );
        assert!(
            !said.contains("--dev") && !said.contains("--index"),
            "{said}"
        );
    }

    #[test]
    fn a_package_a_shell_reads_is_quoted_by_the_rule_the_destination_flags_use() {
        // The flags' own quoting is tested beside `shell_word`, in `OpEnv`.
        for (package, quoted) in [("a&b", "'a&b'"), ("a;b", "'a;b'"), ("gamma", "gamma")] {
            let said = written_as(
                &EditError::Cancelled,
                "install",
                package,
                &DependencyTable::Dependencies,
                None,
                Some("--prefix dest"),
            );
            assert!(
                said.ends_with(&format!("run `lvpm install {quoted} --prefix dest` again")),
                "{said}"
            );
        }
    }

    fn foreign() -> EditError {
        EditError::ForeignInInstallation {
            packages: vec![("delta".into(), "1.0".into()), ("eps".into(), "2.1".into())],
        }
    }

    #[test]
    fn a_sync_that_would_uninstall_what_you_did_not_name_says_so_and_the_way_around() {
        assert_eq!(
            written_in_project(&foreign(), "install", "gamma@>=1.0"),
            "the sync would uninstall packages from LabVIEW that you did not name: \
             delta 1.0, eps 2.1\n\
             next: run `lvpm install 'gamma@>=1.0' --no-project --prefix dest` to install it \
             without syncing the project"
        );
        assert!(
            written_in_project(&foreign(), "uninstall", "gamma").ends_with(
                "next: run `lvpm uninstall gamma --no-project --prefix dest` to uninstall it \
                 without syncing the project"
            )
        );
    }

    /// What `lvpm uninstall gamma --project <dir>` printed: no other flag picked
    /// the target.
    fn written_with_project_only(error: &EditError, dir: &str) -> String {
        let typed = Typed {
            verb: "uninstall",
            package: "gamma",
            table: &DependencyTable::Dependencies,
            index: None,
            destination: Some("--project 'my proj'"),
            target: None,
            project: Some(dir),
        };
        super::error(error, &typed)
    }

    #[test]
    fn a_hint_that_drops_the_project_says_which_directory_when_project_alone_picked_the_target() {
        let not_a_dependency = EditError::NotADependency {
            name: "gamma".into(),
            installed: true,
        };
        assert_eq!(
            written_with_project_only(&not_a_dependency, "my proj"),
            "gamma is not a dependency of this project\n\
             next: in 'my proj', `lvpm uninstall gamma --no-project` uninstalls it from the \
             target without changing the project"
        );
        assert_eq!(
            written_with_project_only(&foreign(), "p")
                .lines()
                .last()
                .unwrap(),
            "next: in p, run `lvpm uninstall gamma --no-project` to uninstall it without \
             syncing the project"
        );
        // Another destination flag is the target: nothing silently changes.
        let typed = Typed {
            verb: "uninstall",
            package: "gamma",
            table: &DependencyTable::Dependencies,
            index: None,
            destination: Some("--prefix dest --project p"),
            target: Some("--prefix dest"),
            project: Some("p"),
        };
        assert!(
            super::error(&not_a_dependency, &typed)
                .contains("next: `lvpm uninstall gamma --no-project --prefix dest` uninstalls")
        );
    }

    #[test]
    fn every_command_a_hint_names_quotes_each_word_a_shell_reads_once() {
        let table = group("g h");
        let typed = |package, index| Typed {
            verb: "install",
            package,
            table: &table,
            index,
            destination: Some("--prefix 'a b'"),
            target: Some("--prefix 'a b'"),
            project: None,
        };
        let last = |error: &EditError, package, index| {
            super::error(error, &typed(package, index))
                .lines()
                .last()
                .unwrap()
                .to_string()
        };
        assert_eq!(
            last(&EditError::Cancelled, "gamma@*", Some("my src")),
            "next: run `lvpm install 'gamma@*' --group 'g h' --index 'my src' --prefix 'a b'` again"
        );
        assert_eq!(
            last(
                &EditError::NotInGroup {
                    name: "a b".into(),
                    group: "g h".into()
                },
                "x",
                None
            ),
            "next: name a package that [dependency-groups.g h] lists, or add it with \
             `lvpm install 'a b' --group 'g h' --prefix 'a b'`"
        );
        assert_eq!(
            last(
                &EditError::NotADependency {
                    name: "O'Brien".into(),
                    installed: true
                },
                "x",
                None
            ),
            "next: `lvpm uninstall \"O'Brien\" --no-project --prefix 'a b'` uninstalls it from \
             the target without changing the project"
        );
        assert_eq!(
            last(
                &EditError::EmptyConstraint { name: "a b".into() },
                "x",
                None
            ),
            "next: run `lvpm install 'a b@>=0.5' --group 'g h' --prefix 'a b'` with the \
             constraint you mean, or `lvpm install 'a b' --group 'g h' --prefix 'a b'` for the \
             newest version"
        );
        let required = EditError::Required {
            name: "lib".into(),
            required_by: vec!["a b".into()],
            roots: vec![("a b".into(), vec![group("g h")])],
        };
        assert_eq!(
            last(&required, "x", None),
            "next: run `lvpm uninstall 'a b' --group 'g h' --prefix 'a b'` first"
        );
        assert_eq!(
            super::direct_error(&required, Some("--prefix 'a b'"), true),
            "lib is required by installed packages: a b\n\
             next: run `lvpm uninstall 'a b' --no-project --prefix 'a b'` first, \
             or pass --force to remove lib anyway"
        );
        let (mut renderer, out, _) = renderer();
        renderer.render(&Event::UninstalledStillListed {
            name: "a b".into(),
            dev: false,
            group: Some("g h".into()),
            destination: Some("--prefix 'a b'".into()),
        });
        assert!(
            out.text()
                .contains("`lvpm uninstall 'a b' --group 'g h' --prefix 'a b'` removes it"),
            "{}",
            out.text()
        );
        let newer = EditError::Lock(LockError::NeedsNewerLabview {
            name: "a b".into(),
            version: "1.0".into(),
            needs_year: 2026,
            project_year: 2024,
        });
        assert!(last(&newer, "a b", Some("my src")).contains(
            "`lvpm install 'a b@VERSION' --group 'g h' --index 'my src' --prefix 'a b'`"
        ));
    }
}
