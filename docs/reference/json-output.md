# JSON output and exit codes

{{#include ../../README.md:json-output}}

## `install` and `uninstall` in a project

`lvpm install <name>` and `lvpm uninstall <name>` in a project edit `lvpm.toml`,
lock and sync. `add` and `remove` are the same commands, so `--json` names them
`install` and `uninstall`: the last line is `{"result": {"command": "install"}}`
or `{"result": {"command": "uninstall"}}`, whichever name you typed.

Before it edits `lvpm.toml`, and only once the lock is made and the package is
known to be one to add or remove, the command writes one announce event:
`install_adds_to_project` or `uninstall_removes_from_project`. `package` is the
name as typed. `destination` is the destination flag that picked where the project
syncs (`--prefix`, `--global` or `--labview-version`; the last only when no venv wins), and is left out when there
was none. `dry_run` is `true` under `--dry-run`, when nothing is written, and is
left out otherwise. A refused removal, an unknown package or a lock that fails
writes none.

```json
{"event": "install_adds_to_project", "data": {"package": "beta", "destination": "--prefix"}}
{"event": "uninstall_removes_from_project", "data": {"package": "alpha", "destination": "--prefix"}}
```

The `manifest_edited`, `lock_changed` and sync events follow, as for `sync`.

## The `held_back` event

When `[sources] min-age-days` kept out a version newer than the one a
resolving command chose (`lvpm lock`, `lvpm lock --upgrade`, a named
`lvpm install` or `lvpm uninstall` in a project, `lvpm sync`), it writes one `held_back` event for each package,
naming the newest such version. `lvpm sync --locked` and `--frozen` never
resolve, so they write none. `released` and `eligible` are RFC 3339 UTC instants;
`eligible` is `released` plus `min_age_days` days, the first moment the
version may be chosen.

```json
{"event": "held_back", "data": {"name": "alpha", "version": "1.1", "chosen": "1.0", "released": "2026-10-28T11:46:49Z", "eligible": "2026-11-11T11:46:49Z", "min_age_days": 14}}
```
