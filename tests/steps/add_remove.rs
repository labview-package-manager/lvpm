//! Steps of the add and remove scenarios. The project is the one every
//! scenario starts from: fixture packages in the sandbox's local folder, a
//! project for LabVIEW 2024 that depends on `beta`, locked and synced into the
//! `--prefix` tree `dest`. The `@core` scenarios call `lvpm-core` in this
//! process; the rest run the built `lvpm`.
//!
//! Nothing here reaches a real LabVIEW installation: every destination is
//! `dest` or the project's venv, and the sandbox refuses a command that would
//! do otherwise (`real_installation_risk`).

use super::lock::name_and_version_pairs;
use super::sync::{printed_plain, quoted};
use crate::CliWorld;
use crate::support::events::RecordingSink;
use crate::support::feed::BROKEN_FEED_DIR;
use crate::support::fixtures::{
    ADD_PACKAGES, DELTA_1, FixturePackage, INSTALL_PACKAGES, OMEGA_NEEDING_ALPHA_2,
    OMEGA_NEEDING_LABVIEW_2026,
};
use crate::support::golden::Output;
use crate::support::project::{MANIFEST_FILE, ProjectManifest};
use crate::support::sandbox::{DEST_DIR, LABVIEW_YEAR, LOCAL_PACKAGES_DIR, Sandbox};
use crate::support::venv::VENV_DIR;
use cucumber::{given, then, when};
use lvpm_core::config::Config;
use lvpm_core::errors::{EditError, RouteError, SyncError};
use lvpm_core::install;
use lvpm_core::manifest_edit::{self, DependencyTable};
use lvpm_core::ops::OpEnv;
use lvpm_core::ops::add_remove;
use lvpm_core::ops::install::InstallOptions;
use lvpm_core::ops::route::{self, InstallMode, InstallRequest};
use lvpm_core::target::Roots;
use std::path::{Path, PathBuf};

/// The LabVIEW the project is meant for.
const PROJECT_LABVIEW: &str = "2024";
/// The venv directory the config names; no `@core` scenario here has a venv.
const CONFIG_VENV_DIR: &str = ".lv-venv";

/// What an add or remove scenario has seen of a call into core, and the
/// second world of the one that compares two.
#[derive(Debug, Default)]
pub struct AddRemoveState {
    /// Why the last call into core failed, if it did.
    failure: Option<EditError>,
    /// What routing an install request gave.
    routed: Option<Result<InstallMode, RouteError>>,
    twin: Option<Twin>,
}

#[derive(Debug)]
struct Twin {
    sandbox: Sandbox,
    output: Output,
}

/// What the `Background` gives a world, once its fixture packages are in the
/// sandbox's local folder: a project that depends on `name`, locked and synced
/// into `dest`.
fn set_up_project(sandbox: &Sandbox, name: &str, constraint: &str) {
    sandbox.use_local_project(Some(PROJECT_LABVIEW));
    manifest_edit::set_file(
        &sandbox.work_dir().join(MANIFEST_FILE),
        &DependencyTable::Dependencies,
        name,
        constraint,
    )
    .expect("edit the manifest");
    sandbox.make_empty_dest();
    sandbox.setup_lvpm(&["lock"]);
    sandbox.setup_lvpm(&["sync", "--prefix", DEST_DIR]);
}

/// What a `[table]` of the manifest has: each name with its constraint.
fn entries_of(manifest: &str, table: &str) -> Vec<(String, String)> {
    let parsed: toml::Table = manifest.parse().expect("the manifest is TOML");
    parsed
        .get(table)
        .and_then(toml::Value::as_table)
        .map(|entries| {
            entries
                .iter()
                .map(|(name, value)| (name.clone(), value.as_str().unwrap_or_default().into()))
                .collect()
        })
        .unwrap_or_default()
}

/// The lines of `text`, trimmed.
fn lines_of(text: &str) -> Vec<&str> {
    text.lines().map(str::trim).collect()
}

/// Every file under `dir`, as text.
fn files_under(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        found.push(path.to_string_lossy().into_owned());
        if path.is_dir() {
            found.extend(files_under(&path));
        }
    }
    found
}

impl CliWorld {
    /// Remember the manifest, the lock and the destination as they are now.
    fn note_state(&mut self) {
        self.note_the_project();
        self.lock.manifest_before = Some(self.manifest_bytes());
    }

    fn manifest_text_before(&self) -> String {
        let before = self.lock.manifest_before.clone();
        String::from_utf8(before.expect("the manifest was noted")).expect("the manifest is text")
    }

    /// The command printed `part` on stdout, as part of a line.
    fn assert_said(&self, part: &str) {
        assert!(
            self.output().stdout.lines().any(|line| line.contains(part)),
            "{part:?} is not in a line of:\n{}",
            self.printed()
        );
    }

    fn is_locked(&self, name: &str) -> bool {
        self.locked_versions()
            .iter()
            .any(|(locked, _)| locked.eq_ignore_ascii_case(name))
    }

    /// An environment that reads the sandbox's local packages, from the
    /// project in the working directory, with no destination flag.
    fn plain_env<'s>(&self, sink: &'s RecordingSink) -> OpEnv<'s> {
        let config = Config {
            log_dir: Default::default(),
            venv_title: Default::default(),
            cache: self.sandbox.cache_dir(),
            hooks_run: Vec::new(),
            local_sources: vec![self.sandbox.root().join(LOCAL_PACKAGES_DIR)],
            venv_dir: PathBuf::from(CONFIG_VENV_DIR),
        };
        OpEnv::new(sink, config, Vec::new(), self.sandbox.work_dir())
    }
}

// ---- the project ----

