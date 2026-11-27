//! `add` and `remove`: change one entry of `lvpm.toml`, lock the result, and
//! sync the destination to it.
//!
//! The new manifest is text in memory until everything that can fail without
//! changing anything has passed: the lock is solved, every archive it holds is
//! in the cache and hashed, and the sync is planned and its archives fetched.
//! Only then are `lvpm.toml` and `lvpm.lock` written and the destination put
//! right, and if that fails, both files are put back byte for byte.

use super::env::{Destination, OpEnv};
use super::hook_policy::HookAllow;
use super::install::{InstallOptions, announce_target, finish};
use super::lock::{self as locking, Moving, announce_written, write_manifest_and_lock};
use super::session::InstallSession;
use super::snapshot::restoring_on_failure;
use super::sync::{
    Selection, SyncOptions, SyncPlan, announce_migration, apply_sync, fetch_locked, plan_sync,
    refuse_unknown_groups,
};
use crate::dependency::roots_above;
use crate::errors::{EditError, LockError, SyncError};
use crate::events::{Event, ManifestEdit, Phase, RemoveReason, SyncStep};
use crate::index::Index;
use crate::install;
use crate::lockfile::{self, Lockfile, constraint_text};
use crate::manifest_edit::{self, DependencyTable};
use crate::project::{self, Constraint, PackageArg, PackageArgError, Project};
use crate::solve;
use anyhow::{Context, anyhow};
use std::path::PathBuf;

/// Put `spec` (`name`, or `name@constraint`) in `table` of `lvpm.toml`, lock,
/// and sync the destination. Without a constraint the entry is a floor at the
/// version the new lock holds for it (`>=1.0`); with one, the constraint is
/// written as given. An entry that is already there has its constraint
/// replaced. With a `source_pin` the entry is `index = "<source_pin>"` too, so
/// the package comes only from that `[sources]` entry; without one an `index`
/// the entry has stays.
///
/// Both files are written only once the lock is made and every archive of the
/// sync is in hand. With `options.dry_run` nothing is written, and the manifest
/// line, the lock changes and the sync plan are reported instead.
pub fn add(
    env: &OpEnv,
    spec: &str,
    table: DependencyTable,
    source_pin: Option<&str>,
    options: &InstallOptions,
) -> Result<(), EditError> {
    add_and_sync(env, spec, table, source_pin, options, None)
}

/// [`add`] for a command that says what it is doing: `announce` is said once
/// the lock is made and not before, so a refusal (a package no source lists, a
/// conflict) never says it.
pub fn add_announcing(
    env: &OpEnv,
    spec: &str,
    table: DependencyTable,
    source_pin: Option<&str>,
    options: &InstallOptions,
    announce: Event,
) -> Result<(), EditError> {
    add_and_sync(env, spec, table, source_pin, options, Some(announce))
}

fn add_and_sync(
    env: &OpEnv,
    spec: &str,
    table: DependencyTable,
    source_pin: Option<&str>,
    options: &InstallOptions,
    announce: Option<Event>,
) -> Result<(), EditError> {
    let request = Request::parse(spec, source_pin)?;
    let prepared = Prepared::of(env)?;
    project::refuse_old_form_edit(&prepared.found.1, &table, &request.name)?;
    let dest = env.destination()?;
    announce_project(env, &prepared, &dest);

    // The index is made for the project as the edit will leave it, so that a
    // narrowed constraint on a GitHub dependency is judged as written; with no
    // constraint given, as `*`, which `floor` then narrows.
    let asked = request.constraint.as_deref().unwrap_or("*");
    let source_pin = request.source_pin.as_deref();
    let edit = prepared.set(&table, &request.name, asked, source_pin)?;
    let edited = (prepared.found.0.clone(), edit.project.clone());
    let moving = Moving::Nothing;
    let index = locking::load_index(env, Some(&edited), prepared.previous.as_ref(), &moving)?;
    let edit = match request.constraint {
        Some(_) => edit,
        None => {
            let floor = floor(env, &prepared, &index, &table, &request.name, source_pin)?;
            prepared.set(&table, &request.name, &floor, source_pin)?
        }
    };
    commit(env, prepared, edit, &index, dest, options, announce)
}

/// Take `name` out of `table` of `lvpm.toml`, lock, and sync the destination.
/// A package that another locked package requires is refused, naming the
/// packages to remove first, and nothing is changed.
///
/// Both files are written only once the lock is made and every archive of the
/// sync is in hand. With `options.dry_run` nothing is written, and the manifest
/// line, the lock changes and the sync plan are reported instead.
pub fn remove(
    env: &OpEnv,
    name: &str,
    table: DependencyTable,
    options: &InstallOptions,
) -> Result<(), EditError> {
    take_out_and_sync(env, name, table, options, None)
}

/// [`remove`] for a command that says what it is doing: `announce` is said once
/// the lock is made and nothing requires `name`, and not before, so a refusal
/// (not a dependency, wrong table, required by another package) never says it.
pub fn remove_announcing(
    env: &OpEnv,
    name: &str,
    table: DependencyTable,
    options: &InstallOptions,
    announce: Event,
) -> Result<(), EditError> {
    take_out_and_sync(env, name, table, options, Some(announce))
}

fn take_out_and_sync(
    env: &OpEnv,
    name: &str,
    table: DependencyTable,
    options: &InstallOptions,
    announce: Option<Event>,
) -> Result<(), EditError> {
    let prepared = Prepared::of(env)?;
    project::refuse_old_form_edit(&prepared.found.1, &table, name)?;
    if let DependencyTable::Group(group) = &table {
        refuse_unknown_groups(&prepared.found.1, std::slice::from_ref(group))?;
    }
    let edit = prepared.take_out(&table, name).map_err(|why| match why {
        // A package the manifest does not list but another one requires is
        // locked and installed all the same: that is what stops it.
        EditError::NotInGroup { ref name, .. } => (prepared.previous.as_ref())
            .and_then(|lock| required(lock, &prepared.found.1, name))
            .unwrap_or(why),
        EditError::NotADependency { name, .. } => prepared
            .previous
            .as_ref()
            .and_then(|lock| required(lock, &prepared.found.1, &name))
            .unwrap_or_else(|| {
                let installed = installed_in_the_target(env, &name);
                EditError::NotADependency { name, installed }
            }),
        other => other,
    })?;
    let dest = env.destination()?;
    announce_project(env, &prepared, &dest);

    let edited = (prepared.found.0.clone(), edit.project.clone());
    let index = locking::load_index(
        env,
        Some(&edited),
        prepared.previous.as_ref(),
        &Moving::Nothing,
    )?;
    commit(env, prepared, edit, &index, dest, options, announce)
}

/// Some table of `project` lists `name`.
fn listed(project: &Project, name: &str) -> bool {
    (project.tables()).any(|(_, entries)| {
        entries
            .iter()
            .any(|(entry, _)| project::same_name(entry, name))
    })
}

/// `name` cannot go while `lock` holds packages that require it: they are
/// named, and so are the ones the project lists that need it (the top ones and
/// any it lists in between), to go first and top-down, each with the tables of
/// `project` that list it: `uninstall` needs the flag.
fn required(lock: &Lockfile, project: &Project, name: &str) -> Option<EditError> {
    let required_by = lock.dependents_of(name);
    (!required_by.is_empty()).then(|| EditError::Required {
        name: name.to_string(),
        roots: roots_above(
            name,
            |package| lock.dependents_of(package),
            |package| listed(project, package),
        )
        .into_iter()
        .map(|root| {
            let tables = (project.tables())
                .filter(|(_, entries)| {
                    entries
                        .iter()
                        .any(|(entry, _)| project::same_name(entry, &root))
                })
                .map(|(table, _)| table)
                .collect();
            (root, tables)
        })
        .collect(),
        required_by,
    })
}

/// `name` is installed in the target, as far as it can be told: a target that
/// cannot be resolved or read holds nothing. It picks the target the way
/// [`OpEnv::destination`] does (`--prefix`, `--global` and `--labview-version`
/// through [`OpEnv::flagged_roots`], so `--global` alone settles as it does for
/// a write) but says nothing about it: this is a probe for a refusal that
/// changes nothing, so a venv is found, never "selected".
// shortcut: a headless venv-less project is not probed, so it reads as holding nothing; when that matters, make `destination` quiet.
fn installed_in_the_target(env: &OpEnv, name: &str) -> bool {
    let explicit = env.flags.prefix.is_some() || env.flags.global;
    let store = match (explicit, env.find_venv()) {
        (false, Ok(Some(venv))) => Ok(venv.store()),
        (true, _) => env.flagged_roots(),
        (false, Ok(None)) => env.roots(),
        (false, Err(why)) => Err(why),
    };
    store.is_ok_and(|store| {
        install::list_installed(&store)
            .is_ok_and(|all| all.iter().any(|m| project::same_name(&m.name, name)))
    })
}

/// What `add` was asked for: a package, the constraint if one was written, and
/// the `[sources]` name it is to come from if `--index` named one.
#[derive(Debug, PartialEq)]
struct Request {
    name: String,
    constraint: Option<String>,
    source_pin: Option<String>,
}

impl Request {
    fn parse(spec: &str, source_pin: Option<&str>) -> Result<Request, EditError> {
        let PackageArg { name, constraint } = PackageArg::parse(spec).map_err(|why| match why {
            PackageArgError::EmptyConstraint { name } => EditError::EmptyConstraint { name },
            other => anyhow!(other).into(),
        })?;
        if let Some(written) = &constraint {
            Constraint::parse(written)?;
        }
        Ok(Request {
            name,
            constraint,
            source_pin: source_pin.map(str::to_string),
        })
    }
}

