//! Steps of the VI Server property scenarios (issue #78): the protocol document,
//! and `vi-probe` reporting each VI's Execution:State.

use crate::CliWorld;
use crate::steps::docs::{repo_file, section_of};
use cucumber::{then, when};
use std::path::Path;

const PROTOCOL_DOC: &str = "docs/vi-server-protocol.md";

/// The document with its line breaks and runs of spaces folded, so a phrase
/// may wrap anywhere.
fn folded(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn properties_section() -> String {
    folded(&section_of(&repo_file(PROTOCOL_DOC), "Properties"))
}

fn says(text: &str, phrase: &str) {
    assert!(text.contains(phrase), "`{phrase}` is not in:\n{text}");
}

#[when("the VI Server protocol document is read")]
async fn read_protocol_doc(_world: &mut CliWorld) {
    assert!(
        !repo_file(PROTOCOL_DOC).is_empty(),
        "{PROTOCOL_DOC} is empty"
    );
}

#[then(r#"its "Properties" section shows the request and the reply field by field"#)]
async fn shows_the_layout(_world: &mut CliWorld) {
    let section = properties_section();
    for phrase in [
        "u32 flag (1 read, 0 write)",
        "u32 prop id",
        "00000200",
        "read reply",
        "write reply",
    ] {
        says(&section, phrase);
    }
}

#[then("it says a read declares the type it wants back")]
async fn says_a_read_declares_its_type(_world: &mut CliWorld) {
    says(
        &properties_section(),
        "A read declares the type it wants back",
    );
}

#[then("it says an enum property is declared as an enum, and a plain U32 is error 42")]
async fn says_the_enum_rule(_world: &mut CliWorld) {
    let section = properties_section();
    says(&section, "declared as an enum");
    says(&section, "error 42");
}

#[then("it says the ids are the codes LabVIEW stores in Property and Invoke Nodes")]
async fn says_where_ids_come_from(_world: &mut CliWorld) {
    says(
        &folded(&repo_file(PROTOCOL_DOC)),
        "the codes LabVIEW stores in its own Property and Invoke Nodes",
    );
}

#[when(regex = r#"^I run vi-probe on the check fixture "([^"]+)"$"#)]
async fn vi_probe_on_fixture(world: &mut CliWorld, name: String) {
    let vi = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/check/project")
        .join(&name);
    let vi = vi.to_string_lossy().into_owned();
    let output = world.sandbox.run_lvpm(&["vi-probe", &vi], None);
    world.last_output = Some(world.sandbox.normalised(&output));
}

#[then(regex = r#"^stdout says "([^"]+)"$"#)]
async fn stdout_says(world: &mut CliWorld, phrase: String) {
    says(&world.output().stdout, &phrase);
}