#[given(
    r#"fixture packages "alpha" 1.0 and 2.0, "beta" 1.0 requiring "alpha" >= 1.0, "gamma" 0.5 and 1.0, "caraya" 1.0"#
)]
async fn fixture_packages(world: &mut CliWorld) {
    let ids: Vec<String> = ADD_PACKAGES.iter().map(FixturePackage::id).collect();
    assert_eq!(
        ids,
        [
            "alpha-1.0",
            "alpha-2.0",
            "beta-1.0",
            "gamma-0.5",
            "gamma-1.0",
            "caraya-1.0"
        ],
        "the feature and the fixture table disagree"
    );
    world.publish(&ADD_PACKAGES.iter().collect::<Vec<_>>());
}

#[given(
    r#"fixture packages "alpha" 1.0, "app" 1.0 requiring "lib" >= 1.0, "app2" 1.0 requiring "lib" >= 1.0, "lib" 1.0, "caraya" 1.0"#
)]
async fn install_fixture_packages(world: &mut CliWorld) {
    let ids: Vec<String> = INSTALL_PACKAGES.iter().map(FixturePackage::id).collect();
    assert_eq!(
        ids,
        ["alpha-1.0", "app-1.0", "app2-1.0", "lib-1.0", "caraya-1.0"],
        "the feature and the fixture table disagree"
    );
    world.publish(&INSTALL_PACKAGES.iter().collect::<Vec<_>>());
}

#[given(
    regex = r#"^a project with labview "2024" depending on "([^"]+)" "([^"]*)", locked and synced into scratch destination "dest"$"#
)]
async fn the_project(world: &mut CliWorld, name: String, constraint: String) {
    set_up_project(&world.sandbox, &name, &constraint);
    world.note_state();
}

#[given("a project")]
async fn a_project(world: &mut CliWorld) {
    let manifest = world.sandbox.work_dir().join(MANIFEST_FILE);
    assert!(manifest.is_file(), "there is no project");
}

#[given(regex = r#"^no lvpm\.lock exists and "([^"]+)" (\S+) is installed in dest$"#)]
async fn no_lock_and_a_package_outside_it(world: &mut CliWorld, name: String, version: String) {
    assert_eq!(
        (DELTA_1.name, DELTA_1.version),
        (name.as_str(), version.as_str())
    );
    std::fs::remove_file(world.lock_path()).expect("remove the lock");
    world.publish(&[&DELTA_1]);
    world.install_into_dest(&name, &version);
    world.note_state();
}

#[given(regex = r#"^every "([^"]+)" version needs LabVIEW (\d{4})$"#)]
async fn every_version_needs_a_newer_labview(world: &mut CliWorld, name: String, year: String) {
    let package = &OMEGA_NEEDING_LABVIEW_2026;
    assert_eq!(package.name, name);
    assert_eq!(
        package.lv_min,
        format!("{:.1}", year.parse::<f64>().expect("a year") - 2000.0)
    );
    world.publish(&[package]);
}

#[given(regex = r#"^the project also depends on "([^"]+)" "([^"]*)", locked and synced$"#)]
async fn the_project_also_depends_on(world: &mut CliWorld, name: String, constraint: String) {
    world.depend(DependencyTable::Dependencies, &name, &constraint);
    world.lock_the_project();
    world.sync_into_dest();
    world.note_state();
}

#[given(regex = r#"^the project is also synced into scratch destination "([^"]+)"$"#)]
async fn the_project_is_also_synced_into(world: &mut CliWorld, folder: String) {
    world.sandbox.setup_lvpm(&["sync", "--prefix", &folder]);
    world.note_state();
}

/// `"caraya" 1.0 is installed in dest with "lvpm install caraya --no-project --prefix dest"`
#[given(regex = r#"^"([^"]+)" (\S+) is installed in dest with "lvpm ([^"]+)"$"#)]
async fn installed_in_dest_with(
    world: &mut CliWorld,
    name: String,
    version: String,
    arguments: String,
) {
    let arguments: Vec<&str> = arguments.split_whitespace().collect();
    world.sandbox.setup_lvpm(&arguments);
    let pair = (name, version);
    assert!(world.installed().contains(&pair), "{pair:?} is not in dest");
    world.note_state();
}

#[given(regex = r#"^dev dependency "([^"]+)" "([^"]*)", locked and synced$"#)]
async fn a_dev_dependency_locked_and_synced(
    world: &mut CliWorld,
    name: String,
    constraint: String,
) {
    world.depend(DependencyTable::DevDependencies, &name, &constraint);
    world.lock_the_project();
    world.sync_into_dest();
    world.note_state();
}

#[given(regex = r#"^dev dependency "([^"]+)" "([^"]*)"$"#)]
async fn a_dev_dependency(world: &mut CliWorld, name: String, constraint: String) {
    world.depend(DependencyTable::DevDependencies, &name, &constraint);
    world.note_state();
}

#[given("lvpm.toml contains comments")]
async fn the_manifest_has_comments(world: &mut CliWorld) {
    let text = world.manifest_text();
    let entry = "beta = \">=1.0\"";
    assert!(text.contains(entry), "{text}");
    let commented = format!(
        "# a project that wants its comments kept\n{}",
        text.replace(
            entry,
            &format!("# the one thing it needs\n{entry} # keep this")
        )
    );
    std::fs::write(world.sandbox.work_dir().join(MANIFEST_FILE), commented)
        .expect("write the manifest");
    world.note_state();
}

#[given(
    regex = r#"^lvpm\.toml pins "([^"]+)" = "([^"]*)" and the only "([^"]+)" requires "([^"]+)" >= (\S+)$"#
)]
async fn the_manifest_pins_a_package_the_only_omega_cannot_have(
    world: &mut CliWorld,
    pinned: String,
    pin: String,
    only: String,
    needs: String,
    floor: String,
) {
    let package = &OMEGA_NEEDING_ALPHA_2;
    assert_eq!(package.name, only);
    assert!(package.requires.contains(&(needs.as_str(), floor.as_str())));
    world.publish(&[package]);
    world.depend(DependencyTable::Dependencies, &pinned, &pin);
    world.note_state();
}

