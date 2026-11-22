# Working on the fork adoption

How to take one slice of [#43](https://github.com/labview-package-manager/lvpm/issues/43)
from issue to merged PR. #43 replaces `main` with ThatsTheEnd/lvpm. Its
sub-issues are the slices, and this branch, `integration/fork`, is where they
land. It starts from the fork at `9b359fd`, which has upstream 0.3.1
(`720822b`) in its history.

The steps are a lighter form of the
[agentic-dev-team](https://github.com/bdfinst/agentic-dev-team) workflow. The
full one starts many agents per slice, which is more than most slices need. We
are trying this form on
[#44](https://github.com/labview-package-manager/lvpm/issues/44) first.

## Rules

- The issue's Gherkin scenarios are the spec and the plan. No code before a
  scenario that fails.
- One slice per branch, per PR and per Claude session. A fresh session keeps
  the context small.
- One code review per PR.
- Skip `/specs`, `/plan` and `/build`, except `/plan` and `/build` for
  [#54](https://github.com/labview-package-manager/lvpm/issues/54) and
  [#55](https://github.com/labview-package-manager/lvpm/issues/55). Don't use
  `/ship`, `/autoship` or `--candidates`.
- Slices are tracked in this repository, not on the fork's board that
  `docs/vision/build-slices.md` names.
- PRs go into `integration/fork`. From a clone of a fork, GitHub and `gh` can
  pick another repository or branch, so check the target before you open one.

## Set up, once

In each clone where you use it, so the plugin stays out of your other projects:

```bash
claude plugin marketplace add bdfinst/agentic-dev-team
claude plugin install dev-team@bfinster --scope local
```

Start a new Claude session afterwards. `/setup` is not needed. The plugin's
hooks need Python 3.

## One slice, step by step

The prompts use #44. Put your issue number and a short name in their place.

### 1. Scenarios that fail

Start a new session and note `/usage`. Then:

> Work on issue #44 on a branch `slice/44-bare-year` from `integration/fork`.
> Read the issue and only the code it needs. Don't use /specs, /plan or /build.
> Write the scenarios as a feature file first, add it to `FEATURE_FILES` in
> tests/acceptance.rs, run them, show me they fail, then stop.

Check that the scenarios say what you want, and that they fail for the right
reason.

### 2. The smallest change

> Make them pass with the smallest change, run the tests, show me the diff, and
> stop.

Check the diff against the issue, and settle its open questions here. For #44:

- Where the warning appears. The manifest parser seems to have no way to report
  a warning yet, so this is most of the work.
- Whether `.lvpkg` files accept a bare year too. `lvpkg.rs` calls the same
  function. The suggestion is to keep those strict.

### 3. Docs

> Update the docs this changes, and commit.

For #44 that is the manifest page: `labview` is required, and written as `>=`.

### 4. One review

```text
/dev-team:code-review
```

Then have it fix the findings you agree with. The number of review agents grows
with the diff, so small slices cost less.

### 5. PR

> Open a PR into integration/fork that says "Part of #44".

The plugin stops Claude from running `gh pr create` in Bash until step 4 is
done for the branch as it is.

After the merge, close the issue by hand. GitHub closes issues from a PR only
when it merges into `main`.

### 6. What it cost

Compare `/usage` with step 1, and write both on the issue. After #44 we decide
whether the other slices run this way.

## The final merge into main

- Merge with a merge commit, never a squash. The history records which upstream
  commit each port came from.
- Settle [#58](https://github.com/labview-package-manager/lvpm/issues/58) first.
  `tag.yml` and `release-please.yml` run on every push to `main`. An accidental
  merge on 2026-11-22 (PR 42, reverted) did try to release; it stopped only
  because this repository has no `RELEASE_PLEASE_TOKEN`.
- The fork's commits use its own issue numbers. Titles such as "(#43)" will link
  to unrelated issues here, which is only untidy. Two commits say `Closes #62`
  (`a3c376c`) and `Closes #77` (`f3cbed8`): if we have those issues and they are
  still open, reopen them after the merge.
- [#41](https://github.com/labview-package-manager/lvpm/pull/41) (SBOM) and
  [#35](https://github.com/labview-package-manager/lvpm/pull/35)
  (`lvpm package build`) overlap with features of the fork. They are integrated
  after the merge.
