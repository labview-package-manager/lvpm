# lvpm

## Slices and issues

The fork adoption is tracked in labview-package-manager/lvpm: every slice is a
sub-issue of [#43](https://github.com/labview-package-manager/lvpm/issues/43).
Issue and PR numbers in older commits, and in `docs/` and `plans/`, are
ThatsTheEnd/lvpm's own.

How to run a slice is in [docs/fork-adoption.md](docs/fork-adoption.md). In
short:

- Branch from `integration/fork`, and open the PR into it, never into `main`.
- Write the issue's Gherkin scenarios first, and see them fail.
- The PR says `Part of #N`. Close the issue by hand after the merge.

Name the repository when you use `gh` in a clone of a fork, where it may pick
another one: `gh pr create -R labview-package-manager/lvpm --base integration/fork`.

## Build and test

```bash
cargo test --workspace
```

Scenarios are in `tests/features/` and run in `tests/acceptance.rs`. Add a new
feature file to `FEATURE_FILES` there, or it does not run.

Don't add features to CHANGELOG.md: it is generated at release.