/// The package is listed by a feed of its own, under the project's `[sources]`.
#[given(
    regex = r#"^the fixture "([^"]+)" (\S+) has a valid index entry and a fixed SHA-256, but its archive is not a valid zip$"#
)]
async fn a_package_with_an_archive_that_is_not_a_zip(
    world: &mut CliWorld,
    name: String,
    version: String,
) {
    assert_eq!((name.as_str(), version.as_str()), ("broken_zip", "1.0"));
    let source = world.sandbox.feed().source_url(BROKEN_FEED_DIR);
    let manifest = ProjectManifest::new()
        .labview(PROJECT_LABVIEW)
        .source("broken", &source);
    std::fs::write(
        world.sandbox.work_dir().join(MANIFEST_FILE),
        manifest.render(),
    )
    .expect("write the manifest");
    world.depend(DependencyTable::Dependencies, "beta", ">=1.0");
    world.lock_the_project();
    world.note_state();
}

/// The recording was made with nothing of the project about: no manifest, no
/// packages of its own, nothing in the destination, and the two default feeds
/// in the cache so that the run asks for neither.
#[given(
    "a directory without lvpm.toml, no local packages and no dest, with the default feeds cached"
)]
async fn a_bare_directory(world: &mut CliWorld) {
    std::fs::remove_file(world.sandbox.work_dir().join(MANIFEST_FILE))
        .expect("remove the manifest");
    world.sandbox.clear_local_packages();
    std::fs::remove_dir_all(world.dest_dir()).expect("remove dest");
    world.sandbox.setup_lvpm(&["search", "a"]);
}

// ---- running ----

#[when(regex = r#"^I run "([^"]+)" \(argument quoted\)$"#)]
async fn run_with_a_quoted_argument(world: &mut CliWorld, command_line: String) {
    // The harness hands each word to `lvpm` as one argument, as quotes would.
    world.run(&command_line, None);
}

#[when(regex = r#"^I run "([^"]+)" inside the project$"#)]
async fn run_inside_the_project(world: &mut CliWorld, command_line: String) {
    world.run(&command_line, None);
}

#[when(regex = r#"^I run "([^"]+)", and "([^"]+)" in an identical world$"#)]
async fn run_in_two_worlds(world: &mut CliWorld, first: String, second: String) {
    let twin = Sandbox::new();
    for package in ADD_PACKAGES {
        twin.put_local_package(package);
    }
    set_up_project(&twin, "beta", ">=1.0");
    twin.detect_labview();
    twin.use_loopback_project_for_labview(LABVIEW_YEAR);
    twin.setup_lvpm(&["venv", "create", "--labview-version", LABVIEW_YEAR]);
    world.run(&first, None);
    let mut words = second.split_whitespace();
    assert_eq!(words.next(), Some("lvpm"), "{second}");
    let args: Vec<&str> = words.collect();
    let output = twin.normalised(&twin.run_lvpm(&args, None));
    world.add_remove.twin = Some(Twin {
        sandbox: twin,
        output,
    });
}

// ---- a call into core ----

/// The first file `name` installed becomes a folder, which cannot be removed as
/// a file: the uninstall of that package fails. Core runs in this process, so
/// it reads the install manifests' paths from this process's directory, not
/// the project's: `dest` is installed again under its absolute path first.
#[given(expr = "an OpEnv whose install session fails when it uninstalls {string}")]
async fn an_uninstall_that_fails(world: &mut CliWorld, name: String) {
    let dest = world.dest_dir();
    std::fs::remove_dir_all(&dest).expect("empty dest");
    world.sandbox.make_empty_dest();
    let prefix = dest.to_str().expect("a path of text");
    world.sandbox.setup_lvpm(&["sync", "--prefix", prefix]);
    let manifest =
        install::read_manifest(&Roots::scratch(&dest), &name).expect("the package is installed");
    let file = PathBuf::from(manifest.files.first().expect("the package has a file"));
    assert!(file.is_absolute(), "{}", file.display());
    std::fs::remove_file(&file).expect("remove the installed file");
    std::fs::create_dir(&file).expect("put a folder where the file was");
    world.note_state();
}

#[when(expr = "core removes {string}")]
async fn core_removes(world: &mut CliWorld, name: String) {
    let sink = RecordingSink::default();
    let mut env = world.plain_env(&sink);
    env.flags.prefix = Some(world.dest_dir());
    let outcome = add_remove::remove(
        &env,
        &name,
        DependencyTable::Dependencies,
        &InstallOptions::default(),
    );
    world.core_events = sink.events();
    world.add_remove.failure = outcome.err();
}

#[then(expr = "the result is a sync error naming {string}")]
async fn a_sync_error_naming(world: &mut CliWorld, name: String) {
    match &world.add_remove.failure {
        Some(EditError::Sync(SyncError::Incomplete { left, cause, .. })) => assert!(
            left.iter().any(|step| step.name() == name),
            "{name} is not among what is left: {left:?} ({cause})"
        ),
        other => panic!("expected a sync error naming {name}, got {other:?}"),
    }
}

