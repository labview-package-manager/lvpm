# Plan: Run upstream's tests/venv.rs against the integration build

**Created**: 2026-11-24
**Branch**: slice/65-venv-tests
**Status**: approved
**Gherkin persistence**: plan-file-only
**Spec**: labview-package-manager/lvpm#65 (the issue is the spec; no /specs run)

## Goal

Upstream's `tests/venv.rs` (org/main `daa141a`, 15 tests) drives the venv workflow
through the `lvpm` binary on a fake LabVIEW 2099, with no LabVIEW installed. Make it
run on `integration/fork`, so the slices after #47 (#48, #50, #54, #55) each find
upstream's check ready: every test passes, or is ignored with the number of the slice
it waits on.

## Acceptance Criteria

- [ ] A debug build reads `LVPM_FAKE_LABVIEW=<version>,<bitness>,<dir>` as the one LabVIEW installed. Unchanged and not tested here: a release build never compiles the hook (`cfg(debug_assertions)`), and `LVPM_TEST_LABVIEW=1` is not touched.
- [ ] With each malformed `LVPM_FAKE_LABVIEW` (missing field, non-numeric version, bitness 7), `lvpm venv create` in a debug build exits non-zero and stderr names `<version>,<bitness>,<dir>`. It never falls back to the machine's LabVIEW.
- [ ] `cargo test --test venv`: every test passes or is `#[ignore = "#NN"]`, where #NN is an open sub-issue of labview-package-manager/lvpm#43. The allowed set is #45, #48, #50, #54, #55.
- [ ] `cargo test --test venv -- --ignored`: every ignored test fails on its own assertion, not on setup, so no ignore hides a test that already passes.
- [ ] No test reads the machine's own LabVIEW: the harness sets the fake 2099 and removes `LVPM_TEST_LABVIEW` from the child's environment.
- [ ] `grep -n -- --no-save tests/venv.rs` returns nothing. Assertions on those lines (upstream 392, 446, 525, 579–580) that do not need `--no-save` are kept without it.
- [ ] The PR body lists every changed assertion, with upstream's text and the fork's.
- [ ] The `crates/` diff touches only the detection code in `target.rs`.
- [ ] An adapt test that hits a behaviour gap no open slice owns stops the build and goes to the user.
- [ ] `CONTRIBUTING.md` names `LVPM_FAKE_LABVIEW`, says it is separate from `LVPM_TEST_LABVIEW=1`, and says the hook needs a debug build.

## Approach stance (decision-defaults)

- **Format fidelity / replace-vs-merge**: the port keeps upstream's file whole (names, structure, helpers). Only assertions change.
- **The adaptation rule**:
  - Where the fork does the same thing in other words, the assertion takes the fork's current text. The slice that changes that text updates the test.
  - Where the fork does not do the thing yet, the test is `#[ignore = "#NN"]`, unchanged.
  - A test is never rewritten to assert the fork's different behaviour as the goal.
- **Scope**: no change to lvpm behaviour beyond the hook. Differences found are reported, not fixed.
- **Integration**: PR into `integration/fork`, body `Part of #65`, no closing keyword, no auto-merge (docs/fork-adoption.md).

## Spike result (2026-11-24, hook in place, file copied as-is)

2 pass (`a_venv_is_never_bound…`, `commands_find_the_venv…`). 13 fail:

