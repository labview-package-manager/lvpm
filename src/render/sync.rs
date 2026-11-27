//! The wording of `lvpm sync`: its events, and the errors it ends in. Core's
//! errors say what went wrong; the flags and the commands to type are the CLI's.

use super::lock::{error as lock_error, stale_reason};
use super::{Renderer, Stream};
use lvpm_core::errors::{
    ArchiveFault, DirectBecause, LockError, LockIncompatible, LockedArchiveProblem, RouteError,
    SyncError,
};
use lvpm_core::events::{Event, PackageRef, RemoveReason, SyncStep};
use lvpm_core::ops::env::shell_word;

pub(super) fn render(r: &mut Renderer, event: &Event) {
    match event {
        Event::SyncLockMissing { dry_run: false, .. } => r.line(
            Stream::Out,
            "no lvpm.lock found: writing one from lvpm.toml (commit lvpm.lock with the project)",
        ),
        Event::SyncLockMissing { dry_run: true, .. } => r.line(
            Stream::Out,
            "no lvpm.lock found: one would be written from lvpm.toml",
        ),
        Event::SyncUnresolved { name, required_by } => r.line(
            Stream::Err,
            &format!(
                "warning: {name}, required by {required_by}, is not installed: no source \
                 listed it when lvpm.lock was made"
            ),
        ),
        Event::SyncMigration { removing } => r.line(Stream::Out, &migration(removing)),
        Event::SyncPlanned { steps, dry_run } => {
            r.line(Stream::Out, &plan_heading(steps));
            for step in steps {
                r.line(Stream::Out, &format!("  {}", step_line(step)));
            }
            if *dry_run {
                r.line(Stream::Out, "dry run: nothing changed");
            }
        }
        Event::SyncDeclined { removing } => {
            // The names come from feeds.
            let text = super::printable(declined(removing, &r.rerun));
            r.line(Stream::Out, &text);
        }
        Event::SyncNothingToDo => r.line(
            Stream::Out,
            "nothing to do: the destination already matches lvpm.lock",
        ),
        _ => unreachable!("routed here by Renderer::render: {event:?}"),
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// How many packages `package_list` names before `and N more`.
const PACKAGES_NAMED: usize = 5;

/// `extra 1.0, zeta 1.0`: the first five of `packages`, then `and N more`. The
/// question sync asks and the note of a first sync name packages this way.
fn package_list(packages: &[String]) -> String {
    let (shown, rest) = packages.split_at(packages.len().min(PACKAGES_NAMED));
    match rest.len() {
        0 => shown.join(", "),
        more => format!("{} and {more} more", shown.join(", ")),
    }
}

/// `2 packages` and `extra 1.0, zeta 1.0`: how many packages go, and which.
fn counted_and_listed(removing: &[PackageRef]) -> (String, String) {
    let names: Vec<String> = removing
        .iter()
        .map(|package| format!("{} {}", package.name, package.version))
        .collect();
    (
        format!("{} package{}", names.len(), plural(names.len())),
        package_list(&names),
    )
}

/// The first sync of a project: what the new lock does not list goes. It reads
/// as the question sync asks, `remove 1 package (gamma 1.0)?`, would.
fn migration(removing: &[PackageRef]) -> String {
    let (packages, list) = counted_and_listed(removing);
    format!("note: {packages} not in lvpm.lock will be removed ({list})")
}

/// The question a sync that would remove `removing` puts: `target` is the
/// LabVIEW a `--global` sync would change.
pub fn removal_question(removing: &[PackageRef], target: Option<&str>) -> String {
    let (packages, list) = counted_and_listed(removing);
    let from = target.map(|target| format!(" from {target}"));
    // The names come from feeds.
    super::printable(format!(
        "remove {packages}{} ({list})? [y/N] ",
        from.unwrap_or_default()
    ))
}

/// What a sync that would remove `removing` says when it cannot ask: why not,
/// and `rerun`, the command to go ahead. `json` is on when `--json` is why.
pub fn removal_refusal(
    removing: &[PackageRef],
    target: Option<&str>,
    json: bool,
    rerun: &str,
) -> String {
    let (packages, list) = counted_and_listed(removing);
    let from = target.map(|target| format!(" from {target}"));
    let why = match json {
        true => "--json gives no terminal to ask in",
        false => "no terminal to ask in",
    };
    format!(
        "sync would remove {packages}{}: {list}
{why}
next: {rerun}",
        from.unwrap_or_default()
    )
}

/// What the user said no to, and the two ways on: the manifest, or `rerun`, the
/// command they typed with `--yes`.
fn declined(removing: &[PackageRef], rerun: &str) -> String {
    let add = match removing {
        [only] => format!(
            "add it to lvpm.toml with lvpm add {}",
            shell_word(&only.name)
        ),
        _ => "add them to lvpm.toml".to_string(),
    };
    format!("sync cancelled: nothing was changed\nnext: {add}, or run {rerun}")
}

/// `sync: 2 to install, 1 to remove`: what is going to happen, in counts.
fn plan_heading(steps: &[SyncStep]) -> String {
    let count = |wanted: fn(&SyncStep) -> bool| steps.iter().filter(|step| wanted(step)).count();
    let parts: Vec<String> = [
        (count(|s| matches!(s, SyncStep::Install { .. })), "install"),
        (count(|s| matches!(s, SyncStep::Replace { .. })), "replace"),
        (count(|s| matches!(s, SyncStep::Remove { .. })), "remove"),
    ]
    .into_iter()
    .filter(|(number, _)| *number > 0)
    .map(|(number, what)| format!("{number} to {what}"))
    .collect();
    format!("sync: {}", parts.join(", "))
}

/// One change, with its reason where it has one.
pub(super) fn step_line(step: &SyncStep) -> String {
    match step {
        SyncStep::Install { name, version } => format!("install {name} {version}"),
        SyncStep::Replace { name, from, to } => format!("replace {name} {from} -> {to}"),
        SyncStep::Remove {
            name,
            version,
            reason,
            groups,
        } => {
            let why = match reason {
                RemoveReason::NotInLock => "not in lvpm.lock".to_string(),
                RemoveReason::DevOnly => {
                    "only dev dependencies need it, and --no-dev is given".to_string()
                }
                RemoveReason::GroupsOnly => groups_only(groups),
            };
            format!("remove {name} {version} ({why})")
        }
    }
}

/// Why a package only unselected groups need is removed.
fn groups_only(groups: &[String]) -> String {
    match groups {
        [one] => format!("only the group {one} needs it, and it is not selected"),
        [rest @ .., last] => format!(
            "only the groups {} and {last} need it, and none is selected",
            rest.join(", ")
        ),
        [] => "no selected group needs it".to_string(),
    }
}

/// What `lvpm sync` prints when it fails: what happened, then the next thing
/// to do.
pub fn error(error: &SyncError) -> String {
    match error {
        SyncError::Lock(LockError::NoLock { .. }) => {
            "there is no lvpm.lock\nnext: run `lvpm lock`, and commit lvpm.lock".to_string()
        }
        SyncError::Lock(LockError::Stale(reason)) => format!(
            "lvpm.lock is out of date: {}\nnext: run `lvpm lock` and commit lvpm.lock",
            stale_reason(reason)
        ),
        SyncError::Lock(other) => lock_error(other),
        SyncError::Archives(problems) => archives_text(problems),
        SyncError::Incomplete { done, left, cause } => incomplete_text(done, left, cause),
        SyncError::Cancelled => {
            format!("{error}; nothing was changed\nnext: run `lvpm sync` again")
        }
        SyncError::Other(cause) => format!("{cause:#}"),
    }
}

/// What `lvpm sync` prints when it fails on a headless machine where only
/// headless made it `--locked`: for a lock that is missing or out of date, the
/// way out is to lock where a person is, or to pass `--no-locked`.
pub fn headless_error(error: &SyncError) -> String {
    match error {
        SyncError::Lock(LockError::NoLock { .. }) => {
            "no lvpm.lock: run `lvpm lock` locally and commit it, or pass --no-locked".to_string()
        }
        SyncError::Lock(LockError::Stale(reason)) => format!(
            "lvpm.lock is out of date: {}\n\
             next: run `lvpm lock` locally and commit lvpm.lock, or pass --no-locked",
            stale_reason(reason)
        ),
        other => self::error(other),
    }
}

fn archives_text(problems: &[LockedArchiveProblem]) -> String {
    let mut text = format!(
        "cannot sync: {} locked archive{} could not be had, and nothing was changed:",
        problems.len(),
        plural(problems.len())
    );
    for problem in problems {
        let LockedArchiveProblem {
            name,
            version,
            sha256,
            fault,
        } = problem;
        text.push_str("\n  ");
        text.push_str(&match fault {
            ArchiveFault::NotFound => format!(
                "{name} {version}: no archive with SHA-256 {sha256} is in the cache or in any source"
            ),
            ArchiveFault::Unfetched { reason, in_full } => {
                super::lock::unfetched_line(name, version, reason, *in_full)
            }
            ArchiveFault::DiffersFromLock { found_sha256 } => format!(
                "the archive of {name} {version} does not match lvpm.lock: it has SHA-256 \
                 {found_sha256} and the lock has {sha256}, so it was not installed"
            ),
        });
    }
    text.push_str(
        "\nnext: add the archive lvpm.lock was made from with `lvpm cache add <file>`, or list a \
         folder that holds it under sources.local (`lvpm config set sources.local <dir>`), then \
         run `lvpm sync` again",
    );
    text
}

fn incomplete_text(done: &[SyncStep], left: &[SyncStep], cause: &str) -> String {
    let mut text = incomplete_body(done, left, cause);
    text.push_str("\nnext: fix what is wrong, then re-run lvpm sync to finish");
    text
}

/// What a sync that stopped part-way did and did not do, without what to do
/// about it.
pub(super) fn incomplete_body(done: &[SyncStep], left: &[SyncStep], cause: &str) -> String {
    let mut text = format!("sync stopped before it was done: {cause}");
    for (heading, steps) in [("done:", done), ("not done:", left)] {
        if steps.is_empty() {
            continue;
        }
        text.push('\n');
        text.push_str(heading);
        for step in steps {
            text.push_str(&format!("\n  {}", step_line(step)));
        }
    }
    text
}

/// What `lvpm install` prints when it cannot tell which operation it means.
pub fn route_error(error: &RouteError) -> String {
    match error {
        RouteError::NoPackage { searched_from } => format!(
            "give a package name\n\
             there is no project to sync: no lvpm.toml found in {} or above\n\
             next: name a package to install, or run this in a project, a directory with an \
             lvpm.toml",
            searched_from.display()
        ),
        RouteError::NotWithLock { option, adding } => {
            let (flag, next) = match (option, adding) {
                (LockIncompatible::NoDeps, false) => (
                    "--no-deps",
                    "drop --no-deps, or install one package without changing the project: \
                     `lvpm install <package> --no-deps --no-project --prefix <dir>`",
                ),
                (LockIncompatible::NoDeps, true) => (
                    "--no-deps",
                    "drop --no-deps, or install the package without changing the project: \
                     `lvpm install <package> --no-deps --no-project --prefix <dir>`",
                ),
                (LockIncompatible::AllowDowngrade, adding) => (
                    "--allow-downgrade",
                    match adding {
                        false => {
                            "drop --allow-downgrade (a sync installs the version lvpm.lock names, \
                             older or newer), or install one package without changing the project: \
                             `lvpm install <package> --allow-downgrade --no-project --prefix <dir>`"
                        }
                        true => {
                            "drop --allow-downgrade (a sync installs the version lvpm.lock names, \
                             older or newer), or install the package without changing the project: \
                             `lvpm install <package> --allow-downgrade --no-project --prefix <dir>`"
                        }
                    },
                ),
                (LockIncompatible::Manifest, false) => (
                    "--manifest",
                    "drop --manifest to sync this project, or name a package and install it \
                     without changing the project: \
                     `lvpm install <package> --no-project --prefix <dir>`",
                ),
                (LockIncompatible::Manifest, true) => (
                    "--manifest",
                    "drop --manifest to add <package> to this project, or install the package \
                     without changing the project: \
                     `lvpm install <package> --no-project --prefix <dir>`",
                ),
            };
            format!(
                "{flag} cannot be used in a project, because lvpm.lock decides the dependencies\n\
                 next: {next}"
            )
        }
        RouteError::UninstallNotWithLock { flag } => format!(
            "{error}\n\
             next: drop {flag}, or uninstall the package without changing the project: \
             `lvpm uninstall <package> {flag} --no-project`"
        ),
        RouteError::OnlyInAProject { flag, because } => {
            let way_in = match because {
                DirectBecause::NoPackage => "name a package",
                DirectBecause::NoProject => "drop --no-project",
                DirectBecause::NotInAProject => {
                    "run this in a project, where uninstalling a package takes it out of lvpm.toml"
                }
            };
            format!("{error}\nnext: drop {flag}, or {way_in}")
        }
        RouteError::SaveOnlyInAProject { flags, no_project } => {
            let way_in = match no_project {
                true => "drop --no-project",
                false => "run this in a project, where installing a package adds it to lvpm.toml",
            };
            // `--group` is the other meaning, and the only one an install with no package has.
            let aside = match flags.contains(&"--group") {
                true => " (with no package it installs that group)",
                false => "",
            };
            let flags = flags.join(" and ");
            format!("{error}\nnext: drop {flags}{aside}, or {way_in}")
        }
        RouteError::Other(cause) => format!("{cause:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::renderer;
    use super::*;
    use lvpm_core::lockfile::StaleReason;
    use std::path::PathBuf;

    fn install(name: &str, version: &str) -> SyncStep {
        SyncStep::Install {
            name: name.into(),
            version: version.into(),
        }
    }

    fn remove(name: &str, version: &str, reason: RemoveReason) -> SyncStep {
        SyncStep::Remove {
            name: name.into(),
            version: version.into(),
            reason,
            groups: Vec::new(),
        }
    }

    fn remove_for_groups(name: &str, groups: &[&str]) -> SyncStep {
        SyncStep::Remove {
            name: name.into(),
            version: "1.0".into(),
            reason: RemoveReason::GroupsOnly,
            groups: groups.iter().map(|group| group.to_string()).collect(),
        }
    }

    #[test]
    fn a_plan_lists_each_step_with_the_reason_a_removal_has_and_a_dry_run_says_it_changed_nothing()
    {
        let (mut renderer, out, err) = renderer();
        renderer.render(&Event::SyncPlanned {
            steps: vec![
                remove("delta", "1.0", RemoveReason::NotInLock),
                SyncStep::Replace {
                    name: "alpha".into(),
                    from: "1.0".into(),
                    to: "2.0".into(),
                },
                install("beta", "1.0"),
            ],
            dry_run: true,
        });
        assert_eq!(
            out.text(),
            "sync: 1 to install, 1 to replace, 1 to remove\n\
             \x20 remove delta 1.0 (not in lvpm.lock)\n\
             \x20 replace alpha 1.0 -> 2.0\n\
             \x20 install beta 1.0\n\
             dry run: nothing changed\n"
        );
        assert_eq!(err.text(), "");
    }

    #[test]
    fn a_dev_only_removal_says_which_flag_asked_for_it() {
        assert_eq!(
            step_line(&remove("caraya", "1.0", RemoveReason::DevOnly)),
            "remove caraya 1.0 (only dev dependencies need it, and --no-dev is given)"
        );
    }

    #[test]
    fn a_removal_for_one_group_names_it() {
        assert_eq!(
            step_line(&remove_for_groups("ci_tools", &["ci"])),
            "remove ci_tools 1.0 (only the group ci needs it, and it is not selected)"
        );
    }

    #[test]
    fn a_removal_for_several_groups_names_them_all() {
        assert_eq!(
            step_line(&remove_for_groups("caraya", &["ci", "docs"])),
            "remove caraya 1.0 (only the groups ci and docs need it, and none is selected)"
        );
        assert_eq!(
            step_line(&remove_for_groups("caraya", &["ci", "dev", "docs"])),
            "remove caraya 1.0 (only the groups ci, dev and docs need it, and none is selected)"
        );
    }

    #[test]
    fn a_removal_for_groups_is_json_with_a_string_reason_and_the_groups_only_then() {
        assert_eq!(
            serde_json::to_value(remove_for_groups("ci_tools", &["ci", "dev"])).unwrap(),
            serde_json::json!({
                "step": "remove", "name": "ci_tools", "version": "1.0",
                "reason": "groups_only", "groups": ["ci", "dev"],
            })
        );
        assert_eq!(
            serde_json::to_value(remove("gamma", "1.0", RemoveReason::NotInLock)).unwrap(),
            serde_json::json!({
                "step": "remove", "name": "gamma", "version": "1.0", "reason": "not_in_lock",
            })
        );
    }

    #[test]
    fn the_first_sync_says_what_it_takes_out_and_to_commit_the_lock() {
        let (mut renderer, out, _) = renderer();
        renderer.render(&Event::SyncLockMissing {
            path: PathBuf::from("lvpm.lock"),
            dry_run: false,
        });
        renderer.render(&Event::SyncMigration {
            removing: vec![PackageRef {
                name: "gamma".into(),
                version: "1.0".into(),
            }],
        });
        assert_eq!(
            out.text(),
            "no lvpm.lock found: writing one from lvpm.toml (commit lvpm.lock with the project)\n\
             note: 1 package not in lvpm.lock will be removed (gamma 1.0)\n"
        );
    }

    fn gone(name: &str) -> PackageRef {
        PackageRef {
            name: name.into(),
            version: "1.0".into(),
        }
    }

    #[test]
    fn a_decline_says_nothing_changed_and_the_two_ways_on_as_typed() {
        let declined = |removing: Vec<PackageRef>, rerun: &str| {
            let (renderer, out, _) = renderer();
            let mut renderer = renderer.rerun_with(rerun.into());
            renderer.render(&Event::SyncDeclined { removing });
            out.text()
        };
        assert_eq!(
            declined(vec![gone("extra")], "lvpm sync --prefix dest --yes"),
            "sync cancelled: nothing was changed\n\
             next: add it to lvpm.toml with lvpm add extra, or run lvpm sync --prefix dest --yes\n"
        );
        assert_eq!(
            declined(
                vec![gone("extra"), gone("zeta")],
                "lvpm install --prefix 'my dest' --group docs --yes"
            ),
            "sync cancelled: nothing was changed\n\
             next: add them to lvpm.toml, or run lvpm install --prefix 'my dest' --group docs --yes\n"
        );
    }

    #[test]
    fn what_a_feed_calls_a_package_cannot_send_escapes_to_the_terminal() {
        let evil = PackageRef {
            name: "x\u{1b}[31m".into(),
            version: "1.0".into(),
        };
        assert_eq!(
            removal_question(std::slice::from_ref(&evil), None),
            "remove 1 package (x?[31m 1.0)? [y/N] "
        );
        let (renderer, out, _) = renderer();
        let mut renderer = renderer.rerun_with("lvpm sync --yes".into());
        renderer.render(&Event::SyncDeclined {
            removing: vec![evil],
        });
        assert!(!out.text().contains('\u{1b}'), "{:?}", out.text());
        assert!(
            out.text()
                .starts_with("sync cancelled: nothing was changed\n")
                && out.text().contains("x?[31m"),
            "{:?}",
            out.text()
        );
    }

    #[test]
    fn a_list_of_packages_names_five_and_counts_the_rest() {
        let names =
            |count: usize| -> Vec<String> { (1..=count).map(|n| format!("p{n} 1.0")).collect() };
        assert_eq!(
            package_list(&names(5)),
            "p1 1.0, p2 1.0, p3 1.0, p4 1.0, p5 1.0"
        );
        assert_eq!(
            package_list(&names(8)),
            "p1 1.0, p2 1.0, p3 1.0, p4 1.0, p5 1.0 and 3 more"
        );
    }

    #[test]
    fn a_requirement_nobody_listed_is_a_warning_on_stderr_and_nothing_to_do_is_a_line_on_stdout() {
        let (mut renderer, out, err) = renderer();
        renderer.render(&Event::SyncUnresolved {
            name: "ni_lib_missing".into(),
            required_by: "beta".into(),
        });
        renderer.render(&Event::SyncNothingToDo);
        assert_eq!(
            err.text(),
            "warning: ni_lib_missing, required by beta, is not installed: no source listed it \
             when lvpm.lock was made\n"
        );
        assert_eq!(
            out.text(),
            "nothing to do: the destination already matches lvpm.lock\n"
        );
    }

    #[test]
    fn a_lock_a_headless_machine_refuses_says_to_lock_locally_or_pass_no_locked() {
        let missing = headless_error(&SyncError::Lock(LockError::NoLock {
            path: PathBuf::from("lvpm.lock"),
        }));
        assert_eq!(
            missing,
            "no lvpm.lock: run `lvpm lock` locally and commit it, or pass --no-locked"
        );
        let stale = headless_error(&SyncError::Lock(LockError::Stale(
            StaleReason::LabviewChanged,
        )));
        assert_eq!(
            stale,
            "lvpm.lock is out of date: labview changed\n\
             next: run `lvpm lock` locally and commit lvpm.lock, or pass --no-locked"
        );
    }

    #[test]
    fn a_failure_headless_did_not_cause_reads_as_it_does_anywhere() {
        let failure = SyncError::Incomplete {
            done: vec![],
            left: vec![install("beta", "1.0")],
            cause: "it broke".into(),
        };
        assert_eq!(headless_error(&failure), error(&failure));
    }

    #[test]
    fn a_missing_archive_names_the_package_version_and_hash_and_how_to_supply_it() {
        let text = error(&SyncError::Archives(vec![LockedArchiveProblem {
            name: "alpha".into(),
            version: "2.0".into(),
            sha256: "ab".repeat(32),
            fault: ArchiveFault::NotFound,
        }]));
        for part in [
            "alpha 2.0",
            &"ab".repeat(32),
            "nothing was changed",
            "`lvpm cache add <file>`",
            "sources.local",
        ] {
            assert!(text.contains(part), "{part:?} is not in:\n{text}");
        }
    }

    #[test]
    fn an_archive_that_could_not_be_fetched_says_why_in_the_causes_words() {
        let text = error(&SyncError::Archives(vec![LockedArchiveProblem {
            name: "alpha".into(),
            version: "2.0".into(),
            sha256: "ab".repeat(32),
            fault: ArchiveFault::Unfetched {
                reason: "alpha (github.com/o/r): not found. If it is private, run gh auth login"
                    .into(),
                in_full: false,
            },
        }]));
        assert!(
            text.contains("alpha 2.0: alpha (github.com/o/r): not found. If it is private"),
            "{text}"
        );
        assert!(!text.contains("is in the cache or in any source"), "{text}");
    }

    #[test]
    fn a_whole_sentence_about_an_archive_is_shown_as_it_is() {
        let text = error(&SyncError::Archives(vec![LockedArchiveProblem {
            name: "alpha".into(),
            version: "1.1.0".into(),
            sha256: "ab".repeat(32),
            fault: ArchiveFault::Unfetched {
                reason: "cannot fetch alpha 1.1.0 by digest from source \"team\": refused".into(),
                in_full: true,
            },
        }]));
        assert!(
            text.contains("\n  cannot fetch alpha 1.1.0 by digest from source \"team\": refused\n"),
            "{text}"
        );
    }

    #[test]
    fn an_archive_with_other_bytes_is_said_not_to_match_the_lock() {
        let text = error(&SyncError::Archives(vec![LockedArchiveProblem {
            name: "alpha".into(),
            version: "2.0".into(),
            sha256: "ab".repeat(32),
            fault: ArchiveFault::DiffersFromLock {
                found_sha256: "cd".repeat(32),
            },
        }]));
        assert!(
            text.contains("the archive of alpha 2.0 does not match lvpm.lock"),
            "{text}"
        );
        assert!(text.contains(&"cd".repeat(32)), "{text}");
    }

    #[test]
    fn a_sync_that_stopped_lists_what_is_done_and_what_is_not_and_ends_with_how_to_finish() {
        let text = error(&SyncError::Incomplete {
            done: vec![install("alpha", "2.0")],
            left: vec![install("beta", "1.0")],
            cause: "it broke".into(),
        });
        assert_eq!(
            text,
            "sync stopped before it was done: it broke\n\
             done:\n  install alpha 2.0\n\
             not done:\n  install beta 1.0\n\
             next: fix what is wrong, then re-run lvpm sync to finish"
        );
    }

    #[test]
    fn a_missing_or_stale_lock_says_to_lock_and_commit() {
        let missing = error(&SyncError::Lock(LockError::NoLock {
            path: PathBuf::from("lvpm.lock"),
        }));
        assert!(missing.starts_with("there is no lvpm.lock"), "{missing}");
        let stale = error(&SyncError::Lock(LockError::Stale(
            lvpm_core::lockfile::StaleReason::DependencyAdded {
                name: "gamma".into(),
                table: lvpm_core::manifest_edit::DependencyTable::Dependencies,
            },
        )));
        assert_eq!(
            stale,
            "lvpm.lock is out of date: dependency \"gamma\" was added\n\
             next: run `lvpm lock` and commit lvpm.lock"
        );
    }

    #[test]
    fn a_cancelled_sync_says_cancelled_and_not_that_locking_was() {
        assert_eq!(
            error(&SyncError::Cancelled),
            "cancelled; nothing was changed\nnext: run `lvpm sync` again"
        );
    }

    #[test]
    fn a_flag_a_lock_cannot_honor_is_named_with_what_to_do_instead() {
        for adding in [false, true] {
            let text = route_error(&RouteError::NotWithLock {
                option: LockIncompatible::NoDeps,
                adding,
            });
            assert!(
                text.starts_with(
                    "--no-deps cannot be used in a project, because lvpm.lock decides the dependencies\nnext: "
                ),
                "{text}"
            );
            // The way out of it is never the install it refuses.
            assert!(
                text.ends_with("`lvpm install <package> --no-deps --no-project --prefix <dir>`"),
                "{text}"
            );
        }
    }

    #[test]
    fn allow_downgrade_in_a_project_says_the_lock_names_the_version_and_names_no_project() {
        for adding in [false, true] {
            let text = route_error(&RouteError::NotWithLock {
                option: LockIncompatible::AllowDowngrade,
                adding,
            });
            assert!(
                text.starts_with(
                    "--allow-downgrade cannot be used in a project, because lvpm.lock decides the dependencies\nnext: drop --allow-downgrade (a sync installs the version lvpm.lock names, older or newer), or install "
                ),
                "{text}"
            );
            assert!(
                text.ends_with(
                    "without changing the project: `lvpm install <package> --allow-downgrade --no-project --prefix <dir>`"
                ),
                "{text}"
            );
        }
    }

    #[test]
    fn an_uninstall_flag_the_lock_overrules_names_no_project_as_the_way_out() {
        let text = route_error(&RouteError::UninstallNotWithLock { flag: "--force" });
        assert_eq!(
            text,
            "--force cannot be used in a project, because lvpm.lock decides what stays\n\
             next: drop --force, or uninstall the package without changing the project: \
             `lvpm uninstall <package> --force --no-project`"
        );
    }

    #[test]
    fn a_project_only_flag_on_a_direct_uninstall_is_named_and_the_way_out_is_a_project() {
        let text = route_error(&RouteError::OnlyInAProject {
            flag: "--dev",
            because: DirectBecause::NotInAProject,
        });
        assert_eq!(
            text,
            "--dev can only be used when uninstalling a package from a project\n\
             next: drop --dev, or run this in a project, where uninstalling a package takes it \
             out of lvpm.toml"
        );
    }

    #[test]
    fn a_saving_flag_on_a_direct_install_says_where_to_go_and_group_names_its_other_meaning() {
        let said = |flags: &[&'static str], no_project| {
            route_error(&RouteError::SaveOnlyInAProject {
                flags: flags.to_vec(),
                no_project,
            })
        };
        let refusal = "can only be used when installing a package into a project, which saves it in lvpm.toml\n";
        assert_eq!(
            said(&["--dev"], false),
            format!(
                "--dev {refusal}next: drop --dev, or run this in a project, where installing a package adds it to lvpm.toml"
            )
        );
        assert_eq!(
            said(&["--group"], true),
            format!(
                "--group {refusal}next: drop --group (with no package it installs that group), or drop --no-project"
            )
        );
    }

    #[test]
    fn every_saving_flag_given_is_named_in_the_refusal_and_in_what_to_drop() {
        let said = |flags: &[&'static str]| {
            route_error(&RouteError::SaveOnlyInAProject {
                flags: flags.to_vec(),
                no_project: false,
            })
        };
        // clap refuses --dev and --index with --no-project, so these two are
        // only ever met outside a project.
        assert_eq!(
            said(&["--dev", "--index"]),
            "--dev and --index can only be used when installing a package into a project, which saves it in lvpm.toml\n\
             next: drop --dev and --index, or run this in a project, where installing a package adds it to lvpm.toml"
        );
        assert_eq!(
            said(&["--group", "--index"]),
            "--group and --index can only be used when installing a package into a project, which saves it in lvpm.toml\n\
             next: drop --group and --index (with no package it installs that group), or run this in a project, where installing a package adds it to lvpm.toml"
        );
    }

    #[test]
    fn the_way_in_to_a_project_only_flag_follows_why_the_uninstall_is_direct() {
        let said = |because| {
            route_error(&RouteError::OnlyInAProject {
                flag: "--dry-run",
                because,
            })
        };
        let refusal = "--dry-run can only be used when uninstalling a package from a project\n";
        assert_eq!(
            said(DirectBecause::NoPackage),
            format!("{refusal}next: drop --dry-run, or name a package")
        );
        assert_eq!(
            said(DirectBecause::NoProject),
            format!("{refusal}next: drop --dry-run, or drop --no-project")
        );
    }

    #[test]
    fn manifest_in_a_project_says_to_sync_or_to_add_by_whether_a_package_is_named() {
        let said = |adding| {
            route_error(&RouteError::NotWithLock {
                option: LockIncompatible::Manifest,
                adding,
            })
        };
        let syncing = said(false);
        assert!(
            syncing.starts_with(
                "--manifest cannot be used in a project, because lvpm.lock decides the dependencies\nnext: drop --manifest to sync this project, "
            ),
            "{syncing}"
        );
        let adding = said(true);
        assert!(
            adding.contains("next: drop --manifest to add <package> to this project, "),
            "{adding}"
        );
        assert!(!adding.contains("sync this project"), "{adding}");
        for text in [&syncing, &adding] {
            assert!(
                text.ends_with("`lvpm install <package> --no-project --prefix <dir>`"),
                "{text}"
            );
        }
    }

    #[test]
    fn nothing_to_install_outside_a_project_starts_with_the_text_it_always_had_and_says_where_it_looked()
     {
        let text = route_error(&RouteError::NoPackage {
            searched_from: PathBuf::from("here"),
        });
        assert!(text.starts_with("give a package name\n"), "{text}");
        assert!(
            text.contains("no lvpm.toml found in here or above"),
            "{text}"
        );
        assert!(text.lines().last().unwrap().starts_with("next: "), "{text}");
    }
}