/// `lvpm install <args>` as the routing table writes it: `(none)` is no
/// argument at all.
#[when(expr = "core routes {string}")]
async fn core_routes(world: &mut CliWorld, command_line: String) {
    let mut words = command_line.split_whitespace();
    assert_eq!(
        (words.next(), words.next()),
        (Some("lvpm"), Some("install")),
        "{command_line}"
    );
    let sink = RecordingSink::default();
    let mut env = world.plain_env(&sink);
    let (mut package, mut no_deps, mut no_project) = (None, false, false);
    while let Some(word) = words.next() {
        match word {
            "(none)" => {}
            "--global" => env.flags.global = true,
            "--no-deps" => no_deps = true,
            "--no-project" => no_project = true,
            "--prefix" => env.flags.prefix = words.next().map(PathBuf::from),
            "--labview-version" => env.flags.labview_version = words.next().map(str::to_string),
            named => package = Some(named),
        }
    }
    let request = InstallRequest {
        package,
        manifest: None,
        no_deps,
        no_project,
        saving_flag: None,
    };
    world.add_remove.routed = Some(route::install_mode(&env, &request));
}

#[then(regex = r"^the mode is (Add|Additive|Sync)$")]
async fn the_mode_is(world: &mut CliWorld, mode: String) {
    match &world.add_remove.routed {
        Some(Ok(routed)) => assert_eq!(format!("{routed:?}"), mode),
        other => panic!("expected the mode {mode}, got {other:?}"),
    }
}

#[then(regex = r#"^the mode is error "([^"]+)"$"#)]
async fn the_mode_is_an_error(world: &mut CliWorld, said: String) {
    match &world.add_remove.routed {
        Some(Err(why)) => assert!(why.to_string().contains(&said), "{why}"),
        other => panic!("expected the error {said:?}, got {other:?}"),
    }
}

// ---- what an add or a remove did ----

#[then(regex = r#"^lvpm\.lock and dest include "([^"]+)" (\S+)$"#)]
async fn the_lock_and_dest_include(world: &mut CliWorld, name: String, version: String) {
    world.assert_succeeded();
    let pair = (name, version);
    assert!(
        world.locked_versions().contains(&pair),
        "{pair:?} is not locked"
    );
    assert!(
        world.installed().contains(&pair),
        "{pair:?} is not installed"
    );
}

/// `dest` or `"other"`: a destination folder of the work directory.
fn folder_named(world: &CliWorld, said: &str) -> PathBuf {
    world.sandbox.work_dir().join(said.trim_matches('"'))
}

/// `lvpm.lock and "other" include "caraya" 1.0 and "alpha" 1.0`
#[then(regex = r#"^lvpm\.lock and ("[^"]+") include ("[^"]+" \S+(?: and "[^"]+" \S+)*)$"#)]
async fn the_lock_and_a_folder_include(world: &mut CliWorld, folder: String, packages: String) {
    world.assert_succeeded();
    let installed = world.installed_at(&folder_named(world, &folder));
    for pair in name_and_version_pairs(&packages) {
        assert!(
            world.locked_versions().contains(&pair),
            "{pair:?} is not locked"
        );
        assert!(
            installed.contains(&pair),
            "{pair:?} is not in {folder}: {installed:?}"
        );
    }
}

#[then(regex = r#"^(dest|"[^"]+") includes "([^"]+)" (\S+)$"#)]
async fn a_folder_includes(world: &mut CliWorld, folder: String, name: String, version: String) {
    world.assert_succeeded();
    let pair = (name, version);
    let installed = world.installed_at(&folder_named(world, &folder));
    assert!(
        installed.contains(&pair),
        "{pair:?} is not in {folder}: {installed:?}"
    );
}

#[then(regex = r#"^dest does not include "([^"]+)"$"#)]
async fn dest_does_not_include(world: &mut CliWorld, name: String) {
    world.assert_succeeded();
    assert!(!world.is_installed(&name), "{name} is installed in dest");
}

#[then(regex = r#"^(dest|"[^"]+") still includes "([^"]+)" (\S+)$"#)]
async fn a_folder_still_includes(
    world: &mut CliWorld,
    folder: String,
    name: String,
    version: String,
) {
    a_folder_includes(world, folder, name, version).await;
}

#[then(regex = r#"^the output says "([^"]+)" and names "([^"]+)"$"#)]
async fn the_output_says_and_names(world: &mut CliWorld, said: String, named: String) {
    world.assert_succeeded();
    for part in [said, named] {
        assert!(
            world.printed().contains(&part),
            "{part:?} is not in:\n{}",
            world.printed()
        );
    }
}

#[then(
    regex = r#"^lvpm\.toml says "([^"]+)" = "([^"]*)" and the output says the constraint was updated$"#
)]
async fn the_manifest_says_and_the_output_says_updated(
    world: &mut CliWorld,
    name: String,
    constraint: String,
) {
    world.assert_succeeded();
    let entries = entries_of(&world.manifest_text(), "dependencies");
    assert!(entries.contains(&(name.clone(), constraint)), "{entries:?}");
    world.assert_said(&format!("lvpm.toml was updated: {name} \""));
}

#[then(regex = r#"^\[dev-dependencies\] says "([^"]+)" = "([^"]*)"$"#)]
async fn the_dev_table_says(world: &mut CliWorld, name: String, constraint: String) {
    world.assert_succeeded();
    let entries = entries_of(&world.manifest_text(), "dev-dependencies");
    assert!(entries.contains(&(name, constraint)), "{entries:?}");
}

#[then(regex = r#"^\[dependencies\] itself has the "([^"]+)" line$"#)]
async fn the_dependencies_table_itself_has(world: &mut CliWorld, name: String) {
    world.assert_succeeded();
    let entries = entries_of(&world.manifest_text(), "dependencies");
    let line = entries.iter().find(|(entry, _)| *entry == name);
    assert!(
        line.is_some_and(|(_, constraint)| !constraint.is_empty()),
        "{entries:?}"
    );
}