/// The project an edit is made to, as it is on disk before anything changes.
struct Prepared {
    /// The manifest's path, and what it says.
    found: (PathBuf, Project),
    /// The manifest's text, which an edit changes in memory.
    text: String,
    lock_path: PathBuf,
    /// The lock that is there, if it can be read.
    previous: Option<Lockfile>,
    /// There is no lock at all, so this is the project's first.
    lock_missing: bool,
}

impl Prepared {
    fn of(env: &OpEnv) -> Result<Prepared, EditError> {
        let found = locking::find_manifest(env)?;
        let text = std::fs::read_to_string(&found.0)
            .with_context(|| format!("reading manifest {}", found.0.display()))?;
        let lock_path = found.0.with_file_name(lockfile::FILE_NAME);
        let (previous, lock_missing) = match locking::read_lock(&lock_path) {
            Ok(lock) => (Some(lock), false),
            Err(LockError::NoLock { .. }) => (None, true),
            Err(_) => (None, false),
        };
        Ok(Prepared {
            found,
            text,
            lock_path,
            previous,
            lock_missing,
        })
    }

    /// The entry of `table` called `name` in any case, as the manifest has it.
    fn entry(&self, table: &DependencyTable, name: &str) -> Option<&(String, Constraint)> {
        let (_, entries) = self.found.1.tables().find(|(listed, _)| listed == table)?;
        entries
            .iter()
            .find(|(entry, _)| project::same_name(entry, name))
    }

    /// The manifest with `name` in `table` asking for `constraint`, and with a
    /// `source_pin` coming only from that source. An entry that is not given
    /// one keeps the one it has.
    fn set(
        &self,
        table: &DependencyTable,
        name: &str,
        constraint: &str,
        source_pin: Option<&str>,
    ) -> Result<Edit, EditError> {
        let pinned_to = (source_pin.or(self.found.1.source_pin_of(name))).map(str::to_string);
        let key = self
            .entry(table, name)
            .map_or(name, |(entry, _)| entry.as_str());
        let text =
            manifest_edit::set_source_pinned(&self.text, table, key, constraint, source_pin)?;
        let change = match self.entry(table, name) {
            Some((entry, old)) => {
                let from = constraint_text(old);
                // The text, not the project-wide pin: a pin in another table
                // does not make this line's rewrite a no-op.
                let rewritten = text != self.text;
                (from != constraint || rewritten).then(|| ManifestEdit::Updated {
                    name: entry.clone(),
                    from,
                    to: constraint.to_string(),
                    index: pinned_to,
                })
            }
            None => Some(ManifestEdit::Added {
                name: name.to_string(),
                constraint: constraint.to_string(),
                index: pinned_to,
            }),
        };
        Edit::of(text, change, table, self.is_new_group(table))
    }

    /// The table is a `[dependency-groups]` table the manifest does not have yet.
    fn is_new_group(&self, table: &DependencyTable) -> bool {
        matches!(table, DependencyTable::Group(group)
            if !self.found.1.groups.iter().any(|(name, _)| name == group))
    }

    /// The manifest without `name` in `table`; the error says where the name
    /// is when it is not there.
    fn take_out(&self, table: &DependencyTable, name: &str) -> Result<Edit, EditError> {
        let Some((entry, constraint)) = self.entry(table, name) else {
            return Err(self.not_in(table, name));
        };
        let change = ManifestEdit::Removed {
            name: entry.clone(),
            constraint: constraint_text(constraint),
        };
        let text = manifest_edit::remove(&self.text, table, entry)?;
        Edit::of(text, Some(change), table, false)
    }

    /// Why `name` is not in `table`: every other table that holds it, else none.
    fn not_in(&self, table: &DependencyTable, name: &str) -> EditError {
        let found_in: Vec<DependencyTable> = (self.found.1.tables())
            .filter(|(listed, entries)| {
                listed != table
                    && entries
                        .iter()
                        .any(|(entry, _)| project::same_name(entry, name))
            })
            .map(|(listed, _)| listed)
            .collect();
        let name = name.to_string();
        match table {
            _ if !found_in.is_empty() => EditError::InOtherTable { name, found_in },
            DependencyTable::Group(group) => EditError::NotInGroup {
                name,
                group: group.clone(),
            },
            // `remove` says whether it is installed; this cannot tell.
            _ => EditError::NotADependency {
                name,
                installed: false,
            },
        }
    }
}

/// The manifest after an edit: its text, what that reads as, and what changed.
struct Edit {
    text: String,
    project: Project,
    /// What the edit did to the manifest; `None` when it left it as it was.
    change: Option<ManifestEdit>,
    /// The entry is in `[dev-dependencies]`.
    dev: bool,
    /// The entry is in `[dependency-groups.<group>]`.
    group: Option<String>,
    /// The edit made that group's table.
    created_table: bool,
}

impl Edit {
    fn of(
        text: String,
        change: Option<ManifestEdit>,
        table: &DependencyTable,
        created_table: bool,
    ) -> Result<Edit, EditError> {
        let project = project::parse(&text).context("parsing the new manifest")?;
        let (dev, group) = table.flags();
        Ok(Edit {
            text,
            project,
            change,
            dev,
            group,
            created_table,
        })
    }
}

/// The constraint `add` writes for a package it was given none for: a floor at
/// the version the lock would hold for it. A package no source lists is `*`,
/// so that locking says so.
fn floor(
    env: &OpEnv,
    prepared: &Prepared,
    index: &Index,
    table: &DependencyTable,
    name: &str,
    source_pin: Option<&str>,
) -> Result<String, EditError> {
    let project = &prepared.found.1;
    let gate = locking::lv_gate(project.labview.as_deref().ok_or(LockError::NoLabview)?)?;
    let Some(newest) = index.versions_of(name).into_iter().next() else {
        return Ok("*".to_string());
    };
    if index.best(name, None, Some(gate)).is_none()
        && let Some(gated) = solve::gated(newest, gate)
    {
        return Err(LockError::NeedsNewerLabview {
            name: newest.name.clone(),
            version: newest.version.to_string(),
            needs_year: gated.needs_year,
            project_year: gated.project_year,
        }
        .into());
    }
    let any = prepared.set(table, name, "*", source_pin)?;
    let resolved =
        locking::resolved_version(env, &any.project, index, prepared.previous.as_ref(), name)?;
    Ok(resolved.map_or_else(|| "*".to_string(), |version| format!(">={version}")))
}

/// With `--global` typed, a sync into LabVIEW itself that would uninstall
/// anything but the package the edit takes away is refused: the rest is what
/// other projects, or the user, put into the installation, or what they may
/// still use (the dependencies the relock drops). A headless project that
/// resolves to the installation without `--global` is not guarded: it chose it.
// A named add or remove does not ask, so this refuses instead. It also refuses the dependencies the edit drops, since lvpm cannot tell who put them there.
fn refuse_foreign_packages(
    env: &OpEnv,
    dest: &Destination,
    plan: &SyncPlan,
    change: Option<&ManifestEdit>,
) -> Result<(), EditError> {
    if !env.flags.global || !matches!(dest, Destination::Global(_)) {
        return Ok(());
    }
    let taken_away = match change {
        Some(ManifestEdit::Removed { name, .. }) => Some(name),
        _ => None,
    };
    let packages: Vec<(String, String)> = (plan.steps.iter())
        .filter_map(|step| match step {
            SyncStep::Remove {
                name,
                version,
                reason: RemoveReason::NotInLock,
                ..
            } if !taken_away.is_some_and(|gone| project::same_name(gone, name)) => {
                Some((name.clone(), version.clone()))
            }
            _ => None,
        })
        .collect();
    match packages.is_empty() {
        true => Ok(()),
        false => Err(EditError::ForeignInInstallation { packages }),
    }
}

/// What the edit reads and where it goes, as `sync` says it first.
fn announce_project(env: &OpEnv, prepared: &Prepared, dest: &Destination) {
    let (path, project) = &prepared.found;
    env.sink.emit(Event::PhaseStarted {
        phase: Phase::Resolve,
    });
    env.sink.emit(Event::ManifestInUse {
        path: path.clone(),
        name: project.name.clone(),
        dependencies: project.dependencies.len(),
        labview: project.labview.clone(),
    });
    announce_target(env, &dest.store());
}

