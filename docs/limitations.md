# Known limitations

- `Replace Mode = If Newer` only writes when the file is absent; doing it
  properly means comparing archive timestamps against disk.
- Installing a package that is already at that version does nothing, so a
  package VI edited or resaved by mistake cannot be repaired that way;
  `lvpm uninstall` and then `lvpm install` does it.
- Dependency conflicts are solved only when a project is locked. A named
  install outside a project still lets the first resolution of a name win,
  and a floor resolves to the newest version satisfying it.
- No mass compile; LabVIEW recompiles installed VIs on first load.
- Install hooks are off by default and the allowlist is by package name, not
  by VI: allowing a package allows every hook VI it ships, and a hook runs
  with your rights. The rules are in [Hooks](labview-integration.md#hooks).
- A hook VI's own `error out` is not read back. A hook that fails internally
  but runs to completion reports as ok.
- A failure part-way through unpacking leaves files behind with no record,
  and one failed download aborts the rest of the plan.
- In a venv, no install or uninstall hook runs during an install or uninstall,
  and nothing allows one (`lvpm run-hooks` runs a kept PostInstall by hand): they
  act on the LabVIEW installation, not on an overlay. File groups aimed at
  machine locations (`<OS ...>`, `<temp>`) are skipped. Both are recorded and
  shown by `lvpm list`.
- Some packages don't work in a venv yet (Panther Dashboard for DQMH), and
  some need a hook run by hand (AsciiDoc for LabVIEW).
  [Package notes](package-notes.md) lists them.
- `lvpm launch` checks install records, not file contents: a package VI
  edited or resaved since install is not detected.
- In NI's LabVIEW Linux images CI runs only the smoke flow
  ([Continuous integration](https://github.com/ThatsTheEnd/lvpm/blob/main/CONTRIBUTING.md#continuous-integration));
  the `@labview` acceptance scenarios run in the Windows image only. Linux
  does not yet honour a package's `Exclusive_OS` ([Linux](https://github.com/ThatsTheEnd/lvpm/blob/main/docs/linux.md)).
- `lvpm build` packages one library's members as a `.lvpkg`, lvpm's own
  format: no build hooks, no probes or templates from outside the library.
  VIPM cannot read it, so don't hand it to VIPM users
  ([ADR 6](adr/0006-lvpkg-only-vip-read-only.md)).

The [roadmap](https://github.com/ThatsTheEnd/lvpm/blob/main/docs/roadmap.md)
has what is planned.