#[then(regex = r#"^lvpm\.toml differs from before by exactly the "([^"]+)" line$"#)]
async fn the_manifest_differs_by_one_line(world: &mut CliWorld, name: String) {
    world.assert_succeeded();
    let (before, after) = (world.manifest_text_before(), world.manifest_text());
    let line = after
        .split_inclusive('\n')
        .find(|line| line.starts_with(&format!("{name} = ")))
        .unwrap_or_else(|| panic!("no {name} line in:\n{after}"));
    assert_eq!(
        after.replacen(line, "", 1),
        before,
        "something else changed"
    );
}

#[then("the output shows the manifest line, the lock changes and the sync plan")]
async fn the_dry_run_shows_the_plan(world: &mut CliWorld) {
    world.assert_succeeded();
    let stdout = world.output().stdout.clone();
    let lines = lines_of(&stdout);
    for shown in [
        "lvpm.toml would get: gamma = \">=1.0\"",
        "lvpm.lock would be updated:",
        "+ gamma 1.0",
        "install gamma 1.0",
        "dry run: nothing changed",
    ] {
        assert!(
            lines.contains(&shown),
            "{shown:?} is not a line of:\n{stdout}"
        );
    }
}

#[then(regex = r#"^the output shows the removed manifest line and "([^"]+)"$"#)]
async fn the_dry_run_shows_the_removal(world: &mut CliWorld, planned: String) {
    world.assert_succeeded();
    let stdout = world.output().stdout.clone();
    let lines = lines_of(&stdout);
    let name = planned.split_whitespace().nth(1).expect("a package name");
    let line = format!("lvpm.toml would lose: {name} = ");
    assert!(
        lines.iter().any(|shown| shown.starts_with(&line)),
        "{line:?} is not a line of:\n{stdout}"
    );
    assert!(
        lines.iter().any(|shown| shown.starts_with(&planned)),
        "{planned:?} is not a line of:\n{stdout}"
    );
}

#[then("lvpm.toml, lvpm.lock and dest are byte-identical to before")]
async fn nothing_in_the_project_changed(world: &mut CliWorld) {
    world.assert_succeeded();
    world.assert_nothing_changed();
}

#[then("nothing changed")]
async fn nothing_changed(world: &mut CliWorld) {
    world.assert_nothing_changed();
}

impl CliWorld {
    fn assert_nothing_changed(&self) {
        self.assert_dest_unchanged();
        assert_eq!(self.lock_bytes(), self.lock.before, "the lock changed");
        assert_eq!(
            self.manifest_bytes(),
            self.noted().manifest,
            "the manifest changed"
        );
    }
}

#[then(
    regex = r#"^the output says no lvpm\.lock was found, names "([^"]+)" as to be removed, and says to commit lvpm\.lock$"#
)]
async fn the_output_explains_the_migration(world: &mut CliWorld, name: String) {
    world.assert_succeeded();
    world.assert_said("no lvpm.lock found");
    let removing = world
        .output()
        .stdout
        .lines()
        .find(|line| line.contains("will be removed"))
        .unwrap_or_else(|| panic!("nothing is said to be removed in:\n{}", world.printed()))
        .to_string();
    assert!(removing.contains(&name), "{removing}");
    world.assert_said("commit lvpm.lock");
}

#[then(regex = r#"^"([^"]+)" is gone from lvpm\.toml$"#)]
async fn gone_from_the_manifest(world: &mut CliWorld, name: String) {
    world.assert_succeeded();
    let entries = entries_of(&world.manifest_text(), "dependencies");
    assert!(
        entries.iter().all(|(entry, _)| *entry != name),
        "{entries:?}"
    );
}

#[then(regex = r#"^"([^"]+)" is gone from lvpm\.toml, lvpm\.lock and dest$"#)]
async fn gone_from_everywhere(world: &mut CliWorld, name: String) {
    gone_from_the_manifest(world, name.clone()).await;
    assert!(!world.is_locked(&name), "{name} is still locked");
    assert!(!world.is_installed(&name), "{name} is still installed");
}

#[then(regex = r#"^"([^"]+)" is gone from lvpm\.toml, lvpm\.lock and ("[^"]+")$"#)]
async fn gone_from_the_manifest_the_lock_and_a_folder(
    world: &mut CliWorld,
    name: String,
    folder: String,
) {
    gone_from_the_manifest(world, name.clone()).await;
    assert!(!world.is_locked(&name), "{name} is still locked");
    let installed = world.installed_at(&folder_named(world, &folder));
    assert!(
        installed.iter().all(|(entry, _)| *entry != name),
        "{name} is still in {folder}: {installed:?}"
    );
}

#[then(regex = r#"^"([^"]+)" and "([^"]+)" are gone from lvpm\.toml, lvpm\.lock and dest$"#)]
async fn both_are_gone_from_everywhere(world: &mut CliWorld, first: String, second: String) {
    gone_from_everywhere(world, first).await;
    gone_from_everywhere(world, second).await;
}

#[then(regex = r#"^"([^"]+)" is gone from lvpm\.lock and dest$"#)]
async fn gone_from_the_lock_and_dest(world: &mut CliWorld, name: String) {
    world.assert_succeeded();
    assert!(!world.is_locked(&name), "{name} is still locked");
    assert!(!world.is_installed(&name), "{name} is still installed");
}

#[then(regex = r#"^"([^"]+)" is gone from \[dev-dependencies\]$"#)]
async fn gone_from_the_dev_table(world: &mut CliWorld, name: String) {
    world.assert_succeeded();
    let entries = entries_of(&world.manifest_text(), "dev-dependencies");
    assert!(
        entries.iter().all(|(entry, _)| *entry != name),
        "{entries:?}"
    );
}

