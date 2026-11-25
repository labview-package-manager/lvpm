# What an install does in LabVIEW

After the files are copied, an install prepares them for their new location
(linkfix, or a LabVIEW relink when asked), runs the install hooks you
allowed, and rebuilds palettes and menus. Only the relink, the hooks and a
venv's resave ([usage](usage.md#the-resave)) need a running LabVIEW; lvpm
talks to it over VI Server.

In a venv, the packages' palette files are kept apart and merged into one
addon after every install and uninstall
([How a venv works](venvs.md#palettes)).

## Linkfix

**Linkfix runs by default after copying, before the PostInstall hooks.** It
makes the LabVIEW files the install wrote ready for their location: it
repairs their dependency links and separates their compiled code, without
opening LabVIEW. Source logic and the saved LabVIEW version are kept; package
archives and unrelated files are not changed. The output names what it
edited and what it skipped, rather than guessing at a layout it does not
know.

`--no-linkfix` skips it; `lvpm linkfix --dry-run --verbose` inspects an
existing tree. It saves LabVIEW searches and save prompts, but does not
replace a package's setup hooks and does not promise that a package works.
[linkfix](linkfix.md) has what it changes, where it stops, its per-file
reports and how it is validated.

## Relinking

Freshly unpacked VIs still declare the paths their build machine wrote:
`<vilib>`-style tokens for the most part, plus a residue of absolute paths
from the build machine.

**The LabVIEW load-and-save pass is opt-in.** `--relink` runs it for every
package instead of linkfix; a package marked `relink = true`, below, gets it
after linkfix without the flag. In measurements across 10 packages saved
between LabVIEW 8.6 and 2020, unrelinked VIs load and run from a venv
untouched: LabVIEW resolves what the linker tables miss by searching vi.lib,
and an LVAddons overlay *is* vi.lib. LabVIEW can also record dependencies
that the packaged file did not list. None of that means every package works
without preparation, or that linkfix reproduces every LabVIEW save.
`tools/linkfix/audit.py` inspects packaged paths;
`tools/linkfix/deptree.py check` checks that declared dependencies resolve.

A package whose VIs only LabVIEW can repair can be marked `relink = true` in
the manifest; it is then relinked after linkfix
([manifest](https://github.com/ThatsTheEnd/lvpm/blob/main/docs/manifest.md#relinking-a-package-in-labview)).

With `--relink`, a small relink VI, shipped beside `lvpm.exe` as
`relink-package.vi` and copied to lvpm's work folder (`%TEMP%` on Windows, the
cache on Linux) when needed, is driven over every installed package folder through
VI Server. It loads each VI, lets LabVIEW resolve the links, saves, and
reports the files it actually rewrote. The VI is saved for LabVIEW 2020, so
any LabVIEW from 2020 on can load it.

- One pass over the whole plan, so no package is relinked before a later
  package's files are on disk.
- Overlapping folders are collapsed first: a folder nested inside another
  package's folder joins that walk instead of repeating it.
- Transient VI Server refusals (error 1000: a reference invalidated by the
  relink's own saves, or a VI not yet out of its running state) retry with a
  fresh reference and backoff.
- `lvpm relink <name>` / `lvpm relink --all` reruns the pass from the
  install manifests, without reinstalling.

## Hooks

A hook is a VI a package ships. lvpm runs it inside your LabVIEW, with your
rights: it can do anything you can. So **install hooks are off by default**.
A hook runs only for a package you allowed.

| Where the package goes | Its install hooks run when |
| --- | --- |
| A LabVIEW installation, from a project (`sync`, `add`) | The package's line in the project's `lvpm.toml` has `hooks = "run"` ([manifest](https://github.com/ThatsTheEnd/lvpm/blob/main/docs/manifest.md#allowing-install-hooks)) |
| A LabVIEW installation, outside a project (`lvpm install <name>`) | The name is in the user config `hooks.run` (`lvpm config set hooks.run <name>...`; [configuration](usage.md#configuration)) |
| Either of those, for one command | The command has `--hooks`, which allows every package |
| A venv | Never during an install or uninstall, and nothing allows one. `lvpm run-hooks <pkg>` runs a kept PostInstall by hand, in the LabVIEW installation the venv is bound to |
| A LabVIEW that runs headless (`LV_RTE_HEADLESS`) | Only with `--hooks`. `hooks = "run"` and `hooks.run` do not apply there |
| A scratch `--prefix` tree | Never. No warning; a plain note says the scratch tree has no LabVIEW to run them |

Only the root `lvpm.toml` allows a package. A `hooks = "run"` in a
dependency's own `lvpm.toml` allows nothing. To allow a package that is only a
dependency of a dependency, list it:
`name = { version = "*", hooks = "run" }`.

A skipped hook is not an error: the files are installed and the command
exits 0. Each skipped package gets a warning on stderr that says what was
skipped, how to run it now and how to allow it for good:

```
warning: pilot_a: skipped install hook PostInstall (hooks are off by default; see docs/labview-integration.md#hooks)
  run it now:     lvpm run-hooks pilot_a
  always run it:  add hooks = "run" to the pilot_a line in [dependencies] of lvpm.toml
```

Outside a project the always-run-it line is `lvpm config set hooks.run <names>`.
It appears once, on the last warning, and lists the names already in
`hooks.run` and every package that command skipped. A headless install says
`(LabVIEW runs headless; hooks = "run" and hooks.run do not apply)` and
offers `add --hooks to this command`. `--dry-run` says `would skip`.

A package whose line says `hooks = "skip"` gets no run-it line. Its warning
says instead that `hooks = "skip"` does nothing and to delete the key. A
package without hooks gets no warning.

The allowlist applies from the next install or upgrade. A package that is
already installed keeps the hook it skipped: run `lvpm run-hooks <pkg>`.
Installing it again says so with `install hook PostInstall was skipped when it
was installed`.

A package with only a PreInstall hook has nothing for `lvpm run-hooks` to
run, because PreInstall runs only at install. It stays off until you
uninstall it, allow it and install it again.

### Uninstall hooks

Uninstall hooks run only if no hook of that package was skipped at install.
When they are held back, `lvpm uninstall` says so:

```
note: declared but not run (its install hook did not run; next time, run lvpm run-hooks pilot_a before uninstalling): PreUninstall, PostUninstall
```

A `lvpm run-hooks <pkg>` that succeeds counts the package's hooks as run, so
its uninstall hooks run again. Run it before you uninstall. A package with
only a PreInstall hook stays off, as above. `lvpm run-hooks --all` does this
for every installed package with a PostInstall hook, allowed or not.

Records written before this change are read as they are. A global,
non-headless install from before it lists no skipped hook, so its uninstall
hooks keep running. A headless install, and a package whose PostInstall
failed, already listed their install hooks as skipped. Their uninstall hooks
no longer run: run `lvpm run-hooks <pkg>` to get them back. `lvpm list` shows
which hooks a package skipped.

### Script-VI hooks

The install hooks a package declares are extracted from the archive and run
at their proper moments. Each is handed the action-info variant hook VIs
read their parameters from (`Package Name`, `LabVIEW Target Version`,
`Quiet Mode = TRUE`, `Files Installed`, ...), so they run without parking
dialogs over the install.

| Hook               | Moment                                                                                                            |
| ------------------ | ----------------------------------------------------------------------------------------------------------------- |
| `PreInstall.vi`    | before the package's files are copied                                                                             |
| `PostInstall.vi`   | after linkfix, or after the relink pass when one runs                                                             |
| `PreUninstall.vi`  | before the package's files are removed                                                                            |
| `PostUninstall.vi` | after removal (uninstall hooks are extracted at install time; the archive is long gone when uninstall needs them) |

Measured on the 22 hooks of a 50-package project: six are harmful from a
venv, and the rest open documentation pages, recompile examples or leave
markers. The one that was needed is NI's Advanced HTTP Client's. It repairs
the Call Library path that VI Package Builder broke when it built the
package, a known VIPM issue: [VI Package Builder removes cross platform
wildcards from Call Library Function nodes during the build process](https://support.vipm.io/hc/en-us/articles/360053941572-VI-Package-Builder-removes-cross-platform-wildcards-from-Call-Library-Function-nodes-during-the-build-process).
Linkfix's `clfn-wildcard` method now makes that repair itself
([linkfix](linkfix.md#call-library-wildcards)). Dataflow G's G-Image works
around the same issue differently: its hook unzips clean copies of its VIs
over the installed ones. Linkfix covers that one too.

[Package notes](package-notes.md) has what we found about commonly used
packages in a venv: whose hooks are needed, and which don't work there yet.

Hook failures are reported, never fatal: the files are installed either way,
and an uninstall is never left half-done. `lvpm run-hooks <name>` reruns a
PostInstall that failed or was skipped. After the hooks, `install` rebuilds the palettes and
menus so a package's palette and Tools entries appear without restarting
LabVIEW; `lvpm refresh` does that step alone.

## VI Server

The transport underneath relink and hooks is a typed VI Server TCP client
(`viserver.rs`): no LabVIEWCLI, no ActiveX. It speaks the flattened-data
formats (scalars, strings, paths, n-dimensional arrays, and variants with
named attributes) and is documented in
[docs/vi-server-protocol.md](vi-server-protocol.md). lvpm reads the
target's port from its `LabVIEW.ini` (LabVIEW 2025 defaults to 3363, 2026 to 3364) and starts the IDE when nothing answers there. Only relinking, hooks
and a venv's resave need LabVIEW; everything else runs without any LabVIEW
process.

For diagnosis the primitives are exposed directly:

- `lvpm vi-probe <VI>` opens a reference and releases it, proving the
  transport.
- `lvpm vi-run <VI> --set name=type:value --get name` sets controls, runs,
  and reads values back (types `bool|i32|dbl|str`, plus `type[]:` arrays).
- `lvpm vi-save <VI>` loads and saves one VI, the relink primitive.
- `lvpm app-probe <ID>...` invokes parameterless Application methods by raw
  id. It invokes whatever it hits; sweep with care.

Verified end to end: a 46-dependency project manifest resolved to 48
packages / 8766 files, installed, relinked in one deduplicated pass and its
15 post-install hooks run, from a single `lvpm install`. This was recorded
before install hooks were off by default; today those hooks run only when allowed.