/// Lock the edited manifest, plan and fetch the sync, and only then write the
/// files and put the destination right. A removal that leaves a locked package
/// requiring what it took out is refused once the lock is made, and `announce`
/// is said after that.
fn commit(
    env: &OpEnv,
    prepared: Prepared,
    edit: Edit,
    index: &Index,
    dest: Destination,
    options: &InstallOptions,
    announce: Option<Event>,
) -> Result<(), EditError> {
    let Prepared {
        found: (manifest_path, _),
        lock_path,
        previous,
        lock_missing,
        text: before,
    } = prepared;
    // Locking warns about the requirements no source lists.
    let (lock, changes, after) =
        locking::relock_with(env, &edit.project, index, previous.as_ref())?;
    // Still listed in another table, a package leaves this one and stays.
    if let Some(ManifestEdit::Removed { name, .. }) = &edit.change
        && !listed(&edit.project, name)
        && let Some(refusal) = required(&lock, &edit.project, name)
    {
        return Err(refusal);
    }
    // The edited group is installed with the default selection. A package the
    // lock lists stays where it is: only what the lock drops goes.
    let selection = Selection {
        groups: edit.group.iter().cloned().collect(),
        keep_listed: true,
        ..Selection::default()
    };
    let plan = plan_sync(&lock, &install::list_installed(&dest.store())?, &selection);
    refuse_foreign_packages(env, &dest, &plan, edit.change.as_ref())?;
    // After the refusals: an edit that goes nowhere writes no lock.
    if lock_missing {
        env.sink.emit(Event::SyncLockMissing {
            path: lock_path.clone(),
            dry_run: options.dry_run,
        });
    }
    if let Some(event) = announce {
        env.sink.emit(event);
    }
    if lock_missing {
        announce_migration(env, &plan);
    }

    if options.dry_run {
        announce_edit(env, &edit, true);
        if !changes.is_empty() {
            env.sink.emit(Event::LockWouldChange { changes });
        }
        after.into_iter().for_each(|event| env.sink.emit(event));
        env.sink.emit(match plan.is_empty() {
            true => Event::SyncNothingToDo,
            false => Event::SyncPlanned {
                steps: plan.steps,
                dry_run: true,
            },
        });
        return Ok(());
    }

    let planned = fetch_locked(env, &edit.project, &plan.wanted)?;
    let lock_text = lock.to_toml()?;
    // The edited manifest's own lines allow hooks, so `hooks = "run"` on the
    // line just added counts.
    let sync_options = SyncOptions {
        install: InstallOptions {
            hook_allow: HookAllow::for_command(&env.config, Some(&edit.project)),
            relink_marked: edit.project.relink.iter().cloned().collect(),
            ..options.clone()
        },
        ..SyncOptions::default()
    };
    let manifest = (edit.text != before).then_some((manifest_path.as_path(), edit.text.as_str()));
    // From here a cancel is refused: the first file is written, with or
    // without a sync plan to apply after it.
    env.cancel.begin_unpack()?;
    restoring_on_failure(&[&manifest_path, &lock_path], || {
        write_manifest_and_lock(manifest, &lock_path, &lock_text)?;
        announce_edit(env, &edit, false);
        announce_written(env, lock_path.clone(), &lock, changes, after);
        match plan.is_empty() {
            true => env.sink.emit(Event::SyncNothingToDo),
            false => {
                env.sink.emit(Event::SyncPlanned {
                    steps: plan.steps.clone(),
                    dry_run: false,
                });
                let mut session = InstallSession::new(dest);
                let applied = apply_sync(
                    env,
                    &mut session,
                    &lock,
                    &plan,
                    &selection,
                    planned,
                    &sync_options,
                )?;
                finish(env, &mut session, &applied, options)?;
            }
        }
        Ok(())
    })
    .map_err(edit_error)
}

/// What a failure of the writes and the sync is. A sync error stays one; a
/// file that could not be put back adds a context ahead of the failure, and
/// that is the text the user must see, so such a failure is not unwrapped.
fn edit_error(failure: anyhow::Error) -> EditError {
    if failure.downcast_ref::<String>().is_some() {
        return EditError::Other(failure);
    }
    match failure.downcast::<SyncError>() {
        Ok(sync) => sync.into(),
        Err(other) => EditError::Other(other),
    }
}