#[then(regex = r#"^"([^"]+)" is gone from \[dev-dependencies\], lvpm\.lock and dest$"#)]
async fn gone_from_the_dev_table_the_lock_and_dest(world: &mut CliWorld, name: String) {
    world.assert_succeeded();
    let entries = entries_of(&world.manifest_text(), "dev-dependencies");
    assert!(
        entries.iter().all(|(entry, _)| *entry != name),
        "{entries:?}"
    );
    assert!(!world.is_locked(&name), "{name} is still locked");
    assert!(!world.is_installed(&name), "{name} is still installed");
}

#[then(regex = r#"^"([^"]+)" stays in lvpm\.lock and dest$"#)]
async fn stays_in_the_lock_and_dest(world: &mut CliWorld, name: String) {
    assert!(world.is_locked(&name), "{name} is no longer locked");
    assert!(world.is_installed(&name), "{name} is no longer installed");
}

#[then(
    regex = r"^\[dependencies\] is empty, lvpm\.lock lists no packages and dest has no lvpm-installed packages$"
)]
async fn everything_is_empty(world: &mut CliWorld) {
    world.assert_succeeded();
    assert_eq!(entries_of(&world.manifest_text(), "dependencies"), []);
    assert_eq!(world.locked_versions(), []);
    assert_eq!(world.installed(), []);
}

#[then(regex = r#"^dest holds no "([^"]+)" files$"#)]
async fn dest_holds_none_of_it(world: &mut CliWorld, name: String) {
    let found: Vec<String> = files_under(&world.dest_dir())
        .into_iter()
        .filter(|path| path.contains(&name))
        .collect();
    assert_eq!(found, Vec::<String>::new());
    assert!(!world.is_installed(&name), "{name} is installed");
}

// ---- what a refused one said ----

#[then(
    regex = r#"^the command fails saying a constraint is expected after "@" and to quote it, e\.g\. "([^"]+)"$"#
)]
async fn fails_asking_for_a_constraint(world: &mut CliWorld, example: String) {
    world.assert_failed();
    let printed = world.printed();
    for said in [
        "a constraint is expected after \"@\"".to_string(),
        format!("quote it, e.g. \"{example}\""),
    ] {
        assert!(printed.contains(&said), "{said:?} is not in:\n{printed}");
    }
}

#[then(
    regex = r#"^it says the shell may have created a file named "([^"]+)" that can be deleted$"#
)]
async fn says_the_shell_made_a_file(world: &mut CliWorld, file: String) {
    let said = format!("the shell may have created a file named \"{file}\"");
    let printed = world.printed();
    assert!(printed.contains(&said), "{said:?} is not in:\n{printed}");
    assert!(printed.contains("can be deleted"), "{printed}");
}

#[then(
    regex = r"^the command fails saying (\w+) requires LabVIEW (\d{4}) and this project is locked for (\d{4})$"
)]
async fn fails_saying_it_needs_a_newer_labview(
    world: &mut CliWorld,
    name: String,
    needs: String,
    project: String,
) {
    world.assert_failed();
    let said = format!("requires LabVIEW {needs}; this project is locked for {project}");
    let printed = world.printed();
    assert!(
        printed.contains(&format!("{name} ")) && printed.contains(&said),
        "{name} ... {said:?} is not in:\n{printed}"
    );
}

#[then(regex = r#"^the command fails with a conflict naming "([^"]+)" "([^"]*)" and "([^"]+)"$"#)]
async fn fails_with_a_conflict(world: &mut CliWorld, pinned: String, pin: String, other: String) {
    world.assert_failed();
    let printed = world.printed();
    for said in [
        "no set of versions satisfies lvpm.toml".to_string(),
        format!("lvpm.toml: {pinned} = \"{pin}\""),
        other,
    ] {
        assert!(printed.contains(&said), "{said:?} is not in:\n{printed}");
    }
}

#[then(
    regex = r#"^the command fails naming "([^"]+)" and saying its archive could not be unpacked$"#
)]
async fn fails_naming_an_archive_that_would_not_unpack(world: &mut CliWorld, name: String) {
    world.assert_failed();
    let said = format!("the archive of {name} 1.0 could not be unpacked");
    let printed = world.printed();
    assert!(printed.contains(&said), "{said:?} is not in:\n{printed}");
}

#[then(
    regex = r#"^the command fails saying "([^"]+)" is in \[dev-dependencies\] and to use --dev$"#
)]
async fn fails_pointing_to_dev(world: &mut CliWorld, name: String) {
    world.assert_failed();
    let printed = printed_plain(world);
    let said = format!("{name} is in [dev-dependencies]");
    assert!(printed.contains(&said), "{said:?} is not in:\n{printed}");
    assert!(printed.contains("use --dev"), "{printed}");
}

#[then(regex = r#"^the command fails saying "([^"]+)" is in (\[[^\]]+\])$"#)]
async fn fails_saying_it_is_in_another_table(world: &mut CliWorld, name: String, table: String) {
    world.assert_failed();
    let said = format!("{name} is in {table}");
    let printed = printed_plain(world);
    assert!(printed.contains(&said), "{said:?} is not in:\n{printed}");
}

/// `flag` is the option's name: what follows it on the command line (the
/// `x.toml` of `--manifest x.toml`) is not part of the message.
#[then(
    regex = r"^the command fails saying (--[\w-]+)(?: \S+)? cannot be used in a project, because lvpm\.lock decides the dependencies$"
)]
async fn fails_saying_the_lock_decides(world: &mut CliWorld, flag: String) {
    world.assert_failed();
    let said =
        format!("{flag} cannot be used in a project, because lvpm.lock decides the dependencies");
    assert!(
        world.printed().contains(&said),
        "{said:?} is not in:\n{}",
        world.printed()
    );
}

