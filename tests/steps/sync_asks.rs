//! Steps of the sync-asks scenarios: `lvpm sync` in or out of a terminal, with a
//! stray package in `dest`.
//!
//! "In a terminal" is stdin only: a debug `lvpm` treats `LVPM_TEST_STDIN_TERMINAL`
//! as a terminal. `LVPM_TEST_TERMINAL`, which turns on the update notice, stays
//! unset. The sandbox starts every child with an empty environment, so
//! `LVPM_YES` is unset until a scenario sets it.

use super::lock::name_and_version_pairs;
use crate::CliWorld;
use crate::support::fixtures::{FixturePackage, SYNC_ASKS_PACKAGES};
use crate::support::project::MANIFEST_FILE;
use cucumber::{given, then, when};
use lvpm_core::manifest_edit::{self, DependencyTable};

const STDIN_TERMINAL: &str = "LVPM_TEST_STDIN_TERMINAL";

// ---- the project ----

#[given(r#"fixture packages "alpha" 1.0, "alpha" 2.0, "extra" 1.0 and "zeta" 1.0"#)]
async fn fixture_packages(world: &mut CliWorld) {
    let ids: Vec<String> = SYNC_ASKS_PACKAGES.iter().map(FixturePackage::id).collect();
    assert_eq!(
        ids,
        ["alpha-1.0", "alpha-2.0", "extra-1.0", "zeta-1.0"],
        "the feature and the fixture table disagree"
    );
    world.publish(&SYNC_ASKS_PACKAGES.iter().collect::<Vec<_>>());
}

#[given("the project has no lvpm.lock")]
async fn the_project_has_no_lock(world: &mut CliWorld) {
    assert!(world.lock_bytes().is_none(), "a lock exists");
}

/// Locked while the manifest also listed `name`, which it no longer does: the
/// lock is outdated, so a sync that goes ahead rewrites it.
#[given(regex = r#"^the project has an lvpm\.lock made when it also depended on "([^"]+)"$"#)]
async fn the_project_has_an_outdated_lock(world: &mut CliWorld, name: String) {
    world.depend(DependencyTable::Dependencies, &name, ">=1.0");
    world.lock_the_project();
    manifest_edit::remove_file(
        &world.sandbox.work_dir().join(MANIFEST_FILE),
        &DependencyTable::Dependencies,
        &name,
    )
    .expect("take the dependency out of the manifest");
    let locked: Vec<String> = world
        .locked_versions()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert!(
        locked.contains(&name),
        "{name} is not in lvpm.lock: {locked:?}"
    );
}

// ---- what dest holds ----

#[given(regex = r"^dest has (.+) installed$")]
async fn dest_has(world: &mut CliWorld, packages: String) {
    for (name, version) in name_and_version_pairs(&packages) {
        world.install_into_dest(&name, &version);
    }
}

// ---- the terminal, and the answer ----

#[given(regex = r#"^LVPM_YES is set to "(.*)"$"#)]
async fn lvpm_yes_is_set_to(world: &mut CliWorld, value: String) {
    match value.is_empty() {
        true => world.sandbox.remove_child_env("LVPM_YES"),
        false => world.sandbox.set_child_env("LVPM_YES", &value),
    }
}

#[when(expr = "I run {string} in a terminal")]
async fn run_in_a_terminal(world: &mut CliWorld, command_line: String) {
    world.sandbox.set_child_env(STDIN_TERMINAL, "1");
    world.run(&command_line, None);
}

#[when(expr = "I run {string} in a terminal and answer {string}")]
async fn run_in_a_terminal_and_answer(world: &mut CliWorld, command_line: String, answer: String) {
    world.sandbox.set_child_env(STDIN_TERMINAL, "1");
    world.run(&command_line, Some(&format!("{answer}\n")));
}

#[when(expr = "I run {string} in a terminal with stdin closed")]
async fn run_in_a_terminal_with_stdin_closed(world: &mut CliWorld, command_line: String) {
    world.sandbox.set_child_env(STDIN_TERMINAL, "1");
    world.run(&command_line, Some(""));
}

#[when(expr = "I run {string} with no terminal")]
async fn run_with_no_terminal(world: &mut CliWorld, command_line: String) {
    world.sandbox.remove_child_env(STDIN_TERMINAL);
    world.run(&command_line, None);
}

// ---- what it did ----

#[then("dest has no lvpm-installed packages")]
async fn dest_holds_nothing(world: &mut CliWorld) {
    assert_eq!(world.installed(), [], "what dest holds");
}

/// The plan lists `name` under "remove", as a line of its own.
#[then(regex = r#"^the output names "([^"]+)" as a package to remove$"#)]
async fn the_output_names_a_removal(world: &mut CliWorld, name: String) {
    let stdout = &world.output().stdout;
    assert!(
        stdout
            .lines()
            .any(|line| line.trim_start().starts_with(&format!("remove {name} "))),
        "{name} is not listed to remove in:\n{stdout}"
    );
}

fn manifest_depends(world: &CliWorld, name: &str) -> bool {
    let text = std::fs::read_to_string(world.sandbox.work_dir().join(MANIFEST_FILE))
        .expect("read the manifest");
    let manifest: toml::Table = text.parse().expect("a TOML manifest");
    manifest
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .is_some_and(|table| table.contains_key(name))
}

#[then(regex = r#"^the manifest depends on "([^"]+)"$"#)]
async fn the_manifest_depends_on(world: &mut CliWorld, name: String) {
    assert!(manifest_depends(world, &name), "lvpm.toml has no {name}");
}

#[then(regex = r#"^the manifest no longer depends on "([^"]+)"$"#)]
async fn the_manifest_no_longer_depends_on(world: &mut CliWorld, name: String) {
    assert!(
        !manifest_depends(world, &name),
        "lvpm.toml still has {name}"
    );
}