| Test | Class | Treatment |
|---|---|---|
| create_binds_the_project… | same, fork writes `labview = ">=2099"` | adapt |
| venv_installs_merge_palette_trees… | same, hint says `lvpm sync` | adapt |
| a_package_labview_already_has… | same, fork re-reports shadowing on every install | adapt: the `lacks` line goes, with a comment; listed in the PR for Jonathan |
| hooks_and_machine_locations… | same, other words (`hooks not run: PostInstall`) | adapt; the `--no-linkfix` check stays, without `--no-save` |
| every_command_on_the_venv… | same, the log holds JSON events | adapt to the `manifest_edited` event |
| manifest_install_brings_in… | `[dependencies.test]` not parsed | ignore `#45` |
| install_by_name_keeps_the_manifest… | fork words and `--no-save` | ignore `#54`; drop the `--no-save` lines |
| uninstall_keeps_packages… | uninstall does not edit `lvpm.toml` | ignore `#54`; drop the `--no-save` lines |
| uninstall_takes_the_named_package… | same | ignore `#54` |
| launch_checks_the_venv… | needs `lvpm.lock` | ignore `#50`; drop the `--no-save` line |
| an_unreadable_record… | sync removes the unlocked package | ignore `#55` |
| import_writes_vipm_files… | import | ignore `#48` |
| a_vipc_carries_its_packages… | `.vipc` `<Version>` format | ignore `#48` |

An "adapt" test that hits a real behaviour gap further down moves to "ignore" with the slice that owns it. If no open slice owns it, it stops the build and goes to the user.

## Slices

### Slice 1: venv workflow tests on a fake LabVIEW

**Depends-on:** none
**Files:** `crates/lvpm-core/src/target.rs`, `tests/venv.rs`, `CONTRIBUTING.md`

**Behavior:**

```gherkin
Feature: the venv workflow is tested without LabVIEW

  Scenario: the venv tests run on a machine with no LabVIEW
    Given a machine with no LabVIEW installed
    When I run "cargo test --test venv"
    Then every test passes or is ignored with the number of the slice it waits on
    And no test reads the machine's own LabVIEW

  Scenario: no ignore hides a passing test
    When I run "cargo test --test venv -- --ignored"
    Then every ignored test fails

  Scenario: a malformed fake LabVIEW is refused
    Given LVPM_FAKE_LABVIEW is "99.0,64" with no folder
    When I run "lvpm venv create" in a debug build
    Then it fails, naming the form "<version>,<bitness>,<dir>"
```

**Steps:**

#### Step 1.1: The fake-LabVIEW hook

**Complexity**: standard
**IMPLEMENT**: `detect()` returns the `LVPM_FAKE_LABVIEW` target in debug builds, else `detect_installed()` (the existing Windows/Linux detection, renamed). A port of upstream's `test_labview`.
**TEST**:
- `parse_fake_labview(&str) -> Result<LvTarget>` holds the parse and one error message; `fake_labview()` is the env wrapper. Its unit test, gated on `debug_assertions`, covers a good spec, an empty value, a missing field, a bad number, bitness 7, and a dir containing a comma.
- One test in `tests/venv.rs` runs `venv create` with `99.0,64`, `x,64,<dir>` and `99.0,7,<dir>`, and asserts each failure names the form.
**REFACTOR**: Keep it next to `detect_installed`. The doc comment says the hook depends on release profiles keeping debug-assertions off.
**Files**: `crates/lvpm-core/src/target.rs`
**Commit**: `test(venv): a debug build takes LVPM_FAKE_LABVIEW as the one LabVIEW installed`

#### Step 1.2: Port tests/venv.rs

**Complexity**: standard
**IMPLEMENT**:
- Copy `tests/venv.rs` from org/main with `LVPM_FAKE_LABVIEW`.
- Apply the table above: adapt, ignore, drop `--no-save`.
- The harness removes `LVPM_TEST_LABVIEW` from the child's environment.
- Every `LVPM_TEST_LABVIEW` in the header becomes `LVPM_FAKE_LABVIEW`. A header line says where the port differs from upstream.
**TEST**:
- `cargo test --test venv` gives only passes and ignores.
- `grep 'ignore = "' tests/venv.rs` shows only `#45`, `#48`, `#50`, `#54` and `#55`, all open.
- `cargo test --test venv -- --ignored` fails every ignored test.
- `cargo test --workspace` and `cargo clippy --all-targets` are green.
**REFACTOR**: Remove helpers that no running test uses only if clippy flags them. Ignored tests keep theirs.
**Files**: `tests/venv.rs`
**Commit**: `test(venv): port upstream's venv workflow tests; slices still open are ignored by number`