#[then(regex = r#"^the error's next line names "([^"]+)"$"#)]
async fn the_next_line_names(world: &mut CliWorld, flag: String) {
    world.assert_failed();
    let printed = printed_plain(world);
    let next: Vec<&str> = printed
        .lines()
        .filter(|line| line.starts_with("next: "))
        .collect();
    assert_eq!(next.len(), 1, "one next line in:\n{printed}");
    assert!(next[0].contains(&flag), "{flag:?} is not in: {}", next[0]);
}

/// `commands` is every command the line names, in order, as `"a" and "b"`; the
/// plain text of the line is `next: run a ... and b ... first`, each command
/// followed by whatever flags it carries.
#[then(regex = r#"^the error's next line says to run ("[^"]+"(?: and "[^"]+")*) first$"#)]
async fn the_next_line_says_to_run_first(world: &mut CliWorld, commands: String) {
    world.assert_failed();
    let printed = printed_plain(world);
    let next: Vec<&str> = printed
        .lines()
        .filter(|line| line.starts_with("next: "))
        .collect();
    assert_eq!(next.len(), 1, "one next line in:\n{printed}");
    let wanted: Vec<&str> = commands
        .split(" and ")
        .map(|c| c.trim_matches('"'))
        .collect();
    let mut rest = next[0]
        .strip_prefix("next: run ")
        .unwrap_or_else(|| panic!("not a run line: {}", next[0]));
    for (at, command) in wanted.iter().enumerate() {
        rest = rest
            .strip_prefix(command)
            .unwrap_or_else(|| panic!("{command:?} is not next in: {}", next[0]));
        let end = if at + 1 < wanted.len() {
            " and "
        } else {
            " first"
        };
        let flags_end = rest
            .find(end)
            .unwrap_or_else(|| panic!("{end:?} does not follow {command:?} in: {}", next[0]));
        rest = &rest[flags_end + end.len()..];
    }
}

#[then("the error's next line says to check the name")]
async fn the_next_line_says_to_check_the_name(world: &mut CliWorld) {
    world.assert_failed();
    let printed = printed_plain(world);
    let next: Vec<&str> = printed
        .lines()
        .filter(|line| line.starts_with("next: "))
        .collect();
    assert_eq!(next.len(), 1, "one next line in:\n{printed}");
    assert!(next[0].contains("check the name"), "{}", next[0]);
}

#[then(regex = r#"^the error does not mention "([^"]+)"$"#)]
async fn the_error_does_not_mention(world: &mut CliWorld, said: String) {
    world.assert_failed();
    // Stderr only: a destination flag makes stdout say what --no-project is for.
    let error = world.output().stderr.clone();
    assert!(!error.contains(&said), "{said:?} is in:\n{error}");
}

#[then(regex = r#"^the command fails saying "([^"]+)" is in no configured source$"#)]
async fn fails_saying_it_is_in_no_configured_source(world: &mut CliWorld, name: String) {
    world.assert_failed();
    let said = format!("{name} is in no configured source");
    assert!(
        world.printed().contains(&said),
        "{said:?} is not in:\n{}",
        world.printed()
    );
}

#[then("the command fails saying no lvpm.toml was found, at or above the current directory")]
async fn fails_saying_no_manifest_was_found(world: &mut CliWorld) {
    world.assert_failed();
    let printed = world.printed();
    for said in ["no lvpm.toml found in", " or above"] {
        assert!(printed.contains(said), "{said:?} is not in:\n{printed}");
    }
}

// ---- install and uninstall inside a project ----

#[then(regex = r"^(\w+) is installed in that destination$")]
async fn installed_where_it_was_sent(world: &mut CliWorld, name: String) {
    world.assert_succeeded();
    assert!(world.is_installed(&name), "{name} is not installed");
}

#[then(regex = r#"^"([^"]+)" is uninstalled$"#)]
async fn uninstalled(world: &mut CliWorld, name: String) {
    world.assert_succeeded();
    assert!(!world.is_installed(&name), "{name} is still installed");
}

#[then(regex = r#"^"([^"]+)" is uninstalled from dest$"#)]
async fn uninstalled_from_dest(world: &mut CliWorld, name: String) {
    uninstalled(world, name).await;
}

#[then(
    regex = r#"^the output says "([^"]+)" is still in lvpm\.toml and "([^"]+)" removes it for good$"#
)]
async fn says_it_is_still_listed(world: &mut CliWorld, name: String, command: String) {
    let printed = printed_plain(world);
    for said in [
        format!("{name} is still in lvpm.toml"),
        command,
        "removes it for good".to_string(),
    ] {
        assert!(printed.contains(&said), "{said:?} is not in:\n{printed}");
    }
}

#[then(
    regex = r"^the output says (\w+) is being added to lvpm\.toml and the project synced to lvpm\.lock$"
)]
async fn the_output_says_it_is_an_add(world: &mut CliWorld, name: String) {
    let stdout = &world.output().stdout;
    let said = [
        format!("adding {name} to lvpm.toml"),
        "syncing the project to lvpm.lock".to_string(),
    ];
    assert!(
        stdout
            .lines()
            .any(|line| said.iter().all(|part| line.contains(part))),
        "{said:?} is not on one line of:\n{stdout}"
    );
}

