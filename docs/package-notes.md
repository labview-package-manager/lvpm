# Package notes

What we have found about packages the LabVIEW community commonly uses when
they are installed into a venv: which need more than a plain install, and
which do not work there yet. Each entry names the version it was checked
with; other versions may behave differently.

Checked against LabVIEW 2026 Q3 (64-bit) on Windows, 2026-11-14, by reading
each hook VI's link table and diagram strings (`tools/linkfix/linkobjs.py`,
[lvkit](https://github.com/Freddd13/lvkit) `describe`) and running the hooks
that matter in a LabVIEW installation.

A venv runs no install hook, and nothing allows one there
([Hooks](labview-integration.md#hooks)). "Hook not needed" below means the
package works without it.

| Package | Checked | Status |
| ------- | ------- | ------ |
| [`dataflow_g_lib_g_image`](#dataflow_g_lib_g_image) | — | Works; its hook is not needed |
| [`delacor_lib_delacor_qmh_event_scripter`](#delacor_lib_delacor_qmh_event_scripter) | 7.1.2.1547 | Works; hook not needed |
| [`delacor_lib_dqmh_documentation`](#delacor_lib_dqmh_documentation) | 7.1.2.1547 | Works; hook not needed |
| [`delacor_lib_dqmh_toolkit`](#delacor_lib_dqmh_toolkit) | 7.1.2.1547 | Works; hook not needed |
| [`delacor_lib_qmh`](#delacor_lib_qmh) | 7.1.2.1547 | Works; its hook would edit LabVIEW itself |
| [`ni_lib_advanced_http_client_api`](#ni_lib_advanced_http_client_api) | — | Works; its hook is not needed |
| [`pantherlab_lib_panther_dashboard`](#pantherlab_lib_panther_dashboard) | 9.4.9.78 | **Does not work** |
| [`sklein_lib_webview2`](#sklein_lib_webview2) | 2.1.1.2 | Works with `relink = true` |
| [`wovalab_lib_antidoc`](#wovalab_lib_antidoc) | 3.3.2.887 | Works; hook not needed |
| [`wovalab_lib_asciidoc_for_labview`](#wovalab_lib_asciidoc_for_labview) | 1.12.1.492 | Works once its PostInstall hook has run: `lvpm run-hooks` |

## Patterns that break in a venv

Most problems come from a few assumptions packages make about being
installed into LabVIEW itself. A package not listed below may still hit one
of them.

- **Paths built from `App.Dir`.** A package that finds its own files, or
  another package's, from the LabVIEW folder (`App.Dir` + `project/...`,
  `vi.lib/...`) looks in the LabVIEW installation. In a venv those files are
  under `.lv-venv/packages/<pkg>/1/`, so the package reports them missing.
  DQMH add-ons that look for DQMH's scripting tools this way are the common
  case (see [Panther Dashboard](#pantherlab_lib_panther_dashboard)).
- **The same package also installed in LabVIEW.** Packages installed into
  LabVIEW earlier, usually by VIPM, win over the venv's copy: LabVIEW loads
  the installation's copy whatever version the venv has, and the Tools menu
  shows both copies' entries. `lvpm install`, `lvpm launch` and
  `lvpm venv status` warn about it and name the packages; remove them from
  LabVIEW with the tool that installed them
  ([Keeping the LabVIEW installation clean](venvs.md#keeping-the-labview-installation-clean)).
- **Hooks that set something up.** Install hooks were written for VIPM
  installing into LabVIEW: many edit palette files in `menus/`, write
  LabVIEW's ini or run installers. A venv runs none of them, so setup that a
  package does only in its hook is missing
  (see [AsciiDoc for LabVIEW](#wovalab_lib_asciidoc_for_labview)).
- **.NET invoke nodes built against another assembly build.** Only a load
  and save in LabVIEW re-resolves them: set
  [`relink = true`](https://github.com/ThatsTheEnd/lvpm/blob/main/docs/manifest.md#relinking-a-package-in-labview).
- **Call Library wildcards removed at build time.** VI Package Builder can
  replace `name*.*` with one bitness's DLL. Linkfix's
  [`clfn-wildcard`](linkfix.md#call-library-wildcards) restores them, so the
  hooks some packages ship for this are not needed.

## Packages

### `dataflow_g_lib_g_image`

G-Image.

- **Hook not needed.** Its PostInstall unzips clean copies of its VIs over
  the installed ones, working around the Call Library wildcards VI Package
  Builder removed. Linkfix restores those wildcards itself.

### `delacor_lib_delacor_qmh_event_scripter`

DQMH Event Scripter. Checked: 7.1.2.1547.

- **Hook not needed.** Its PostInstall only deletes the DQMH install marker
  (see [`delacor_lib_qmh`](#delacor_lib_qmh)).

### `delacor_lib_dqmh_documentation`

DQMH Documentation. Checked: 7.1.2.1547.

- **Hook not needed.** Its PostInstall deletes the DQMH install marker and
  refreshes menus.

### `delacor_lib_dqmh_toolkit`

DQMH Toolkit, the scripting tools behind **Tools » DQMH Consortium**.
Checked: 7.1.2.1547.

- **Hook not needed.** Its PostInstall deletes the DQMH install marker and
  the temporary installer slide-show VI, and opens DQMH's post-install UI.
- **Its `project/` files are in the venv.** Tools that look for them under
  the LabVIEW folder do not find them (see
  [Panther Dashboard](#pantherlab_lib_panther_dashboard)).

### `delacor_lib_qmh`

DQMH Framework: Palette. Checked: 7.1.2.1547.

- **Its hook would edit LabVIEW itself.** Its PostInstall adds DQMH to the
  Functions palette with `Read Palette.vi` / `Write Palette.vi`, writing
  `functions_Delacor_lib_QMH.mnu` into the palette's `dir.mnu` under the
  LabVIEW installation, not the venv. A venv not running it keeps that
  installation as it was.
- **The DQMH install marker.** Each Delacor package's PreInstall creates
  `DQMH Install In Progress.txt` and its PostInstall deletes it. A venv runs
  neither, so the marker never exists.

### `ni_lib_advanced_http_client_api`

NI Advanced HTTP Client.

- **Hook not needed.** Its hook restores the Call Library wildcards VI
  Package Builder removed; linkfix's
  [`clfn-wildcard`](linkfix.md#call-library-wildcards) does that itself.

### `pantherlab_lib_panther_dashboard`

Panther Dashboard for DQMH. Checked: 9.4.9.78.

- **Does not work in a venv.** **Tools » Panther Dashboard for DQMH** shows
  the splash and project-selection screens, the scan runs, and the dashboard
  window never opens. LabVIEW does not crash and every dependency resolves
  (`deptree.py check`). `Main.vi` shows its panel only after
  `Scan DQMH Modules from Project.vi` returns without error, so either the
  scan returns an error or `Main.vi` is broken in the venv. Cause not known
  yet.
- **Its DQMH scripting buttons cannot find DQMH.** `Get DQMH Scripting UI.vi`
  and `Check if HSE Scripting Tools are installed.vi` build their paths from
  `App.Dir` + `project/Delacor/DQMH/...`, the LabVIEW folder. In a venv,
  DQMH's `project/` files are under
  `.lv-venv/packages/delacor_lib_dqmh_toolkit/`, so both report the tools as
  missing. The `PantherDashboard Scripting Hooks` VIs read `App.Dir` the same
  way.
- **Its PostInstall hook is needed, but would not make it work.** It moves
  the DQMH scripting plugins into the default data folder, writes Panther's
  boolean setting into LabVIEW's ini and opens the Panther site. Without it,
  DQMH's scripting tools do not see Panther's plugins.

### `sklein_lib_webview2`

WebView2. Checked: 2.1.1.2.

- **Needs `relink = true`.** Its .NET invoke nodes are built against another
  build of the assembly; only a load and save in LabVIEW fixes them (see
  [Relinking a package in LabVIEW](https://github.com/ThatsTheEnd/lvpm/blob/main/docs/manifest.md#relinking-a-package-in-labview)).

### `wovalab_lib_antidoc`

Antidoc. Checked: 3.3.2.887.

- **Hook not needed.** Its PostInstall shows a post-installation dialog and
  opens the Antidoc documentation.
- **Renders through AsciiDoc for LabVIEW.** HTML and PDF output needs
  [its toolchain](#wovalab_lib_asciidoc_for_labview).

### `wovalab_lib_asciidoc_for_labview`

AsciiDoc for LabVIEW. Checked: 1.12.1.492.

- **Its PostInstall hook must run, and a venv install does not run it.** The hook
  checks for the Asciidoctor toolchain (Ruby) and installs or updates it.
  Windows only; takes about 40 s. Without it, the toolkit, and Antidoc on top
  of it, cannot render HTML or PDF.
- **The toolchain is per user, not per venv.** The hook installs it under
  `%LOCALAPPDATA%\Asciidoc For LabVIEW\RubyInstance`, so one run serves every
  venv and LabVIEW installation of that user, and removing the venv leaves it
  behind.
- **Run it once by hand.** In the project, `lvpm run-hooks
  wovalab_lib_asciidoc_for_labview` runs the PostInstall the venv kept. It
  runs in the LabVIEW installation the venv is bound to, not in the venv's
  own LabVIEW, which is fine here: what it installs is per user, not per
  LabVIEW.

## Adding an entry

Check a package's hooks before you write it up, and record the version you
checked:

- `python tools/linkfix/deps.py <pkg>` lists which packages in a closure
  carry install hooks.
- A venv install keeps the hook VIs it does not run, as
  `.lv-venv/hooks/<pkg>-PostInstall.vi` and so on.
  `python tools/linkfix/linkobjs.py show .lv-venv/hooks/<pkg>-PostInstall.vi`
  shows what the hook calls.
- [lvkit](https://github.com/Freddd13/lvkit) (`pip install lvkit`) reads VIs
  without LabVIEW: `lvkit describe <vi>` shows the hook's diagram, its
  constants, cases and calls.

Add the package to the table at the top and give it a section under
[Packages](#packages), in alphabetical order. If what you found applies to
other packages too, add it to [Patterns that break in a venv](#patterns-that-break-in-a-venv).
