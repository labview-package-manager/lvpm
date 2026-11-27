# Install lvpm

Windows 10/11 with LabVIEW 2020 or later; venvs need LabVIEW 2024 Q1 or
later. Linux is built and tested in NI's LabVIEW Linux images (see
[Continuous integration](https://github.com/ThatsTheEnd/lvpm/blob/main/CONTRIBUTING.md#continuous-integration)).

## Upgrading lvpm

Run the install script again for the newest release, or download and run the
newer MSI, which upgrades the existing installation in place. For a manual
installation, replace `lvpm.exe` with the one from the new release.

When a command succeeds in a terminal, lvpm says so if a newer stable release
is out, but never installs it
([Update notice](usage.md#update-notice)).

## With the install script

[scripts/install.ps1](https://github.com/ThatsTheEnd/lvpm/blob/main/scripts/install.ps1) fetches the latest release,
verifies the zip's SHA-256 against the digest GitHub records for it (and
against a `SHA256SUMS` file when a release ships one), extracts `lvpm.exe` and the VIs it ships beside it
to `%LOCALAPPDATA%\lvpm\bin`, removes Windows' mark-of-the-web from the
files and adds the folder to your user `PATH`. It runs nothing it downloaded
except `lvpm --version` at the end. Read it first; it is short.

```powershell
cd $env:TEMP
irm https://raw.githubusercontent.com/ThatsTheEnd/lvpm/main/scripts/install.ps1 -OutFile install.ps1
.\install.ps1
```

The `cd` matters: the script is written to the current directory, and an
elevated PowerShell starts in `C:\WINDOWS\system32`, which you cannot write
to. `Access to the path 'C:\WINDOWS\system32\install.ps1' is denied` means
you are in a directory that is not yours, not that anything is wrong with
the download. lvpm installs per-user and needs no elevation.

Options: `-Version v0.4.0` pins a release, `-InstallDir <DIR>` and
`-NoPath` control where it goes, `-Sha256 <hash>` adds a hash you obtained
elsewhere as a further check, `-Stable` skips pre-releases. If PowerShell
refuses to run scripts:
`powershell -ExecutionPolicy Bypass -File .\install.ps1`.

## By hand

Download `lvpm-x86_64-pc-windows-msvc.msi` from the
[Releases](https://github.com/ThatsTheEnd/lvpm/releases) page and run it. It
installs per-user to `%LOCALAPPDATA%\lvpm\bin`, adds that folder to your user
`PATH`, and upgrades an earlier install in place.

Or download `lvpm-x86_64-pc-windows-msvc.zip`, right-click it, tick
**Unblock**, extract, and put the folder on your `PATH`. Releases up to
v0.27.0 name it `lvpm-<version>-windows-x64.zip` and ship no MSI.

## On Linux

```sh
curl -fsSLO https://raw.githubusercontent.com/labview-package-manager/lvpm/main/scripts/install.sh
sh install.sh
```

`install.sh` is `install.ps1`'s twin: it picks the newest release (or
`--version <tag>`, `--stable`), checks the tarball against GitHub's digest,
the release's checksum files and an optional `--sha256`, and installs `lvpm`
with its VIs to `~/.local/bin`, or `/usr/local/bin` as root
(`--install-dir`). Unless you pass `--no-path`, it adds that directory to
`PATH` in `~/.profile`. `--archive <file> --sha256 <hash>` installs a local
tarball; `sh install.sh --help` lists every option. It needs curl, tar,
gzip, sed, awk and sha256sum, and no root.

By hand: download `lvpm-x86_64-unknown-linux-gnu.tar.gz` and its `.sha256` from the
newest [release](https://github.com/labview-package-manager/lvpm/releases), then:

```sh
sha256sum -c lvpm-x86_64-unknown-linux-gnu.tar.gz.sha256
tar -xzf lvpm-x86_64-unknown-linux-gnu.tar.gz
./lvpm-x86_64-unknown-linux-gnu/lvpm --version
```

Releases up to v0.27.0 name it `lvpm-<version>-linux-x64.tar.gz`, with the
checksum in `SHA256SUMS` and `lvpm` at the archive's top level.

Keep `lvpm`, `relink-package.vi` and `build-app.vi` together: relink and
`build --app` look for the VIs beside `lvpm`. Move all three to a folder on
your `PATH`, for example `~/.local/bin`, or symlink `lvpm` there.

## From source

```console
cargo build --release      # target\release\lvpm.exe
cargo test
```

Toolchain setup is in [CONTRIBUTING.md](https://github.com/ThatsTheEnd/lvpm/blob/main/CONTRIBUTING.md).

## Test a local release installation

From a Windows checkout with the build tools installed:

```powershell
.\scripts\install.ps1 -BuildLocal
```

This runs `cargo build --release --locked`, packages the build into a zip and
installs it with the same verification and extraction as a release, into
`%LOCALAPPDATA%\lvpm\bin` and on your user `PATH`. It publishes nothing. For
a test that leaves your installed lvpm and your `PATH` alone:

```powershell
.\scripts\install.ps1 -BuildLocal -InstallDir "$PWD\sandbox\local-installer\bin" -NoPath
.\sandbox\local-installer\bin\lvpm.exe --version
```

`-Archive <FILE> -Sha256 <HASH>` installs a release zip you already have
instead. Neither can be combined with `-Version` or `-Stable`.