#### Step 1.3: Document it

**Complexity**: trivial
**IMPLEMENT**: Add upstream's `## Tests` paragraph to CONTRIBUTING.md, with `LVPM_FAKE_LABVIEW`. Say that it is separate from `LVPM_TEST_LABVIEW=1`, and that the container suite is `ci/e2e/`.
**TEST**: CONTRIBUTING.md contains `LVPM_FAKE_LABVIEW`, `LVPM_TEST_LABVIEW` and "debug build".
**REFACTOR**: n/a, docs only.
**Files**: `CONTRIBUTING.md`
**Commit**: `docs: how the venv tests run without LabVIEW`

## Parallelization

```mermaid
graph TD
  S1[Slice 1]
```

| Wave | Slices (parallel) |
|------|-------------------|
| 1 | 1 |

## Pre-PR Quality Gate

- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --all-targets` clean
- [ ] `/code-review` passes
- [ ] CONTRIBUTING.md updated
- [ ] Cost comment on #65

## Skipped (low value)

| Finding | Rationale (one line) |
|---|---|
| A test that a release build ignores the hook | `#[cfg(debug_assertions)]` is checked by the compiler; the test costs a full release build for one attribute |
| A test that `LVPM_TEST_LABVIEW=1` keeps its meaning | Nothing touches that variable; a check needs a LabVIEW machine and would pass vacuously in CI |
| A unit test that `detect()` skips `detect_installed` with the hook set | It needs process-env mutation (racy); the venv tests already see only LabVIEW 2099 |
| Rejecting a dir that does not exist | Upstream does not; detection of the fake is the test's own setup |

## Risks & Open Questions

- **An adapt that turns into a behaviour gap.** If no open slice owns the gap, it goes to the user and is not decided here.
- **Upstream is wrong for the fork.** Upstream's test can encode behaviour the fork rejects on purpose: shadow reporting, the JSON log, `>=` years. In those cases the test takes the fork's way, and the PR lists each one for Jonathan.
- **#45 lands first or later.** The groups test is ignored `#45` either way. #45 removes the ignore. The only conflict is the `FEATURE_FILES` line in `tests/acceptance.rs`, and this slice does not touch that file.
- The resume guard reported `resume` from an old ThatsTheEnd "#65" in `plans/lvpm-attestations-s6.md`. That is a false positive.

## Criteria gate (build Step 3)

spec-compliance-review flagged C2–C8. Applied: C2 (three malformed forms through the binary), C3 (allowed set and repo), C4 (own assertion), C6 (grep), plus the scope and stop-rule criteria. Not applied: C5 harness assertion test (the harness sets both variables in one place; a test of it re-reads that line), C7 row count check, C8 wording check (reviewer reads them).

## Build Progress

### Slices (grouped by wave)

#### Wave 1
- [x] Slice 1: venv workflow tests on a fake LabVIEW
  - [x] Step 1.1: The fake-LabVIEW hook
  - [x] Step 1.2: Port tests/venv.rs
  - [x] Step 1.3: Document it

## Plan Review Summary

Plan tier: standard. Reviewers: Acceptance, Design. UX skipped: no UI surface. Parallelization skipped: single-slice plan.

- Acceptance, round 1: needs-revision, 3 blockers. The changes:
  - The malformed-hook check now runs the binary.
  - The `LVPM_TEST_LABVIEW` scenario was dropped and moved to Skipped.
  - The release-build check was moved to Skipped, with the reason.
  - `--ignored` must fail, so no ignore hides a passing test.
  - The ignore reasons are grepped.
  - The `--no-linkfix` assertion is kept.
  - The PR lists every changed assertion.
  - The harness removes `LVPM_TEST_LABVIEW`.
- Design: approve. The changes:
  - `parse_fake_labview` holds the parse and one error message.
  - Bitness is limited to 32 or 64.
  - A comment says the hook depends on debug-assertions staying off in release profiles.
  - Noted: every command calls `detect()`, so a malformed variable breaks every command. That is acceptable in a debug-only hook.