/// `install` says it is an add, once the lock is made; what else it prints is
/// `add`'s own output.
#[then("both manifests, locks and destinations are identical")]
async fn both_worlds_are_identical(world: &mut CliWorld) {
    let twin = world.add_remove.twin.as_ref().expect("a second world");
    let (one, two) = (world.output(), &twin.output);
    assert_eq!(one.exit_code, two.exit_code, "{one:?}\n{two:?}");
    let not_its_own =
        |line: &&str| !(line.starts_with("adding ") || line.starts_with("note: in a project,"));
    assert_eq!(
        one.stdout.lines().filter(not_its_own).collect::<Vec<_>>(),
        two.stdout.lines().collect::<Vec<_>>(),
        "what they print besides install's own lines"
    );
    assert_eq!(one.stderr, two.stderr);
    // Each world has a feed of its own, so its address is in both files.
    let text_of = |sandbox: &Sandbox, file: &str| {
        let bytes = std::fs::read(sandbox.work_dir().join(file)).expect("a project file");
        sandbox.normalised_text(&String::from_utf8(bytes).expect("a text file"))
    };
    for file in [MANIFEST_FILE, "lvpm.lock"] {
        assert_eq!(
            text_of(&world.sandbox, file),
            text_of(&twin.sandbox, file),
            "{file}"
        );
    }
    assert_eq!(
        world.sandbox.list_files(VENV_DIR),
        twin.sandbox.list_files(VENV_DIR),
        "the venvs"
    );
}

// ---- dependency groups ----

#[given("a current lvpm.lock")]
async fn a_current_lock(world: &mut CliWorld) {
    world.lock_the_project();
    world.note_the_project();
}

/// The entries of `[dependency-groups.<group>]`, none when the manifest has no such table.
fn group_entries(manifest: &str, group: &str) -> Vec<(String, String)> {
    let parsed: toml::Table = manifest.parse().expect("the manifest is TOML");
    parsed
        .get("dependency-groups")
        .and_then(|groups| groups.get(group))
        .and_then(toml::Value::as_table)
        .map(|entries| {
            entries
                .iter()
                .map(|(name, value)| (name.clone(), value.as_str().unwrap_or_default().into()))
                .collect()
        })
        .unwrap_or_default()
}

/// One quoted phrase, as a sentence, or one flag or `<PLACEHOLDER>` as clap
/// prints it: the other steps of this name take several parts, a manifest
/// line, or a single word (`first`).
#[then(regex = r#"^the command fails naming "(<[A-Z_]+>|--[\w-]+|[^"\[][^"]* [^"]*)"$"#)]
async fn fails_naming_a_phrase(world: &mut CliWorld, said: String) {
    world.assert_failed();
    let printed = world.printed();
    assert!(printed.contains(&said), "{said:?} is not in:\n{printed}");
}

#[then(regex = r#"^the manifest-edited event has "dev": (true|false) and "group": "([^"]+)"$"#)]
async fn the_manifest_edited_event_has(world: &mut CliWorld, dev: String, group: String) {
    world.assert_succeeded();
    let events: Vec<serde_json::Value> = world
        .output()
        .stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .collect();
    let edited = events
        .iter()
        .find(|event| event["event"] == "manifest_edited")
        .unwrap_or_else(|| panic!("no manifest_edited event in {events:?}"));
    assert_eq!(edited["data"]["dev"], dev == "true", "{edited}");
    assert_eq!(edited["data"]["group"], group, "{edited}");
}

/// The result is compared with another command's, run on the project as it
/// was at the start: the manifest and the lock are put back first.
#[then(
    regex = r#"^lvpm\.toml and lvpm\.lock are byte-identical to the result of "([^"]+)" from the same start$"#
)]
async fn the_same_result_as_another_command(world: &mut CliWorld, other: String) {
    world.assert_succeeded();
    let result = (world.manifest_bytes(), world.lock_bytes());
    let manifest = world.sandbox.work_dir().join(MANIFEST_FILE);
    std::fs::write(manifest, &world.noted().manifest).expect("put the manifest back");
    match world.lock.before.clone() {
        Some(lock) => std::fs::write(world.lock_path(), lock).expect("put the lock back"),
        None => std::fs::remove_file(world.lock_path()).expect("remove the lock"),
    }
    world.run(&other, None);
    world.assert_succeeded();
    assert_eq!(
        String::from_utf8_lossy(&result.0),
        String::from_utf8_lossy(&world.manifest_bytes()),
        "lvpm.toml differs"
    );
    assert_eq!(
        result.1.as_deref().map(String::from_utf8_lossy),
        world.lock_bytes().as_deref().map(String::from_utf8_lossy),
        "lvpm.lock differs"
    );
}

#[then(regex = r#"^"([^"]+)" is gone from \[dependency-groups\.([\w-]+)\], lvpm\.lock and dest$"#)]
async fn gone_from_the_group_the_lock_and_dest(world: &mut CliWorld, name: String, group: String) {
    world.assert_succeeded();
    let entries = group_entries(&world.manifest_text(), &group);
    assert!(
        entries.iter().all(|(entry, _)| *entry != name),
        "{entries:?}"
    );
    assert!(!world.is_locked(&name), "{name} is still locked");
    assert!(!world.is_installed(&name), "{name} is still installed");
}

/// `the command fails saying "beta" is in [a] and [b], with one next line
/// each: "lvpm remove beta" and "lvpm remove beta --group ci"`.
#[then(
    regex = r#"^the command fails saying "([^"]+)" is in (.+), with one next line each: ("[^"]+"(?: and "[^"]+")*)$"#
)]
async fn fails_listing_every_table_holding_it(
    world: &mut CliWorld,
    name: String,
    tables: String,
    commands: String,
) {
    world.assert_failed();
    let printed = printed_plain(world);
    let said = format!("{name} is in {tables}");
    assert!(printed.contains(&said), "{said:?} is not in:\n{printed}");
    let commands = quoted(&commands);
    let next_lines: Vec<&str> = printed
        .lines()
        .filter(|line| line.starts_with("next: "))
        .collect();
    assert_eq!(next_lines.len(), commands.len(), "{printed}");
    for command in commands {
        // What follows the command keeps `lvpm remove beta` from matching `... --group ci`.
        let hint = format!("next: {command} removes it");
        assert!(
            next_lines.iter().any(|line| line.starts_with(&hint)),
            "{hint:?} is not a line of:\n{printed}"
        );
    }
}
