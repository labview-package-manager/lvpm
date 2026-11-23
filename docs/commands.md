# Commands

The user reference for `lvpm`'s commands and flags. `lvpm <command> --help`
has the full list of each command's options. Design notes on the interface,
and what changes before it is called stable, are in [cli.md](cli.md).

| Command                                              | What it does                                                                                                                                 |
| ---------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| `lvpm install <name>[@<version>]`                    | Resolve dependencies, download, verify MD5, unpack, prepare links and source-only files, run hooks |
| `lvpm install`                                       | Inside a project: install everything `lvpm.toml` lists, preparing links after the whole plan is copied (`--manifest <FILE>` selects a manifest) |
| `lvpm linkfix` / `--tree <DIR>`                     | Prepare an existing venv or explicit LabVIEW-shaped tree; flags in [linkfix.md](linkfix.md) |
| `lvpm uninstall <name>` / `--all`                    | Remove installed files; optionally remove unshared dependencies                                                                              |
| `lvpm list`                                          | What lvpm has installed into the selected target, and which hooks it did not run                                                             |
| `lvpm search <query>`                                | Search the package indexes                                                                                                                   |
| `lvpm targets`                                       | Detected LabVIEW installations                                                                                                               |
| `lvpm relink <name>` / `--all`                       | Rerun the relink pass from the install manifests, without reinstalling                                                                       |
| `lvpm run-hooks <name>` / `--all`                    | Rerun a package's PostInstall hook, for one that failed during install                                                                       |
| `lvpm refresh`                                       | Rebuild palettes and the File/Tools/Help menus from disk (`install` does this itself)                                                        |
| `lvpm start`                                         | Start the selected LabVIEW and wait until its VI Server answers                                                                              |
| `lvpm venv create` / `status` / `remove`             | Manage the project's venv (see [venvs.md](venvs.md))                                                                                         |
| `lvpm launch [<lvproj>]`                             | Check the venv against the manifest, then start a LabVIEW on it (see [venvs.md](venvs.md#launching-labview))                                 |
| `lvpm config get` / `set`                            | Read or change user-level settings (see [configuration.md](configuration.md))                                                                |
| `lvpm cache add <path>`                              | Import package archives into the shared cache (see [package-sources.md](package-sources.md#the-cache))                                      |
| [`lvpm sbom`](#sbom)                                  | Export installed packages, saved LabVIEW projects and build specifications, or selected artifacts as CycloneDX 1.5 JSON |
| `lvpm import <file>`                                 | Write VIPM's `vipm.toml` or a `.vipc` into `lvpm.toml` once (see [compatibility.md](compatibility.md))                                      |
| `lvpm vi-probe` / `vi-run` / `vi-save` / `app-probe` | VI Server primitives, for diagnosis (see [install-process.md](install-process.md#vi-server))                                                 |

## Flags for every command

| Flag                       | Meaning                                                                                                   |
| -------------------------- | --------------------------------------------------------------------------------------------------------- |
| `--labview-version <YYYY>` | Install into this LabVIEW version (see `lvpm targets`)                                                    |
| `--project <DIR>`          | Use that project's venv instead of looking upwards from the current directory                             |
| `--global`                 | Use the LabVIEW installation itself, even from inside a project that has a venv                           |
| `--prefix <DIR>`           | Install into a scratch directory instead of LabVIEW; linkfix still runs, but LabVIEW relink and hooks cannot |
| `--refresh`                | Ask each index whether it changed (a conditional request) instead of trusting the cache in `%LOCALAPPDATA%\lvpm\cache` |
| `--log-level <LEVEL>`      | Lowest level written to the log file (see [configuration.md](configuration.md#logs))                      |

See [Global flags by SBOM mode](#global-flags-by-sbom-mode) for their effect on SBOM export, merge, and diff.

## `install`

`--dry-run` shows every file, relink folder and hook and changes nothing;
`--no-deps` skips dependencies; `--allow-downgrade` permits replacing an
installed package with an older version; `--no-linkfix` leaves package files
as copied, skipping link repair and source-only preparation. `--relink` loads
and saves VIs in LabVIEW **instead of linkfix** (off by default); the legacy
`--no-relink` is accepted but does not disable linkfix. Hooks are off by default
in a venv and on a headless machine, and on by default in a LabVIEW
installation; `--hooks` runs every package's hooks whatever the manifest says,
`--no-hooks` runs none, and `hooks = "run"` or `"skip"` sets one package (see
[the manifest](manifest.md#dependency-tables-hooks-and-relink) and
[ci-container.md](ci-container.md)). `--relink-timeout <SECS>` (default 900)
bounds one folder's relink; `--keep-open` leaves the LabVIEW started for a venv
relink running.

Inside a project, `--with <GROUP>`, `--without <GROUP>` and `--only <GROUP>`
choose the manifest's dependency groups (see
[manifest.md](manifest.md#named-dependency-groups)).

In a venv, `lvpm install <name>` also writes the package into `lvpm.toml`
(`--group <GROUP>` picks the group, `--no-save` skips it); see
[venvs.md](venvs.md).

## `sbom`

Run `lvpm sbom --output bom.cdx.json` after installing the project's packages.
The command uses the `lvpm.toml` and package records for the selected target.
It follows installed dependencies from the manifest's `main` group by default.
Use the same `--with`, `--without`, or `--only` flags as `install` to select other
manifest groups. The project name and version come from `[project]` when set.
Missing installed dependencies or versions that do not meet the selected
manifest constraints fail the export.

Package PURLs use `pkg:generic/vipm/<name>@<version>`. Installation records
MD5 and SHA-256 of the original package archive and reads `[Description]`
`Vendor` and `License` from its spec. The export uses the vendor as supplier
and preserves the declared license name, with provenance for each.
Package archive hashes and delivered artifact hashes are marked with
their corresponding digest subject. Blank declarations are omitted.
The generic VIPM PURL identifies the package by the local lvpm naming
convention. No repository origin is assumed.
Packages installed with older versions of lvpm still export, with archive
hashes and spec metadata available after reinstalling them.

Use `lvpm sbom --include manual.cdx.json --output bom.cdx.json` to add
manually tracked components as direct dependencies of the generated product.
The input is a CycloneDX 1.5 JSON inventory with a document header and a flat
`components` array. Each entry needs a name, type, and non-empty `bom-ref`.
Identical entries are reused. Conflicting entries with the same PURL or
`bom-ref` fail the export. The input needs no product metadata or dependency
graph.

### Global flags by SBOM mode

The SBOM commands share the [global flags](#flags-for-every-command).
Place them before or after `sbom`, `sbom merge`, or `sbom diff`.
Their effect depends on the export mode.

| Flag | Export installed packages from `lvpm.toml` | Static VI dependency scan |
| --- | --- | --- |
| `--labview-version <YYYY>` | Selects the LabVIEW installation for a global export. A selected venv uses its bound version. | Selects the LabVIEW installation when no venv is selected. A selected venv uses its bound version. |
| `--project <DIR>` | Uses the project's venv and its bound manifest. | Uses the project's venv for file ownership. Pass the saved project separately with `--lvproj`. |
| `--global` | Bypasses the venv. Use `--labview-version` with it to export records from a LabVIEW installation. | Bypasses the venv and uses the selected or detected LabVIEW installation. |
| `--prefix <DIR>` | Reads package records from the scratch installation under `DIR`. The export still needs a project `lvpm.toml`. | Bypasses the venv like `--global`. The scan does not read package records or files from the prefix. |

`--project` conflicts with `--global` and `--prefix`. For an
installed-package export, `--prefix` and `--labview-version` are
mutually exclusive. A static scan can combine them to bypass the
venv and select a LabVIEW version.

The four target selection flags leave the results unchanged for file-only
exports from `--nipkg`, `--lvlibp`, or `--artifact` without a static VI
scan, for buildspec exports without a static VI scan, and for `merge`
and `diff`.

SBOM commands use local inputs, so `--refresh` does not query package
indexes. The CLI validates `--log-level` and returns exit code 2 for
invalid levels. SBOM commands keep file logging disabled and report
diagnostics separately.

### Scan a saved LabVIEW project

Use `--lvproj` on its own to scan the saved source items of all targets.
lvpm starts with the VI and library items, follows their static links and
looks inside physical folders declared in the project. The saved build
specifications and LabVIEW's generated Dependencies tree are outside the
root selection. Files shared by multiple targets are scanned once in the
selected LabVIEW environment.

```powershell
lvpm sbom --lvproj MyProject.lvproj --output project.cdx.json
lvpm sbom --lvproj MyProject.lvproj --include manual.cdx.json --output project-with-manual.cdx.json
```

The product name comes from the project filename. The scan matches files
against lvpm installation records and, on Windows, the selected LabVIEW
installation's VIPM database when present. It reads the database directly
from disk. Use `--vipm-database <DIR>` to override the database. Unowned
external files are counted in the SBOM. The scan follows the installed
requirements of the packages it reaches, as described under
[Scan static VI dependencies](#scan-static-vi-dependencies).
This mode needs no project manifest.

The scan reads recorded imports from available PPLs. Explicit DLL items
and DLLs under declared physical folders include a file hash and available
PE import evidence. These components describe project files. For a release
file inventory, use the corresponding build specification and its
actual output files. See [Inventory published build outputs](#inventory-published-build-outputs)
for PE import resolution limits.

Use `--allow-incomplete` to export a partial graph when some sources or
links cannot be read. The command retains scan issues in the SBOM.
`--include` adds manually tracked components as direct project dependencies.
The project-wide mode rejects `--artifact`.
For a build-specific scan, the target and build specification must be
supplied together.

### Build specification support

Use `--lvproj`, `--lvproj-target` and `--lvproj-build-spec` to select a
saved specification by its exact target and name. Use `--artifact` to
add the files that will ship.

| Build specification | Saved project evidence | Built output evidence with `--artifact` |
| --- | --- | --- |
| EXE | Static VI graph, installed package ownership and referenced PPLs | Selected file hashes, primary EXE and PE import evidence |
| Source Distribution (SRC) | Static VI graph, installed package ownership and referenced PPLs | Selected distribution file hashes |
| Packed Project Library (PPL) | Static VI graph from the source library and included items, installed package ownership and referenced PPLs | Primary `.lvlibp` and selected file hashes. Use `--lvlibp` to export direct binary PPL references on their own. |
| NI Package (NIPKG) | Configured NI requirements, selected EXE and SRC dependency graphs, and available selected PPLs | Built control requirements, hashes for direct TAR members and supported PPL references. Saved declarations remain as metadata. |
| Installer | Configured additional installers and selected PPLs | Selected file hashes and, on Windows, declared MSI product, file and feature metadata |
| Shared Library (DLL) | Configured PPL selections when available | Primary DLL, selected file hashes and PE import evidence. Full shared-library source traversal is outside this mode. |
| ZIP (LabVIEW `Zip File`) | Supported when configured PPL selections are recognized | Hashes for selected outputs. ZIP contents are outside the scan. |

Nested CAB and MSI inspection inside NIPKG is Windows-only. An MSI file
table describes possible installation contents. A selected CAB outside
NIPKG is recorded as an artifact with a hash. See
[Inventory published build outputs](#inventory-published-build-outputs)
for artifact selection and coverage limits.

### Export a release SBOM for an EXE shipped in an NIPKG

This example starts with a saved `MyProject.lvproj` with `EXE` and `NIPKG`
specifications under `My Computer`. The existing release outputs are
`out/exe/MyApp.exe`, its accompanying files in `out/exe`, and
`out/nipkg/myapp_1.2.3_windows_x64.nipkg` containing that exact EXE.
Run the commands from the project root with the project's LabVIEW and
package environment available. Keep SBOM output in `sboms`, outside the
selected artifact directories.

Record any separately verified application dependencies in
`manual.cdx.json`. For example, use this entry if the application uses
SQLite 3.46.1.

```json
{
  "bomFormat": "CycloneDX",
  "specVersion": "1.5",
  "version": 1,
  "components": [
    {
      "type": "library",
      "bom-ref": "sqlite-3.46.1",
      "name": "SQLite",
      "version": "3.46.1",
      "purl": "pkg:generic/sqlite@3.46.1"
    }
  ]
}
```

Export the application and package inventories. The example uses
`--allow-incomplete` for the EXE so reviewed static scan gaps can remain
in the SBOM. The separate `files` policy still requires complete file
evidence. Review any runtime warnings as part of the release.

```powershell
New-Item -ItemType Directory -Force sboms | Out-Null
lvpm --labview-version 2026 sbom --lvproj MyProject.lvproj --lvproj-target "My Computer" `
    --lvproj-build-spec EXE --artifact out/exe --artifact-root out/exe `
    --primary-artifact MyApp.exe --include manual.cdx.json --allow-incomplete `
    --fail-on-incomplete=files --diagnostics-json --output sboms/exe.cdx.json
if ($LASTEXITCODE -ne 0) { throw "Application SBOM export failed" }

lvpm sbom --lvproj MyProject.lvproj --lvproj-target "My Computer" `
    --lvproj-build-spec NIPKG --artifact out/nipkg/myapp_1.2.3_windows_x64.nipkg `
    --artifact-root out/nipkg --fail-on-incomplete=files --diagnostics-json `
    --output sboms/nipkg.cdx.json
if ($LASTEXITCODE -ne 0) { throw "Package SBOM export failed" }
```

The NIPKG export reads the built control requirements and retains the
saved declarations for comparison. `--primary-artifact` accepts a path
relative to `--artifact-root` and disambiguates multiple matching files.

Verify that the package inventory contains exactly one file with the
same SHA-256 as the selected EXE.

```powershell
$application = Get-Content -Raw sboms/exe.cdx.json | ConvertFrom-Json
$package = Get-Content -Raw sboms/nipkg.cdx.json | ConvertFrom-Json
$exeHash = ($application.metadata.component.hashes | Where-Object alg -eq "SHA-256").content
$payloadMatches = @($package.metadata.component.components | Where-Object {
    $_.hashes | Where-Object { $_.alg -eq "SHA-256" -and $_.content -eq $exeHash }
})
if (!$exeHash -or $payloadMatches.Count -ne 1) { throw "EXE digest must match one NIPKG payload file" }
```

Save this plan as `sboms/release-plan.json`. The child SBOM path in the
plan is relative to the plan file.

```json
{
  "children": {
    "app": "exe.cdx.json"
  },
  "relationships": [
    {
      "parent": "primary",
      "child": "app",
      "kind": "contains",
      "evidence": "The EXE SHA-256 matches a unique file in the NIPKG payload."
    }
  ]
}
```

```powershell
lvpm sbom merge --primary sboms/nipkg.cdx.json --relationships sboms/release-plan.json `
    --fail-on-incomplete=files --diagnostics-json --output sboms/release.cdx.json
if ($LASTEXITCODE -ne 0) { throw "Release SBOM merge failed" }
```

The merged SBOM keeps the NIPKG as the product and enriches its matching
EXE payload component with the application SBOM. NI requirements stay on
the package and SQLite stays on the EXE. Add a `requires` relationship
when a separately evidenced product is a dependency of a specified
component. See [composition plans](#compose-explicitly-related-artifact-sboms)
and [coverage policy](#sbom-validation-and-ci-exit-codes). A release
policy requiring complete runtime dependency evidence can use
`--fail-on-incomplete=runtime`. A file gate can pass while the runtime
gate returns exit code 4.

### Read NI package requirements

To export the explicitly configured NI package requirements from a LabVIEW
package build specification, without running LabVIEW or building the package:

```powershell
lvpm sbom --lvproj TEMPLATE.lvproj --lvproj-target "My Computer" --lvproj-build-spec NIPKG --output nipkg.cdx.json
```

Select the exact target and build specification names. This mode reads the
saved project XML and needs no `lvpm.toml`. Reading the NI requirement
declarations needs no installed packages. Required packages appear under the
product's `dependsOn`. Recommended and suggested packages are listed with
their declared relationship. The version bounds stay in component properties
and the dependency versions are omitted because no versions are resolved.
The product version is the configured package version. The export also
scans available PPL files selected for the package. When the saved NIPKG
selects an EXE or Source Distribution build specification as a source, the
export attaches that build as a configured child and scans the source
build's static VI dependencies. NI requirements stay on the package.
VIP packages matched to files and their installed package requirements
stay in the child build's graph.
The scan can use `--vipm-database <DIR>` to override the default database.
The resulting SBOM describes the package configuration. Use a built
artifact for evidence of the final payload.

To read the declared requirements and packed libraries of a built NI package:

```powershell
lvpm sbom --nipkg template_1.0.0-8_windows_all.nipkg --output nipkg-built.cdx.json
```

The export reads `control` from the archive's `control.tar.gz` or
`control.tar` member. It uses the package name and version recorded in the
artifact. Single-package clauses in `Depends` become direct product
dependencies. `Recommends`, `Suggests` and alternative clauses are retained
as declarations. Alternative packages are listed without adding a
required edge for each choice. The metadata keeps the full control fields
for comparison with the build-specification export. Minimum and maximum
constraints are also recorded on dependency components. Their installed
versions remain unknown. The export also reads regular `.lvlibp` files
directly from `data.tar.gz` or `data.tar`. It records each file's path,
SHA-256 and any available version. It links relative PPL references to
files in the same payload when their library identifiers agree. Other
PPL references stay as declarations.

On Windows, the export also reads regular CAB and MSI files in the TAR
payload. It uses the MSI file and media tables to match cabinet entries
to their configured target paths, with support for sibling CABs and
cabinet streams inside the MSI. For other CAB entries it uses the path
and filename in the cabinet. A nested path is recorded as
`container!/file`. Relative PPL references can resolve within the same
container.

This uses temporary files and Windows' offline archive and database
readers. It does not install packages or run installer actions. On other
platforms, the export reads direct TAR entries and reports that MSI and
CAB contents could not be scanned. Multi-volume cabinet entries are
reassembled when the required volumes are in the same package directory
or in the same MSI. A required volume that is missing or inconsistent
is reported as a payload scan issue. Other embedded archive types remain
outside the scan. The PPL dependency graph is marked
incomplete because it covers recorded references and leaves embedded
library inventory open.

### Scan static VI dependencies

For EXE, Source Distribution and Packed Project Library build specifications,
the same flags select the saved build configuration and export packages reached
through static VI links.

For a Packed Project Library, select the named build specification, for example
`--lvproj-build-spec "My Packed Library"`. The scan starts from its source
`.lvlib` and included items and follows members of nested libraries and classes.
It exports the product as a CycloneDX library. When the scan reaches a built
`.lvlibp`, it reads its direct PPL references offline and preserves them
as dependency edges beneath that library. A relative reference is followed
to a local PPL when its recorded library identifier matches. Unresolved
references stay in the graph. These binary scans are marked incomplete for
the affected components because they do not inventory embedded packages.

Installer and NI Package build specifications scan PPLs selected in their
configured sources or explicitly selected build outputs, when those files
are available. Other build types can use this scan when their saved source
settings use the standard LabVIEW item selection fields. Components selected
as sources have the `lvpm:ppl:shipment` property set to `configured-source`.
A library found only through a link may be an external runtime dependency.
The build specification records configured inputs, so this does not prove
which files are present in the finished artifact. The installer and package
source reader supports ordinary project paths and output destinations
relative to the project directory. LabVIEW-token and installation-relative
distribution paths need additional resolution.

For EXE, Source Distribution and Packed Project Library, the scan follows subVIs
starting from the top-level and explicitly included build sources. It follows links inside verified
embedded VI instances in the caller's VINS resources. Class API references
also lead to the referenced class's saved non-static method VIs. From a
venv, or with `--project <DIR>`, the scan uses the LabVIEW version bound to
that venv. Its
installed addons supply library files that are absent from the bound LabVIEW
installation. If both have the same file, LabVIEW loads its installed copy.
The scan uses the same order. Use `--global`
to bypass a venv, or `--labview-version <YYYY>` to select a LabVIEW
installation when no venv is in effect.

File ownership is matched against the paths actually scanned. lvpm reads
the active venv's records and the base installation's records. On Windows,
it also reads the matching VIPM database when present. Use
`--vipm-database <DIR>` to supply a different database. The offline reader
uses the absolute paths in each package's `files-installed` list and reads
the package version, metadata and `Requires` from its archived spec. The
graph follows VI links through unowned files to identify package owners,
then walks the installed package requirements transitively. For packages
installed by lvpm, it uses the dependency names saved in the installation
records. Archived specs can list package names alone or supply a version
comparison using `>=`, `<=`, `>`, `<`, `=` or `==`. Separate entries with a
comma or semicolon. Each selected installed copy must satisfy all comparisons
for that dependency from the requiring package.

Each package appears once per version, with a count of its matched files.
A package reached only through declared requirements has a count of zero.
If several configured NIPKG sources share a package, the reported count is
the largest found in any one source scan.
The requiring component records each resolved declaration in a
`lvpm:package:requires` property. A lone `>=` comparison appears as `minimum`.
Other comparisons or combinations appear in `constraints` with their operators
and required versions. The dependency graph retains both the observed VI links
and these declared package dependencies. A package
declaration can include development tools and examples. Use the built
artifact's file inventory to establish what ships. Unowned external
files are counted in metadata. Arbitrary dynamic loads remain outside
the scan.

Unresolved links, unreadable files, ambiguous package ownership and
unsatisfied package requirements fail the scan.
`--allow-incomplete` writes the resolved graph with an incomplete
composition and scan diagnostics in metadata. This mode reads project
files and the selected installation without launching LabVIEW or building
an artifact.

### Compare a NIPKG build specification with its built package

```powershell
lvpm sbom diff --buildspec nipkg-buildspec.cdx.json --built nipkg-built.cdx.json
```

Pass the two exports from `lvpm sbom --lvproj ...` and `lvpm sbom --nipkg ...`
for the same package name. The command prints a JSON report with
`added` (built only), `missing` (build specification only), and `changed`
requirements. Changed entries show the before and after requirements.
Equivalent minimum and maximum declarations are compared in the same
operator format. The comparison covers required edges, optional
relationships, version constraints and alternatives. Product versions are
included for context. The diff compares declarations and does not resolve
dependencies or build a package.

### Read a built Packed Project Library

```powershell
lvpm sbom --lvlibp example.lvlibp --output ppl.cdx.json
```

The export reads the built file offline. It supports the observed LEIF
layouts `0x1209` and `0x1309`, either directly or in a Windows PE resource.
The product
has the input filename stem, the file's SHA-256 and the PE file version
when available. Direct references from the LEIF table become library
components and product dependency edges. Each keeps the recorded path and
library identifier. The export leaves dependency versions unset.

The composition is marked incomplete because this scan covers direct
PPL references. It does not inventory packed-in VI/package contents,
resolve the referenced PPLs, or infer native DLLs and runtime dependencies.

### Merge a PPL SBOM into an existing component

```powershell
lvpm sbom merge --primary exe.cdx.json --child sboms/licence.cdx.json sboms/helper.cdx.json --output merged.cdx.json
```

Each PPL SBOM can be stored separately from its library. Pass one or more
files after `--child`, or repeat the option. Files are merged in argument order
and the output is written only if every merge succeeds. A child product with
`lvpm:ppl:artifact-name` is matched to an existing PPL component by SHA-256
when possible, otherwise
by a unique configured `.lvlibp` filename with compatible versions when
both are known. Conflicting hashes and ambiguous or absent matches fail.
Product display names and SBOM filenames do not participate in matching.
Both PPL export modes record the artifact filename when available.

The matched component keeps its reference and incoming edges. The merge
adds the PPL SBOM's dependency graph below it and preserves the original
binary references and completeness information. Other child products are
attached as direct dependencies of the primary product.

### Add a child SBOM as a dependency

Use `--child` when the child product is a direct dependency. For an EXE
shipped inside the NIPKG, use the `contains` relationship in the
[release example](#export-a-release-sbom-for-an-exe-shipped-in-an-nipkg).

```powershell
lvpm sbom merge --primary nipkg.cdx.json --child exe.cdx.json --output merged.cdx.json
```

The primary product remains in `metadata.component`. The child product
is listed as a component and becomes a direct dependency of the primary
product. Component references are remapped as needed so each product
keeps its dependency graph. Child scan properties and composition
records are preserved, including incomplete scan markers. Identical
shared components are reused. Conflicting components with the same PURL
and dangling dependency references fail the merge.

Merge reads CycloneDX 1.5 JSON component inventories, including nested
components, dependencies, and compositions. It requires a product
component with a `bom-ref`. Services, vulnerabilities, annotations,
document-level properties, and signatures are outside the merge format.
The operation works on the JSON files without LabVIEW or a package build.

### Inventory published build outputs

After a build, select the exact files and directories to deliver. This
works with or without a saved LabVIEW build specification.

```powershell
lvpm sbom --artifact "out\MyApp.exe" --artifact "out\data" --artifact-root out --product-version 1.2.3 --output "sboms\exe.cdx.json"
lvpm --labview-version 2026 sbom --lvproj Example.lvproj --lvproj-target "My Computer" --lvproj-build-spec EXE --artifact out --artifact-root out --output "sboms\exe.cdx.json"
```

Repeat `--artifact` for multiple outputs. The artifact root determines
portable relative paths. If it is omitted, lvpm uses the common parent
of the selected paths. It requires the actual files to exist and rejects
overlapping scopes and symbolic links or reparse points. Put the SBOM
outside any selected output directory. The output includes SHA-256 and
size for files. Directory scopes are enumerated in path order. Files that
need binary inspection are copied to temporary storage while their hashes
are computed. lvpm normally stages copies beside the output. For long MSI
paths on Windows it can use the system temporary directory outside the
selected artifact directories. Before replacing the SBOM, lvpm rechecks
the original file digests and directory contents. It rejects changed inputs
and removes the temporary copies when the command ends.

For EXE, DLL, PPL and NIPKG specifications, lvpm selects an output of
the expected kind. It checks the configured filename when one is
available for EXE, DLL or PPL and checks the package name in NIPKG
control metadata. Use `--primary-artifact RELATIVE/PATH` when more than
one candidate matches. The primary file gets the product hash. Other
files are nested components in the product assembly. Without a build
specification, use `--product-name` if the name cannot be derived from a
single file or directory. `--product-version` can set the release
version independently of a binary file version.

Saved build settings, static VI references and installed ownership
records remain configuration and source evidence. The explicit output
selection establishes which file bytes ship. The DLL buildspec path
uses the actual DLL and any configured PPL source information. It does
not claim full shared-library source traversal. Artifact export marks
runtime coverage unknown when the provided evidence is incomplete.

The PE reader reports the COFF machine, file version if present, and
regular and delay imports for selected EXE and DLL files. A regular
import creates a dependency on the named module with unresolved file
identity. A unique name and machine match is recorded as a candidate.
Delay imports are recorded as observations. DLL search order and
runtime-resolved plugins can require additional evidence. The reader
does not execute binaries or search the host for dependencies.

For a selected NIPKG, direct regular payload members are nested under
the package with verified SHA-256 hashes. For a primary NIPKG, the direct
file inventory and packed-library scan share one TAR traversal and its
expanded-byte, entry and path limits. The declared NI requirements remain
separate components and edges of the package. Packed libraries
identified by the package reader retain their links. Other nested
archive contents carry incomplete-coverage diagnostics. With a NIPKG
buildspec and matching built package, the built control fields supply
the observed requirements and the saved declarations remain available
as metadata.

A saved Installer buildspec retains its `DistPart` additional installer
declarations. On Windows, selected MSI files also supply read-only
File, Component and Feature table observations, product properties and
default destinations where resolvable. File rows are declarations,
including conditional features. They do not establish what an MSI
run installs or which cabinet bytes are available. Use `--include` or
an explicit composition plan for separately evidenced prerequisites.
[NI additional installer metadata](https://knowledge.ni.com/KnowledgeArticleDetails?id=kA0VU000000DqDh0AK&l=en-US)
· [Windows Installer File tables](https://learn.microsoft.com/en-us/windows/win32/msi/file-tables-group)

### Compose explicitly related artifact SBOMs

Use a relationship plan when a child is shipped as part of a product
or when the dependency has a particular owner. The plan can contain
relative paths to the other SBOMs. Child aliases are local to this
plan. The reserved alias `primary` refers to `--primary`.

```json
{
  "children": {
    "app": "exe.cdx.json",
    "runtime": "runtime.cdx.json"
  },
  "relationships": [
    {"parent": "primary", "child": "app", "kind": "contains", "evidence": "Verified installer payload"},
    {"parent": "app", "child": "runtime", "kind": "requires", "evidence": "Published runtime prerequisite"}
  ]
}
```

```powershell
lvpm sbom merge --primary nipkg.cdx.json --relationships release-plan.json --output release.cdx.json
```

`contains` creates nested CycloneDX components and `requires` adds
an edge to `dependencies`. If an artifact digest identifies a
unique existing file, the corresponding product inventory can enrich
that node. Compatible additional hash algorithms and missing supplier
metadata can enrich the component. Conflicting digests for the same
algorithm, supplier declarations, license declarations and ambiguous
artifact matches stop the merge before it replaces the output.

The import records source BOM digests, revisions, serial numbers
when supplied, supported metadata and a map of imported references.
Reapplying a child checks its mapped inventory, dependency edges,
containment and declared composition against the merged graph. If
these no longer match, or an older output lacks the map, run the
merge again from the original inputs. A merge with a numbered primary
BOM creates a deterministic serial number for the resulting document
and starts it at revision 1. The original serial and revision remain
in the source record. `--child` remains available for the default
dependency placement.

The portable Rust tests generate their inputs and run without LabVIEW.

```powershell
cargo test --locked -p lvpm --test sbom_artifacts
```

### SBOM validation and CI exit codes

Export and merge validate the result against the embedded CycloneDX 1.5
JSON schema before replacing the output. The schema and its references
are pinned to the upstream specification tag `1.7.2`. Validation runs
offline. lvpm also checks fixed digest lengths, unique `bom-ref` values,
duplicate dependency rows and references in the inventory graph. A missing
dependency row can represent unknown dependencies. Merge and diff validate
their input BOMs. Manual component lists still accept identical entries.

The output must be distinct from every file used as input, including the
manual inventory and all merge inputs. Aliases such as hard links and
symbolic links are checked. Use an existing parent directory. If validation,
policy or the output safety checks fail, an existing destination is kept. Diff writes to stdout.
Use a separate file if redirecting that stream in the shell.

| Exit | Meaning for `lvpm sbom` |
| --- | --- |
| `0` | Success. The output can include warnings about partial coverage. |
| `1` | Processing failure, such as failure to determine the installed environment. |
| `2` | CLI usage error. |
| `3` | Invalid or unsupported input, invalid schema or graph, or output conflict. |
| `4` | Rejected by the incomplete-coverage policy. An existing output is kept. |
| `5` | Package requirement changes with `diff --fail-on-change`. The JSON diff is still written to stdout. |
| `6` | Input or output I/O failure. |

Add `--diagnostics-json` to the export options or after `merge` or `diff`
to emit SBOM diagnostics to stderr as JSON lines.
Each record has `severity`, `code`, `location` and `message` fields.
Coverage diagnostics also report `coverage_scope` (`files`, `runtime`,
or `all` for combined or ambiguous evidence). Locations include the relevant file or option and may include a JSON
pointer. The stable error codes are `sbom.processing-failed`, `sbom.invalid-usage`,
`sbom.invalid-input`, `sbom.invalid-schema`, `sbom.invalid-graph`,
`sbom.output-conflict`, `sbom.incomplete`, `sbom.diff-changed` and
`sbom.io`. Coverage warnings use `sbom.coverage` or `sbom.scan-issue`.
Argument parser diagnostics use the normal CLI format.

SBOM diagnostics retain relative paths and JSON pointers. Locations for
absolute inputs use the file name. Project scans label paths relative to
the project, environment or LabVIEW installation when available. Path
references outside those roots carry an opaque identifier and the file
name. Diagnostic properties stored in the BOM also use portable paths.

An unresolved project scan still needs `--allow-incomplete` to produce
a partial BOM. Export and merge keep valid partial BOMs by default.
Set the coverage policy on either command.

| Option | What blocks the output |
| --- | --- |
| `--fail-on-incomplete` or `--fail-on-incomplete=all` | Any recorded scan issue or incomplete or unknown composition. |
| `--fail-on-incomplete=files` | Missing complete file evidence for the primary product, or incomplete or unknown file evidence. |
| `--fail-on-incomplete=runtime` | Missing complete dependency coverage for the primary product, or incomplete or unknown runtime evidence. |

```powershell
lvpm sbom --artifact template.nipkg --output built.cdx.json --fail-on-incomplete=files --diagnostics-json
lvpm sbom --artifact template.nipkg --output runtime.cdx.json --fail-on-incomplete=runtime --diagnostics-json
lvpm sbom diff --buildspec configured.cdx.json --built built.cdx.json --fail-on-change
```

The file scope covers selected files and the direct NIPKG payload. TAR
members omitted from the inventory and incompletely inspected installer
archives leave file coverage incomplete. Use `--artifact` to produce the
file evidence. The standalone `--nipkg` option reads control metadata
and packed libraries. File coverage is recorded as a CycloneDX
assembly composition, with a `bom-ref` starting with
`lvpm:sbom:file-inventory:`. Dependency coverage has its own composition.
A complete inventory can coexist with unknown runtime coverage.
For a merge plan, a `contains` relationship is file-verified when the parent's
inventory already contains the child artifact and records its matching SHA-256.
A caller declaration or a separate child inventory leaves membership
unknown. A permissive merge records it as declared evidence and reports a
file-coverage warning. The original coverage statement describes the
inspected inventory. The `files` policy rejects such declarations in the primary assembly,
including imported ones. Rebuild earlier merges from the original inventories
to record the matched digest.
Warnings outside the selected scope are still emitted.

Without `--fail-on-change`, a successful diff returns 0 even when there
are changes. Changes to the product version alone do not count as
changes to package requirements.

## `launch`

`lvpm launch [<lvproj>]` starts a LabVIEW on the project's venv and optionally
opens a project file in it. `--ini KEY=VALUE` (repeatable) adds a key to the
venv's `labview.ini` for this launch, e.g. `--ini NIER=True` for a crash
report. `--no-check` skips the check of the venv against the manifest. See
[venvs.md](venvs.md#launching-labview).

## `uninstall` and dependencies

`lvpm uninstall <name>` refuses a package another installed package
requires, naming those packages; `--force` removes it anyway, leaving them
without it. If it has installed dependencies that are not needed by other
packages, lvpm asks whether to remove them too. Shared dependencies (and
dependencies they need) are kept and reported. In a venv, packages declared
in any group of its bound project manifest, and their transitive dependencies,
are also kept. Uninstalling a declared package by name also removes it from
the manifest, so what only it needed can go with it; with `--no-save` it
stays declared, and lvpm warns.

For scripts, pass `--with-deps` to remove unshared dependencies or
`--keep-deps` to keep them without prompting. Without either option, an
uninstall that would prompt fails before removing anything when standard
input is not a terminal. `uninstall --all` still removes every package.

Installation records must contain dependency information; old or invalid
records are rejected rather than ignored. Venvs created before manifest
bindings must be removed and recreated, then their packages reinstalled.
For global installations with old records, uninstall the affected packages
with the previous lvpm version before reinstalling them.