/// Say what the edit did to the manifest.
fn announce_edit(env: &OpEnv, edit: &Edit, dry_run: bool) {
    let Some(change) = &edit.change else {
        return;
    };
    env.sink.emit(Event::ManifestEdited {
        edit: change.clone(),
        dev: edit.dev,
        group: edit.group.clone(),
        created_table: edit.created_table,
        dry_run,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancel::CancelToken;
    use crate::config::Config;
    use crate::events::EventSink;
    use crate::ops::testing::{self, CancelOn};
    use crate::target::Roots;
    use std::io::Write;
    use std::path::Path;
    use std::sync::Mutex;

    fn prepared(text: &str) -> Prepared {
        Prepared {
            found: (PathBuf::from("lvpm.toml"), project::parse(text).unwrap()),
            text: text.to_string(),
            lock_path: PathBuf::from("lvpm.lock"),
            previous: None,
            lock_missing: true,
        }
    }

    const MANIFEST: &str = "# mine\n[project]\nlabview = \">=2024\"\n\n[dependencies]\n\
                            Beta = \">=1.0\" # keep\n\n[dev-dependencies]\ncaraya = \"*\"\n";

    #[test]
    fn a_request_is_a_name_and_a_constraint_that_is_written_when_it_is_given() {
        let parsed = |spec: &str| Request::parse(spec, None).unwrap();
        assert_eq!(
            parsed("gamma"),
            Request {
                name: "gamma".into(),
                constraint: None,
                source_pin: None
            }
        );
        assert_eq!(parsed(" gamma@>=0.5 ").constraint.as_deref(), Some(">=0.5"));
        assert_eq!(parsed("gamma@1.0").constraint.as_deref(), Some("1.0"));
        assert_eq!(parsed("gamma@*").constraint.as_deref(), Some("*"));
    }

    #[test]
    fn nothing_after_the_at_sign_is_an_error_that_names_the_package() {
        // An unquoted `gamma@>=0.5` reaches the program as `gamma@`.
        for spec in ["gamma@", "gamma@ "] {
            assert!(
                matches!(Request::parse(spec, None), Err(EditError::EmptyConstraint { name }) if name == "gamma"),
                "{spec:?}"
            );
        }
    }

    #[test]
    fn no_name_and_a_constraint_lvpm_cannot_read_are_refused_before_anything_is_read() {
        for spec in ["", "@1.0", "gamma@<2.0", "gamma@~1.0"] {
            let why = Request::parse(spec, None).unwrap_err().to_string();
            assert!(
                why.contains("not a package name") || why.contains("unsupported version"),
                "{spec:?}: {why}"
            );
        }
    }

    #[test]
    fn a_new_entry_goes_at_the_end_of_its_table_and_nothing_else_changes() {
        let project = prepared(MANIFEST);
        let edit = project
            .set(&DependencyTable::Dependencies, "gamma", ">=1.0", None)
            .unwrap();
        assert_eq!(
            edit.text,
            MANIFEST.replace("# keep\n", "# keep\ngamma = \">=1.0\"\n")
        );
        assert_eq!(
            edit.change,
            Some(ManifestEdit::Added {
                name: "gamma".into(),
                constraint: ">=1.0".into(),
                index: None
            })
        );
        assert!(!edit.dev);
        assert_eq!(edit.project.dependencies.len(), 2);
    }

    #[test]
    fn an_entry_that_is_there_is_updated_under_its_own_spelling() {
        let project = prepared(MANIFEST);
        let edit = project
            .set(&DependencyTable::Dependencies, "beta", ">=0.5", None)
            .unwrap();
        assert_eq!(edit.text, MANIFEST.replace(">=1.0", ">=0.5"));
        assert_eq!(
            edit.change,
            Some(ManifestEdit::Updated {
                name: "Beta".into(),
                from: ">=1.0".into(),
                to: ">=0.5".into(),
                index: None
            })
        );
    }

    const PINNED: &str = "[project]\nlabview = \">=2024\"\n\n[sources]\n\
                          first = \"http://host/first\"\nsecond = \"http://host/second\"\n\n\
                          [dependencies]\n\
                          alpha = { version = \"1.0\", index = \"first\" }\nbeta = \"*\"\n";

    #[test]
    fn a_new_entry_with_a_source_pin_is_added_as_a_table_and_says_where_it_comes_from() {
        let project = prepared(PINNED);
        let edit = project
            .set(
                &DependencyTable::Dependencies,
                "gamma",
                ">=1.0",
                Some("second"),
            )
            .unwrap();
        assert_eq!(
            edit.change,
            Some(ManifestEdit::Added {
                name: "gamma".into(),
                constraint: ">=1.0".into(),
                index: Some("second".into())
            })
        );
        assert_eq!(edit.project.source_pin_of("gamma"), Some("second"));
    }

    #[test]
    fn a_pin_alone_is_a_change_and_the_same_pin_and_constraint_is_none() {
        let project = prepared(PINNED);
        let table = DependencyTable::Dependencies;
        let moved = project.set(&table, "alpha", "1.0", Some("second")).unwrap();
        assert_eq!(
            moved.change,
            Some(ManifestEdit::Updated {
                name: "alpha".into(),
                from: "1.0".into(),
                to: "1.0".into(),
                index: Some("second".into())
            })
        );
        assert!(moved.text.contains("index = \"second\""), "{}", moved.text);
        let pinned = project.set(&table, "beta", "*", Some("first")).unwrap();
        assert_eq!(
            pinned.change,
            Some(ManifestEdit::Updated {
                name: "beta".into(),
                from: "*".into(),
                to: "*".into(),
                index: Some("first".into())
            }),
            "a short line gaining a pin"
        );
        let same = project.set(&table, "alpha", "1.0", Some("first")).unwrap();
        assert_eq!((same.change, same.text.as_str()), (None, PINNED));
    }

    #[test]
    fn a_short_line_gaining_the_pin_another_table_has_is_a_change() {
        let project = prepared(
            "[sources]
second = \"https://example.com/\"

[dependencies]
alpha = { version = \"1.0\", index = \"second\" }

[dev-dependencies]
alpha = \"1.0\"
",
        );
        let edit = project
            .set(
                &DependencyTable::DevDependencies,
                "alpha",
                "1.0",
                Some("second"),
            )
            .unwrap();
        assert_eq!(
            edit.change,
            Some(ManifestEdit::Updated {
                name: "alpha".into(),
                from: "1.0".into(),
                to: "1.0".into(),
                index: Some("second".into())
            }),
            "{}",
            edit.text
        );
    }

    #[test]
    fn no_source_pin_keeps_the_pin_an_entry_has_and_says_it_is_from_that_source() {
        let project = prepared(PINNED);
        let edit = project
            .set(&DependencyTable::Dependencies, "alpha", "2.0", None)
            .unwrap();
        assert!(edit.text.contains("index = \"first\""), "{}", edit.text);
        assert_eq!(
            edit.change,
            Some(ManifestEdit::Updated {
                name: "alpha".into(),
                from: "1.0".into(),
                to: "2.0".into(),
                index: Some("first".into())
            })
        );
    }

    #[test]
    fn a_source_pin_that_names_no_source_is_refused_naming_the_line() {
        let project = prepared(PINNED);
        let why = project
            .set(&DependencyTable::Dependencies, "gamma", "*", Some("nope"))
            .err()
            .unwrap();
        assert!(
            format!("{why:#}")
                .contains("[dependencies] gamma: index = \"nope\" is not a [sources] name"),
            "{why:#}"
        );
    }

    #[test]
    fn a_source_pin_on_a_github_line_is_refused() {
        let manifest = PINNED.replace(
            "beta = \"*\"",
            "beta = { github = \"o/r\", version = \"*\" }",
        );
        let why = prepared(&manifest)
            .set(&DependencyTable::Dependencies, "beta", "*", Some("first"))
            .err()
            .unwrap();
        assert!(
            format!("{why:#}").contains("github and index cannot be used together"),
            "{why:#}"
        );
    }

    #[test]
    fn a_request_carries_the_source_pin_it_was_given() {
        let request = Request::parse("gamma@1.0", Some("second")).unwrap();
        assert_eq!(request.source_pin.as_deref(), Some("second"));
        assert_eq!(Request::parse("gamma", None).unwrap().source_pin, None);
    }

    #[test]
    fn the_constraint_an_entry_already_has_is_no_change() {
        let project = prepared(MANIFEST);
        let edit = project
            .set(&DependencyTable::Dependencies, "Beta", ">=1.0", None)
            .unwrap();
        assert_eq!(edit.text, MANIFEST);
        assert_eq!(edit.change, None);
    }

    #[test]
    fn a_dev_entry_goes_to_the_dev_table() {
        let project = prepared(MANIFEST);
        let edit = project
            .set(&DependencyTable::DevDependencies, "delta", "1.0", None)
            .unwrap();
        assert!(edit.dev);
        assert_eq!(edit.project.dev_dependencies.len(), 2);
        assert_eq!(edit.project.dependencies.len(), 1);
    }

    #[test]
    fn a_removal_takes_only_the_entrys_line_and_names_it_as_the_manifest_does() {
        let project = prepared(MANIFEST);
        let edit = project
            .take_out(&DependencyTable::Dependencies, "BETA")
            .unwrap();
        assert_eq!(edit.text, MANIFEST.replace("Beta = \">=1.0\" # keep\n", ""));
        assert_eq!(
            edit.change,
            Some(ManifestEdit::Removed {
                name: "Beta".into(),
                constraint: ">=1.0".into()
            })
        );
    }

    #[test]
    fn a_name_that_is_not_in_the_table_is_told_apart_from_one_in_the_other_table() {
        let project = prepared(MANIFEST);
        let refused = |table, name: &str| project.take_out(table, name).err().unwrap();
        assert!(matches!(
            refused(&DependencyTable::Dependencies, "zeta"),
            EditError::NotADependency { name, .. } if name == "zeta"
        ));
        let in_dev = refused(&DependencyTable::Dependencies, "caraya");
        assert!(matches!(
            &in_dev,
            EditError::InOtherTable { found_in, .. }
                if *found_in == [DependencyTable::DevDependencies]
        ));
        assert_eq!(in_dev.to_string(), "caraya is in [dev-dependencies]");
        let in_normal = refused(&DependencyTable::DevDependencies, "beta");
        assert_eq!(in_normal.to_string(), "beta is in [dependencies]");
    }

    fn ci() -> DependencyTable {
        DependencyTable::Group("ci".to_string())
    }

    const IN_EVERY_TABLE: &str = "[project]\nlabview = \">=2024\"\n\n[dependencies]\nbeta = \"*\"\n\n\
                                  [dev-dependencies]\nbeta = \"*\"\n\n[dependency-groups.ci]\n\
                                  beta = \"*\"\n";

    #[test]
    fn a_name_that_is_not_in_the_table_is_listed_with_every_other_table_that_holds_it() {
        let project = prepared(IN_EVERY_TABLE);
        let why = project
            .take_out(&DependencyTable::Group("docs".into()), "beta")
            .err()
            .unwrap();
        assert_eq!(
            why.to_string(),
            "beta is in [dependencies], [dev-dependencies] and [dependency-groups.ci]"
        );
    }

    #[test]
    fn a_name_that_is_in_no_table_is_told_apart_for_a_group_and_for_the_others() {
        let project = prepared(IN_EVERY_TABLE);
        let why = project.take_out(&ci(), "zeta").err().unwrap();
        assert!(
            matches!(&why, EditError::NotInGroup { name, group } if name == "zeta" && group == "ci"),
            "{why:?}"
        );
        assert_eq!(why.to_string(), "zeta is not in [dependency-groups.ci]");
        let why = project
            .take_out(&DependencyTable::DevDependencies, "zeta")
            .err()
            .unwrap();
        assert!(matches!(why, EditError::NotADependency { .. }), "{why:?}");
    }

    #[test]
    fn an_entry_of_a_group_that_is_not_there_yet_says_the_edit_made_its_table() {
        let project = prepared(MANIFEST);
        let edit = project.set(&ci(), "gamma", "*", None).unwrap();
        assert_eq!(
            (edit.dev, edit.group.as_deref(), edit.created_table),
            (false, Some("ci"), true)
        );
        assert_eq!(edit.project.groups.len(), 1);

        let again = prepared(&edit.text);
        let edit = again.set(&ci(), "delta", "*", None).unwrap();
        assert_eq!(
            (edit.dev, edit.group.as_deref(), edit.created_table),
            (false, Some("ci"), false)
        );
        let edit = again.take_out(&ci(), "gamma").unwrap();
        assert!(!edit.created_table);
    }

    #[test]
    fn no_other_table_is_a_new_group() {
        let project = prepared(MANIFEST);
        for table in [
            DependencyTable::Dependencies,
            DependencyTable::DevDependencies,
        ] {
            let edit = project.set(&table, "delta", "*", None).unwrap();
            assert_eq!((edit.group, edit.created_table), (None, false));
        }
    }

    // ---- the whole operations, on real files in a scratch destination ----

    #[derive(Default)]
    struct Recorder(Mutex<Vec<Event>>);

    impl EventSink for Recorder {
        fn emit(&self, event: Event) {
            self.0.lock().unwrap().push(event);
        }
    }

    impl Recorder {
        fn events(&self) -> Vec<Event> {
            self.0.lock().unwrap().clone()
        }
    }

    /// `(name, version, requires)`: each is installed as `vi.lib/unit/<name>.txt`.
    type Fixture<'a> = (&'a str, &'a str, &'a str);

    const PACKAGES: &[Fixture<'static>] = &[
        ("alpha", "1.0", ""),
        ("alpha", "2.0", ""),
        ("beta", "1.0", "alpha>=1.0"),
        ("gamma", "0.5", ""),
        ("gamma", "1.0", ""),
    ];

    const PROJECT: &str = "[project]\nlabview = \">=2024\"\n\n[sources]\ndefaults = false\n\n\
                           [dependencies]\nbeta = \">=1.0\"\n";

    fn payload_path(name: &str) -> String {
        format!("vi.lib/unit/{name}.txt")
    }

    fn write_vip(path: &Path, fixture: Fixture) {
        write_vip_for_labview(path, fixture, "20.0");
    }

    /// The package needs LabVIEW `lv_min` (in the index's units) or newer.
    fn write_vip_for_labview(path: &Path, (name, version, requires): Fixture, lv_min: &str) {
        let options = zip::write::SimpleFileOptions::default();
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let mut spec = format!("[Package]\nName=\"{name}\"\nVersion=\"{version}\"\n\n");
        if !requires.is_empty() {
            spec.push_str(&format!("[Dependencies]\nRequires=\"{requires}\"\n\n"));
        }
        spec.push_str(&format!(
            "[Platform]\nExclusive_LabVIEW_Version=\"LabVIEW>={lv_min}\"\n\n\
             [Files]\nNum File Groups=\"1\"\n\n[File Group 0]\nTarget Dir=\"<application>\"\n\
             Replace Mode=\"Always\"\nNum Files=1\nFile 0=\"{}\"\n",
            payload_path(name)
        ));
        writer.start_file("spec", options).unwrap();
        writer.write_all(spec.as_bytes()).unwrap();
        writer
            .start_file(format!("File Group 0/{}", payload_path(name)), options)
            .unwrap();
        writer
            .write_all(format!("{name} {version}\n").as_bytes())
            .unwrap();
        std::fs::write(path, writer.finish().unwrap().into_inner()).unwrap();
    }

    /// A project, a folder of packages beside it, and a scratch destination.
    struct World {
        root: PathBuf,
    }

    impl World {
        fn new(tag: &str, manifest: &str) -> World {
            let root =
                std::env::temp_dir().join(format!("lvpm-edit-test-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("packages")).unwrap();
            std::fs::write(root.join(project::FILE_NAME), manifest).unwrap();
            for package in PACKAGES {
                let file = format!("{}-{}.vip", package.0, package.1);
                write_vip(&root.join("packages").join(file), *package);
            }
            World { root }
        }

        fn env<'a>(&self, sink: &'a dyn EventSink) -> OpEnv<'a> {
            let config = Config {
                cache: self.root.join("cache"),
                hooks_run: Vec::new(),
                local_sources: vec![self.root.join("packages")],
                venv_dir: PathBuf::from(".lv-venv"),
                log_dir: Default::default(),
                venv_title: Default::default(),
            };
            let mut env = OpEnv::new(sink, config, Vec::new(), self.root.clone());
            env.flags.prefix = Some(self.root.join("dest"));
            env
        }

        fn manifest(&self) -> String {
            std::fs::read_to_string(self.root.join(project::FILE_NAME)).unwrap()
        }

        fn lock(&self) -> Option<Vec<u8>> {
            std::fs::read(self.root.join(lockfile::FILE_NAME)).ok()
        }

        fn locked(&self) -> Vec<String> {
            let lock = Lockfile::read(&self.root.join(lockfile::FILE_NAME)).unwrap();
            lock.packages
                .iter()
                .map(|package| format!("{} {}", package.name, package.version))
                .collect()
        }

        fn installed(&self) -> Vec<String> {
            let store = Roots::scratch(&self.root.join("dest"));
            install::list_installed(&store)
                .unwrap()
                .iter()
                .map(|package| format!("{} {}", package.name, package.version))
                .collect()
        }
    }

    impl Drop for World {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn run_add(world: &World, spec: &str, options: &InstallOptions) -> Result<(), EditError> {
        add(
            &world.env(&Recorder::default()),
            spec,
            DependencyTable::Dependencies,
            None,
            options,
        )
    }

    #[test]
    fn adding_without_a_constraint_writes_a_floor_at_the_locked_version_then_locks_and_syncs() {
        let world = World::new("add", PROJECT);
        let sink = Recorder::default();
        add(
            &world.env(&sink),
            "gamma",
            DependencyTable::Dependencies,
            None,
            &InstallOptions::default(),
        )
        .unwrap();
        assert_eq!(world.manifest(), format!("{PROJECT}gamma = \">=1.0\"\n"));
        let expected = ["alpha 2.0", "beta 1.0", "gamma 1.0"];
        assert_eq!(world.locked(), expected);
        assert_eq!(world.installed(), expected);
        let lock = Lockfile::read(&world.root.join(lockfile::FILE_NAME)).unwrap();
        assert_eq!(
            lock.staleness(&project::parse(&world.manifest()).unwrap()),
            None
        );
        assert!(sink.events().contains(&Event::ManifestEdited {
            edit: ManifestEdit::Added {
                name: "gamma".into(),
                constraint: ">=1.0".into(),
                index: None
            },
            dev: false,
            group: None,
            created_table: false,
            dry_run: false
        }));
    }

    #[test]
    fn the_floor_is_the_version_the_lock_holds_when_a_pin_keeps_it_below_the_newest() {
        let manifest = format!("{PROJECT}alpha = \"1.0\"\n");
        let world = World::new("pinned", &manifest);
        let packages = world.root.join("packages");
        write_vip(
            &packages.join("omega-1.0.vip"),
            ("omega", "1.0", "alpha>=1.0"),
        );
        write_vip(
            &packages.join("omega-2.0.vip"),
            ("omega", "2.0", "alpha>=2.0"),
        );
        // omega 2.0 is the newest there is, and cannot be had beside alpha 1.0.
        run_add(&world, "omega", &InstallOptions::default()).unwrap();
        assert_eq!(world.manifest(), format!("{manifest}omega = \">=1.0\"\n"));
        assert!(world.locked().contains(&"omega 1.0".to_string()));
    }

    #[test]
    fn a_package_no_source_lists_is_a_conflict_that_says_so_and_changes_nothing() {
        let world = World::new("unlisted", PROJECT);
        let why = run_add(&world, "zeta", &InstallOptions::default()).unwrap_err();
        assert!(
            matches!(&why, EditError::Lock(LockError::Conflict(conflict))
                if conflict.to_string().contains("zeta")),
            "{why:?}"
        );
        assert_eq!(world.manifest(), PROJECT);
        assert_eq!(world.lock(), None);
    }

    #[test]
    fn a_package_whose_every_version_needs_a_newer_labview_says_which_and_changes_nothing() {
        let world = World::new("gated", PROJECT);
        write_vip_for_labview(
            &world.root.join("packages").join("omega-1.0.vip"),
            ("omega", "1.0", ""),
            "26.0",
        );
        let why = run_add(&world, "omega", &InstallOptions::default()).unwrap_err();
        assert!(
            matches!(
                &why,
                EditError::Lock(LockError::NeedsNewerLabview { name, needs_year: 2026, project_year: 2024, .. })
                    if name == "omega"
            ),
            "{why:?}"
        );
        assert_eq!(world.manifest(), PROJECT);
        assert_eq!(world.lock(), None);
    }

    #[test]
    fn a_requirement_no_source_lists_is_warned_about_once() {
        let world = World::new("unresolved", PROJECT);
        write_vip(
            &world.root.join("packages").join("delta-1.0.vip"),
            ("delta", "1.0", "ni_lib_missing>=1.0"),
        );
        let sink = Recorder::default();
        add(
            &world.env(&sink),
            "delta",
            DependencyTable::Dependencies,
            None,
            &InstallOptions::default(),
        )
        .unwrap();
        let warnings: Vec<Event> = sink
            .events()
            .into_iter()
            .filter(|event| {
                matches!(
                    event,
                    Event::SyncUnresolved { .. } | Event::RequirementUnresolved { .. }
                )
            })
            .collect();
        assert_eq!(
            warnings,
            [Event::RequirementUnresolved {
                name: "ni_lib_missing".into(),
                required_by: "delta".into()
            }]
        );
    }

    #[test]
    fn a_dry_run_reports_the_edit_the_lock_and_the_plan_and_writes_nothing() {
        let world = World::new("dry", PROJECT);
        let sink = Recorder::default();
        let options = InstallOptions {
            dry_run: true,
            ..InstallOptions::default()
        };
        add(
            &world.env(&sink),
            "gamma",
            DependencyTable::Dependencies,
            None,
            &options,
        )
        .unwrap();
        assert_eq!(world.manifest(), PROJECT);
        assert_eq!(world.lock(), None);
        assert!(!world.root.join("dest").exists());
        let events = sink.events();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::ManifestEdited { dry_run: true, .. })),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::LockWouldChange { .. })),
            "{events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                Event::SyncPlanned { dry_run: true, steps } if steps.len() == 3
            )),
            "{events:?}"
        );
    }

    #[test]
    fn a_failed_lock_changes_nothing_and_names_the_conflict() {
        let manifest = format!("{PROJECT}alpha = \"1.0\"\n");
        let world = World::new("conflict", &manifest);
        write_vip(
            &world.root.join("packages").join("omega-1.0.vip"),
            ("omega", "1.0", "alpha>=2.0"),
        );
        let why = run_add(&world, "omega", &InstallOptions::default()).unwrap_err();
        assert!(
            matches!(why, EditError::Lock(LockError::Conflict(_))),
            "{why:?}"
        );
        assert_eq!(world.manifest(), manifest);
        assert_eq!(world.lock(), None);
    }

    #[test]
    fn a_source_pin_that_names_no_source_changes_nothing_and_names_the_line() {
        let world = World::new("unknown-source", PROJECT);
        let why = add(
            &world.env(&Recorder::default()),
            "gamma",
            DependencyTable::Dependencies,
            Some("nope"),
            &InstallOptions::default(),
        )
        .unwrap_err();
        let said = format!("{why:#}");
        assert!(
            said.contains("[dependencies] gamma: index = \"nope\" is not a [sources] name"),
            "{said}"
        );
        assert_eq!(world.manifest(), PROJECT);
        assert_eq!(world.lock(), None);
        assert_eq!(world.installed(), Vec::<String>::new());
    }

    #[test]
    fn a_locked_version_is_not_pinned_to_other_bytes_when_the_lock_is_made_again() {
        let world = World::new("repin", PROJECT);
        run_add(&world, "gamma@0.5", &InstallOptions::default()).unwrap();
        let (manifest, lock) = (world.manifest(), world.lock().unwrap());

        // The feed now serves other bytes for alpha 2.0, and the cache is gone.
        let altered = world.root.join("packages/alpha-2.0.vip");
        write_vip_for_labview(&altered, ("alpha", "2.0", ""), "21.0");
        std::fs::remove_dir_all(world.root.join("cache")).unwrap();

        let why = run_add(&world, "gamma", &InstallOptions::default()).unwrap_err();
        let EditError::Lock(LockError::LockedArchives(problems)) = why else {
            panic!("expected archives that differ from the lock, got {why:?}");
        };
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].name, "alpha");
        assert_eq!(
            problems[0].fault,
            crate::errors::ArchiveFault::DiffersFromLock {
                found_sha256: crate::cache::sha256_hex(&std::fs::read(&altered).unwrap())
            }
        );
        assert_eq!(world.manifest(), manifest);
        assert_eq!(world.lock().unwrap(), lock);
    }

    #[test]
    fn a_sync_that_fails_part_way_puts_the_manifest_and_the_lock_back() {
        let world = World::new("restore", PROJECT);
        run_add(&world, "alpha", &InstallOptions::default()).unwrap();
        let (manifest, lock) = (world.manifest(), world.lock().unwrap());

        // gamma's one file cannot be written: a folder is where it goes.
        let blocked = world.root.join("dest/LabVIEW").join(payload_path("gamma"));
        std::fs::create_dir_all(&blocked).unwrap();
        let why = run_add(&world, "gamma", &InstallOptions::default()).unwrap_err();

        assert!(
            matches!(&why, EditError::Sync(SyncError::Incomplete { left, .. })
                if left.iter().any(|step| step.name() == "gamma")),
            "{why:?}"
        );
        assert_eq!(world.manifest(), manifest);
        assert_eq!(world.lock().unwrap(), lock);
    }

    #[test]
    fn a_group_add_that_fails_part_way_counts_the_group_package_as_left() {
        let world = World::new("restore-group", PROJECT);
        std::fs::create_dir_all(world.root.join("dest/LabVIEW").join(payload_path("gamma")))
            .unwrap();
        let sink = Recorder::default();
        let why = add(
            &world.env(&sink),
            "gamma",
            ci(),
            None,
            &InstallOptions::default(),
        )
        .unwrap_err();

        assert!(
            matches!(&why, EditError::Sync(SyncError::Incomplete { done, left, .. })
                if !done.iter().any(|step| step.name() == "gamma")
                    && left.iter().any(|step| step.name() == "gamma")),
            "{why:?}"
        );
    }

    #[test]
    fn a_lock_that_did_not_exist_is_removed_again_when_the_sync_fails() {
        let world = World::new("restore-new", PROJECT);
        std::fs::create_dir_all(world.root.join("dest/LabVIEW").join(payload_path("gamma")))
            .unwrap();
        run_add(&world, "gamma", &InstallOptions::default()).unwrap_err();
        assert_eq!(world.manifest(), PROJECT);
        assert_eq!(world.lock(), None);
    }

    #[test]
    fn a_sync_failure_stays_a_sync_error_and_one_that_could_not_be_put_back_says_so_first() {
        let sync_failed = || anyhow::Error::new(SyncError::Archives(Vec::new()));
        assert!(matches!(
            edit_error(sync_failed()),
            EditError::Sync(SyncError::Archives(_))
        ));

        let world = World::new("stuck", PROJECT);
        let file = world.root.join("stuck");
        std::fs::write(&file, b"old").unwrap();
        // By the time the guard restores it, the file is a folder.
        let failure = restoring_on_failure(&[&file], || {
            std::fs::remove_file(&file)?;
            std::fs::create_dir(&file)?;
            Err::<(), _>(sync_failed())
        })
        .unwrap_err();
        let shown = edit_error(failure);
        assert!(matches!(shown, EditError::Other(_)), "{shown:?}");
        let text = format!("{shown:#}");
        assert!(text.starts_with("and could not put back "), "{text}");
    }

    #[test]
    fn removing_the_last_dependency_empties_the_lock_and_the_destination() {
        let world = World::new("remove-last", PROJECT);
        run_add(&world, "gamma", &InstallOptions::default()).unwrap();
        let sink = Recorder::default();
        let env = world.env(&sink);
        remove(
            &env,
            "gamma",
            DependencyTable::Dependencies,
            &InstallOptions::default(),
        )
        .unwrap();
        remove(
            &env,
            "beta",
            DependencyTable::Dependencies,
            &InstallOptions::default(),
        )
        .unwrap();
        assert_eq!(world.manifest(), PROJECT.replace("beta = \">=1.0\"\n", ""));
        assert_eq!(world.locked(), Vec::<String>::new());
        assert_eq!(world.installed(), Vec::<String>::new());
    }

    #[test]
    fn a_package_another_one_requires_is_refused_naming_it_and_changes_nothing() {
        let manifest = format!("{PROJECT}alpha = \">=1.0\"\n");
        let world = World::new("still", &manifest);
        run_add(&world, "alpha@>=1.0", &InstallOptions::default()).unwrap();
        let (manifest, lock, installed) = (world.manifest(), world.lock(), world.installed());
        for dry_run in [false, true] {
            let why = remove(
                &world.env(&Recorder::default()),
                "alpha",
                DependencyTable::Dependencies,
                &InstallOptions {
                    dry_run,
                    ..InstallOptions::default()
                },
            )
            .unwrap_err();
            assert!(
                matches!(&why, EditError::Required { name, required_by, roots }
                    if name == "alpha" && required_by == &["beta"]
                    && roots == &[("beta".to_string(), vec![DependencyTable::Dependencies])]),
                "dry_run {dry_run}: {why:?}"
            );
            assert_eq!(world.manifest(), manifest);
            assert_eq!(world.lock(), lock);
            assert_eq!(world.installed(), installed);
        }
    }

    #[test]
    fn a_listed_package_in_the_middle_is_named_with_the_top_and_the_top_comes_first() {
        // zeta -> beta -> alpha, all three listed: alpha cannot go until zeta
        // and then beta have. "zeta" sorts after "beta", so a name sort is wrong.
        let manifest = format!("{PROJECT}alpha = \">=1.0\"\nzeta = \">=1.0\"\n");
        let world = World::new("middle-listed", &manifest);
        write_vip(
            &world.root.join("packages").join("zeta-1.0.vip"),
            ("zeta", "1.0", "beta>=1.0"),
        );
        run_add(&world, "alpha@>=1.0", &InstallOptions::default()).unwrap();
        run_add(&world, "zeta@>=1.0", &InstallOptions::default()).unwrap();
        let lock = world.lock();
        let why = remove(
            &world.env(&Recorder::default()),
            "alpha",
            DependencyTable::Dependencies,
            &InstallOptions::default(),
        )
        .unwrap_err();
        let EditError::Required { roots, .. } = &why else {
            panic!("expected Required, got {why:?}");
        };
        let named: Vec<&str> = roots.iter().map(|(root, _)| root.as_str()).collect();
        assert_eq!(named, ["zeta", "beta"], "{why:?}");
        assert_eq!(world.lock(), lock);
    }

    #[test]
    fn a_package_that_is_listed_in_another_table_too_is_taken_out_of_this_one_only() {
        // beta requires alpha, and [dependencies] lists alpha: leaving
        // [dev-dependencies] is not leaving the project.
        let manifest =
            format!("{PROJECT}alpha = \">=1.0\"\n\n[dev-dependencies]\nalpha = \">=1.0\"\n");
        let world = World::new("listed-twice", &manifest);
        run_add(&world, "alpha@>=1.0", &InstallOptions::default()).unwrap();
        remove(
            &world.env(&Recorder::default()),
            "alpha",
            DependencyTable::DevDependencies,
            &InstallOptions::default(),
        )
        .unwrap();
        assert!(
            !world.manifest().contains("[dev-dependencies]\nalpha"),
            "{}",
            world.manifest()
        );
        assert!(world.manifest().contains("alpha = \">=1.0\""));
        assert!(
            world.locked().contains(&"alpha 2.0".to_string()),
            "{:?}",
            world.locked()
        );
        assert!(world.installed().contains(&"alpha 2.0".to_string()));
    }

    #[test]
    fn a_package_the_manifest_does_not_list_but_another_requires_is_required_not_a_non_dependency()
    {
        let world = World::new("unlisted-required", PROJECT);
        run_add(&world, "gamma", &InstallOptions::default()).unwrap();
        let lock = world.lock();
        let why = remove(
            &world.env(&Recorder::default()),
            "alpha",
            DependencyTable::Dependencies,
            &InstallOptions::default(),
        )
        .unwrap_err();
        assert!(
            matches!(&why, EditError::Required { name, roots, .. }
                if name == "alpha"
                    && roots == &[("beta".to_string(), vec![DependencyTable::Dependencies])]),
            "{why:?}"
        );
        assert_eq!(world.lock(), lock);
    }

    #[test]
    fn a_package_no_table_lists_but_another_requires_is_required_from_a_group_too() {
        let manifest = format!(
            "{PROJECT}
[dependency-groups.ci]
"
        );
        let world = World::new("unlisted-required-group", &manifest);
        run_add(&world, "gamma", &InstallOptions::default()).unwrap();
        let why = remove(
            &world.env(&Recorder::default()),
            "alpha",
            DependencyTable::Group("ci".into()),
            &InstallOptions::default(),
        )
        .unwrap_err();
        assert!(
            matches!(&why, EditError::Required { name, .. } if name == "alpha"),
            "{why:?}"
        );
    }

    /// The roots a refused `remove alpha` names, for a manifest that lists
    /// alpha and the packages that need it.
    fn roots_of_the_refusal(name: &str, manifest: &str) -> Vec<(String, Vec<DependencyTable>)> {
        let world = World::new(name, manifest);
        run_add(&world, "alpha@>=1.0", &InstallOptions::default()).unwrap();
        let why = remove(
            &world.env(&Recorder::default()),
            "alpha",
            DependencyTable::Dependencies,
            &InstallOptions::default(),
        )
        .unwrap_err();
        match why {
            EditError::Required { roots, .. } => roots,
            other => panic!("expected Required, got {other:?}"),
        }
    }

    #[test]
    fn each_root_is_named_with_the_tables_of_the_manifest_that_list_it() {
        let head = "[project]\nlabview = \">=2024\"\n\n[sources]\ndefaults = false\n\n";
        let alpha = "[dependencies]\nalpha = \">=1.0\"\n";
        let beta = |table: &str| format!("\n[{table}]\nbeta = \">=1.0\"\n");
        let cases = [
            (
                "roots-dev",
                beta("dev-dependencies"),
                vec![DependencyTable::DevDependencies],
            ),
            ("roots-group", beta("dependency-groups.ci"), vec![ci()]),
            (
                "roots-two",
                format!(
                    "{}{}",
                    beta("dev-dependencies"),
                    beta("dependency-groups.ci")
                ),
                vec![DependencyTable::DevDependencies, ci()],
            ),
        ];
        for (name, listing, tables) in cases {
            let roots = roots_of_the_refusal(name, &format!("{head}{alpha}{listing}"));
            assert_eq!(roots, [("beta".to_string(), tables)], "{name}");
        }
    }

    #[test]
    fn a_refusal_in_a_project_with_no_lock_yet_does_not_say_one_is_being_written() {
        let manifest = format!("{PROJECT}alpha = \">=1.0\"\n");
        let world = World::new("refuse-no-lock", &manifest);
        let sink = Recorder::default();
        let why = remove(
            &world.env(&sink),
            "alpha",
            DependencyTable::Dependencies,
            &InstallOptions::default(),
        )
        .unwrap_err();
        assert!(matches!(why, EditError::Required { .. }), "{why:?}");
        assert!(
            !sink
                .events()
                .iter()
                .any(|event| matches!(event, Event::SyncLockMissing { .. })),
            "{:?}",
            sink.events()
        );
        assert_eq!(world.lock(), None);
        assert_eq!(world.manifest(), manifest);
    }

    #[test]
    fn a_cancel_accepted_once_the_lock_is_made_stops_a_removal_that_has_nothing_to_sync() {
        let world = World::new("cancel-late", PROJECT);
        write_vip(
            &world.root.join("packages").join("delta-1.0.vip"),
            ("delta", "1.0", "ni_lib_missing>=1.0"),
        );
        run_add(&world, "delta", &InstallOptions::default()).unwrap();
        add(
            &world.env(&Recorder::default()),
            "delta",
            DependencyTable::DevDependencies,
            None,
            &InstallOptions::default(),
        )
        .unwrap();
        let (manifest, lock) = (world.manifest(), world.lock().unwrap());

        // [dependencies] still lists delta: the lock holds what it did, and
        // the destination already has it all.
        let token = CancelToken::new();
        let sink = CancelOn::new(&token, |event| {
            matches!(event, Event::RequirementUnresolved { .. })
        });
        let mut env = world.env(&sink);
        env.cancel = token;
        let why = remove(
            &env,
            "delta",
            DependencyTable::DevDependencies,
            &InstallOptions::default(),
        )
        .unwrap_err();
        assert_eq!(sink.answered(), Some(true), "the cancel was not accepted");
        assert!(matches!(&why, EditError::Cancelled), "{why:?}");
        assert_eq!(world.manifest(), manifest);
        assert_eq!(world.lock().unwrap(), lock);
    }

    #[test]
    fn adding_to_a_new_group_makes_its_table_locks_and_installs_it_and_a_later_add_keeps_it() {
        let world = World::new("group-add", PROJECT);
        let sink = Recorder::default();
        add(
            &world.env(&sink),
            "gamma",
            ci(),
            None,
            &InstallOptions::default(),
        )
        .unwrap();
        assert_eq!(
            world.manifest(),
            format!("{PROJECT}\n[dependency-groups.ci]\ngamma = \">=1.0\"\n")
        );
        let expected = ["alpha 2.0", "beta 1.0", "gamma 1.0"];
        assert_eq!(world.locked(), expected);
        assert_eq!(world.installed(), expected);
        assert!(sink.events().contains(&Event::ManifestEdited {
            edit: ManifestEdit::Added {
                name: "gamma".into(),
                constraint: ">=1.0".into(),
                index: None
            },
            dev: false,
            group: Some("ci".into()),
            created_table: true,
            dry_run: false
        }));

        // The default selection does not name ci, and gamma stays: the lock lists it.
        run_add(&world, "alpha", &InstallOptions::default()).unwrap();
        assert_eq!(world.installed(), expected);
    }

    #[test]
    fn a_group_nobody_installs_by_default_is_not_installed_by_an_add_to_another_table() {
        let manifest = format!("{PROJECT}\n[dependency-groups.ci]\ngamma = \"*\"\n");
        let world = World::new("group-apart", &manifest);
        run_add(&world, "alpha", &InstallOptions::default()).unwrap();
        assert_eq!(world.locked(), ["alpha 2.0", "beta 1.0", "gamma 1.0"]);
        assert_eq!(world.installed(), ["alpha 2.0", "beta 1.0"]);
    }

    #[test]
    fn removing_from_a_group_takes_the_package_out_of_the_lock_and_the_destination() {
        let world = World::new("group-remove", PROJECT);
        let sink = Recorder::default();
        let env = world.env(&sink);
        add(&env, "gamma", ci(), None, &InstallOptions::default()).unwrap();
        remove(&env, "gamma", ci(), &InstallOptions::default()).unwrap();
        assert_eq!(
            world.manifest(),
            format!("{PROJECT}\n[dependency-groups.ci]\n")
        );
        assert_eq!(world.locked(), ["alpha 2.0", "beta 1.0"]);
        assert_eq!(world.installed(), ["alpha 2.0", "beta 1.0"]);
    }

    #[test]
    fn an_add_that_takes_a_package_out_announces_the_plan_once_after_the_lock_is_written() {
        let world = World::new("plan-once", PROJECT);
        let setup = Recorder::default();
        add(
            &world.env(&setup),
            "gamma",
            ci(),
            None,
            &InstallOptions::default(),
        )
        .unwrap();
        // The manifest forgets the ci group: the lock the add makes drops gamma, which is installed.
        std::fs::write(world.root.join(project::FILE_NAME), PROJECT).unwrap();
        let sink = Recorder::default();

        add(
            &world.env(&sink),
            "alpha",
            DependencyTable::Dependencies,
            None,
            &InstallOptions::default(),
        )
        .unwrap();

        assert_eq!(world.installed(), ["alpha 2.0", "beta 1.0"]);
        let events = sink.events();
        let planned = |event: &&Event| matches!(event, Event::SyncPlanned { dry_run: false, .. });
        assert_eq!(events.iter().filter(planned).count(), 1, "{events:?}");
        let position = |wanted: fn(&Event) -> bool| events.iter().position(wanted).unwrap();
        let written = position(|e| matches!(e, Event::LockWritten { .. }));
        let announced = position(|e| matches!(e, Event::SyncPlanned { .. }));
        let removed = position(|e| matches!(e, Event::PackageRemoved { .. }));
        assert!(written < announced && announced < removed, "{events:?}");
    }

    #[test]
    fn removing_from_a_group_the_manifest_does_not_have_changes_nothing_and_says_which_are_defined()
    {
        let manifest = format!("{PROJECT}\n[dependency-groups.ci]\ngamma = \"*\"\n");
        let world = World::new("group-unknown", &manifest);
        let sink = Recorder::default();
        let why = remove(
            &world.env(&sink),
            "gamma",
            DependencyTable::Group("nope".into()),
            &InstallOptions::default(),
        )
        .unwrap_err();
        assert!(matches!(why, EditError::Sync(_)), "{why:?}");
        assert_eq!(
            why.to_string(),
            "unknown group 'nope'; defined groups: ci, dev"
        );
        assert!(sink.events().is_empty(), "{:?}", sink.events());
        assert_eq!(world.manifest(), manifest);
        assert_eq!(world.lock(), None);
    }

    #[test]
    fn a_name_that_is_not_a_dependency_changes_nothing_and_syncs_nothing() {
        for in_a_venv in [false, true] {
            let world = World::new(&format!("not-a-dependency-{in_a_venv}"), PROJECT);
            let sink = Recorder::default();
            let mut env = world.env(&sink);
            if in_a_venv {
                // The venv is found, never announced: nothing was selected.
                testing::venv_in(&world.root);
                env.flags.prefix = None;
                env.targets = vec![testing::labview_2026()];
            }
            let why = remove(
                &env,
                "zeta",
                DependencyTable::Dependencies,
                &InstallOptions::default(),
            )
            .unwrap_err();
            assert!(
                matches!(
                    why,
                    EditError::NotADependency {
                        installed: false,
                        ..
                    }
                ),
                "venv {in_a_venv}: {why:?}"
            );
            assert!(
                sink.events().is_empty(),
                "venv {in_a_venv}: {:?}",
                sink.events()
            );
            assert_eq!(world.manifest(), PROJECT);
            assert_eq!(world.lock(), None);
        }
    }

    #[test]
    fn what_a_removal_announces_is_said_once_the_lock_is_made_and_only_if_it_goes_ahead() {
        let announced = Event::UninstallRemovesFromProject {
            package: "beta".into(),
            destination: None,
            dry_run: false,
        };
        let manifest = format!(
            "{PROJECT}alpha = \">=1.0\"
"
        );
        let world = World::new("announce", &manifest);
        run_add(&world, "alpha@>=1.0", &InstallOptions::default()).unwrap();
        let refused = |table, name: &str, dry_run| {
            let sink = Recorder::default();
            let why = remove_announcing(
                &world.env(&sink),
                name,
                table,
                &InstallOptions {
                    dry_run,
                    ..InstallOptions::default()
                },
                announced.clone(),
            )
            .unwrap_err();
            (why, sink.events())
        };
        // Not a dependency, in another table, in no group, and required: a
        // dry run is refused just as a real one is.
        for (table, name, dry_run) in [
            (DependencyTable::Dependencies, "zeta", false),
            (DependencyTable::DevDependencies, "beta", false),
            (ci(), "beta", false),
            (DependencyTable::Dependencies, "alpha", false),
            (DependencyTable::Dependencies, "alpha", true),
        ] {
            let (why, events) = refused(table, name, dry_run);
            assert!(
                !events.contains(&announced),
                "{name} (dry run {dry_run}): {why:?} said {events:?}"
            );
        }
        let sink = Recorder::default();
        remove_announcing(
            &world.env(&sink),
            "beta",
            DependencyTable::Dependencies,
            &InstallOptions::default(),
            announced.clone(),
        )
        .unwrap();
        let events = sink.events();
        let said = events.iter().position(|event| *event == announced);
        let edited = events
            .iter()
            .position(|event| matches!(event, Event::ManifestEdited { .. }));
        assert!(said < edited && said.is_some(), "{events:?}");
        assert_eq!(
            events.iter().filter(|event| **event == announced).count(),
            1
        );
    }

    #[test]
    fn what_an_add_announces_is_said_once_the_lock_is_made_and_not_for_a_package_no_source_lists() {
        let announced = Event::InstallAddsToProject {
            package: "gamma".into(),
            destination: None,
            dry_run: false,
        };
        let world = World::new("announce-add", PROJECT);
        let add_announcing = |spec: &str, sink: &Recorder, dry_run| {
            add_announcing(
                &world.env(sink),
                spec,
                DependencyTable::Dependencies,
                None,
                &InstallOptions {
                    dry_run,
                    ..InstallOptions::default()
                },
                announced.clone(),
            )
        };
        for dry_run in [false, true] {
            let sink = Recorder::default();
            add_announcing("zeta", &sink, dry_run).unwrap_err();
            assert!(
                !sink.events().contains(&announced),
                "dry run {dry_run}: {:?}",
                sink.events()
            );
        }

        let sink = Recorder::default();
        add_announcing("gamma", &sink, false).unwrap();
        let events = sink.events();
        let said = events.iter().position(|event| *event == announced);
        let edited = events
            .iter()
            .position(|event| matches!(event, Event::ManifestEdited { .. }));
        assert!(said < edited && said.is_some(), "{events:?}");
    }

    #[test]
    fn a_name_that_is_not_a_dependency_says_whether_the_target_holds_it() {
        let world = World::new("not-a-dependency-installed", PROJECT);
        let sink = Recorder::default();
        let env = world.env(&sink);
        add(
            &env,
            "gamma",
            DependencyTable::Dependencies,
            None,
            &InstallOptions::default(),
        )
        .unwrap();
        // gamma stays installed, but the manifest no longer lists it.
        std::fs::write(world.root.join(project::FILE_NAME), PROJECT).unwrap();
        let asked = |name: &str| {
            remove(
                &env,
                name,
                DependencyTable::Dependencies,
                &InstallOptions::default(),
            )
            .unwrap_err()
        };
        assert!(
            matches!(
                asked("gamma"),
                EditError::NotADependency {
                    installed: true,
                    ..
                }
            ),
            "installed"
        );
        assert!(
            matches!(
                asked("zeta"),
                EditError::NotADependency {
                    installed: false,
                    ..
                }
            ),
            "nowhere"
        );
    }

    #[test]
    fn a_global_alone_sees_what_the_installation_holds_when_a_name_is_not_a_dependency() {
        let world = World::new("not-a-dependency-global", PROJECT);
        let fake = crate::ops::testing::FakeInstallation::new(&world.root);
        let store = fake.roots().store_dir();
        std::fs::create_dir_all(&store).unwrap();
        let record = serde_json::json!({
            "name": "gamma", "version": "1.0", "display_name": null, "installed_at": 0,
            "target": "LabVIEW 2099", "files": [], "skipped_hooks": [],
        });
        std::fs::write(store.join("gamma.json"), record.to_string()).unwrap();
        let sink = Recorder::default();
        let mut env = world.env(&sink);
        env.flags.prefix = None;
        env.flags.global = true;
        fake.place(&mut env);
        let why = remove(
            &env,
            "gamma",
            DependencyTable::Dependencies,
            &InstallOptions::default(),
        )
        .unwrap_err();
        assert!(
            matches!(
                why,
                EditError::NotADependency {
                    installed: true,
                    ..
                }
            ),
            "{why:?}"
        );
    }

    #[test]
    fn a_global_edit_that_would_uninstall_what_no_lock_lists_is_refused_and_changes_nothing() {
        let world = World::new("foreign-global", PROJECT);
        let fake = testing::FakeInstallation::new(&world.root);
        let options = InstallOptions {
            no_relink: true,
            ..InstallOptions::default()
        };
        let sink = Recorder::default();
        let mut env = world.env(&sink);
        env.flags.prefix = None;
        env.flags.global = true;
        fake.place(&mut env);
        let held = || -> Vec<String> {
            let mut held: Vec<String> = install::list_installed(&fake.roots())
                .unwrap()
                .iter()
                .map(|package| package.name.clone())
                .collect();
            held.sort();
            held
        };
        let table = || DependencyTable::Dependencies;

        // Nothing foreign: an add goes ahead, and a removal of what the old
        // lock listed is no foreign removal.
        add(&env, "gamma", table(), None, &options).unwrap();
        assert_eq!(held(), ["alpha", "beta", "gamma"]);
        remove(&env, "gamma", table(), &options).unwrap();
        assert_eq!(held(), ["alpha", "beta"]);

        // Somebody else put delta there: no lock lists it.
        fake.with_package("delta");
        let before = (
            world.manifest(),
            world.lock(),
            testing::snapshot(&fake.target.path),
            testing::snapshot(&world.root.join("machine")),
        );
        for dry_run in [false, true] {
            let options = InstallOptions {
                dry_run,
                ..options.clone()
            };
            let refused = [
                add(&env, "gamma", table(), None, &options),
                remove(&env, "beta", table(), &options),
            ];
            // The removal of beta drops alpha from the lock as well: that is
            // refused too, for another project may use it.
            let named = [
                vec![("delta".to_string(), "1.0".to_string())],
                vec![
                    ("alpha".to_string(), "2.0".to_string()),
                    ("delta".to_string(), "1.0".to_string()),
                ],
            ];
            for (why, expected) in refused.into_iter().zip(named) {
                let why = why.unwrap_err();
                assert!(
                    matches!(&why, EditError::ForeignInInstallation { packages }
                        if packages == &expected),
                    "dry run {dry_run}: {why:?}"
                );
            }
            let after = (
                world.manifest(),
                world.lock(),
                testing::snapshot(&fake.target.path),
                testing::snapshot(&world.root.join("machine")),
            );
            assert!(before == after, "dry run {dry_run}: something was written");
        }
        assert_eq!(held(), ["alpha", "beta", "delta"]);
    }

    /// A `--global` world: the project's destination is the fake installation.
    fn global_env<'a>(
        world: &World,
        sink: &'a dyn EventSink,
    ) -> (OpEnv<'a>, testing::FakeInstallation) {
        let fake = testing::FakeInstallation::new(&world.root);
        let mut env = world.env(sink);
        env.flags.prefix = None;
        env.flags.global = true;
        fake.place(&mut env);
        (env, fake)
    }

    fn held(fake: &testing::FakeInstallation) -> Vec<String> {
        let mut held: Vec<String> = install::list_installed(&fake.roots())
            .unwrap()
            .iter()
            .map(|package| package.name.clone())
            .collect();
        held.sort();
        held
    }

    #[test]
    fn a_global_removal_of_the_named_package_goes_ahead_with_no_lock_to_read() {
        let world = World::new("global-no-lock", PROJECT);
        let options = InstallOptions {
            no_relink: true,
            ..InstallOptions::default()
        };
        let sink = Recorder::default();
        let (env, fake) = global_env(&world, &sink);
        add(&env, "gamma", DependencyTable::Dependencies, None, &options).unwrap();
        std::fs::remove_file(world.root.join(lockfile::FILE_NAME)).unwrap();
        remove(&env, "gamma", DependencyTable::Dependencies, &options).unwrap();
        assert_eq!(held(&fake), ["alpha", "beta"]);
    }

    #[test]
    fn a_global_removal_that_drops_a_dependency_from_the_lock_is_refused_naming_it() {
        let world = World::new("global-drops-dep", PROJECT);
        let options = InstallOptions {
            no_relink: true,
            ..InstallOptions::default()
        };
        let sink = Recorder::default();
        let (env, fake) = global_env(&world, &sink);
        add(&env, "gamma", DependencyTable::Dependencies, None, &options).unwrap();
        let before = (
            world.manifest(),
            world.lock(),
            testing::snapshot(&fake.target.path),
            testing::snapshot(&world.root.join("machine")),
        );
        // beta needs alpha; the lock without beta has no alpha.
        let why = remove(&env, "beta", DependencyTable::Dependencies, &options).unwrap_err();
        assert!(
            matches!(&why, EditError::ForeignInInstallation { packages }
                if packages == &[("alpha".to_string(), "2.0".to_string())]),
            "{why:?}"
        );
        let after = (
            world.manifest(),
            world.lock(),
            testing::snapshot(&fake.target.path),
            testing::snapshot(&world.root.join("machine")),
        );
        assert!(before == after, "something was written");
        assert_eq!(held(&fake), ["alpha", "beta", "gamma"]);
    }

    #[test]
    fn a_headless_project_with_no_venv_is_not_guarded_for_it_chose_the_installation() {
        let world = World::new("headless-unguarded", PROJECT);
        let fake = testing::FakeInstallation::new(&world.root);
        let sink = Recorder::default();
        let mut env = world.env(&sink);
        env.flags.prefix = None;
        env.flags.labview_version = Some("2099".into());
        env.headless = true;
        fake.place(&mut env);
        fake.with_package("delta");
        let options = InstallOptions {
            no_relink: true,
            ..InstallOptions::default()
        };
        add(&env, "gamma", DependencyTable::Dependencies, None, &options).unwrap();
        assert_eq!(held(&fake), ["alpha", "beta", "gamma"]);
    }
}
