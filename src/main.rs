//! lvpm — an open-source package manager for LabVIEW packages.
//!
//! Resolves `.vip` packages by name from the public VIPM indexes, downloads
//! them, verifies the MD5 and unpacks them — either into a scratch tree or
//! into a real LabVIEW installation. No VIPM. Copying the files is followed by
//! the packages' install hooks, run over VI Server — in a venv's own LabVIEW
//! when a venv is in effect, since the paths a hook is handed are the venv's
//! own. `--no-hooks` turns that off; headless (`LV_RTE_HEADLESS`) they are
//! off unless `--hooks` asks for them.
//!
//! The relink pass (see [`relink`]) is opt-in, behind `--relink`: measured
//! across 10 packages saved between LabVIEW 8.6 and 2020, unrelinked VIs load
//! and run from a venv untouched, because LabVIEW resolves what the linker
//! tables miss by searching vi.lib. Relinking only warms that cache.

// lvpm knows where LabVIEW keeps its files on Windows and on Linux. Every
// other platform would build and then look in the wrong places, so it does not
// build: macOS needs a branch of its own (docs/roadmap.md).
#[cfg(not(any(windows, target_os = "linux")))]
compile_error!("lvpm supports Windows and Linux; macOS is on the roadmap (docs/roadmap.md)");

// First, so its print-and-log macros are in scope in every module below.
#[macro_use]
mod logging;
mod lvproj;
mod nipkg;
#[cfg(windows)]
mod nipkg_cab;
#[cfg(windows)]
mod nipkg_msi;
mod package_graph;
mod ppl;
mod distribution;
mod ppl_pe;
mod vi_graph;
mod cache;
mod config;
mod dependency;
mod fetch;
mod index;
mod install;
mod launch;
mod linkfix;
mod palette;
mod project;
mod refresh;
mod relink;
mod resolve;
mod sbom;
mod sbom_artifact_document;
mod sbom_artifacts;
mod sbom_ci;
mod sbom_diff;
mod sbom_graph_validation;
mod sbom_installers;
mod sbom_merge;
mod sbom_metadata;
mod sbom_paths;
mod sbom_pe;
mod sbom_schema;
mod shadow;
mod shortcut;
mod spec;
mod venv_icon;
mod target;
mod update_notice;
mod venv;
mod version;
mod vipm_interop;
mod viserver;
mod watcher;

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use target::Roots;
use version::Version;

#[derive(Parser)]
#[command(
    name = "lvpm",
    version,
    about = "Open-source package manager for LabVIEW (.vip) packages — proof of concept"
)]
struct Cli {
    /// Install into this LabVIEW version, e.g. 2026. See `lvpm targets`.
    #[arg(long = "labview-version", global = true, value_name = "YYYY")]
    labview_version: Option<String>,

    /// Install into a scratch directory instead of a LabVIEW installation.
    #[arg(long, global = true, value_name = "DIR")]
    prefix: Option<PathBuf>,

    /// Use the LabVIEW installation itself, even from inside a project that
    /// has a venv.
    #[arg(long, global = true, conflicts_with = "project")]
    global: bool,

    /// The project whose venv to use, instead of looking upwards from the
    /// current directory.
    #[arg(long, global = true, value_name = "DIR", conflicts_with = "prefix")]
    project: Option<PathBuf>,

    /// Revalidate remote feeds and re-read local packages instead of trusting cached stats.
    #[arg(long, global = true)]
    refresh: bool,

    /// Detail written to the log file (the venv's own, or `lvpm.log` outside a
    /// venv, in lvpm's log folder): off, error, warn, info, debug or trace.
    /// Default: `LVPM_LOG`, else debug. The console output does not change.
    #[arg(long = "log-level", global = true, value_name = "LEVEL")]
    log_level: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

/// Shared by `install` and `relink`, so the two cannot drift apart.
#[derive(clap::Args, Clone)]
struct RelinkArgs {
    /// Give up on a single folder after this many seconds.
    #[arg(long = "relink-timeout", value_name = "SECS", default_value_t = 900)]
    timeout: u64,
    /// Echo this indicator on the relink VI's front panel while it works.
    #[arg(long = "relink-progress", value_name = "NAME")]
    progress: Option<String>,
    /// Leave the LabVIEW started for a venv relink running afterwards.
    #[arg(long)]
    keep_open: bool,
}

/// Which named dependency groups an install or SBOM export takes. `main` names
/// `[dependencies]`, the only group installed by default.
#[derive(clap::Args, Clone)]
struct GroupArgs {
    /// Also include these dependency groups (comma-separated).
    #[arg(long, value_name = "GROUP", value_delimiter = ',', conflicts_with = "only")]
    with: Vec<String>,
    /// Leave these groups out, `main` included (comma-separated).
    #[arg(long, value_name = "GROUP", value_delimiter = ',', conflicts_with = "only")]
    without: Vec<String>,
    /// Include only these groups (comma-separated); add `main` to keep [dependencies].
    #[arg(long, value_name = "GROUP", value_delimiter = ',')]
    only: Vec<String>,
}

#[derive(clap::Args)]
struct SbomProjectArgs {
    /// Read control metadata and packed libraries from a built NI package (.nipkg).
    #[arg(long, value_name = "FILE", conflicts_with_all = ["lvproj", "with", "without", "only"])]
    nipkg: Option<PathBuf>,
    /// Read direct packed-library references from a built .lvlibp.
    #[arg(long, value_name = "FILE", conflicts_with_all = ["nipkg", "lvproj", "with", "without", "only"])]
    lvlibp: Option<PathBuf>,
    /// Scan a saved LabVIEW project or a selected build specification.
    #[arg(long, value_name = "FILE", conflicts_with_all = ["with", "without", "only"])]
    lvproj: Option<PathBuf>,
    /// Select the exact target of --lvproj-build-spec.
    #[arg(long, value_name = "NAME", requires_all = ["lvproj", "lvproj_build_spec"])]
    lvproj_target: Option<String>,
    /// Select the exact build specification name in the target.
    #[arg(long, value_name = "NAME", requires_all = ["lvproj", "lvproj_target"])]
    lvproj_build_spec: Option<String>,
    /// Export a partial VI dependency graph when files or links cannot be read.
    #[arg(long, requires = "lvproj")]
    allow_incomplete: bool,
    /// Override the installed VIPM ownership database used in VI dependency scans.
    #[arg(long, value_name = "DIR", requires = "lvproj")]
    vipm_database: Option<PathBuf>,
    /// Select a delivered output file or directory. Repeat to select multiple outputs.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["nipkg", "lvlibp", "with", "without", "only"])]
    artifact: Vec<PathBuf>,
    /// Set the common root used for artifact paths in the SBOM.
    #[arg(long, value_name = "DIR", requires = "artifact")]
    artifact_root: Option<PathBuf>,
    /// Select the product's file by its path relative to the artifact root.
    #[arg(long, value_name = "PATH", requires = "artifact")]
    primary_artifact: Option<String>,
    /// Override the product name for an artifact export.
    #[arg(long, value_name = "NAME", requires = "artifact")]
    product_name: Option<String>,
    /// Set the release version for an artifact export.
    #[arg(long, value_name = "VERSION", requires = "artifact")]
    product_version: Option<String>,
}

/// Whether `install <pkg>` writes the package into the venv's manifest.
#[derive(clap::Args, Clone)]
struct SaveArgs {
    /// Install without adding the package to the project's lvpm.toml.
    #[arg(long, requires = "package")]
    no_save: bool,
    /// The dependency group to save the package in (`main` is
    /// [dependencies]). Default: the group already declaring it, else main.
    #[arg(long, value_name = "GROUP", requires = "package", conflicts_with = "no_save")]
    group: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List detected LabVIEW installations.
    Targets,
    /// Start the selected LabVIEW and wait until its VI Server answers.
    /// Already running: reports the port and does nothing.
    Start {
        /// Seconds to wait for the VI Server to come up.
        #[arg(long, default_value_t = 120)]
        wait: u64,
    },
    /// Install a package by name, or name@version — or every dependency a
    /// project manifest lists, with `--manifest`.
    ///
    /// On a venv, a package installed by name is also written into the
    /// project's lvpm.toml (the version asked for, else the one installed);
    /// `--no-save` leaves the manifest alone.
    Install {
        /// Package to install. Omit when using --manifest.
        #[arg(conflicts_with_all = ["with", "without", "only"])]
        package: Option<String>,
        /// Install every package listed in an `lvpm.toml`'s `[dependencies]`.
        ///
        /// All packages resolve into one plan. Linkfix (or --relink) runs after
        /// every package's files are on disk, before PostInstall hooks.
        #[arg(long, value_name = "FILE", conflicts_with = "package")]
        manifest: Option<PathBuf>,
        /// Show what would be written, then stop.
        #[arg(long)]
        dry_run: bool,
        /// Do not install dependencies.
        #[arg(long)]
        no_deps: bool,
        /// Leave package files as copied: skip link repair and source-only preparation.
        #[arg(long)]
        no_linkfix: bool,
        /// Load and save installed VIs in LabVIEW instead of running linkfix.
        #[arg(long = "relink")]
        relink_pass: bool,
        /// Accepted and ignored: LabVIEW relink is opt-in. Does not skip linkfix.
        #[arg(long, hide = true)]
        no_relink: bool,
        /// Run no install hooks at all, not even those the manifest enables.
        /// Owed PostInstall hooks stay owed for a later install.
        #[arg(long, conflicts_with = "run_hooks")]
        no_hooks: bool,
        /// Run every package's install hooks, whatever the manifest says. By
        /// default a venv runs only those of dependencies with
        /// `hooks = "run"`, a LabVIEW installation all but `hooks = "skip"`,
        /// and a headless machine (LV_RTE_HEADLESS) only `hooks = "run"`.
        #[arg(long = "hooks")]
        run_hooks: bool,
        /// Replace an installed package with an older version. Without this,
        /// resolving to something older than what is installed is refused.
        #[arg(long)]
        allow_downgrade: bool,
        #[command(flatten)]
        groups: GroupArgs,
        #[command(flatten)]
        save: SaveArgs,
        #[command(flatten)]
        relink: RelinkArgs,
    },
    /// Relink an already-installed package, without reinstalling it.
    ///
    /// The install manifest records what went where, so this is the same pass
    /// `install --relink` runs. Separate from the default Rust linkfix pass.
    Relink {
        /// Package to relink. Omit when using --all.
        package: Option<String>,
        /// Relink every package installed in this target, in one pass.
        #[arg(long, conflicts_with = "package")]
        all: bool,
        #[command(flatten)]
        relink: RelinkArgs,
    },
    /// Re-run a package's PostInstall hook, without reinstalling it.
    ///
    /// For retrying a hook that failed during install, or running one the
    /// hook policy held back. A package named here runs whatever the policy
    /// says; check what its hook does first, since many edit the LabVIEW
    /// installation even when run from a venv.
    RunHooks {
        /// Package whose PostInstall hook to run. Omit when using --all.
        package: Option<String>,
        /// Run the PostInstall hook of every installed package that has one
        /// and that the hook policy allows (in a venv: `hooks = "run"`).
        #[arg(long, conflicts_with = "package")]
        all: bool,
        /// Seconds to wait for one hook to finish.
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
    /// Rebuild the palettes and the File/Tools/Help menus from disk.
    ///
    /// `install` does this on its own, after the post-install hooks. Run it by
    /// hand when LabVIEW was not running then, or after copying palette files
    /// in some other way — it is what makes a package's palette and Tools
    /// entries appear without restarting LabVIEW.
    Refresh {
        /// Seconds to wait for the refresh to finish.
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
    /// Remove a previously installed package.
    ///
    /// On a venv, a package uninstalled by name is also taken out of the
    /// project's lvpm.toml, from every group declaring it; `--no-save` leaves
    /// the manifest alone. `--all` never edits it.
    Uninstall {
        /// Package to remove. Omit when using --all.
        package: Option<String>,
        /// Uninstall without removing the package from the project's lvpm.toml.
        #[arg(long, conflicts_with = "all")]
        no_save: bool,
        /// Remove every package installed in this target.
        #[arg(long, conflicts_with = "package")]
        all: bool,
        /// Remove unshared dependencies too, without prompting.
        #[arg(long, conflicts_with_all = ["all", "keep_deps"])]
        with_deps: bool,
        /// Keep dependencies, without prompting.
        #[arg(long, conflicts_with_all = ["all", "with_deps"])]
        keep_deps: bool,
        /// Remove the package even though other installed packages require
        /// it, leaving them without it. Without this, that is refused.
        #[arg(long, conflicts_with = "all")]
        force: bool,
    },
    /// Invoke parameterless Application-class methods by raw id, and report
    /// what LabVIEW says. Re-establishes method ids on a new LabVIEW: 1036
    /// means "no such method here", anything else means the id resolved.
    ///
    /// **Invokes whatever it hits** — sweep with care.
    AppProbe {
        /// Method id, decimal or 0x-prefixed hex. Repeatable.
        #[arg(value_name = "ID", required = true)]
        ids: Vec<String>,
    },
    /// Open a VI reference over VI Server and release it. Proves the transport
    /// against a running LabVIEW without changing anything on disk.
    ViProbe {
        /// VIs to open references to. Several are opened one after another
        /// on one connection and held until the last one is in, so a shared
        /// hierarchy loads once; a VI that fails to open is reported and the
        /// rest still run.
        #[arg(required = true)]
        vi: Vec<PathBuf>,
        /// Seconds to wait for LabVIEW to answer a single open. A subsystem
        /// top-level VI pulling a large class hierarchy can need minutes.
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },
    /// Load a VI over VI Server and save it back — the relink primitive.
    ViSave {
        /// VI to load and re-save in place.
        vi: PathBuf,
    },
    /// Set controls, run a VI, and read values back — the hook-VI primitive.
    ViRun {
        /// VI to run.
        vi: PathBuf,
        /// Set a control first: name=type:value with type bool|i32|dbl|str,
        /// e.g. --set "Iterations=i32:10". Repeatable.
        #[arg(long = "set", value_name = "NAME=TYPE:VALUE")]
        sets: Vec<String>,
        /// Read one control or indicator afterwards. Repeatable.
        #[arg(long = "get", value_name = "NAME")]
        gets: Vec<String>,
        /// Read every control and indicator afterwards.
        #[arg(long)]
        get_all: bool,
        /// Seconds to wait for the VI to finish.
        #[arg(long, default_value_t = 120)]
        timeout: u64,
        /// Start the VI without waiting and poll this indicator while it runs.
        #[arg(long, value_name = "NAME")]
        watch: Option<String>,
        /// How often to poll --watch, in milliseconds.
        #[arg(long, default_value_t = 250)]
        poll_ms: u64,
        /// Boolean indicator that goes true when the VI is finished.
        #[arg(long, value_name = "NAME", default_value = "Done")]
        done: String,
    },
    /// Manage the project's venv: its own package tree under `.lv-venv/` by default,
    /// mounted into LabVIEW as an LVAddons location by `lvpm launch`.
    #[command(subcommand)]
    Venv(VenvCmd),
    /// Read or change user-level lvpm settings.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Import package archives into the shared cache.
    #[command(subcommand)]
    Cache(CacheCmd),
    /// Start LabVIEW on the project's venv, optionally opening a project file.
    ///
    /// LabVIEW reads the venv's contents when it starts, so a LabVIEW that
    /// was already open on it needs a fresh launch to see newly installed
    /// packages.
    Launch {
        /// A `.lvproj` (or any LabVIEW file) to open in the started LabVIEW.
        lvproj: Option<PathBuf>,
        /// Extra `KEY=VALUE` written into the venv's labview.ini for this
        /// launch, e.g. `--ini NIER=True --ini DPrintfLogging=True` for a
        /// crash report and verbose log. Repeatable.
        #[arg(long = "ini", value_name = "KEY=VALUE")]
        ini_keys: Vec<String>,
        /// Start even when the venv does not match the manifest's
        /// `[dependencies]` (missing, wrong version, files gone, a relink or
        /// PostInstall hook still owed).
        #[arg(long)]
        no_check: bool,
        /// Wait for Enter before exiting on an error, so a console opened by
        /// a shortcut stays up long enough to read it.
        #[arg(long, hide = true)]
        pause_on_error: bool,
    },
    /// Keep a tag in the window titles of one LabVIEW until it exits.
    /// Started hidden by `launch`; see the `venv.title` setting.
    #[command(hide = true)]
    TitleWatch {
        /// Process id of the LabVIEW to watch.
        #[arg(long)]
        pid: u32,
        /// The text to put in each title, e.g. `[my-project]`.
        #[arg(long)]
        tag: String,
        /// `end`, `start` or `off`.
        #[arg(long, default_value = "end")]
        position: String,
        /// Windows app id to give each window, grouping them in the taskbar.
        #[arg(long, requires_all = ["relaunch", "relaunch_icon", "relaunch_name"])]
        app_id: Option<String>,
        /// Command line a taskbar pin of the group runs.
        #[arg(long)]
        relaunch: Option<String>,
        /// Icon of that pin; an `.ico` also goes on each window.
        #[arg(long)]
        relaunch_icon: Option<String>,
        /// Name of that pin.
        #[arg(long)]
        relaunch_name: Option<String>,
    },
    /// Rewrite installed VI links and set libraries/classes source-only the
    /// way LabVIEW saves them. Defaults to the venv in effect; `--tree` takes
    /// any LabVIEW-shaped folder or venv store.
    Linkfix {
        /// A folder to fix instead of the venv: a venv `.project` store, or a
        /// LabVIEW-shaped tree with `vi.lib`, `user.lib`, ... at its top.
        #[arg(long, value_name = "DIR")]
        tree: Option<PathBuf>,
        /// Report what would change, write nothing.
        #[arg(long)]
        dry_run: bool,
        /// Name every changed and every skipped file.
        #[arg(long, short)]
        verbose: bool,
        /// Leave compiled-code settings unchanged. By default VIs are made
        /// source-only and library/class XML is marked source-only too.
        /// Same as leaving `source-only` out of `--fix`.
        #[arg(long)]
        keep_compiled_code: bool,
        /// Apply only these methods, comma-separated (default: all). Running
        /// them one at a time shows which one a later save dialog is about.
        #[arg(long, value_enum, value_delimiter = ',', value_name = "METHOD")]
        fix: Vec<linkfix::Method>,
        /// Write, per changed or skipped file, what each method changed (or
        /// why the file was skipped) as JSON.
        #[arg(long, value_name = "FILE")]
        report: Option<PathBuf>,
        /// The bitness of the LabVIEW a `--tree` is for (32 or 64), which the
        /// `clfn-wildcard` method resolves Call Library wildcards for. A venv
        /// knows its own.
        #[arg(long, value_name = "32|64")]
        bitness: Option<u8>,
    },
    /// Write a VIPM file into the project's lvpm.toml, once: a vipm.toml, or a
    /// package configuration (.vipc), whose top-level packages are written as
    /// exact versions and whose carried packages go into the package cache.
    /// Installs nothing.
    Import {
        /// The vipm.toml or .vipc file.
        file: PathBuf,
        /// For a .vipc: pin every package in the file, not only those nothing
        /// else in it requires.
        #[arg(long)]
        all: bool,
        /// The dependency group to write into (`main` is [dependencies]).
        /// Default: the group already declaring a package, else main.
        #[arg(long, value_name = "GROUP")]
        group: Option<String>,
        /// Show what would change; write neither the manifest nor the cache.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show what lvpm has installed into the selected target.
    List,
    /// Export or merge CycloneDX 1.5 JSON SBOMs.
    #[command(subcommand_negates_reqs = true, args_conflicts_with_subcommands = true)]
    Sbom(Box<SbomArgs>),
    /// Search the indexes.
    Search { query: String },
}

#[derive(clap::Args)]
struct SbomArgs {
    /// Write the SBOM to this file.
    #[arg(long, value_name = "FILE", required = true)]
    output: Option<PathBuf>,
    /// Add the components in a CycloneDX 1.5 JSON file as direct dependencies.
    #[arg(long, value_name = "FILE")]
    include: Option<PathBuf>,
    /// Require adequate SBOM coverage in the selected scope.
    #[arg(long, value_name = "SCOPE", num_args = 0..=1, require_equals = true, default_missing_value = "all")]
    fail_on_incomplete: Option<sbom_ci::CoverageScope>,
    /// Write one JSON diagnostic per line to stderr.
    #[arg(long)]
    diagnostics_json: bool,
    #[command(flatten)]
    groups: GroupArgs,
    #[command(flatten)]
    source: SbomProjectArgs,
    #[command(subcommand)]
    action: Option<SbomCmd>,
}

#[derive(Subcommand)]
enum SbomCmd {
    /// Compare the declared requirements of a build spec and a built NI package.
    Diff {
        /// CycloneDX JSON exported from the NIPKG build specification.
        #[arg(long, value_name = "FILE")]
        buildspec: PathBuf,
        /// CycloneDX JSON exported from the built .nipkg.
        #[arg(long, value_name = "FILE")]
        built: PathBuf,
        /// Return exit code 5 if package requirements were added, removed or changed.
        #[arg(long)]
        fail_on_change: bool,
        /// Write one JSON diagnostic per line to stderr.
        #[arg(long)]
        diagnostics_json: bool,
    },
    /// Merge both graphs, attaching a child product or enriching an existing PPL.
    Merge {
        /// CycloneDX JSON whose product remains the root.
        #[arg(long, value_name = "FILE")]
        primary: PathBuf,
        /// CycloneDX JSON files whose products are merged into the primary graph.
        #[arg(long, value_name = "FILE", required_unless_present = "relationships", conflicts_with = "relationships", num_args = 1..)]
        child: Vec<PathBuf>,
        /// Read the child SBOM aliases and explicit relationships from a JSON file.
        #[arg(long, value_name = "FILE")]
        relationships: Option<PathBuf>,
        /// Write the merged CycloneDX JSON to this file.
        #[arg(long, value_name = "FILE")]
        output: PathBuf,
        /// Require adequate SBOM coverage in the selected scope.
        #[arg(long, value_name = "SCOPE", num_args = 0..=1, require_equals = true, default_missing_value = "all")]
        fail_on_incomplete: Option<sbom_ci::CoverageScope>,
        /// Write one JSON diagnostic per line to stderr.
        #[arg(long)]
        diagnostics_json: bool,
    },
}

#[derive(Subcommand)]
enum VenvCmd {
    /// Create `.lv-venv/` for the project here by default and bind it to a LabVIEW:
    /// `--labview-version`, else the manifest's `labview`.
    Create {
        /// Use this manifest (in lvpm.toml's format, under any name) instead
        /// of looking for an `lvpm.toml`; the venv goes beside it and no
        /// skeleton is written. A `vipm.toml` comes in with `lvpm import`.
        #[arg(long, value_name = "FILE")]
        manifest: Option<PathBuf>,
    },
    /// Which venv commands run here would use, and what it is bound to.
    Status,
    /// Delete the venv and everything installed in it.
    Remove {
        /// Do not ask first.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Write a Windows shortcut that runs `lvpm launch` on this venv, with
    /// LabVIEW's icon. Goes in the project folder unless `--desktop` or
    /// `--out` says otherwise.
    Shortcut {
        /// A `.lvproj` (or any LabVIEW file) the shortcut opens.
        lvproj: Option<PathBuf>,
        /// Put the shortcut on the desktop.
        #[arg(long, conflicts_with = "out")]
        desktop: bool,
        /// Where to write the shortcut: a `.lnk` path, or a folder to put it in.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum CacheCmd {
    /// Import a .vip/.ogp file or the packages directly inside a directory.
    Add { path: PathBuf },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Set a user-level setting. `sources.local` accepts directories; omit them to clear the list.
    Set { key: String, values: Vec<String> },
    /// Show the effective value of a setting.
    Get { key: String },
}

impl Cmd {
    fn sbom_json(&self) -> Option<bool> {
        let Self::Sbom(args) = self else {
            return None;
        };
        Some(match &args.action {
            Some(SbomCmd::Diff { diagnostics_json, .. } | SbomCmd::Merge { diagnostics_json, .. }) => {
                *diagnostics_json
            }
            None => args.diagnostics_json,
        })
    }
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let sbom_json = cli.cmd.sbom_json();
    let level = match logging::level(cli.log_level.as_deref()) {
        Ok(level) => level,
        Err(error) => {
            let code = match sbom_json {
                Some(json) => {
                    let location = if cli.log_level.is_some() { "--log-level" } else { "LVPM_LOG" };
                    sbom_ci::emit_error(
                        &sbom_ci::failure(sbom_ci::Failure::Usage, location, error), json,
                    )
                }
                None => {
                    eprintln!("Error: {error}");
                    1
                }
            };
            return std::process::ExitCode::from(code);
        }
    };
    // A configured log or update-cache path can alias a BOM or an artifact.
    let update_notice = if sbom_json.is_none() {
        logging::init(level);
        update_notice::start()
    } else {
        None
    };
    let result = run(&cli);
    if let (Some(notice), Ok(())) = (update_notice, &result) {
        notice.finish();
    }
    if sbom_json.is_none() {
        logging::finish(&result);
    }
    if let (Cmd::Launch { pause_on_error: true, .. }, Err(e)) = (&cli.cmd, &result) {
        eprintln!("error: {e:#}\n\npress Enter to close");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            let code = match sbom_json {
                Some(json) => sbom_ci::emit_error(&error, json),
                None => {
                    eprintln!("Error: {error:?}");
                    1
                }
            };
            std::process::ExitCode::from(code)
        }
    }
}

fn run(cli: &Cli) -> Result<()> {
    match &cli.cmd {
        Cmd::Targets => cmd_targets(),
        Cmd::Start { wait } => cmd_start(&cli, *wait),
        Cmd::Install {
            package,
            manifest,
            dry_run,
            no_deps,
            no_linkfix,
            relink_pass,
            no_relink: _,
            no_hooks,
            run_hooks,
            allow_downgrade,
            groups,
            save,
            relink,
        } => {
            let started = std::time::Instant::now();
            let result = cmd_install(
                &cli,
                package.as_deref(),
                manifest.as_deref(),
                *dry_run,
                *no_deps,
                *no_linkfix,
                *relink_pass,
                *no_hooks,
                *run_hooks,
                *allow_downgrade,
                groups,
                save,
                relink,
            );
            let took = took(started.elapsed());
            match &result {
                Ok(()) if *dry_run => log!("\ndry run took {took}"),
                Ok(()) => log!("\ninstall took {took}"),
                Err(_) => log!("\ninstall stopped after {took}"),
            }
            result
        }
        Cmd::Relink { package, all, relink } => cmd_relink(&cli, package.as_deref(), *all, relink),
        Cmd::RunHooks { package, all, timeout } => {
            cmd_run_hooks(&cli, package.as_deref(), *all, *timeout)
        }
        Cmd::Refresh { timeout } => cmd_refresh(&cli, *timeout),
        Cmd::Uninstall { package, no_save, all, with_deps, keep_deps, force } =>
            cmd_uninstall(&cli, package.as_deref(), *all, *with_deps, *keep_deps, *no_save, *force),
        Cmd::AppProbe { ids } => cmd_app_probe(&cli, ids),
        Cmd::ViProbe { vi, timeout } => cmd_vi_probe(&cli, vi, *timeout),
        Cmd::ViSave { vi } => cmd_vi_save(&cli, vi),
        Cmd::ViRun { vi, sets, gets, get_all, timeout, watch, poll_ms, done } => {
            cmd_vi_run(&cli, vi, sets, gets, *get_all, *timeout, watch.as_deref(), *poll_ms, done)
        }
        Cmd::Venv(sub) => cmd_venv(&cli, sub),
        Cmd::Cache(CacheCmd::Add { path }) => {
            let config = config::load()?;
            let imports = index::cache_add(&config.cache, path, cli.refresh)?;
            for import in &imports {
                log!("{} {} sha256:{}", if import.reused { "cached" } else { "imported" }, import.path.display(), import.sha256);
            }
            log!("{} package(s) processed", imports.len());
            Ok(())
        }
        Cmd::Config(ConfigCmd::Set { key, values }) => {
            config::set(key, values)?;
            log!("set {key}");
            Ok(())
        }
        Cmd::Config(ConfigCmd::Get { key }) => {
            for value in config::get(key)? {
                log!("{value}");
            }
            Ok(())
        }
        Cmd::Launch { lvproj, ini_keys, no_check, .. } => cmd_launch(&cli, lvproj.as_deref(), ini_keys, *no_check),
        Cmd::TitleWatch { pid, tag, position, app_id, relaunch, relaunch_icon, relaunch_name } => {
            let pos: config::TitlePosition = position.parse()?;
            #[cfg(windows)]
            {
                let taskbar = match (app_id, relaunch, relaunch_icon, relaunch_name) {
                    (Some(id), Some(relaunch), Some(icon), Some(name)) => Some(watcher::Taskbar {
                        id: id.clone(),
                        relaunch: relaunch.clone(),
                        icon: icon.clone(),
                        name: name.clone(),
                    }),
                    _ => None,
                };
                watcher::watch(*pid, tag, pos, taskbar)
            }
            #[cfg(not(windows))]
            {
                let _ = (pid, tag, pos, app_id, relaunch, relaunch_icon, relaunch_name);
                bail!("title-watch is Windows-only");
            }
        }
        Cmd::Linkfix { tree, dry_run, verbose, keep_compiled_code, fix, report, bitness } => {
            let mut methods: Vec<linkfix::Method> = if fix.is_empty() { linkfix::Method::ALL.to_vec() } else { fix.clone() };
            if *keep_compiled_code {
                methods.retain(|m| *m != linkfix::Method::SourceOnly);
            }
            ensure!(bitness.is_none_or(|b| b == 32 || b == 64), "--bitness is 32 or 64");
            cmd_linkfix(&cli, tree.as_deref(), *dry_run, *verbose, &methods, report.as_deref(), *bitness)
        }
        Cmd::Import { file, all, group, dry_run } => cmd_import(&cli, file, *all, group.as_deref(), *dry_run),
        Cmd::List => cmd_list(&cli),
        Cmd::Sbom(args) => run_sbom(cli, args),
        Cmd::Search { query } => cmd_search(&cli, query),
    }
}

fn run_sbom(cli: &Cli, args: &SbomArgs) -> Result<()> {
    match &args.action {
        Some(SbomCmd::Diff {
            buildspec,
            built,
            fail_on_change,
            ..
        }) => {
            let diff = sbom_diff::compare(buildspec, built)?;
            let mut stdout = std::io::stdout().lock();
            serde_json::to_writer_pretty(&mut stdout, &diff)?;
            writeln!(stdout)?;
            stdout.flush()?;
            if *fail_on_change && sbom_diff::has_changes(&diff) {
                return Err(sbom_ci::failure(
                    sbom_ci::Failure::Changed,
                    sbom_paths::diagnostic_path(built),
                    "NI package requirements changed",
                ));
            }
            Ok(())
        }
        Some(SbomCmd::Merge {
            primary,
            child,
            relationships,
            output,
            fail_on_incomplete,
            diagnostics_json,
        }) => {
            let guard = sbom_paths::OutputGuard::new(output)?;
            guard.protect(primary)?;
            let mut document = if let Some(plan) = relationships {
                sbom_merge::merge_with_relationships(primary, plan, &guard)?
            } else {
                for path in child {
                    guard.protect(path)?;
                }
                sbom_merge::merge(primary, child)?
            };
            sbom_ci::write(
                &guard,
                output,
                &mut document,
                *fail_on_incomplete,
                *diagnostics_json,
                None,
            )?;
            log!(
                "wrote merged CycloneDX SBOM to {}",
                sbom_paths::diagnostic_path(output)
            );
            Ok(())
        }
        None => export_sbom(cli, args),
    }
}

fn export_sbom(cli: &Cli, args: &SbomArgs) -> Result<()> {
    let output = args.output.as_deref().context("sbom export requires --output")?;
    let include = args.include.as_deref();
    let source = &args.source;
    let guard = sbom_paths::OutputGuard::new(output)?;
    for path in [
        include,
        source.lvlibp.as_deref(),
        source.nipkg.as_deref(),
        source.lvproj.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        guard.protect(path)?;
    }
    if source.lvproj.is_some() && source.lvproj_build_spec.is_none() && !source.artifact.is_empty() {
        return Err(sbom_ci::failure(
            sbom_ci::Failure::Usage,
            "arguments",
            "--artifact with --lvproj requires a build specification",
        ));
    }

    let post_build = if let Some(first) = source.artifact.first() {
        let inventory = sbom_artifacts::scan(&source.artifact, source.artifact_root.as_deref(), &guard)
            .map_err(|error| sbom_ci::input_error(error, first))?;
        let inferred_name = if source.artifact.len() == 1 {
            first.file_name().and_then(|name| name.to_str())
        } else {
            None
        };
        Some(sbom_artifact_document::PostBuild {
            inventory,
            primary_key: source.primary_artifact.as_deref(),
            product_name: source.product_name.as_deref(),
            product_version: source.product_version.as_deref(),
            inferred_name,
        })
    } else {
        None
    };
    let environment = vi_graph::Environment {
        project: cli.project.as_deref(),
        global: cli.global || cli.prefix.is_some(),
        labview_version: cli.labview_version.as_deref(),
    };
    let mut project_inventory = None;
    let mut document = match (
        &source.lvlibp,
        &source.nipkg,
        &source.lvproj,
        &source.lvproj_target,
        &source.lvproj_build_spec,
    ) {
        (Some(path), _, _, _, _) => sbom::packed_library_document(path, include)
            .map_err(|error| sbom_ci::input_error(error, path))?,
        (_, Some(path), _, _, _) => sbom::nipkg_archive_document(path, include)
            .map_err(|error| sbom_ci::input_error(error, path))?,
        (_, _, Some(path), Some(target), Some(spec)) => {
            let options = sbom::ProjectBuildOptions {
                include,
                environment,
                allow_incomplete: source.allow_incomplete,
                vipm_database: source.vipm_database.as_deref(),
                output: Some(&guard),
            };
            sbom::build_document(path, target, spec, options, post_build.as_ref())
                .map_err(|error| sbom_ci::input_error(error, path))?
        }
        (_, _, Some(path), None, None) => {
            let (document, inventory) = sbom::project_document(
                path,
                environment,
                include,
                source.allow_incomplete,
                source.vipm_database.as_deref(),
                &guard,
            )
            .map_err(|error| sbom_ci::input_error(error, path))?;
            project_inventory = inventory;
            document
        }
        _ if let Some(post_build) = &post_build => sbom::artifact_document(post_build, include)
            .map_err(|error| sbom_ci::input_error(error, &source.artifact[0]))?,
        _ => installed_sbom(cli, include, &args.groups, &guard)?,
    };
    sbom_ci::write(
        &guard,
        output,
        &mut document,
        args.fail_on_incomplete,
        args.diagnostics_json,
        project_inventory
            .as_ref()
            .or_else(|| post_build.as_ref().map(|post_build| &post_build.inventory)),
    )?;
    log!("wrote CycloneDX SBOM to {}", sbom_paths::diagnostic_path(output));
    Ok(())
}

/// A duration for the user: `48.2s`, or `3m 12s` past a minute.
fn took(d: std::time::Duration) -> String {
    let s = d.as_secs_f64();
    match s < 60.0 {
        true => format!("{s:.1}s"),
        false => format!("{}m {:02}s", d.as_secs() / 60, d.as_secs() % 60),
    }
}

/// Resolve `--labview` / `--prefix` into a set of roots.
fn roots_for(cli: &Cli) -> Result<Roots> {
    match (&cli.labview_version, &cli.prefix) {
        (Some(_), Some(_)) => bail!("--labview-version and --prefix are mutually exclusive"),
        (Some(want), None) => {
            let targets = target::detect()?;
            if targets.is_empty() {
                bail!("no LabVIEW installations detected");
            }
            Ok(Roots::labview(&target::select(&targets, want)?))
        }
        (None, Some(p)) => Ok(Roots::scratch(p)),
        (None, None) => bail!(
            "pick a destination: --labview-version <YYYY> for a real install, --prefix <DIR> for a \
             scratch tree, or run inside a project that has a venv (`lvpm venv create`)\n\
             hint: `lvpm targets` lists detected LabVIEW installations"
        ),
    }
}

/// Where a command's packages live.
enum Destination {
    /// The LabVIEW installation itself.
    Global(Roots),
    /// A `--prefix` sandbox.
    Scratch(Roots),
    /// A project's venv.
    Venv(venv::Venv),
}

impl Destination {
    /// Roots for what reads the store or removes from it. Venv-wide for a
    /// venv — each package has roots of its own for the files themselves.
    fn store(&self) -> Roots {
        match self {
            Destination::Global(r) | Destination::Scratch(r) => r.clone(),
            Destination::Venv(v) => v.store(),
        }
    }
}

/// Is LabVIEW headless on this machine? `LV_RTE_HEADLESS` is NI's own global
/// override (LabVIEW 2026+): every LabVIEW start becomes non-interactive —
/// no activation, no dialogs. Nobody sets it on a workstation, since the IDE
/// would stop opening, so its presence says "automation machine, no developer
/// present" more reliably than any flag of ours could. lvpm reads it for the
/// three decisions that hinge on exactly that: whether a manifest without a
/// venv means the machine, whether to relink, whether to rebuild palettes.
fn headless() -> bool {
    std::env::var_os("LV_RTE_HEADLESS").is_some()
}

/// Whose install hooks run. Hooks are VIs a package runs in LabVIEW around
/// its install; written for VIPM installing into the LabVIEW installation,
/// several edit files there (palettes in `menus\`, NI's own `resource\` VIs)
/// or run installers (`msiexec`) — wrong from a venv, which must leave the
/// installation alone. Measured on 22 hooks of a 50-package project: one
/// was needed (NI's Advanced HTTP Client, whose fix linkfix now makes
/// itself), six harmful in a venv, the rest documentation pages, examples
/// recompiles and markers. So a venv runs none unless the manifest opts a
/// package in, and a LabVIEW installation runs them all, as VIPM does,
/// unless the manifest opts one out.
struct HookPolicy {
    mode: HookMode,
    venv: bool,
    headless: bool,
    /// The project manifest's per-dependency `hooks` settings.
    manifest: Vec<(String, project::Hooks)>,
}

#[derive(Clone, Copy, PartialEq)]
enum HookMode {
    /// `--no-hooks`: none.
    Never,
    /// `--hooks`: every package's, whatever the manifest says.
    Always,
    /// The manifest's setting, else the destination's default.
    Default,
}

impl HookPolicy {
    fn new(mode: HookMode, venv: bool, manifest: Option<&project::Project>) -> HookPolicy {
        HookPolicy { mode, venv, headless: headless(), manifest: manifest.map(|p| p.hooks.clone()).unwrap_or_default() }
    }

    fn setting(&self, pkg: &str) -> Option<project::Hooks> {
        self.manifest.iter().find(|(n, _)| n.eq_ignore_ascii_case(pkg)).map(|(_, h)| *h)
    }

    /// Whether `pkg`'s hooks run.
    fn allows(&self, pkg: &str) -> bool {
        match self.mode {
            HookMode::Never => false,
            HookMode::Always => true,
            HookMode::Default => match self.setting(pkg) {
                Some(project::Hooks::Run) => true,
                Some(project::Hooks::Skip) => false,
                None => !self.venv && !self.headless,
            },
        }
    }

    /// Why `pkg`'s hooks do not run, for the report.
    fn why_not(&self, pkg: &str) -> &'static str {
        match (self.mode, self.setting(pkg)) {
            (HookMode::Never, _) => "--no-hooks",
            (_, Some(project::Hooks::Skip)) => "hooks = \"skip\" in lvpm.toml",
            _ if self.venv => "a venv runs a package's hooks only when lvpm.toml lists it as <pkg> = { version = \"…\", hooks = \"run\" }, or with --hooks",
            _ => "LabVIEW runs headless here (LV_RTE_HEADLESS); <pkg> = { version = \"…\", hooks = \"run\" } in lvpm.toml or --hooks runs them",
        }
    }
}

/// Hook entries for printing. The manifest keeps them as `Name=path`, where
/// the path is wherever the VI sat on the package's build machine
/// (`<OS Boot Volume Root>\ci-builds\...`) — meaningless here, and long. Only
/// the name says anything to a reader.
fn hook_names<I, S>(hooks: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    hooks
        .into_iter()
        .map(|h| h.as_ref().split_once('=').map_or_else(|| h.as_ref().to_string(), |(n, _)| n.to_string()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Settle where a command's packages live, the way cargo settles which crate
/// you mean: `--prefix` and `--global` say so outright; otherwise a venv at or
/// above the current directory wins, and only when there is none does
/// `--labview-version` mean the installation itself.
///
/// A manifest with no venv is refused — except on a headless machine, where
/// the refusal would protect a developer who is not there: a container's
/// LabVIEW *is* the sandbox, so `lvpm install` in the project means it.
fn destination(cli: &Cli) -> Result<Destination> {
    let explicit = |r: Roots| match r.target.is_some() {
        true => Destination::Global(r),
        false => Destination::Scratch(r),
    };
    // A venv attaches its own log file; everything else that resolves to a
    // destination logs to lvpm.log from here on, so a run cut off keeps what
    // it logged.
    if cli.prefix.is_some() || cli.global {
        let roots = roots_for(cli)?;
        logging::attach_default();
        return Ok(explicit(roots));
    }
    let cwd = std::env::current_dir()?;
    if headless()
        && cli.project.is_none()
        && let venv::Found::ManifestOnly(repo) = venv::probe(&cwd)?
    {
        let roots = headless_roots(cli, &repo)?;
        logging::attach_default();
        return Ok(Destination::Global(roots));
    }
    if let Some(v) = venv::find(cli.project.as_deref(), &cwd)? {
        if cli.cmd.sbom_json() != Some(true) {
            elog!("{}", v.banner());
        }
        return Ok(Destination::Venv(v));
    }
    let roots = roots_for(cli)?;
    logging::attach_default();
    Ok(explicit(roots))
}

/// The installation a headless `lvpm install` in a venv-less project means:
/// `--labview-version` if given, else the manifest's `labview` — the same
/// choice `lvpm venv create` makes, with the same rule that the manifest's
/// version is a minimum a newer LabVIEW may satisfy and an older one may not.
fn headless_roots(cli: &Cli, repo: &Path) -> Result<Roots> {
    let manifest_path = repo.join(project::FILE_NAME);
    let proj = project::read(&manifest_path)?;
    let min_lv = proj.labview.as_deref();
    let want = cli.labview_version.as_deref().or(min_lv).ok_or_else(|| {
        anyhow::anyhow!(
            "which LabVIEW? pass --labview-version <YYYY>, or set labview in {}\n\
             hint: `lvpm targets` lists what is installed",
            manifest_path.display()
        )
    })?;
    let targets = target::detect()?;
    ensure!(!targets.is_empty(), "no LabVIEW installations detected");
    let t = target::select(&targets, want)?;
    if let Some(min) = min_lv.and_then(|v| v.trim().parse::<u32>().ok())
        && t.year() < min
    {
        bail!(
            "{} is older than the project's labview = {min} — a newer LabVIEW may stand in for \
             the project's minimum, not an older one",
            t.label()
        );
    }
    if cli.cmd.sbom_json() != Some(true) {
        elog!(
            "headless (LV_RTE_HEADLESS): {} has a manifest and no venv — using {} itself",
            repo.display(),
            t.label()
        );
    }
    Ok(Roots::labview(&t))
}

/// Where lvpm puts files a LabVIEW has to open by path: the relink VI, and a
/// PostUninstall hook that must outlive its package. `%TEMP%` is the user's
/// own on Windows; `/tmp` is shared on Linux, where a name another user took
/// first is one this user cannot write — so there it is the configured cache.
fn work_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        Ok(std::env::temp_dir())
    }
    #[cfg(not(windows))]
    {
        Ok(config::load()?.cache)
    }
}

fn state_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        Ok(base.join("lvpm"))
    }
    #[cfg(not(windows))]
    {
        Ok(work_dir()?.join("lvpm"))
    }
}

fn cmd_start(cli: &Cli, wait: u64) -> Result<()> {
    let roots = roots_for(cli)?;
    let Some(t) = roots.target else {
        bail!("start needs a real LabVIEW target, not --prefix");
    };
    let already = viserver::is_listening(&t);
    let port = viserver::ensure_vi_server(&t, std::time::Duration::from_secs(wait))?;
    log!(
        "{} — VI Server answering on port {port}{}",
        t.label(),
        if already { " (was already running)" } else { "" }
    );
    Ok(())
}

fn cmd_targets() -> Result<()> {
    let targets = target::detect()?;
    if targets.is_empty() {
        log!("no LabVIEW installations detected");
        return Ok(());
    }
    for t in &targets {
        log!("{:<34} {}", t.label(), t.path.display());
    }
    Ok(())
}

/// The manifest a command belongs to: `--manifest` names it; else the venv's
/// bound manifest when a venv is in effect; else `--project`'s; else the nearest
/// `lvpm.toml` at or above the working directory. None is fine — a package
/// can be installed without a project.
fn manifest_for(
    cli: &Cli,
    explicit: Option<&Path>,
    venv: Option<&venv::Venv>,
) -> Result<Option<(PathBuf, project::Project)>> {
    let path = match (explicit, venv, &cli.project) {
        (Some(p), _, _) => Some(p.to_path_buf()),
        (None, Some(v), _) => Some(v.manifest_path().context("venv has no bound manifest")?),
        (None, None, Some(dir)) => Some(dir.join(project::FILE_NAME)).filter(|p| p.is_file()),
        (None, None, None) => project::find_manifest(&std::env::current_dir()?),
    };
    match path {
        Some(p) => Ok(Some((std::path::absolute(&p)?, read_manifest(&p)?))),
        None => Ok(None),
    }
}

/// `[dependencies]` for `main`, else `[dependencies.<group>]`.
fn group_label(group: &str) -> String {
    match group == project::MAIN_GROUP {
        true => "[dependencies]".to_string(),
        false => format!("[dependencies.{group}]"),
    }
}

/// The venv's manifest, which `install <pkg>` and `uninstall <pkg>` edit. A
/// `vipm.toml` is VIPM's file and is never written: a venv still bound to
/// one (by an lvpm that read it) is pointed to the import that moves it.
fn editable_manifest(v: &venv::Venv) -> Result<PathBuf> {
    let path = v.manifest_path().context("venv has no bound manifest")?;
    ensure!(
        !vipm_interop::is_toml(&path),
        "{} is VIPM's manifest; lvpm does not edit it\n\
         hint: `lvpm import {}` writes it into the project's lvpm.toml once",
        path.display(),
        path.display()
    );
    Ok(path)
}

/// Write a package installed by name into the venv's manifest. A failure is
/// reported, not raised: the package is installed either way.
fn save_to_manifest(v: &venv::Venv, group: Option<&str>, name: &str, version: &str, dry_run: bool) {
    let saved = editable_manifest(v).and_then(|path| {
        let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if dry_run {
            let into = group.map(group_label).unwrap_or_else(|| "its group".to_string());
            log!("\nwould save {name} = \"{version}\" to {file} ({into})");
            return Ok(());
        }
        match project::save_dependency(&path, group, name, version)? {
            project::Saved::Added { group } => {
                log!("\nsaved {name} = \"{version}\" to {file} {}", group_label(&group))
            }
            project::Saved::Updated { group, from } => {
                log!("\nupdated {name} in {file} {}: \"{from}\" -> \"{version}\"", group_label(&group))
            }
            project::Saved::Unchanged { group } => {
                log!("\n{name} = \"{version}\" is already in {file} {}", group_label(&group))
            }
        }
        Ok(())
    });
    if let Err(e) = saved {
        wlog!("\n{name} is installed, but the manifest was not updated: {e:#}");
    }
}

/// The manifest `lvpm import` writes, before it is opened.
struct ImportPlace {
    /// The `lvpm.toml` written.
    manifest: PathBuf,
    /// Whether it exists already.
    exists: bool,
    /// The `vipm.toml` this project's venv was created from, by an lvpm that
    /// still read one: the import writes `lvpm.toml` beside it and binds the
    /// venv to that, so the venv and what is installed in it stay.
    adopt: Option<PathBuf>,
    /// Whether the file being imported is that `vipm.toml`.
    adopts_source: bool,
}

/// Where `lvpm import` writes, and how. Every change goes into `edit`, in
/// memory, on a dry run too; [`import_finish`] writes the file once, so an
/// import that fails part-way leaves the manifest as it was.
struct ImportTarget {
    place: ImportPlace,
    dry_run: bool,
    edit: project::Edit,
}

impl ImportTarget {
    fn would(&self) -> &'static str {
        if self.dry_run { "would " } else { "" }
    }
}

/// The manifest `lvpm import` writes: the venv's when one is in effect, else
/// `--project`'s or the nearest `lvpm.toml`, else a new one in the working
/// directory. A venv still bound to a `vipm.toml` gets an `lvpm.toml` beside
/// it (see [`ImportPlace::adopt`]).
fn import_target(cli: &Cli, file: &Path) -> Result<ImportPlace> {
    let cwd = std::env::current_dir()?;
    let (manifest, adopt) = match venv::project_manifest(cli.project.as_deref(), &cwd)? {
        Some(p) if vipm_interop::is_toml(&p) => (p.with_file_name(project::FILE_NAME), Some(p)),
        Some(p) => (p, None),
        None => (std::path::absolute(cwd.join(project::FILE_NAME))?, None),
    };
    let exists = manifest.is_file();
    let adopts_source = adopt.as_deref().is_some_and(|was| same_file(was, file));
    Ok(ImportPlace { manifest, exists, adopt, adopts_source })
}

/// Whether two paths name one file, as far as can be told without it existing.
fn same_file(a: &Path, b: &Path) -> bool {
    let full = |p: &Path| std::fs::canonicalize(p).or_else(|_| std::path::absolute(p)).unwrap_or_else(|_| p.to_path_buf());
    full(a) == full(b)
}

/// Name the manifest, open it (a skeleton when there is none), and set the
/// `[project]` keys the imported file gives where the manifest has none.
fn import_open(place: ImportPlace, dry_run: bool, project_keys: &[(&str, Option<&str>)]) -> Result<ImportTarget> {
    log!("\nmanifest: {}{}", place.manifest.display(), if place.exists { "" } else { " (new)" });
    let given = |key: &str| project_keys.iter().find(|(k, _)| *k == key).and_then(|(_, v)| *v);
    let edit = match place.exists {
        true => {
            // A manifest lvpm cannot read is refused here, before any edit.
            project::read(&place.manifest)?;
            project::Edit::open(&place.manifest)?
        }
        false => {
            let dir_name = place
                .manifest
                .parent()
                .and_then(|d| d.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "project".to_string());
            let name = given("name").map(str::to_string).unwrap_or(dir_name);
            project::Edit::from_text(&place.manifest, &project::skeleton(&name, given("labview")))?
        }
    };
    let mut t = ImportTarget { place, dry_run, edit };
    for (key, value) in project_keys {
        let Some(value) = value else { continue };
        match t.edit.set_project_if_missing(key, value)? {
            Some(have) if have != *value => log!("  {key} = {have:?} kept (the file says {value:?})"),
            // Already there, and the same. (In a new manifest the skeleton
            // has just put it there: that is this import setting it.)
            Some(_) if t.place.exists => {}
            _ => log!("  {}set {key} = {value:?}", t.would()),
        }
    }
    Ok(t)
}

/// Add one imported package as an exact version, and say what happened:
/// into `group` when the user named one, else into the group that declares
/// it already, else into `default`.
fn import_save(t: &mut ImportTarget, group: Option<&str>, default: &str, name: &str, version: &str) -> Result<()> {
    let (save, update) = match t.dry_run {
        true => ("would save", "would update"),
        false => ("saved", "updated"),
    };
    match t.edit.save_dependency(group, default, name, version)? {
        project::Saved::Added { group } => log!("  {save} {name} = \"{version}\" {}", group_label(&group)),
        project::Saved::Updated { group, from } => {
            log!("  {update} {name} {}: \"{from}\" -> \"{version}\"", group_label(&group))
        }
        project::Saved::Unchanged { group } => log!("  {name} = \"{version}\" already in {}", group_label(&group)),
    }
    Ok(())
}

/// Write the manifest, once, bind an adopted venv to it, and close with
/// what to do next.
fn import_finish(t: &ImportTarget) -> Result<()> {
    let file = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !t.dry_run {
        t.edit.write()?;
    }
    if let Some(was) = &t.place.adopt {
        let (new, was) = (file(&t.place.manifest), file(was));
        match t.dry_run {
            true => log!("\nwould bind this project's venv to {new}: it was created from {was}, which lvpm no longer reads"),
            false => {
                let repo = t.place.manifest.parent().context("the manifest has no folder")?;
                venv::rebind_manifest(repo, project::FILE_NAME)?;
                log!("\nthis project's venv now follows {new}: it was created from {was}, which lvpm no longer reads");
            }
        }
        if !t.place.adopts_source {
            log!("      `lvpm import {was}` brings that file's packages into {new} too");
        }
    }
    match t.dry_run {
        true => log!("\ndry run: nothing written"),
        false => log!("\nnext: `lvpm install` installs them"),
    }
    Ok(())
}

/// `lvpm import <file>`: a VIPM file written into the project's `lvpm.toml`
/// once. lvpm reads `lvpm.toml` only; this is how VIPM's files come in.
fn cmd_import(cli: &Cli, file: &Path, all: bool, group: Option<&str>, dry_run: bool) -> Result<()> {
    if let Some(g) = group {
        ensure!(!g.trim().is_empty(), "--group needs a name (`main` is [dependencies])");
    }
    let file = std::path::absolute(file)?;
    if vipm_interop::is_toml(&file) {
        ensure!(!all, "--all is for a .vipc; a vipm.toml lists only what the project declares");
        return import_vipm_toml(cli, &file, group, dry_run);
    }
    ensure!(
        vipm_interop::is_vipc(&file),
        "lvpm import reads VIPM's vipm.toml or a .vipc package configuration; got {}",
        file.display()
    );
    import_vipc(cli, &file, all, group, dry_run)
}

/// A `vipm.toml`: `[dependencies]` as exact versions (into `--group` when
/// given), `[dev-dependencies]` into `[dependencies.dev]` unless another
/// group declares the package already, `[nipm.dependencies]` as written, and
/// `[project]` name, version and `labview-version` (as the minimum
/// `labview`) where the manifest has none.
fn import_vipm_toml(cli: &Cli, file: &Path, group: Option<&str>, dry_run: bool) -> Result<()> {
    let c = vipm_interop::read_toml(file)?;
    let place = import_target(cli, file)?;
    log!("import: {} (VIPM manifest)", file.display());
    if let Some(lv) = &c.project.labview {
        log!("  labview-version {lv:?} read as minimum labview = {lv:?}");
    }
    if !c.unused.is_empty() {
        log!("  not used: {}", c.unused.join(", "));
    }
    let mut t = import_open(
        place,
        dry_run,
        &[
            ("name", c.project.name.as_deref()),
            ("version", c.project.version.as_deref()),
            ("labview", c.project.labview.as_deref()),
        ],
    )?;
    for (name, constraint) in &c.project.dependencies {
        let project::Constraint::Exact(v) = constraint else { unreachable!("vipm.toml versions are exact") };
        import_save(&mut t, group, project::MAIN_GROUP, name, &v.raw)?;
    }
    for (gname, g) in &c.project.groups {
        for (name, constraint) in &g.dependencies {
            let project::Constraint::Exact(v) = constraint else { unreachable!("vipm.toml versions are exact") };
            import_save(&mut t, None, gname, name, &v.raw)?;
        }
    }
    if c.project.groups.iter().any(|(n, _)| n == vipm_interop::DEV_GROUP) {
        log!("  [dev-dependencies] are the group {0:?}: `lvpm install --with {0}`", vipm_interop::DEV_GROUP);
    }
    for (name, value) in &c.nipm {
        match (t.edit.save_nipm(name, value)?, dry_run) {
            (true, true) => log!("  would save {name} = {value} [nipm.dependencies]"),
            (true, false) => log!("  saved {name} = {value} [nipm.dependencies]"),
            (false, _) => log!("  {name} = {value} already in [nipm.dependencies]"),
        }
    }
    if c.project.dependencies.is_empty() && c.project.groups.is_empty() && c.nipm.is_empty() {
        log!("\n{} lists no packages", file.display());
    }
    import_finish(&t)
}

/// A `.vipc`: the packages a project would list by hand (those nothing else
/// in the file requires; every package with `--all`) as exact versions, the
/// target as `labview` when the manifest has none, and the packages carried
/// in the file into the cache. Ends by naming the packages that only the
/// local cache has, since a teammate or CI runner will not have them.
fn import_vipc(cli: &Cli, file: &Path, all: bool, group: Option<&str>, dry_run: bool) -> Result<()> {
    let file_name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut vipc = vipm_interop::read_vipc(file)?;
    let place = import_target(cli, file)?;

    log!("import: {} (VIPM package configuration)", file.display());
    match (&vipc.target, &vipc.labview) {
        (Some(tg), Some(lv)) => log!("  target LabVIEW {tg}, read as minimum labview = {lv:?}"),
        (Some(tg), None) => log!("  target LabVIEW {tg}, older than lvpm can name by year; labview not set"),
        (None, _) => log!("  no LabVIEW target"),
    }
    if vipc.target.as_deref().is_some_and(|tg| tg.contains("-bit")) {
        log!("  bitness not used: lvpm takes it from the LabVIEW the venv is bound to");
    }
    for id in &vipc.unversioned {
        wlog!("  {id:?} in config.xml has no version in its name; not imported");
    }
    for spec in &vipc.unreadable_specs {
        wlog!("  {spec} is missing or unreadable; that package counts as having no dependencies");
    }
    // A package carried in the file is what an install will get, whatever
    // config.xml calls it: the pin follows the archive's own spec.
    for m in vipc.follow_embedded().with_context(|| format!("reading the packages inside {file_name}"))? {
        wlog!(
            "  config.xml lists {} {}, but the package inside ({}) is {} {}; importing it as that",
            m.listed.0, m.listed.1, m.member, m.actual.0, m.actual.1
        );
    }

    let roots = vipc.roots();
    let pinned: Vec<usize> = match all {
        true => (0..vipc.packages.len()).collect(),
        false => roots.clone(),
    };
    let by = vipc.required_by(&roots);
    let embedded = vipc.packages.iter().filter(|p| p.embedded.is_some()).count();
    log!(
        "  {} package(s): {} top-level, {} required by them; {embedded} carried in the file",
        vipc.packages.len(),
        roots.len(),
        vipc.packages.len() - roots.len()
    );

    let mut t = import_open(place, dry_run, &[("labview", vipc.labview.as_deref())])?;
    for &i in &pinned {
        let p = &vipc.packages[i];
        import_save(&mut t, group, project::MAIN_GROUP, &p.name, &p.version)?;
    }
    let left: Vec<usize> = (0..vipc.packages.len()).filter(|i| !pinned.contains(i)).collect();
    if !left.is_empty() {
        log!("\nnot written, installed as dependencies of the packages above (--all pins them too):");
        for &i in &left {
            let p = &vipc.packages[i];
            match by[i] {
                Some(r) => log!("  {} {}  (via {})", p.name, p.version, vipc.packages[r].name),
                None => log!("  {} {}", p.name, p.version),
            }
        }
        log!("  the install resolves them again and may pick newer versions than the file lists");
    }

    // The cache before the manifest: a cached package no manifest names is
    // harmless, a manifest naming a package that never reached the cache is not.
    let config = config::load()?;
    if embedded > 0 {
        log!("\n{}package cache, from {file_name}:", if dry_run { "would add to the " } else { "added to the " });
        for p in vipc.packages.iter() {
            let Some((member, bytes)) = &p.embedded else { continue };
            if dry_run {
                log!("  {} {}", p.name, p.version);
                continue;
            }
            let source = format!("{}!{member}", file.display());
            let (_, _, sha, had) = index::cache_add_bytes(&config.cache, &source, bytes)?;
            log!("  {} {}  sha256:{}{}", p.name, p.version, &sha[..12], if had { " (already there)" } else { "" });
        }
    }

    // Which packages no index has: the embedded ones are then only in this
    // machine's cache, the others cannot be installed at all.
    let project = match t.place.exists {
        true => Some((t.place.manifest.clone(), project::read(&t.place.manifest)?)),
        false => None,
    };
    let local_only: Vec<&vipm_interop::VipcPackage> = match load_index(cli, project.as_ref()) {
        Err(e) => {
            wlog!("\ncould not read the package indexes to check availability: {e:#}");
            Vec::new()
        }
        Ok(idx) => {
            let hosted = |p: &vipm_interop::VipcPackage| {
                idx.versions_of(&p.name).iter().any(|e| !index::is_local_url(&e.url) && e.version.raw == p.version)
            };
            let missing: Vec<&vipm_interop::VipcPackage> = pinned
                .iter()
                .map(|&i| &vipc.packages[i])
                .filter(|p| p.embedded.is_none() && !idx.versions_of(&p.name).iter().any(|e| e.version.raw == p.version))
                .collect();
            if !missing.is_empty() {
                wlog!("\nnot on any index lvpm reads, nor in the cache; installing them will fail:");
                for p in &missing {
                    wlog!("  {} {}", p.name, p.version);
                }
                wlog!("  add the index that has them under [sources], or `lvpm cache add` the .vip files");
            }
            vipc.packages.iter().filter(|p| p.embedded.is_some() && !hosted(p)).collect()
        }
    };
    if !local_only.is_empty() {
        log!(
            "\nnote: {} package(s) {} only in your local package cache: no index lvpm reads has these versions.",
            local_only.len(),
            if dry_run { "would be" } else { "are" }
        );
        for p in &local_only {
            log!("  {} {}", p.name, p.version);
        }
        log!(
            "  lvpm.toml names them, but a teammate or CI runner can install them only after\n  \
             `lvpm import {file_name}` there too (or once they are hosted on an index in [sources])."
        );
    }
    if vipc.packages.is_empty() {
        log!("\n{file_name} lists no packages");
    }
    import_finish(&t)
}

/// Read a manifest file. lvpm's manifest is `lvpm.toml` (under any name
/// `--manifest` gives it); a `vipm.toml` is refused with the way in.
fn read_manifest(path: &Path) -> Result<project::Project> {
    ensure!(
        !vipm_interop::is_toml(path),
        "{} is VIPM's manifest; lvpm reads an lvpm.toml\n\
         hint: `lvpm import {}` writes it into the project's lvpm.toml once",
        path.display(),
        path.display()
    );
    project::read(path)
}

/// Every index the command should see: global local folders plus hosted
/// manifest sources, on top of the public indexes unless disabled.
fn load_index(cli: &Cli, manifest: Option<&(PathBuf, project::Project)>) -> Result<index::Index> {
    let config = config::load()?;
    let mut repos: Vec<String> = Vec::new();
    let mut defaults = true;
    if let Some((_, proj)) = manifest {
        repos.extend(proj.resolved_sources()?);
        defaults = proj.default_sources;
    }
    index::load(&config.cache, cli.refresh, &repos, &config.local_sources, defaults)
}

fn cmd_search(cli: &Cli, query: &str) -> Result<()> {
    let manifest = manifest_for(cli, None, None)?;
    let idx = load_index(cli, manifest.as_ref())?;
    let hits = idx.search(query);
    if hits.is_empty() {
        log!("no packages matching {query:?}");
        return Ok(());
    }
    for e in hits.iter().take(40) {
        log!(
            "{:<52} {:<14} {}",
            e.name,
            e.version.to_string(),
            e.display_name.as_deref().unwrap_or("")
        );
    }
    if hits.len() > 40 {
        log!("... and {} more", hits.len() - 40);
    }
    Ok(())
}

fn cmd_list(cli: &Cli) -> Result<()> {
    let dest = destination(cli)?;
    let roots = dest.store();
    let installed = install::list_installed(&roots)?;
    if installed.is_empty() {
        log!("lvpm has installed nothing into this target");
        return Ok(());
    }
    // Listing still works with a manifest that does not read: the owed count
    // then follows the destination's defaults, and says so.
    let policy = hook_policy_for(cli, &dest, HookMode::Default).unwrap_or_else(|e| {
        wlog!("warning: {e:#}\n      hooks owed are counted without the manifest's `hooks` settings");
        HookPolicy::new(HookMode::Default, matches!(dest, Destination::Venv(_)), None)
    });
    let mut owed = 0;
    for m in &installed {
        let post = match (&m.post_install_vi, &m.post_install) {
            (None, _) => "",
            (Some(_), Some(install::HookState::Ran { .. })) => "post-install ran",
            (Some(_), Some(install::HookState::Pending)) => "post-install PENDING",
            (Some(_), Some(install::HookState::Failed { .. })) => "post-install FAILED",
            (Some(_), Some(install::HookState::Skipped { .. })) => "post-install skipped",
            (Some(_), None) => "post-install unknown",
        };
        // Owed: what the hook policy would run and has not run cleanly.
        owed += usize::from(
            policy.allows(&m.name)
                && (post.ends_with("PENDING") || post.ends_with("FAILED") || post.ends_with("unknown") || post.ends_with("skipped")),
        );
        // Hooks a venv can never run (PreInstall, the uninstall hooks) are not
        // shown: there is nothing to do about them.
        log!(
            "{:<52} {:<14} {:>5} files  {:<12} {}",
            m.name,
            m.version,
            m.files.len(),
            // Linkfix prepares every install; LabVIEW's relink is the opt-in exception.
            if m.relinked { "relinked" } else { "" },
            post,
        );
    }
    if owed > 0 {
        log!("\n{owed} post-install hook(s) owed (pending, failed, skipped or of unknown state) — `lvpm run-hooks --all` runs them");
    }
    Ok(())
}

fn installed_sbom(
    cli: &Cli, include: Option<&Path>, groups: &GroupArgs, output: &sbom_paths::OutputGuard,
) -> Result<serde_json::Value> {
    let dest = destination(cli)?;
    let venv = match &dest {
        Destination::Venv(v) => Some(v),
        _ => None,
    };
    let (manifest, project) = manifest_for(cli, None, venv)?
        .context("sbom needs an lvpm.toml project manifest")?;
    output.protect(&manifest)?;
    if let Some(venv) = venv {
        output.protect(&venv.binding_path())?;
    }
    let name = project.name.as_deref().or_else(|| {
        manifest.parent()?.file_name()?.to_str()
    }).context("sbom could not determine a project name; set [project].name")?;
    let selection = project.select(&groups.with, &groups.without, &groups.only)
        .map_err(|error| sbom_ci::input_error(error, &manifest))?;
    let installed = install::list_installed_with(&dest.store(), |path| output.protect(path))?;
    sbom::document(name, project.version.as_deref(), &selection, &installed, include)
}

/// The hook policy for a command that is not an install: the destination's
/// default and the `hooks` settings of the manifest an install there would
/// follow — a venv's bound manifest, else `--project`'s or the nearest
/// `lvpm.toml` (see [`manifest_for`]). A manifest that is there and cannot be
/// read is an error, not a silent return to the defaults: those would run
/// hooks the manifest opts out of.
fn hook_policy_for(cli: &Cli, dest: &Destination, mode: HookMode) -> Result<HookPolicy> {
    let manifest = match dest {
        Destination::Venv(v) => match v.manifest_path().filter(|p| p.is_file()) {
            Some(p) => Some(read_manifest(&p)?),
            None => None,
        },
        _ => manifest_for(cli, None, None)?.map(|(_, p)| p),
    };
    Ok(HookPolicy::new(mode, matches!(dest, Destination::Venv(_)), manifest.as_ref()))
}

fn cmd_refresh(cli: &Cli, timeout: u64) -> Result<()> {
    let roots = roots_for(cli)?;
    let Some(t) = roots.target.clone() else {
        bail!("refresh needs a real LabVIEW target, not --prefix");
    };
    refresh_palettes_and_menus(&t, timeout);
    Ok(())
}

/// A venv LabVIEW reads its palettes at startup: one already running does not
/// show a rebuilt `venv-palettes` addon until it is restarted.
fn note_palette_change(dest: &Destination, changed: bool) {
    if changed && let Destination::Venv(v) = dest && launch::is_running(v) {
        log!("palettes: changed; restart the venv's LabVIEW to see them");
    }
}

fn cmd_uninstall(
    cli: &Cli, package: Option<&str>, all: bool, with_deps: bool, keep_deps: bool, no_save: bool, force: bool,
) -> Result<()> {
    let dest = destination(cli)?;
    let roots = dest.store();
    // The manifest a package uninstalled by name is taken out of, and its name.
    let mut unsave: Option<(PathBuf, String)> = None;
    let names: Vec<String> = match (package, all) {
        (Some(p), _) => {
            let installed = install::list_installed(&roots)?;
            let project_manifest = match &dest {
                Destination::Venv(v) => Some(v.manifest_path().context("venv has no bound manifest")?),
                _ => None,
            };
            let editable = match &dest {
                Destination::Venv(v) if !no_save => Some(editable_manifest(v)?),
                _ => None,
            };
            let mut project_roots = match &project_manifest {
                Some(path) => project_uninstall_roots(path)?,
                None => Vec::new(),
            };
            // A package leaving the manifest is no longer one of its roots,
            // so what only it needed may go with it.
            if editable.is_some() {
                project_roots.retain(|root| !root.eq_ignore_ascii_case(p));
            }
            let manifest_name = project_manifest.as_ref().and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "project manifest".into());
            let plan = dependency::removal_plan(&installed, p, &project_roots)?;
            if plan.project_declared {
                elog!("warning: {} is declared in {}; uninstall does not edit the manifest", plan.root, manifest_name);
            }
            if let Some(path) = editable {
                unsave = Some((path, plan.root.clone()));
            }
            // Removing what another installed package needs leaves that one
            // broken: refused before anything is touched, unless --force.
            if !plan.dependents.is_empty() {
                ensure!(
                    force,
                    "{} is required by installed packages: {}\n\
                     hint: uninstall {} first, or pass --force to remove {} anyway",
                    plan.root,
                    plan.dependents.join(", "),
                    plan.dependents.join(", "),
                    plan.root
                );
                elog!("warning: {} is required by installed packages: {} (--force)", plan.root, plan.dependents.join(", "));
            }
            for (name, owners) in &plan.kept {
                if plan.project_kept.iter().any(|kept| kept.eq_ignore_ascii_case(name)) {
                    if project_roots.iter().any(|root| root.eq_ignore_ascii_case(name)) {
                        elog!("keeping {name}: declared in {manifest_name}");
                    } else {
                        elog!("keeping {name}: required by a package declared in {manifest_name}");
                    }
                } else {
                    elog!("keeping {name}: still required by {}", owners.join(", "));
                }
            }
            let mut names = vec![plan.root];
            if !plan.removable.is_empty() && !keep_deps {
                let remove = if with_deps {
                    true
                } else {
                    let stdin = std::io::stdin();
                    confirm_dependencies(&plan.removable, stdin.is_terminal(), &mut stdin.lock(), &mut std::io::stderr())?
                };
                if remove {
                    names.extend(plan.removable);
                }
            }
            names
        },
        (None, true) => install::list_installed(&roots)?.into_iter().map(|m| m.name).collect(),
        (None, false) => bail!("give a package name, or --all"),
    };
    ensure!(!names.is_empty(), "no packages are installed in this target");

    // Uninstalled by name on a venv: out of the manifest too, from every group
    // declaring it. Run once, after the sweep, or earlier when a dependency's
    // failure ends the sweep with the named package already gone.
    let unsave_now = || {
        if let Some((path, name)) = &unsave {
            let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            match project::remove_dependency(path, name) {
                Ok(groups) if groups.is_empty() => {}
                Ok(groups) => log!(
                    "removed {name} from {file} {}",
                    groups.iter().map(|g| group_label(g)).collect::<Vec<_>>().join(", ")
                ),
                Err(e) => wlog!("{name} is uninstalled, but the manifest was not updated: {e:#}"),
            }
        }
    };

    // One VI Server connection for the whole sweep, and one palette refresh at
    // the end — the hooks are per package, the rebuild is not.
    let mut conn: Option<viserver::Connection> = None;
    let mut removed_any = false;
    let mut uninstalled_any = false;
    let mut failed: Vec<(String, anyhow::Error)> = Vec::new();
    for (i, name) in names.iter().enumerate() {
        match uninstall_one(&roots, name, &mut conn) {
            Ok(removed) => {
                removed_any |= removed > 0;
                uninstalled_any = true;
            }
            // One bad package must not strand the rest of an --all sweep. A
            // single named package still fails the command with its own error.
            Err(e) => {
                if !all {
                    if named_package_removed_before(i) {
                        unsave_now();
                        if roots.venv().is_some() {
                            palette::reconcile(&roots)
                                .context("the named package was removed, but venv palettes could not be reconciled")?;
                        }
                    }
                    return Err(e);
                }
                if names.len() > 1 {
                    wlog!("uninstall {name} FAILED: {e:#}");
                }
                failed.push((name.clone(), e));
            }
        }
    }
    if let Some(c) = conn {
        c.close();
    }

    // Only reached when the named package was removed: a failure of its own
    // returned above, and one of a dependency edited the manifest on the way out.
    unsave_now();

    if uninstalled_any && roots.venv().is_some() {
        let changed = palette::reconcile(&roots)
            .context("packages were removed, but venv palettes could not be reconciled")?;
        note_palette_change(&dest, changed);
    }

    // The same reason as on install, in reverse: the packages' `.mnu` files
    // are gone, but LabVIEW still shows the palette entries until told. Not
    // for a venv — its LabVIEW is restarted on launch and rebuilds them then.
    if removed_any
        && roots.venv().is_none()
        && let Some(t) = &roots.target
    {
        refresh_palettes_and_menus(t, HOOK_TIMEOUT_SECS);
    }

    if names.len() == 1 && let Some((_, e)) = failed.pop() {
        return Err(e);
    }
    ensure!(
        failed.is_empty(),
        "{} of {} packages failed to uninstall: {}",
        failed.len(),
        names.len(),
        failed.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ")
    );
    Ok(())
}

/// `names` starts with the package named on the command line, so a failure
/// anywhere after the first position means that package is already gone and
/// must not stay declared in the manifest; a failure of the first leaves it.
fn named_package_removed_before(failed_at: usize) -> bool {
    failed_at > 0
}

fn project_uninstall_roots(path: &Path) -> Result<Vec<String>> {
    let manifest = read_manifest(path)?;
    Ok(manifest.declared_dependency_names().map(str::to_owned).collect())
}

fn confirm_dependencies(
    names: &[String], interactive: bool, input: &mut impl std::io::BufRead, output: &mut impl Write,
) -> Result<bool> {
    ensure!(interactive,
        "dependencies can be removed: {}. Pass --with-deps or --keep-deps in non-interactive mode",
        names.join(", "));
    write!(output, "remove unshared dependencies ({}) too? [y/N] ", names.join(", "))?;
    output.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(answer.trim().eq_ignore_ascii_case("y") || answer.trim().eq_ignore_ascii_case("yes"))
}

/// Remove one package and run its uninstall hooks. Returns the file count, so
/// the caller knows whether anything left disk and a palette rebuild is owed.
fn uninstall_one(
    roots: &Roots,
    package: &str,
    conn: &mut Option<viserver::Connection>,
) -> Result<usize> {
    let before = install::read_manifest(roots, package)?;
    // Hooks act on the LabVIEW installation; a venv is not one, so none run there.
    let hooks = roots.venv().is_none();

    // PreUninstall runs while the package's files are still on disk. Both
    // uninstall hooks are best-effort: a hook needs a running LabVIEW, and a
    // hook failure must not leave the package half-present.
    if hooks && let (Some(hook), Some(t)) = (&before.pre_uninstall_vi, &roots.target) {
        let info = hook_action_info(&before.name, before.display_name.as_deref(), t, &before.files);
        // Only ever the target's own LabVIEW here: `hooks` is false in a venv,
        // whose instance has to be closed before its files can be removed.
        match viserver::ensure_vi_server(t, std::time::Duration::from_secs(120))
            .and_then(|port| run_hook_vi(conn, port, HOOK_TIMEOUT_SECS, Path::new(hook), &info))
        {
            Ok(took) => log!("pre-uninstall ok ({:.1}s)", took.as_secs_f64()),
            Err(e) => wlog!("pre-uninstall FAILED: {e:#} — uninstalling anyway"),
        }
    }

    // PostUninstall runs after the files are gone — including the extracted
    // hook VI itself, so it runs from a copy that outlives the uninstall.
    let post = match &before.post_uninstall_vi {
        Some(hook) if hooks => {
            work_dir()
                .and_then(|dir| {
                    let tmp = dir.join(format!("lvpm-{}-PostUninstall.vi", before.name));
                    std::fs::create_dir_all(&dir)?;
                    std::fs::copy(hook, &tmp)?;
                    Ok(tmp)
                })
                .map_err(|e| log!("note: cannot stage PostUninstall.vi ({e}) — not running it"))
                .ok()
        }
        _ => None,
    };

    let (removed, m) = install::uninstall(roots, package).with_context(|| match roots.venv() {
        Some(_) => "removing the package's files — if a LabVIEW is running on this venv, close it first",
        None => "removing the package's files",
    })?;
    log!("removed {} {} ({removed} files)", m.name, m.version);

    if let (Some(tmp), Some(t)) = (&post, &roots.target) {
        let info = hook_action_info(&m.name, m.display_name.as_deref(), t, &m.files);
        match viserver::ensure_vi_server(t, std::time::Duration::from_secs(120))
            .and_then(|port| run_hook_vi(conn, port, HOOK_TIMEOUT_SECS, tmp, &info))
        {
            Ok(took) => log!("post-uninstall ok ({:.1}s)", took.as_secs_f64()),
            Err(e) => wlog!("post-uninstall FAILED: {e:#}"),
        }
        let _ = std::fs::remove_file(tmp);
    }

    // Hooks we still do not run, and hooks we could not run here: an install
    // made before uninstall hooks were extracted has nothing on disk to run.
    let unrun: Vec<&String> = m
        .skipped_hooks
        .iter()
        .filter(|h| {
            let ran_pre = hooks && m.pre_uninstall_vi.is_some() && h.starts_with("PreUninstall=");
            let ran_post = post.is_some() && h.starts_with("PostUninstall=");
            !(ran_pre || ran_post)
                && (h.starts_with("PreUninstall=") || h.starts_with("PostUninstall="))
        })
        .collect();
    // In a venv they never run; nothing to say about that.
    if !unrun.is_empty() && hooks {
        log!("note: declared but not run (installed before uninstall hooks existed): {}", hook_names(unrun));
    }
    Ok(removed)
}

/// Uninstall has no --relink-timeout to borrow, and a hook is one VI run.
const HOOK_TIMEOUT_SECS: u64 = 300;

/// What an install does with one resolved package, given what is already
/// in its roots. An installed package is judged by version: the same one is
/// left alone, an older one is upgraded, and a newer one is never silently
/// downgraded.
enum InstallAction {
    Install,
    Same,
    /// The installed, newer version.
    KeepNewer(String),
}

/// What `install <pkg>` on a venv does to the manifest.
#[derive(Debug, PartialEq)]
enum SaveEntry {
    /// Write `name = "version"`.
    Save { name: String, version: String },
    /// Leave it alone: the newer `installed` version was kept, and what
    /// would be written does not allow it.
    Kept { name: String, installed: String, resolved: String },
}

/// What a package named on a venv is saved as: the version asked for
/// (`name@1.2.3`, `name@>=1.2`), else the one resolved, under the index's
/// spelling of the name. Nothing is saved that the venv does not hold: when
/// a newer installed version was kept, the entry is written only if it
/// allows that version (`name@>=1.2` does, an exact version does not).
fn save_entry_for(package: &str, plan: &[index::Entry], actions: &[InstallAction]) -> Option<SaveEntry> {
    let (name, asked) = match package.split_once('@') {
        Some((n, v)) => (n, Some(v)),
        None => (package, None),
    };
    let i = plan.iter().position(|e| e.name.eq_ignore_ascii_case(name))?;
    let e = &plan[i];
    let version = asked.map_or_else(|| e.version.to_string(), str::to_string);
    if let Some(InstallAction::KeepNewer(installed)) = actions.get(i)
        && !project::Constraint::parse(&version).is_ok_and(|c| c.allows(&Version::parse(installed)))
    {
        return Some(SaveEntry::Kept {
            name: e.name.clone(),
            installed: installed.clone(),
            resolved: e.version.to_string(),
        });
    }
    Some(SaveEntry::Save { name: e.name.clone(), version })
}

fn install_action(roots: &Roots, e: &index::Entry, allow_downgrade: bool) -> Result<InstallAction> {
    if !install::is_installed(roots, &e.name) {
        return Ok(InstallAction::Install);
    }
    let old = install::read_manifest(roots, &e.name)?;
    Ok(match Version::parse(&old.version).cmp(&e.version) {
        std::cmp::Ordering::Equal => InstallAction::Same,
        std::cmp::Ordering::Greater if !allow_downgrade => InstallAction::KeepNewer(old.version),
        _ => InstallAction::Install,
    })
}

fn cmd_install(
    cli: &Cli,
    package: Option<&str>,
    manifest: Option<&Path>,
    dry_run: bool,
    no_deps: bool,
    no_linkfix: bool,
    relink_pass: bool,
    no_hooks: bool,
    run_hooks: bool,
    allow_downgrade: bool,
    groups: &GroupArgs,
    save: &SaveArgs,
    relink_args: &RelinkArgs,
) -> Result<()> {
    // Headless (containers, CI): the hooks measured (DQMH's six) write a
    // marker file and add a palette entry — IDE furniture. Skipping them means
    // a headless install never starts LabVIEW at all. A package whose hook
    // does real setup gets `hooks = "run"` or `--hooks`.
    let hook_mode = match (no_hooks, run_hooks) {
        (true, _) => HookMode::Never,
        (_, true) => HookMode::Always,
        _ => HookMode::Default,
    };
    let dest = destination(cli)?;
    // The store is one per destination; in a venv each package gets roots of
    // its own inside the loop, because every package there is its own addon.
    let store = dest.store();
    let venv = match &dest {
        Destination::Venv(v) => Some(v.clone()),
        _ => None,
    };
    let lv_gate = store.target.as_ref().map(|t| t.version);

    // The project this install belongs to, if any: its `[sources]` count even
    // when a single package is named. Inside a project a bare `lvpm install`
    // means the manifest's dependencies — the way `npm install` does.
    let found = manifest_for(cli, manifest, venv.as_ref())?;
    // Its `hooks` settings hold for a named package too.
    let policy = HookPolicy::new(hook_mode, venv.is_some(), found.as_ref().map(|(_, p)| p));
    let (wanted, from_manifest) = match (package, &found) {
        (Some(p), _) => {
            let w = match p.split_once('@') {
                Some((n, v)) => vec![(n.to_string(), project::Constraint::parse(v)?)],
                None => vec![(p.to_string(), project::Constraint::Any)],
            };
            (w, None)
        }
        (None, Some((path, proj))) => {
            let s = proj.select(&groups.with, &groups.without, &groups.only)?;
            (s.dependencies, Some((path, proj, s.groups)))
        }
        (None, None) => bail!("give a package name, or --manifest <FILE>"),
    };

    if let Some((path, proj, selected)) = &from_manifest {
        ensure!(!wanted.is_empty(), "nothing to install: the selected groups ({}) are empty", selected.join(", "));
        elog!(
            "manifest: {}{} — {} dependenc{} ({})",
            path.display(),
            proj.name.as_deref().map(|n| format!(" ({n})")).unwrap_or_default(),
            wanted.len(),
            if wanted.len() == 1 { "y" } else { "ies" },
            selected.join(", ")
        );
        let skipped: Vec<&str> = proj
            .groups
            .iter()
            .map(|(n, _)| n.as_str())
            .filter(|n| !selected.iter().any(|s| s == n))
            .collect();
        if !skipped.is_empty() {
            elog!("      groups not selected: {} (--with <GROUP> adds one)", skipped.join(", "));
        }
        if let Some(v) = &proj.labview {
            elog!("      manifest says labview = {v:?} (minimum)");
        }
    }

    match &store.target {
        Some(t) => elog!("target: {}  ({})", t.label(), t.path.display()),
        None => elog!("target: scratch tree at {}", store.application.display()),
    }

    elog!("loading indexes...");
    let idx = load_index(cli, found.as_ref())?;
    elog!("  {} package versions known", idx.entries.len());

    let resolution = resolve::install(&idx, &wanted, lv_gate, no_deps, |name| {
        let path = install::manifest_path(&store, name);
        if !path.try_exists().with_context(|| format!("checking {}", path.display()))? {
            return Ok(None);
        }
        let manifest = install::read_manifest(&store, name)?;
        Ok(Some(resolve::Installed { name: manifest.name, version: manifest.version }))
    })?;
    for notice in &resolution.notices {
        match notice {
            resolve::Notice::Satisfied { name, version } =>
                elog!("  = {name} {version} already installed, satisfies dependency"),
            resolve::Notice::UnresolvedDependency(name) =>
                ewlog!("  ! dependency {name} unresolved, skipping"),
            resolve::Notice::PinBelowRequirement { name, pinned, required, by } => elog!(
                "  ! {name} is pinned to {pinned}, but {by} requires >= {required}; keeping the pin — \
                 {by} may not work with it"
            ),
        }
    }
    if !resolution.unresolved_roots.is_empty() {
        bail!(
            "not found in the configured indexes:\n  {}\n\
             hint: `lvpm search <name>` to see what is available; a [sources] entry \
             in {} adds a repository",
            resolution.unresolved_roots.join("\n  "),
            project::FILE_NAME
        );
    }

    let plan = resolution.plan;
    if venv.is_some() {
        ensure!(
            !plan.iter().any(|entry| entry.name.eq_ignore_ascii_case(palette::ADDON_NAME)),
            "package name {:?} is reserved for lvpm's merged palette addon",
            palette::ADDON_NAME
        );
    }

    // What the install does with each package of the plan, settled once:
    // the fetch below and the manifest entry both follow from it.
    let actions: Vec<InstallAction> = plan
        .iter()
        .map(|e| {
            let roots = match &venv {
                Some(v) => v.roots_for_pkg(&e.name)?,
                None => store.clone(),
            };
            install_action(&roots, e, allow_downgrade)
        })
        .collect::<Result<_>>()?;

    let save_entry: Option<SaveEntry> = match (package, &venv) {
        (Some(p), Some(_)) if !save.no_save => save_entry_for(p, &plan, &actions),
        (Some(_), None) if save.group.is_some() => {
            elog!("note: --group only applies on a venv; nothing is saved to a manifest here");
            None
        }
        _ => None,
    };

    log!("\nresolved {} package{}:", plan.len(), if plan.len() == 1 { "" } else { "s" });
    for e in &plan {
        log!("  {} {}", e.name, e.version);
    }
    log!();

    if let Some((_, proj, _)) = &from_manifest
        && !proj.nipm.is_empty()
    {
        log!(
            "note: the manifest also lists {} [nipm.dependencies] — those are NI Package",
            proj.nipm.len()
        );
        log!("      Manager packages and lvpm does not install them.
");
    }

    // reqwest's blocking default is 30 s for the whole request, which a large
    // package on a slow link exceeds; keep the connect bound, widen the rest.
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("lvpm/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(120))
        .build()?;
    let config = config::load()?;
    let package_cache = cache::Cache::open(&config.cache)?;

    // Every archive is fetched before the first file is written: a download
    // that fails then leaves the target as it was, instead of half installed
    // with the earlier packages' PostInstall hooks never run. Fetched, not
    // held: the copy loop reads each one back from the cache (or its local
    // file) when its turn comes, so memory holds one archive at a time. Only
    // a download the cache cannot find again (no MD5) stays in hand.
    let mut archives: HashMap<String, Vec<u8>> = HashMap::new();
    {
        let wanted: Vec<&index::Entry> = plan
            .iter()
            .zip(&actions)
            .filter(|(_, a)| matches!(a, InstallAction::Install))
            .map(|(e, _)| e)
            .collect();
        if !wanted.is_empty() {
            logp!("fetching {} package(s) ... ", wanted.len());
        }
        let started = std::time::Instant::now();
        for e in &wanted {
            let bytes = fetch::package(&package_cache, &client, e)
                .with_context(|| format!("{} {}: nothing was installed", e.name, e.version))?;
            if !fetch::rereadable(e) {
                archives.insert(e.name.clone(), bytes);
            }
        }
        if !wanted.is_empty() {
            log!("{}", took(started.elapsed()));
        }
    }
    let copy_started = std::time::Instant::now();

    let mut total_writes = 0usize;
    // Any package declared a hook this install did not run — decides whether
    // the headless note below is owed. Each package already said which.
    let mut hooks_unrun = false;
    // The PostInstall hooks to run, executed only after linkfix or the relink
    // pass has made them runnable; settled from the records after the copy.
    let hook_runs: Vec<(String, PathBuf, Vec<String>)>;
    // Packages a dry run would install that declare a PostInstall hook.
    let mut planned_post: Vec<String> = Vec::new();
    // PreInstall hooks run inline instead, before their package's files are
    // copied — that is the contract their PostInstall counterparts rely on
    // (DQMH's pair passes a marker file between them).
    let mut hook_conn: Option<viserver::Connection> = None;
    // Per package, in install order: the folders its files landed in. Relinking
    // happens after every package is on disk, so no package can be relinked
    // against a dependency that is not there yet.
    let mut relink_work: Vec<(String, Vec<PathBuf>)> = Vec::new();
    let mut linkfix_files = Vec::new();

    // Migrates package-local menus before anything is uninstalled or upgraded.
    // Not fatal: a conflict already in the venv must not block the install
    // that resolves it, and the reconcile after the install is the one that
    // counts — it fails on whatever is still wrong then.
    if !dry_run && venv.is_some()
        && let Err(e) = palette::reconcile(&store)
    {
        wlog!("palettes: {e:#}; retrying after the install");
    }

    for e in &plan {
        // In a venv every package is an addon of its own; the store is shared.
        let roots = match &venv {
            Some(v) => v.roots_for_pkg(&e.name)?,
            None => store.clone(),
        };
        // An installed package is judged by version. The same one is left
        // alone; an older one is upgraded, its recorded footprint removed
        // first so files the new version no longer ships do not linger; a
        // newer one is never silently downgraded.
        let installed = match install::is_installed(&roots, &e.name) {
            true => Some(install::read_manifest(&roots, &e.name)?),
            false => None,
        };
        match install_action(&roots, e, allow_downgrade)? {
            InstallAction::Same => {
                log!("= {} {} already installed", e.name, e.version);
                continue;
            }
            InstallAction::KeepNewer(old) => {
                log!(
                    "= {} {old} installed, newer than {} — not downgrading (pass --allow-downgrade)",
                    e.name, e.version
                );
                continue;
            }
            InstallAction::Install => {}
        }
        if let Some(old) = &installed {
            let mark = if Version::parse(&old.version) < e.version { "^" } else { "v" };
            if dry_run {
                log!(
                    "{mark} {} {} -> {}: {} old files would be removed first",
                    e.name,
                    old.version,
                    e.version,
                    old.files.len()
                );
            } else {
                log!("{mark} {} {} -> {}: removing the old version first", e.name, old.version, e.version);
                uninstall_one(&store, &e.name, &mut hook_conn)?;
            }
        }
        logp!("{} {} {} ... ", if dry_run { "?" } else { "+" }, e.name, e.version);
        std::io::stdout().flush().ok();

        let bytes = match archives.remove(&e.name) {
            Some(bytes) => bytes,
            None => fetch::package(&package_cache, &client, e)?,
        };

        let mut zip = install::open_archive(bytes)?;
        let spec = spec::parse(&install::read_spec(&mut zip)?)?;
        let p = install::plan(&roots, &spec, &mut zip)?;

        total_writes += p.writes.len();
        linkfix_files.extend(p.writes.iter().filter(|w| w.dest.starts_with(&roots.application)).map(|w| w.dest.clone()));

        if dry_run {
            log!("{} files would be written", p.writes.len());
            for w in p.writes.iter().take(6) {
                log!("      {}{}", w.dest.display(), if w.overwrites { "   (overwrites)" } else { "" });
            }
            if p.writes.len() > 6 {
                log!("      ... and {} more", p.writes.len() - 6);
            }
            relink_work.push((e.name.clone(), relink::folders_for_spec(&roots, &spec)?));
            if spec.script_vis.iter().any(|(h, v)| h == "PostInstall" && !v.is_empty()) {
                planned_post.push(e.name.clone());
            }
        } else {
            let pre = match spec.script_vis.iter().any(|(h, v)| h == "PreInstall" && !v.is_empty())
            {
                true => install::extract_hook(&roots, &e.name, &mut zip, "PreInstall.vi")?,
                false => None,
            };
            // PreInstall runs before the files land, so it needs a LabVIEW
            // that is already up: the target's own. A venv's instance is
            // started after the copy (so it sees the new addon), which is too
            // late for a PreInstall — those stay reported, not run.
            if policy.allows(&e.name)
                && venv.is_none()
                && let (Some(vi), Some(t)) = (&pre, &roots.target)
            {
                let planned: Vec<String> =
                    p.writes.iter().map(|w| w.dest.to_string_lossy().into_owned()).collect();
                let info =
                    hook_action_info(&e.name, e.display_name.as_deref(), t, &planned);
                // A LabVIEW that will not come up is a failed hook, never a
                // failed install: the files still go down either way.
                match viserver::ensure_vi_server(t, std::time::Duration::from_secs(120))
                    .and_then(|port| {
                        run_hook_vi(&mut hook_conn, port, relink_args.timeout, vi, &info)
                    }) {
                    Ok(took) => logp!("[pre-install ok, {:.1}s] ", took.as_secs_f64()),
                    Err(err) => logp!("[pre-install FAILED: {err:#}] "),
                }
                std::io::stdout().flush().ok();
            }
            let m = install::apply(
                &roots,
                &spec,
                zip,
                &p,
                pre.as_deref(),
                dependency::package_dependency_names(e, spec.requires.as_deref()),
            )?;
            log!("{} files", m.files.len());
            relink_work.push((e.name.clone(), m.relink_folders.iter().map(PathBuf::from).collect()));
            // The record now holds PostInstall as pending; the hook step
            // after the copy settles it. When the policy holds this package's
            // hooks back, the others did not run either, and `lvpm list`
            // should say so. (In a venv the record already lists them.)
            if !policy.allows(&e.name) && venv.is_none() {
                for (h, v) in spec.script_vis.iter().filter(|(h, _)| h != "PostInstall") {
                    install::mark_hook_skipped(&roots, &e.name, &format!("{h}={v}"))?;
                }
            }
        }

        if !p.kept.is_empty() {
            log!("      {} existing file(s) left alone (Replace Mode = If Newer)", p.kept.len());
        }
        for (dir, n) in &p.skipped_groups {
            log!("      {n} file(s) for {dir} skipped — a venv holds LabVIEW-tree content only");
        }
        for miss in &p.missing_from_archive {
            wlog!("      ! listed in spec but absent from archive: {miss}");
        }
        // Warn only about hooks this install owes and will not run. Allowed
        // globally, PreInstall has run, PostInstall runs after the relink
        // pass, and the uninstall hooks are extracted for `lvpm uninstall` —
        // nothing to say. A package the policy holds back runs none, so every
        // declared one is reported. Hooks a venv can never run (PreInstall)
        // are not: there is nothing to do about them.
        let skipped: Vec<String> = spec
            .script_vis
            .iter()
            .filter(|(_, _)| !policy.allows(&e.name))
            .map(|(h, v)| format!("{h}={v}"))
            .collect();
        if !skipped.is_empty() {
            log!("      hooks not run: {}", hook_names(&skipped));
            hooks_unrun = true;
        }
    }
    if !dry_run && venv.is_some() {
        let changed = palette::reconcile(&store).context("reconciling venv palettes after install")?;
        note_palette_change(&dest, changed);
    }
    if hooks_unrun && hook_mode == HookMode::Default {
        log!(
            "\nhooks: {}. Most install hooks add palettes, menus or markers, open a web page, or change",
            if venv.is_some() { "a venv runs none by default" } else { "skipped where shown" }
        );
        log!("      the LabVIEW installation; enable one package's with hooks = \"run\" in its {} entry.", project::FILE_NAME);
    }

    if dry_run {
        log!("\ndry run: {total_writes} files would be written, nothing changed");
    } else {
        log!("\ndone. {total_writes} files written in {}.", took(copy_started.elapsed()));
    }

    if let Some(c) = hook_conn.take() {
        c.close();
    }

    // The hook step works from the records, not from what this run copied:
    // every package in the target whose PostInstall is still owed — including
    // one an earlier, interrupted install left behind, and one an earlier
    // policy skipped that is now allowed — runs, in plan order. A dry run
    // adds the packages it would install.
    let mut unknown_hooks = Vec::new();
    let mut owed = Vec::new();
    // Lenient: a record this sweep cannot read is some other package's, and
    // stopping here would leave what was just copied without linkfix, its
    // manifest entry and its hooks.
    let (records, unreadable) = install::list_installed_lenient(&store)?;
    // The error names the file already.
    for (_, e) in &unreadable {
        wlog!(
            "\nwarning: {e:#}\n      \
             whether that package's post-install hook is owed is unknown; uninstall it with the lvpm that installed it"
        );
    }
    for m in records {
        match (&m.post_install_vi, &m.post_install) {
            (Some(vi), Some(install::HookState::Pending | install::HookState::Skipped { .. })) => {
                owed.push((m.name, PathBuf::from(vi), m.files))
            }
            (Some(_), None) => unknown_hooks.push(m.name),
            _ => {}
        }
    }
    for name in planned_post {
        if !owed.iter().any(|(n, _, _)| n.eq_ignore_ascii_case(&name)) {
            owed.push((name, PathBuf::from("PostInstall.vi"), Vec::new()));
        }
    }
    let rank = |n: &str| plan.iter().position(|e| e.name.eq_ignore_ascii_case(n)).unwrap_or(usize::MAX);
    owed.sort_by(|a, b| rank(&a.0).cmp(&rank(&b.0)).then_with(|| a.0.cmp(&b.0)));
    let (allowed, held): (Vec<_>, Vec<_>) = owed.into_iter().partition(|(n, _, _)| policy.allows(n));
    hook_runs = allowed;
    if !held.is_empty() {
        // Recorded as skipped, so `lvpm list` stops calling them owed; they
        // run once the policy allows them. `--no-hooks` defers instead.
        if !dry_run && hook_mode != HookMode::Never {
            for (n, _, _) in &held {
                if let Err(e) = install::set_post_install(&store, n, install::HookState::Skipped { at: install::now_secs() }) {
                    log!("      could not record that {n}'s hook was skipped: {e:#}");
                }
            }
        }
        let mut by_reason: Vec<(&str, Vec<&str>)> = Vec::new();
        for (n, _, _) in &held {
            let why = policy.why_not(n);
            match by_reason.iter_mut().find(|(w, _)| *w == why) {
                Some((_, names)) => names.push(n),
                None => by_reason.push((why, vec![n])),
            }
        }
        // A count on screen; which packages is `lvpm list`'s job, and the log's.
        for (why, names) in by_reason {
            log!(
                "\n{} post-install hook(s) {} ({why}); `lvpm list` shows which",
                names.len(),
                if dry_run { "would not run" } else { "not run" },
            );
            log::info!("post-install hooks held back ({why}): {}", names.join(", "));
        }
    }

    if !linkfix_files.is_empty() {
        if no_linkfix {
            log!("\nlinkfix: skipped (--no-linkfix); files left as copied");
        } else if relink_pass {
            log!("\nlinkfix: skipped (--relink selects LabVIEW instead)");
        } else if dry_run {
            log!("\nlinkfix: would prepare newly written LabVIEW-tree files (repair links and separate compiled code; no LabVIEW needed)");
        } else {
            let roots = match &venv {
                Some(v) => linkfix::venv_roots(&v.dir),
                None => Ok(vec![store.application.clone()]),
            };
            let bitness = venv.as_ref().map(|v| v.target.bitness).or(store.target.as_ref().map(|t| t.bitness));
            log!("\nlinkfix: preparing installed files (repair links and separate compiled code; no LabVIEW needed)");
            let started = std::time::Instant::now();
            match roots.and_then(|roots| {
                linkfix::fix_files(&roots, &linkfix_files, false, linkfix::Methods::of(&linkfix::Method::ALL, bitness))
            }) {
                Ok((_, outcomes)) => {
                    report_linkfix(&outcomes, false, false, &linkfix::Method::ALL, None, started.elapsed())?
                }
                Err(error) => log!("  linkfix could not complete: {error:#}; package files remain installed. Retry with `lvpm linkfix --tree <DIR> --verbose`."),
            }
        }
    }

    // The files are installed: a package named on a venv goes into its
    // manifest now, before LabVIEW gets involved, so a relink or hook that
    // fails later does not leave the venv holding an undeclared package.
    match (&save_entry, &venv) {
        (Some(SaveEntry::Save { name, version }), Some(v)) => {
            save_to_manifest(v, save.group.as_deref(), name, version, dry_run)
        }
        // The manifest never names a version the venv does not hold.
        (Some(SaveEntry::Kept { name, installed, resolved }), Some(v)) => log!(
            "\n{} not changed: {name} {installed} stays installed (`--allow-downgrade` installs {resolved})",
            v.manifest_path().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_else(|| "the manifest".into())
        ),
        _ => {}
    }

    // Relinking and the PostInstall hooks are the second phase, and both need
    // a LabVIEW: a scratch tree has none, so both are skipped there. They
    // share one instance, because starting a venv's LabVIEW twice costs more
    // than the whole relink pass it would be repeating.
    //
    // `--relink` relinks every package instead of linkfix. Without it, only
    // the packages the manifest marks `relink = true` are relinked, after
    // linkfix: those whose VIs need LabVIEW itself to re-resolve them, which
    // linkfix cannot (.NET invoke nodes storing a member index from another
    // build of the assembly, as sklein_lib_webview2 2.1.1.2 ships them).
    if !relink_pass {
        let marked: Vec<&str> = found
            .as_ref()
            .map(|(_, p)| p.relink.iter().filter(|(_, on)| *on).map(|(n, _)| n.as_str()).collect())
            .unwrap_or_default();
        relink_work.retain(|(pkg, _)| marked.iter().any(|m| m.eq_ignore_ascii_case(pkg)));
        if !relink_work.is_empty() {
            log!(
                "\nrelink: {} (relink = true in the manifest)",
                relink_work.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>().join(", ")
            );
        }
    }
    let folders: usize = relink_work.iter().map(|(_, f)| f.len()).sum();
    let target = store.target.clone();
    let want_relink = folders > 0;

    if dry_run {
        if want_relink {
            log!("\nrelink: {folders} folder(s) across {} package(s):", relink_work.len());
            for (pkg, dirs) in &relink_work {
                for d in dirs {
                    log!("      {pkg}: {}", d.display());
                }
            }
        }
        if !hook_runs.is_empty() {
            log!("\npost-install hooks that would run:");
            for (pkg, vi, _) in &hook_runs {
                log!("  {pkg}: {}", vi.display());
            }
        }
    } else if !want_relink && hook_runs.is_empty() {
        // Nothing to do in LabVIEW — do not start one.
    } else if let Some(t) = &target {
        match &venv {
            // A LabVIEW of the venv's own, started after the files landed so
            // that it sees them, and closed again unless asked to stay.
            Some(v) => {
                // The files are in place either way: a LabVIEW that does not
                // come up only fails the relink, if one was asked for. The
                // hooks stay pending for the next install or `run-hooks`.
                let inst = match launch::ensure_instance(v, true, std::time::Duration::from_secs(300)) {
                    Ok(inst) => Some(inst),
                    // Not a return: the notes and the shadowing warning
                    // below are owed on this path too.
                    Err(e) if !want_relink => {
                        wlog!(
                            "\npost-install hooks not run: {e:#}\n      they stay pending; \
                             `lvpm run-hooks --all` runs them once LabVIEW is up"
                        );
                        None
                    }
                    Err(e) => return Err(e),
                };
                if let Some(inst) = inst {
                    let outcome = match want_relink {
                        true => run_relink(&store, inst.port, relink_args, &relink_work),
                        false => Ok(()),
                    };
                    if !hook_runs.is_empty() {
                        run_post_install_hooks(&store, t, inst.port, relink_args, &hook_runs);
                    }
                    if relink_args.keep_open {
                        log!("\nleaving the venv LabVIEW open (--keep-open), VI Server on port {}", inst.port);
                    } else {
                        inst.shutdown_if_spawned();
                    }
                    outcome?;
                }
            }
            None => {
                let port = viserver::ensure_vi_server(t, std::time::Duration::from_secs(120))?;
                let outcome = match want_relink {
                    true => run_relink(&store, port, relink_args, &relink_work),
                    false => Ok(()),
                };
                if !hook_runs.is_empty() {
                    run_post_install_hooks(&store, t, port, relink_args, &hook_runs);
                }
                outcome?;
            }
        }
    } else {
        log!("\nrelink and post-install hooks: skipped — a scratch tree has no LabVIEW");
    }
    if !unknown_hooks.is_empty() {
        log!(
            "\nnote: {} package(s) were installed by an older lvpm, which did not record whether their\n      \
             post-install hook ran: {}\n      `lvpm run-hooks --all` runs them and records the result",
            unknown_hooks.len(),
            unknown_hooks.join(", ")
        );
    }

    // Palettes and menus last, after the hooks: a hook is free to write more
    // palette files of its own, and this has to see what it wrote. Not for a venv: its LabVIEW
    // is started fresh on launch and builds them from the overlay then.
    if total_writes > 0 && !dry_run && venv.is_none() {
        match &target {
            // A headless LabVIEW (containers, CI) shows no palette to anyone,
            // and rebuilding one would start LabVIEW for nothing when the
            // install had no hooks to run.
            Some(_) if headless() => {
                log!("\npalettes and menus: not refreshed — LabVIEW runs headless here (`lvpm refresh` if wanted)");
            }
            Some(t) => refresh_palettes_and_menus(t, HOOK_TIMEOUT_SECS),
            // Nothing to refresh: a scratch tree's palettes belong to no IDE.
            None => {}
        }
    }
    // What this install put in the venv that the LabVIEW installation has a
    // copy of too: that copy is the one LabVIEW would load.
    if let Some(v) = &venv
        && !dry_run
    {
        let records: Vec<install::Manifest> =
            plan.iter().filter_map(|e| install::read_manifest(&store, &e.name).ok()).collect();
        shadow::warn(v, &shadow::find(v, &records));
    }
    if total_writes > 0 && !dry_run && venv.is_some() {
        log!("\nnext: `lvpm launch` starts LabVIEW on this venv — a LabVIEW already open on it");
        log!("      has to be restarted to see what was just installed");
    }

    Ok(())
}

/// Drive `Relink Package.vi` over every folder, one package at a time.
///
/// One LabVIEW session for the whole run: the relink VI is loaded once, and
/// each folder is a fresh call into it. A folder that fails is reported and the
/// rest still run — the alternative is that one bad package leaves the others
/// copied but unlinked, which is the state this pass exists to get out of.
/// Re-run PostInstall hooks for already-installed packages. The hook VI was
/// extracted at install time; the package's files are not touched.
fn cmd_run_hooks(cli: &Cli, package: Option<&str>, all: bool, timeout: u64) -> Result<()> {
    // Venv-aware the way the `vi-*` primitives are: a venv's hooks run in the
    // venv's own LabVIEW, against the venv paths its manifest recorded.
    let dest = destination(cli)?;
    let roots = dest.store();
    let Some(t) = roots.target.clone() else {
        bail!("run-hooks needs a real LabVIEW target, not --prefix");
    };
    let venv = match &dest {
        Destination::Venv(v) => Some(v.clone()),
        _ => None,
    };

    let manifests = if all {
        install::list_installed(&roots)?
    } else {
        let Some(p) = package else { bail!("give a package name, or --all") };
        vec![install::read_manifest(&roots, p)?]
    };

    let mut work: Vec<install::Manifest> =
        manifests.into_iter().filter(|m| m.post_install_vi.is_some()).collect();
    // `--all` follows the hook policy; a package named here is run as asked.
    if all {
        let policy = hook_policy_for(cli, &dest, HookMode::Default)?;
        let (allowed, held): (Vec<_>, Vec<_>) = work.into_iter().partition(|m| policy.allows(&m.name));
        if !held.is_empty() {
            log!(
                "not running {} hook(s) the policy holds back ({}); `lvpm list` shows which\n      \
                 `lvpm run-hooks <package>` runs one anyway",
                held.len(),
                policy.why_not(&held[0].name),
            );
            log::info!(
                "hooks held back: {}",
                held.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", ")
            );
        }
        work = allowed;
    }
    if work.is_empty() {
        log!("nothing to do: no package with an extracted PostInstall hook to run");
        return Ok(());
    }

    let args = RelinkArgs { timeout, progress: None, keep_open: false };
    // The venv's own instance, started if it is not up and left running — it
    // is the user's to close, the way `vi-probe` leaves it.
    let instance = match &venv {
        Some(v) => Some(launch::ensure_instance(v, false, std::time::Duration::from_secs(180))?),
        None => None,
    };
    let port = match &instance {
        Some(i) => i.port,
        None => viserver::ensure_vi_server(&t, std::time::Duration::from_secs(120))?,
    };
    let mut conn: Option<viserver::Connection> = None;
    let mut failed = 0usize;

    for m in &work {
        let vi = PathBuf::from(m.post_install_vi.as_ref().unwrap());
        if !vi.is_file() {
            log!("{}: extracted hook is gone ({}) — reinstall the package", m.name, vi.display());
            failed += 1;
            continue;
        }

        logp!("{} ... ", m.name);
        std::io::stdout().flush().ok();
        let info = hook_action_info(&m.name, m.display_name.as_deref(), &t, &m.files);
        let state = match run_hook_vi(&mut conn, port, args.timeout, &vi, &info) {
            Ok(took) => {
                log!("ok ({:.1}s)", took.as_secs_f64());
                install::HookState::Ran { at: install::now_secs() }
            }
            Err(e) => {
                wlog!("FAILED: {e:#}");
                failed += 1;
                install::HookState::Failed { at: install::now_secs(), error: format!("{e:#}") }
            }
        };
        install::set_post_install(&roots, &m.name, state)?;
    }
    if let Some(c) = conn {
        c.close();
    }
    // A hook that writes palette files needs the same refresh an install
    // gives it, otherwise re-running one by hand fixes nothing visible.
    // `refresh::run` drives the target's own LabVIEW, so in a venv it would
    // refresh the wrong instance: a venv rebuilds its palettes at launch.
    match &instance {
        Some(i) => log!(
            "\npalettes: not refreshed — the venv's LabVIEW (port {}) builds them at launch;\n      \
             restart it with `lvpm launch` to pick up anything a hook dropped",
            i.port
        ),
        None => refresh_palettes_and_menus(&t, args.timeout),
    }
    if failed > 0 {
        bail!("{failed} hook(s) failed");
    }
    Ok(())
}

/// Run each package's extracted `PostInstall.vi`, one at a time, and report.
///
/// The run is open, run to completion, release. A failure is printed but does
/// not fail the install — the package's files are already in place. Either
/// way the outcome goes into the package's record, for `lvpm list` and so the
/// next install knows what is still owed.
fn run_post_install_hooks(
    roots: &Roots,
    target: &target::LvTarget,
    port: u16,
    args: &RelinkArgs,
    hooks: &[(String, PathBuf, Vec<String>)],
) {
    log!("
running {} post-install hook(s)", hooks.len());
    let mut conn: Option<viserver::Connection> = None;
    for (pkg, vi, files) in hooks {
        logp!("  {pkg} ... ");
        std::io::stdout().flush().ok();
        let info = hook_action_info(pkg, None, target, files);
        let state = match run_hook_vi(&mut conn, port, args.timeout, vi, &info) {
            Ok(took) => {
                log!("ok ({:.1}s)", took.as_secs_f64());
                install::HookState::Ran { at: install::now_secs() }
            }
            Err(e) => {
                wlog!("FAILED: {e:#}");
                install::HookState::Failed { at: install::now_secs(), error: format!("{e:#}") }
            }
        };
        if let Err(e) = install::set_post_install(roots, pkg, state) {
            log!("      could not record the result: {e:#}");
        }
    }
    if let Some(c) = conn {
        c.close();
    }
}

/// Rebuild the palettes and the File/Tools/Help menus from what is now on
/// disk — the last step of an install, after the post-install hooks.
///
/// Without it a package's palette and Tools entries stay invisible until
/// LabVIEW is restarted, however well the files were copied and relinked. See
/// [`refresh`] for why it runs LabVIEW's own VIs rather than the two
/// Application methods directly.
///
/// Best-effort, like the hooks: the files are installed either way, and a
/// refresh that fails costs the user a restart, not the package.
fn refresh_palettes_and_menus(target: &target::LvTarget, timeout_secs: u64) {
    log!("\nrefreshing palettes and menus");
    let outcomes = match refresh::run(target, std::time::Duration::from_secs(timeout_secs)) {
        Ok(o) => o,
        Err(e) => {
            wlog!("  SKIPPED: {e:#}");
            log!("  the files are installed; `lvpm refresh` retries just this step");
            return;
        }
    };
    for o in &outcomes {
        match &o.result {
            Ok(took) => log!("  {:<9} ok ({:.1}s)", o.what, took.as_secs_f64()),
            // A restart does what the refresh would have: say so, since
            // otherwise the package looks broken rather than merely unlisted.
            Err(e) => wlog!("  {:<9} FAILED: {e:#}\n      restart LabVIEW to pick these up", o.what),
        }
    }
}

/// The attribute names are the ones hook VIs read back with Get Variant Attribute
/// (observed in the DQMH hooks); `Quiet Mode` is the one that matters — FALSE
/// is what turns a hook error into a modal dialog parked over the install.
fn hook_action_info(
    package: &str,
    display_name: Option<&str>,
    target: &target::LvTarget,
    files: &[String],
) -> viserver::LvValue {
    use viserver::LvValue;
    // Manifests store forward slashes; a Windows LabVIEW wants its own.
    let native = |f: &String| if cfg!(windows) { f.replace('/', "\\") } else { f.clone() };
    let paths: Vec<LvValue> = files.iter().map(|f| LvValue::Path(native(f))).collect();
    let files_installed = LvValue::array(paths)
        .unwrap_or_else(|_| LvValue::empty_array(viserver::TD_PATH));
    LvValue::Variant {
        value: Box::new(LvValue::Str(String::new())),
        attrs: vec![
            ("Package Name".into(), LvValue::Str(package.into())),
            (
                "Package Display Name".into(),
                LvValue::Str(display_name.unwrap_or(package).into()),
            ),
            ("LabVIEW Target Version".into(), LvValue::Str(format!("{}", target.version))),
            ("VIPM Version".into(), LvValue::Str(format!("lvpm {}", env!("CARGO_PKG_VERSION")))),
            ("Quiet Mode".into(), LvValue::Bool(true)),
            ("Mass Compile On".into(), LvValue::Bool(false)),
            ("Files Installed".into(), files_installed),
            ("Folders Created".into(), LvValue::empty_array(viserver::TD_PATH)),
        ],
    }
}

/// Run one hook VI to completion over a lazily opened, shared connection.
///
/// Takes a port rather than a target so a venv's own LabVIEW can run the hook:
/// `ni_lib_advanced_http_client_api` rewrites its Call Library paths to the
/// right DLL bitness, and without that its VIs are broken (error 1003) wherever
/// they were installed.
fn run_hook_vi(
    conn: &mut Option<viserver::Connection>,
    port: u16,
    timeout_secs: u64,
    vi: &Path,
    action_info: &viserver::LvValue,
) -> Result<std::time::Duration> {
    let timeout = std::time::Duration::from_secs(timeout_secs);
    if conn.is_none() {
        *conn = Some(viserver::Connection::connect("127.0.0.1", port, timeout)?);
    }
    let c = conn.as_mut().unwrap();
    // Timed from the open: loading the hook's hierarchy is most of what a hook
    // costs (a DQMH pre-install reads as 0.0s otherwise), and it is work the
    // hook caused, not the connection.
    let t0 = std::time::Instant::now();
    let r = c.open_vi_reference(vi)?;
    // Best-effort: the standard hook template has this control, but a hook is
    // free not to — running it matters more than parameterising it.
    let _ = c.ctrl_val_set(r, "Variant", action_info.clone());
    let run = c.run_vi(r);
    let _ = c.release(r);
    run.map(|()| t0.elapsed())
}

/// Drive `Relink Package.vi` over every folder through the LabVIEW answering
/// VI Server on `port` — the target's own, or one started on a venv. Making it
/// answer is the caller's job; this only talks to it.
fn run_relink(
    roots: &Roots,
    port: u16,
    args: &RelinkArgs,
    work: &[(String, Vec<PathBuf>)],
) -> Result<()> {
    let vi = relink::locate_vi()?;
    // One walk covers every folder nested under it, so overlapping packages
    // share a run instead of relinking the same tree twice — 18 of the first
    // pass's 109 folders were nested repeats costing 17 of its 61 minutes.
    let asked: usize = work.iter().map(|(_, f)| f.len()).sum();
    let plan = relink::collapse_work(work);

    logp!("\nrelinking {} folder(s) with {}", plan.len(), vi.display());
    if plan.len() < asked {
        logp!("  ({} covered by a parent)", asked - plan.len());
    }
    log!();
    let mut r = relink::Relinker::open(port, &vi, std::time::Duration::from_secs(args.timeout))
        .with_context(|| {
            format!(
                "cannot reach LabVIEW to relink on VI Server port {port}\n\
                 the files are installed; `lvpm relink <package>` retries just this pass"
            )
        })?;
    r.watch(args.progress.as_deref());

    let mut failed: Vec<String> = Vec::new();
    for (d, pkgs) in &plan {
        logp!("  {}\n      {} ... ", pkgs.join(", "), d.display());
        std::io::stdout().flush().ok();
        match r.run(d) {
            Ok(o) => {
                log!("{:.1}s", o.took.as_secs_f64());
                // The VI's own account of what it saved, as a count and
                // folder-relative names — see `relink::summarize_log`.
                for line in relink::summarize_log(&o.log, d) {
                    log!("        {line}");
                }
            }
            Err(e) => {
                wlog!("FAILED");
                log!("      {e:#}");
                // The folder may have been walking for several packages.
                failed.extend(pkgs.iter().cloned());
            }
        }
    }
    for (pkg, _) in work {
        if !failed.iter().any(|f| f == pkg) {
            install::mark_relinked(roots, pkg)?;
        }
    }
    r.close();

    if !failed.is_empty() {
        failed.sort();
        failed.dedup();
        bail!(
            "relink failed for: {}\nthe files are installed; fix the cause and run \
             `lvpm relink <package>`",
            failed.join(", ")
        );
    }
    Ok(())
}

fn cmd_relink(cli: &Cli, package: Option<&str>, all: bool, args: &RelinkArgs) -> Result<()> {
    let dest = destination(cli)?;
    let roots = dest.store();
    let Some(t) = roots.target.clone() else {
        bail!("relink needs a real LabVIEW target, not --prefix");
    };
    let manifests = match (package, all) {
        (Some(p), _) => vec![install::read_manifest(&roots, p)?],
        // Same one pass `install` runs, over everything already installed:
        // one VI reference, every folder, no package relinked before another
        // package's files are on disk.
        (None, true) => install::list_installed(&roots)?,
        (None, false) => bail!("give a package name, or --all"),
    };
    ensure!(!manifests.is_empty(), "no packages are installed in this target");

    let mut work: Vec<(String, Vec<PathBuf>)> = Vec::new();
    for m in manifests {
        // Manifests from before the folders were recorded fall back to the
        // per-file approximation, which is all they can support.
        let folders: Vec<PathBuf> = if m.relink_folders.is_empty() {
            relink::folders_for(&m.files)
        } else {
            m.relink_folders.iter().map(PathBuf::from).collect()
        };
        if folders.is_empty() {
            log!(
                "{} {} installed no VIs, libraries or LLBs — nothing to relink",
                m.name, m.version
            );
            continue;
        }
        work.push((m.name, folders));
    }
    if work.is_empty() {
        return Ok(());
    }
    match &dest {
        // Fresh, as on install: a LabVIEW that predates the files on disk
        // cannot see them, and relinking against what it does see is wrong.
        Destination::Venv(v) => {
            let inst = launch::ensure_instance(v, true, std::time::Duration::from_secs(180))?;
            let outcome = run_relink(&roots, inst.port, args, &work);
            if args.keep_open {
                log!("\nleaving the venv LabVIEW open (--keep-open), VI Server on port {}", inst.port);
            } else {
                inst.shutdown_if_spawned();
            }
            outcome
        }
        _ => {
            let port = viserver::ensure_vi_server(&t, std::time::Duration::from_secs(120))?;
            run_relink(&roots, port, args, &work)
        }
    }
}

fn cmd_venv(cli: &Cli, sub: &VenvCmd) -> Result<()> {
    let cwd = std::env::current_dir()?;
    match sub {
        VenvCmd::Create { manifest } => {
            let start = cli.project.clone().unwrap_or(cwd);
            let explicit = match manifest {
                Some(p) => Some((std::path::absolute(p)?, read_manifest(p)?)),
                None => None,
            };
            let install_hint = match &explicit {
                Some((p, _)) => format!("`lvpm install --manifest {}`", p.display()),
                None => format!("`lvpm install` puts {}'s [dependencies]", project::FILE_NAME),
            };
            let v = venv::create(&start, cli.labview_version.as_deref(), explicit)?;
            log!("created {}", v.dir.display());
            log!("  bound to       {}  ({})", v.target.label(), v.target.path.display());
            log!("  VI Server port {}", v.port);
            log!("  log            {}", v.log_path().display());
            log!("\nnext: {install_hint} into it, `lvpm launch` starts LabVIEW on it");
            Ok(())
        }
        // A project with no venv yet is an answer here, not an error: the
        // refusal `find` gives is for commands that would install somewhere.
        VenvCmd::Status if cli.project.is_none() && matches!(venv::probe(&cwd)?, venv::Found::ManifestOnly(_)) => {
            log!("no venv for the project at or above {} yet — `lvpm venv create` makes one", cwd.display());
            Ok(())
        }
        VenvCmd::Status => match venv::find(cli.project.as_deref(), &cwd)? {
            Some(v) => {
                let installed = install::list_installed(&v.store())?;
                log!("{}", v.dir.display());
                log!("  project        {}", v.repo.display());
                log!("  bound to       {}  ({})", v.target.label(), v.target.path.display());
                log!("  VI Server port {}", v.port);
                log!(
                    "  packages       {} installed, {} not yet relinked",
                    installed.len(),
                    installed.iter().filter(|m| !m.relinked).count()
                );
                log!(
                    "  LabVIEW        {}",
                    if launch::is_listening(v.port) {
                        "running on this venv"
                    } else if launch::is_running(&v) {
                        "starting on this venv"
                    } else {
                        "not running on this venv"
                    }
                );
                log!("  log            {}", v.log_path().display());
                let shadowed = shadow::find(&v, &installed);
                match shadowed.is_empty() {
                    true => log!("  shadowed       none: LabVIEW has no copy of its own of any of them"),
                    false => log!("  shadowed       {} also in LabVIEW {} (below)", shadowed.len(), v.target.year()),
                }
                shadow::warn(&v, &shadowed);
                Ok(())
            }
            None => {
                log!("no venv at or above {} — `lvpm venv create` makes one", cwd.display());
                Ok(())
            }
        },
        VenvCmd::Remove { yes } => {
            let Some(v) = venv::find_for_removal(cli.project.as_deref(), &cwd)? else {
                bail!("no venv at or above {}", cwd.display());
            };
            venv::remove(&v, *yes)
        }
        VenvCmd::Shortcut { lvproj, desktop, out } => {
            let Some(v) = venv::find(cli.project.as_deref(), &cwd)? else {
                bail!("no venv at or above {} — `lvpm venv create` makes one", cwd.display());
            };
            let lvproj = match lvproj {
                Some(p) => {
                    let p = std::path::absolute(p)?;
                    ensure!(p.is_file(), "no such project file: {}", p.display());
                    Some(p)
                }
                None => None,
            };
            let name = shortcut::file_name(&v, lvproj.as_deref());
            let lnk = match (out, desktop) {
                (Some(p), _) if p.is_dir() => std::path::absolute(p)?.join(&name),
                (Some(p), _) => std::path::absolute(p)?.with_extension("lnk"),
                (None, true) => shortcut::desktop()?.join(&name),
                (None, false) => v.repo.join(&name),
            };
            shortcut::create(&v, lvproj.as_deref(), &lnk)?;
            log!("wrote {}", lnk.display());
            log!("  runs           lvpm {}", shortcut::arguments(&v, lvproj.as_deref()));
            Ok(())
        }
    }
}

/// Start LabVIEW on the venv. No relinking here — that happened at install —
/// and no waiting: the IDE is the user's from the moment it appears.
/// Refuse to start LabVIEW on a venv that does not hold what the manifest's
/// `[dependencies]` ask for. Reads only the install records: a file edited or
/// resaved since install goes unnoticed (that needs hashes at install time).
fn check_venv_for_launch(cli: &Cli, dest: &Destination, v: &venv::Venv) -> Result<()> {
    let Some(path) = v.manifest_path().filter(|p| p.is_file()) else {
        elog!("venv has no bound manifest — skipping the package check");
        return Ok(());
    };
    let proj = read_manifest(&path)?;
    // `main` only: which optional groups the venv was installed with is not
    // recorded, so their absence cannot be told from a choice.
    let wanted = proj.select(&[], &[], &[])?.dependencies;
    let store = v.store();
    let policy = hook_policy_for(cli, dest, HookMode::Default)?;
    let mut problems = venv_problems(
        &wanted,
        &proj.relink,
        |name| policy.allows(name),
        |name| match install::is_installed(&store, name) {
            true => install::read_manifest(&store, name).map(Some),
            false => Ok(None),
        },
        |p| p.exists(),
    )?;
    if let Err(error) = palette::verify(&store) {
        problems.push((format!("palettes: {error:#}"), Fix::Install));
    }
    if problems.is_empty() {
        elog!("checked {} package(s) against {}", wanted.len(), path.display());
        return Ok(());
    }
    let mut msg = format!("the venv does not match {}:", path.display());
    for (p, _) in &problems {
        msg.push_str("\n  ");
        msg.push_str(p);
    }
    msg.push('\n');
    msg.push_str(&launch_check_hint(&problems));
    bail!(msg)
}

/// The command that clears a launch-check problem. `lvpm install` only picks
/// up hooks that are `Pending` or `Skipped`, so a failed or unknown hook
/// needs `run-hooks`.
#[derive(Debug, PartialEq, Eq)]
enum Fix {
    Install,
    RunHooks(String),
}

/// The closing line(s) of the launch refusal: only the commands that fix
/// something that was reported, then the way to skip the check.
fn launch_check_hint(problems: &[(String, Fix)]) -> String {
    let mut hooks: Vec<&str> = Vec::new();
    for (_, fix) in problems {
        if let Fix::RunHooks(name) = fix
            && !hooks.contains(&name.as_str())
        {
            hooks.push(name);
        }
    }
    let mut lines = Vec::new();
    if problems.iter().any(|(_, f)| *f == Fix::Install) {
        lines.push("run `lvpm install` to bring it in line".to_string());
    }
    if !hooks.is_empty() {
        let each: Vec<String> = hooks.iter().map(|n| format!("`lvpm run-hooks {n}` reruns {n}'s PostInstall hook")).collect();
        lines.push(each.join(", "));
    }
    lines.push("or `lvpm launch --no-check` to start anyway".to_string());
    lines.join("; ")
}

/// Every way the installed records fall short of `wanted`, one line each,
/// with the command that fixes it.
/// `hooks_run` says whether the hook policy runs a package's hooks here;
/// `record` reads a package's install record, `None` when there is none.
fn venv_problems(
    wanted: &[(String, project::Constraint)],
    relink: &[(String, bool)],
    hooks_run: impl Fn(&str) -> bool,
    record: impl Fn(&str) -> Result<Option<install::Manifest>>,
    exists: impl Fn(&Path) -> bool,
) -> Result<Vec<(String, Fix)>> {
    let mut problems = Vec::new();
    for (name, constraint) in wanted {
        let Some(m) = record(name)? else {
            problems.push((format!("{name}: not installed"), Fix::Install));
            continue;
        };
        if !constraint.allows(&Version::parse(&m.version)) {
            problems.push((format!("{name}: {} installed, manifest asks for {name}{constraint}", m.version), Fix::Install));
        }
        let missing: Vec<&String> = m.files.iter().filter(|f| !exists(Path::new(f.as_str()))).collect();
        if let Some(first) = missing.first() {
            problems.push((format!("{name}: {} of {} files missing, e.g. {first}", missing.len(), m.files.len()), Fix::Install));
        }
        let wants_relink = relink.iter().any(|(n, on)| *on && n.eq_ignore_ascii_case(name));
        if wants_relink && !m.relinked {
            problems.push((format!("{name}: relink = true but the relink pass has not completed"), Fix::Install));
        }
        if m.post_install_vi.is_some() {
            match &m.post_install {
                Some(install::HookState::Failed { error, .. }) => {
                    problems.push((format!("{name}: PostInstall failed: {error}"), Fix::RunHooks(name.clone())))
                }
                Some(install::HookState::Ran { .. }) => {}
                // An install sweep runs these two; a record without a state is from an older lvpm and only run-hooks reaches it.
                Some(install::HookState::Pending | install::HookState::Skipped { .. }) if hooks_run(name) => {
                    problems.push((format!("{name}: PostInstall has not run"), Fix::Install))
                }
                None if hooks_run(name) => problems.push((format!("{name}: PostInstall has not run"), Fix::RunHooks(name.clone()))),
                _ => {}
            }
        }
    }
    Ok(problems)
}

fn cmd_launch(cli: &Cli, lvproj: Option<&Path>, ini_keys: &[String], no_check: bool) -> Result<()> {
    let dest = destination(cli)?;
    let Destination::Venv(v) = &dest else {
        bail!("launch needs a venv: run it inside a project that has one, or pass --project <DIR>");
    };
    let v = v.clone();
    if !no_check {
        check_venv_for_launch(cli, &dest, &v)?;
    }
    // Not a reason to refuse: the copies may be the same version. But the
    // user should know which one LabVIEW is about to load.
    let records = install::list_installed(&v.store()).unwrap_or_default();
    shadow::warn(&v, &shadow::find(&v, &records));
    let extra = ini_keys
        .iter()
        .map(|kv| {
            let (k, val) = kv.split_once('=').with_context(|| format!("--ini {kv}: expected KEY=VALUE"))?;
            ensure!(!k.trim().is_empty(), "--ini {kv}: empty key");
            Ok((k.trim().to_string(), val.trim().to_string()))
        })
        .collect::<Result<Vec<_>>>()?;
    if launch::is_running(&v) {
        bail!(
            "a LabVIEW is already running or starting on this venv (VI Server port {}) — open the \
             project from that window, or close it and launch again",
            v.port
        );
    }
    let lvproj = match lvproj {
        Some(p) => {
            let p = std::path::absolute(p)?;
            ensure!(p.is_file(), "no such project file: {}", p.display());
            Some(p)
        }
        None => None,
    };
    let ini = launch::write_ini(&v, &extra)?;
    let child = launch::spawn(&v, &ini, lvproj.as_deref())?;
    start_title_watcher(&v, child.id());
    log!("started {} on {}", v.target.label(), v.dir.display());
    log!("  ini            {}", ini.display());
    for (k, val) in &extra {
        log!("  ini override   {k}={val}");
    }
    log!("  VI Server port {}", v.port);
    if let Some(p) = &lvproj {
        log!("  opening        {}", p.display());
    }
    Ok(())
}

/// Start the hidden watcher beside the LabVIEW with process id `pid` (see
/// `watcher`): it keeps the venv's name in the window titles, and gives the
/// windows the venv's taskbar group and icon. Silent when it works, since it
/// changes nothing the user has to act on; the log file says what it was
/// given. Never fails the launch: a watcher that could not start is a
/// warning. Windows only.
fn start_title_watcher(v: &venv::Venv, pid: u32) {
    let pos = match config::load() {
        Ok(cfg) => cfg.venv_title,
        Err(e) => {
            wlog!("warning: window titles left as they are: {e:#}");
            return;
        }
    };
    if !cfg!(windows) {
        return;
    }
    let name = v.repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "venv".into());
    let tag = format!("[{name}]");
    // The taskbar group is always on. Its icon is the venv's badged one,
    // LabVIEW's own when that could not be made.
    let icon = venv_icon::write(v).unwrap_or_else(|e| {
        wlog!("warning: taskbar pin gets LabVIEW's plain icon: {e:#}");
        v.target.exe()
    });
    let relaunch = match shortcut::relaunch_command(v) {
        Ok(cmd) => cmd,
        Err(e) => {
            wlog!("warning: no taskbar group for the venv: {e:#}");
            return;
        }
    };
    let app_id = shortcut::app_id(v);
    let started = std::env::current_exe().and_then(|exe| {
        let mut cmd = std::process::Command::new(exe);
        cmd.args(["title-watch", "--pid", &pid.to_string(), "--tag", &tag, "--position", pos.as_str()]);
        cmd.args(["--app-id", &app_id, "--relaunch", &relaunch]);
        cmd.arg("--relaunch-icon").arg(&icon);
        cmd.args(["--relaunch-name", &shortcut::display_name(v)]);
        #[cfg(windows)]
        {
            // lvpm is a console program: without this, its watcher would
            // open a console window of its own for LabVIEW's lifetime.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            std::os::windows::process::CommandExt::creation_flags(&mut cmd, CREATE_NO_WINDOW);
        }
        launch::spawn_detached(&mut cmd)
    });
    match started {
        Ok(_) => log::debug!("window watcher: titles {tag} at {} (venv.title), taskbar group {app_id}", pos.as_str()),
        Err(e) => wlog!("warning: could not start the window title watcher: {e}"),
    }
}

/// Invoke Application methods by id and print each answer.
///
/// Error 1036 is the useful one — *Method selector is invalid*, i.e. this
/// LabVIEW has no such method — which is what makes a sweep tell an id that
/// exists from one that does not. A parameterless call is all it sends, so a
/// method that wants arguments answers with a complaint about those instead,
/// and that too proves the id resolved.
fn cmd_app_probe(cli: &Cli, ids: &[String]) -> Result<()> {
    let roots = roots_for(cli)?;
    let Some(t) = roots.target.clone() else {
        bail!("app-probe needs a real LabVIEW target, not --prefix");
    };
    let port = viserver::check_vi_server(&t)?;
    log!("connecting to {} on port {port}", t.label());
    let mut conn =
        viserver::Connection::connect("127.0.0.1", port, std::time::Duration::from_secs(300))?;

    for raw in ids {
        let s = raw.trim();
        let id = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            Some(hex) => u32::from_str_radix(hex, 16),
            None => s.parse::<u32>(),
        }
        .with_context(|| format!("{raw:?} is not a method id"))?;

        let t0 = std::time::Instant::now();
        let r = conn.invoke_app_id(id);
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        match r {
            Ok(()) => log!("  {id:>6} ({id:#06x})  ok                {ms:>7.0} ms"),
            Err(e) => {
                let code = viserver::error_code(&e);
                match code {
                    Some(c) => log!(
                        "  {id:>6} ({id:#06x})  error {c:<11} {ms:>7.0} ms{}",
                        match c {
                            1036 => "  (no such method here)",
                            1032 => "  (exists, but not remotely accessible)",
                            _ => "",
                        }
                    ),
                    None => log!("  {id:>6} ({id:#06x})  {e:#}"),
                }
            }
        }
    }
    conn.close();
    Ok(())
}

/// Fix linker tables in place — see [`linkfix`]. A venv store is a set of
/// `<package>/1/` roots; anything else is one LabVIEW-shaped root.
fn cmd_linkfix(
    cli: &Cli,
    tree: Option<&Path>,
    dry_run: bool,
    verbose: bool,
    methods: &[linkfix::Method],
    report: Option<&Path>,
    bitness: Option<u8>,
) -> Result<()> {
    let (roots, bitness): (Vec<PathBuf>, Option<u8>) = match tree {
        Some(t) => {
            let store_like = linkfix::venv_roots(t)?;
            (if store_like.is_empty() { vec![t.to_path_buf()] } else { store_like }, bitness)
        }
        None => match destination(cli)? {
            Destination::Venv(v) => (linkfix::venv_roots(&v.dir)?, bitness.or(Some(v.target.bitness))),
            Destination::Global(_) | Destination::Scratch(_) => bail!(
                "give --tree <DIR> for an explicit LabVIEW-shaped tree, or run linkfix inside a venv; \
                 install prepares newly written files automatically"
            ),
        },
    };
    ensure!(!roots.is_empty(), "nothing to fix: no package roots found");
    let names: Vec<String> = methods
        .iter()
        .map(|m| clap::ValueEnum::to_possible_value(m).expect("no skipped variants").get_name().to_string())
        .collect();
    log!(
        "{}fixing under {} root(s), methods: {}",
        if dry_run { "dry run: " } else { "" },
        roots.len(),
        names.join(", ")
    );
    if methods.contains(&linkfix::Method::ClfnWildcard) && bitness.is_none() {
        log!("  clfn-wildcard: skipped, the tree's LabVIEW bitness is unknown (pass --bitness 32|64)");
    }

    let started = std::time::Instant::now();
    let (_tree, outcomes) = linkfix::fix_tree(&roots, dry_run, linkfix::Methods::of(methods, bitness))?;
    report_linkfix(&outcomes, dry_run, verbose, methods, report, started.elapsed())
}

fn report_linkfix(
    outcomes: &[linkfix::Outcome],
    dry_run: bool,
    verbose: bool,
    methods: &[linkfix::Method],
    report: Option<&Path>,
    elapsed: std::time::Duration,
) -> Result<()> {
    let mut total = linkfix::Changes::default();
    let (mut changed, mut clean, mut skipped, mut not_separated) = (0usize, 0usize, 0usize, 0usize);
    let mut files = Vec::new();
    for o in outcomes {
        // A warning: the file keeps its embedded compiled code, and LabVIEW
        // may still ask to save it. Counted on the console, named in the log.
        if let Ok(c) = &o.result
            && let Some(why) = &c.not_separated
        {
            not_separated += 1;
            let line = format!("  NOT SEPARATED {}: {why}", o.path.display());
            match verbose {
                true => wlog!("{line}"),
                false => log::warn!("{line}"),
            }
            if c.total() == 0 {
                files.push(serde_json::json!({ "path": o.path, "methods": c.methods(), "changes": c }));
            }
        }
        match &o.result {
            Ok(c) if c.total() > 0 => {
                files.push(serde_json::json!({ "path": o.path, "methods": c.methods(), "changes": c }));
                changed += 1;
                total.owner += c.owner;
                total.relative += c.relative;
                total.absolute += c.absolute;
                total.qualifier += c.qualifier;
                total.vils += c.vils;
                total.separated += c.separated;
                total.clfn += c.clfn;
                // Each file always goes to the log; to the console on request.
                let line = format!(
                    "  {:<60} owner {} rel {} abs {} qual {} vils {} sep {} clfn {}",
                    o.path.display(),
                    c.owner,
                    c.relative,
                    c.absolute,
                    c.qualifier,
                    c.vils,
                    c.separated,
                    c.clfn
                );
                match verbose {
                    true => log!("{line}"),
                    false => log::debug!("{line}"),
                }
            }
            Ok(_) => clean += 1,
            Err(e) => {
                files.push(serde_json::json!({ "path": o.path, "skipped": format!("{e:#}") }));
                skipped += 1;
                let line = format!("  SKIPPED {}: {e:#}", o.path.display());
                match verbose {
                    true => wlog!("{line}"),
                    false => log::warn!("{line}"),
                }
            }
        }
    }
    if let Some(report) = report {
        let doc = serde_json::json!({
            "methods": methods,
            "dry_run": dry_run,
            "looked_at": outcomes.len(),
            "changed": changed,
            "clean": clean,
            "skipped": skipped,
            "not_separated": not_separated,
            "files": files,
        });
        std::fs::write(report, serde_json::to_string_pretty(&doc)?)
            .with_context(|| format!("writing {}", report.display()))?;
        log!("report: {}", report.display());
    }
    log!(
        "\n{} LabVIEW files looked at in {}: {changed} {}, {clean} already clean, {skipped} skipped",
        outcomes.len(),
        took(elapsed),
        if dry_run { "would change" } else { "changed" }
    );
    log!(
        "  edits: owner paths {}, relative paths {}, build-machine absolutes {}, qualifiers {}, owner refs {}, \
         compiled code separated {}, CLFN wildcards restored {}",
        total.owner, total.relative, total.absolute, total.qualifier, total.vils, total.separated, total.clfn
    );
    if not_separated > 0 {
        log!("  compiled code left embedded in {not_separated} file(s); link repairs were still attempted");
    }
    if (skipped > 0 || not_separated > 0) && !verbose {
        match logging::path() {
            Some(log) => log!("  each file and why: {}", log.display()),
            None => log!("  each file and why: --verbose"),
        }
    }
    Ok(())
}

/// Which LabVIEW the `vi-*` primitives talk to: a venv's own instance when a
/// venv is in effect (started if none is running — and left running, it is the
/// user's to close), else the target's. Returns a label and the port.
fn vi_server_for(cli: &Cli, what: &str) -> Result<(String, u16)> {
    match destination(cli)? {
        Destination::Venv(v) => {
            let inst = launch::ensure_instance(&v, false, std::time::Duration::from_secs(180))?;
            Ok((format!("{} on the venv", v.target.label()), inst.port))
        }
        Destination::Global(r) | Destination::Scratch(r) => {
            let Some(t) = r.target else {
                bail!("{what} needs a real LabVIEW target, not --prefix");
            };
            let port = viserver::check_vi_server(&t)?;
            Ok((t.label(), port))
        }
    }
}

fn cmd_vi_probe(cli: &Cli, vis: &[PathBuf], timeout: u64) -> Result<()> {
    let (label, port) = vi_server_for(cli, "vi-probe")?;
    log!("connecting to {label} on port {port}");

    let started = std::time::Instant::now();
    let mut conn = viserver::Connection::connect(
        "127.0.0.1",
        port,
        std::time::Duration::from_secs(timeout),
    )?;
    log!("  handshake ok            {:>7.0} ms", started.elapsed().as_secs_f64() * 1000.0);

    // Every reference stays open until the last VI is in: a hierarchy the
    // VIs share is then loaded once instead of once per VI.
    let mut held = Vec::new();
    let mut failed = 0usize;
    for vi in vis {
        let name = vi.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let t0 = std::time::Instant::now();
        match conn.open_vi_reference(vi) {
            Ok(vi_ref) => {
                log!("  opened {name} -> {vi_ref}  {:>7.0} ms", t0.elapsed().as_secs_f64() * 1000.0);
                held.push(vi_ref);
            }
            Err(e) => {
                failed += 1;
                wlog!("  FAILED {name}: {e:#}  {:>7.0} ms", t0.elapsed().as_secs_f64() * 1000.0);
                // A transport error leaves the connection unusable; a VI Server
                // error code does not.
                if viserver::error_code(&e).is_none() {
                    bail!("connection lost while opening {}", vi.display());
                }
            }
        }
    }

    for vi_ref in held {
        conn.release(vi_ref)?;
    }
    conn.close();
    log!("  released + closed       {:>7.0} ms total", started.elapsed().as_secs_f64() * 1000.0);
    ensure!(failed == 0, "{failed} of {} VIs did not open", vis.len());
    Ok(())
}

fn cmd_vi_save(cli: &Cli, vi: &std::path::Path) -> Result<()> {
    let (_, port) = vi_server_for(cli, "vi-save")?;
    if !vi.is_file() {
        bail!("no such VI: {}", vi.display());
    }
    let before = std::fs::metadata(vi)?.modified()?;

    let mut conn =
        viserver::Connection::connect("127.0.0.1", port, std::time::Duration::from_secs(300))?;

    let t0 = std::time::Instant::now();
    let vi_ref = conn.open_vi_reference(vi)?;
    let load_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let t1 = std::time::Instant::now();
    let result = conn.save_instrument(vi_ref, vi);
    let save_ms = t1.elapsed().as_secs_f64() * 1000.0;

    conn.release(vi_ref)?;
    conn.close();
    result?;

    // The reply carrying err=0 says the request was accepted; the file's
    // timestamp is what says the save actually happened.
    let after = std::fs::metadata(vi)?.modified()?;
    log!("  load {load_ms:>7.0} ms   save {save_ms:>7.0} ms");
    if after > before {
        log!("  file rewritten — relink persisted");
    } else {
        log!("  file NOT rewritten: LabVIEW accepted the save but had nothing to write");
        log!("  (expected when the VI is already current and its links resolved)");
    }
    Ok(())
}

/// Parse a `--set` argument: `name=type:value` with type bool|i32|dbl|str,
/// or `type[]` for a one-dimensional array of that type, whose value is a
/// comma-separated list (`names=str[]:alpha,beta`). There is no escape for a
/// comma inside an array element; `type[]:` on its own is the empty array.
///
/// The type is explicit because LabVIEW rejects a mismatched variant with
/// error 91 — better to be unambiguous here than to guess whether "125"
/// means an integer or a double.
fn parse_set(arg: &str) -> Result<(String, viserver::LvValue)> {
    use viserver::LvValue;
    let (name, rest) = arg
        .split_once('=')
        .with_context(|| format!("--set {arg:?}: expected name=type:value"))?;
    let (ty, raw) = rest
        .split_once(':')
        .with_context(|| format!("--set {arg:?}: expected type:value with type bool|i32|dbl|str"))?;
    let value = match ty.strip_suffix("[]") {
        // An empty array still has to name its element type, so it comes from
        // the type word rather than from a value we could inspect.
        Some(elem) if raw.is_empty() => LvValue::empty_array(element_code(elem, arg)?),
        Some(elem) => {
            LvValue::array(raw.split(',').map(|v| scalar(elem, v, arg)).collect::<Result<_>>()?)?
        }
        None => scalar(ty, raw, arg)?,
    };
    Ok((name.to_string(), value))
}

/// One scalar of `--set`'s named type. `arg` only ever appears in errors.
fn scalar(ty: &str, raw: &str, arg: &str) -> Result<viserver::LvValue> {
    use viserver::LvValue;
    Ok(match ty {
        "bool" => LvValue::Bool(raw.parse().with_context(|| format!("--set {arg:?}: {raw:?} is not a bool"))?),
        "i32" => LvValue::I32(raw.parse().with_context(|| format!("--set {arg:?}: {raw:?} is not an i32"))?),
        "dbl" => LvValue::Dbl(raw.parse().with_context(|| format!("--set {arg:?}: {raw:?} is not a number"))?),
        "str" => LvValue::Str(raw.to_string()),
        other => bail!("--set {arg:?}: unknown type {other:?} (use bool|i32|dbl|str, or type[])"),
    })
}

/// The type code `--set`'s type word names, without a value to go with it.
fn element_code(ty: &str, arg: &str) -> Result<u16> {
    scalar(ty, "0", arg)
        .or_else(|_| scalar(ty, "false", arg))
        .with_context(|| format!("--set {arg:?}: unknown array element type {ty:?}"))?
        .type_code()
}

/// Poll `watch` on a VI that is already running, printing every change, until
/// `done` reads true or the budget runs out.
///
/// The point of the interval statistics is that LabVIEW runs one VI Server per
/// process and serialises calls against it, so a busy VI could in principle
/// starve the polls. Reporting the gaps says whether that happens rather than
/// leaving it to assumption.
fn poll_progress(
    conn: &mut viserver::Connection,
    vi_ref: viserver::VIRef,
    watch: &str,
    done: &str,
    poll_ms: u64,
    timeout: u64,
) -> Result<()> {
    let started = std::time::Instant::now();
    let budget = std::time::Duration::from_secs(timeout);
    let mut last = String::new();
    let mut gaps: Vec<f64> = Vec::new();
    let mut prev = started;
    let mut done_readable = true;

    loop {
        let value = conn.ctrl_val_get(vi_ref, watch)?;
        let now = std::time::Instant::now();
        gaps.push(now.duration_since(prev).as_secs_f64() * 1000.0);
        prev = now;

        let shown = value.to_string();
        if shown != last {
            log!("  {:>7.0} ms  {watch} = {shown}", started.elapsed().as_secs_f64() * 1000.0);
            last = shown;
        }

        // A running VI answers reads exactly as a finished one does, so the VI
        // has to tell us itself.
        if done_readable {
            match conn.ctrl_val_get(vi_ref, done) {
                Ok(viserver::LvValue::Bool(true)) => {
                    log!("  {:>7.0} ms  {done} = true", started.elapsed().as_secs_f64() * 1000.0);
                    break;
                }
                Ok(viserver::LvValue::Bool(false)) => {}
                Ok(other) => {
                    elog!("  note: {done} is {other}, not a boolean — polling until timeout");
                    done_readable = false;
                }
                Err(e) => {
                    elog!("  note: cannot read {done} ({e}) — polling until timeout");
                    done_readable = false;
                }
            }
        }

        if started.elapsed() >= budget {
            log!("  timeout after {timeout}s with {done} still false");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(poll_ms));
    }

    if !gaps.is_empty() {
        let mut sorted = gaps.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let sum: f64 = gaps.iter().sum();
        log!(
            "  {} polls, interval min/median/max {:.0}/{:.0}/{:.0} ms (asked for {poll_ms})",
            gaps.len(),
            sorted[0],
            sorted[sorted.len() / 2],
            sorted[sorted.len() - 1],
        );
        let _ = sum;
    }
    Ok(())
}

fn cmd_vi_run(
    cli: &Cli,
    vi: &std::path::Path,
    sets: &[String],
    gets: &[String],
    get_all: bool,
    timeout: u64,
    watch: Option<&str>,
    poll_ms: u64,
    done: &str,
) -> Result<()> {
    let (_, port) = vi_server_for(cli, "vi-run")?;
    let sets: Vec<_> = sets.iter().map(|s| parse_set(s)).collect::<Result<_>>()?;

    let mut conn =
        viserver::Connection::connect("127.0.0.1", port, std::time::Duration::from_secs(timeout))?;
    let vi_ref = conn.open_vi_reference(vi)?;

    // Everything after the open must not leak the reference on failure, so
    // collect the result and release before reporting it.
    let result = (|| -> Result<()> {
        for (name, value) in &sets {
            conn.ctrl_val_set(vi_ref, name, value.clone())?;
            log!("  set  {name} = {value}");
        }
        let t0 = std::time::Instant::now();
        let name = vi.file_name().unwrap_or_default().to_string_lossy().to_string();
        match watch {
            // Start it and read the front panel while it works.
            Some(w) => {
                conn.run_vi_async(vi_ref)?;
                log!("  started {name} in {:.0} ms (not waiting)", t0.elapsed().as_secs_f64() * 1000.0);
                poll_progress(&mut conn, vi_ref, w, done, poll_ms, timeout)?;
            }
            None => {
                conn.run_vi(vi_ref)?;
                log!("  ran  {name} in {:.0} ms", t0.elapsed().as_secs_f64() * 1000.0);
            }
        }
        if get_all {
            for (name, value) in conn.ctrl_val_get_panel(vi_ref)? {
                log!("  get  {name} = {value}");
            }
        }
        for name in gets {
            let value = conn.ctrl_val_get(vi_ref, name)?;
            log!("  get  {name} = {value}");
        }
        Ok(())
    })();

    conn.release(vi_ref)?;
    conn.close();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_policy_defaults_by_destination_and_follows_the_manifest() {
        let p = project::parse(
            "[dependencies]\nhttp = { version = \"1\", hooks = \"run\" }\ngcli = { version = \"1\", hooks = \"skip\" }\nqmh = \"1\"\n",
        )
        .unwrap();
        let policy = |mode, venv, headless| HookPolicy { mode, venv, headless, manifest: p.hooks.clone() };

        let venv = policy(HookMode::Default, true, false);
        assert!(venv.allows("HTTP"), "opted in, case-insensitively");
        assert!(!venv.allows("gcli") && !venv.allows("qmh"), "a venv runs nothing else");
        assert!(venv.why_not("qmh").contains("hooks = \"run\""));

        let global = policy(HookMode::Default, false, false);
        assert!(global.allows("qmh") && global.allows("http"), "the installation runs them, as VIPM does");
        assert!(!global.allows("gcli"), "unless opted out");

        let headless = policy(HookMode::Default, false, true);
        assert!(headless.allows("http") && !headless.allows("qmh"));

        assert!(!policy(HookMode::Never, false, false).allows("http"), "--no-hooks wins");
        assert!(policy(HookMode::Always, true, false).allows("gcli"), "--hooks wins");
    }

    /// A project folder whose venv binding names `manifest`.
    fn project_with_venv(tag: &str, manifest: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lvpm-import-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let venv_dir = dir.join(config::DEFAULT_VENV_DIR);
        std::fs::create_dir_all(&venv_dir).unwrap();
        std::fs::write(
            venv_dir.join("venv.json"),
            format!(r#"{{"target_key":"k","target_path":"p","version":26.3,"bitness":64,"port":3500,"created_at":7,"manifest":"{manifest}"}}"#),
        )
        .unwrap();
        dir
    }

    #[test]
    fn import_moves_a_venv_created_from_a_vipm_toml_onto_the_lvpm_toml_it_writes() {
        let dir = project_with_venv("adopt", "vipm.toml");
        let vipm = dir.join("vipm.toml");
        let vipm_text = "[project]\nname = \"demo\"\nlabview-version = \"2024\"\n\n[dependencies]\noglib_array = \"6.0.1.20\"\n\n[dev-dependencies]\ncaraya = \"1.4.5.165\"\n";
        std::fs::write(&vipm, vipm_text).unwrap();
        let binding = dir.join(config::DEFAULT_VENV_DIR).join("venv.json");
        let bound_to = || -> String {
            let b: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&binding).unwrap()).unwrap();
            b["manifest"].as_str().unwrap().to_string()
        };
        let cli = Cli::try_parse_from(["lvpm", "--project", dir.to_str().unwrap(), "import", vipm.to_str().unwrap()]).unwrap();

        let place = import_target(&cli, &vipm).unwrap();
        assert_eq!(place.manifest, dir.join("lvpm.toml"));
        assert_eq!(place.adopt.as_deref(), Some(vipm.as_path()));
        assert!(place.adopts_source && !place.exists);

        // A dry run writes nothing and leaves the binding alone.
        import_vipm_toml(&cli, &vipm, None, true).unwrap();
        assert!(!dir.join("lvpm.toml").exists());
        assert_eq!(bound_to(), "vipm.toml");

        import_vipm_toml(&cli, &vipm, None, false).unwrap();
        let p = project::read(&dir.join("lvpm.toml")).unwrap();
        assert_eq!((p.name.as_deref(), p.labview.as_deref()), (Some("demo"), Some("2024")));
        assert_eq!(p.declared_dependency_names().collect::<Vec<_>>(), ["oglib_array", "caraya"]);
        assert_eq!(bound_to(), "lvpm.toml");
        assert_eq!(std::fs::read_to_string(&vipm).unwrap(), vipm_text, "VIPM's file is read, never written");

        // Again: nothing to adopt any more, and nothing changes.
        let before = std::fs::read_to_string(dir.join("lvpm.toml")).unwrap();
        assert!(import_target(&cli, &vipm).unwrap().adopt.is_none());
        import_vipm_toml(&cli, &vipm, None, false).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("lvpm.toml")).unwrap(), before);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_import_that_fails_part_way_leaves_the_manifest_as_it_was() {
        let dir = project_with_venv("atomic", "lvpm.toml");
        let manifest = dir.join("lvpm.toml");
        // `dev` is a package here, so [dev-dependencies] cannot become the group `dev`.
        let before = "[project]\nname = \"demo\"\n\n[dependencies]\ndev = \"1.0\"\n";
        std::fs::write(&manifest, before).unwrap();
        let vipm = dir.join("vipm.toml");
        std::fs::write(&vipm, "[dependencies]\na = \"1.0\"\nb = \"2.0\"\n\n[dev-dependencies]\ncaraya = \"1.4.5.165\"\n").unwrap();
        let cli = Cli::try_parse_from(["lvpm", "--project", dir.to_str().unwrap(), "import", vipm.to_str().unwrap()]).unwrap();
        let err = format!("{:#}", import_vipm_toml(&cli, &vipm, None, false).unwrap_err());
        assert!(err.contains("cannot also be a group"), "{err}");
        assert_eq!(std::fs::read_to_string(&manifest).unwrap(), before);

        // With no manifest at all, a failed import does not leave a skeleton behind.
        std::fs::remove_file(&manifest).unwrap();
        std::fs::write(&vipm, "[dependencies]\ndev = \"1.0\"\n\n[dev-dependencies]\ncaraya = \"1.4.5.165\"\n").unwrap();
        assert!(import_vipm_toml(&cli, &vipm, None, false).is_err());
        assert!(!manifest.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_vipm_toml_is_never_the_manifest_install_and_uninstall_edit() {
        let dir = std::env::temp_dir();
        let mut v = venv::Venv {
            repo: dir.clone(), dir: dir.join(".lv-venv"),
            target: target::LvTarget { version: 26.3, bitness: 64, path: dir.join("LabVIEW") },
            port: 3456, manifest: Some(PathBuf::from("vipm.toml")),
        };
        let err = format!("{:#}", editable_manifest(&v).unwrap_err());
        assert!(err.contains("lvpm does not edit it") && err.contains("lvpm import"), "{err}");
        v.manifest = Some(PathBuf::from("lvpm.toml"));
        assert_eq!(editable_manifest(&v).unwrap(), dir.join("lvpm.toml"));
    }

    #[test]
    fn hook_policy_outside_an_install_reads_the_manifest_an_install_would() {
        let dir = std::env::temp_dir().join(format!("lvpm-hook-policy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("lvpm.toml");
        std::fs::write(&manifest, "[dependencies]\ngcli = { version = \"1\", hooks = \"skip\" }\nhttp = { version = \"1\", hooks = \"run\" }\n").unwrap();
        let cli = Cli::try_parse_from(["lvpm", "--project", dir.to_str().unwrap(), "list"]).unwrap();
        let setting = |p: &HookPolicy, name: &str| p.setting(name);

        // The LabVIEW installation: `--project`'s manifest, as install reads it.
        let global = Destination::Global(Roots::scratch(&dir));
        let policy = hook_policy_for(&cli, &global, HookMode::Default).unwrap();
        assert!(!policy.venv);
        assert!(matches!(setting(&policy, "gcli"), Some(project::Hooks::Skip)));
        assert!(matches!(setting(&policy, "HTTP"), Some(project::Hooks::Run)));
        assert!(setting(&policy, "other").is_none());

        // A venv: its bound manifest.
        let mut v = venv::Venv {
            repo: dir.clone(), dir: dir.join(".lv-venv"),
            target: target::LvTarget { version: 26.3, bitness: 64, path: dir.join("LabVIEW") },
            port: 3456, manifest: Some(PathBuf::from("lvpm.toml")),
        };
        let policy = hook_policy_for(&cli, &Destination::Venv(v.clone()), HookMode::Default).unwrap();
        assert!(policy.venv && matches!(setting(&policy, "gcli"), Some(project::Hooks::Skip)));

        // A manifest that does not read is an error, not the defaults.
        std::fs::write(&manifest, "[dependencies]\ngcli = 1\n").unwrap();
        assert!(hook_policy_for(&cli, &global, HookMode::Default).is_err());
        assert!(hook_policy_for(&cli, &Destination::Venv(v.clone()), HookMode::Default).is_err());
        // One that is gone leaves a venv on its defaults, as the launch check does.
        std::fs::remove_file(&manifest).unwrap();
        v.manifest = Some(PathBuf::from("lvpm.toml"));
        let policy = hook_policy_for(&cli, &Destination::Venv(v), HookMode::Default).unwrap();
        assert!(policy.manifest.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_named_install_saves_nothing_the_venv_does_not_hold() {
        let entry = |name: &str, version: &str| index::Entry {
            name: name.into(),
            version: Version::parse(version),
            url: String::new(),
            md5: None,
            display_name: None,
            requires: Vec::new(),
            lv_min: None,
        };
        let plan = [entry("dep", "3.0"), entry("Foo", "1.0")];
        let save = |version: &str| Some(SaveEntry::Save { name: "Foo".into(), version: version.into() });
        let kept =
            || Some(SaveEntry::Kept { name: "Foo".into(), installed: "2.0".into(), resolved: "1.0".into() });

        // Installed or already there: the version asked for, else the one
        // resolved, under the index's spelling of the name.
        for action in [InstallAction::Install, InstallAction::Same] {
            let actions = [InstallAction::Install, action];
            assert_eq!(save_entry_for("foo", &plan, &actions), save("1.0"));
            assert_eq!(save_entry_for("foo@1.0", &plan, &actions), save("1.0"));
            assert_eq!(save_entry_for("foo@>=0.5", &plan, &actions), save(">=0.5"));
        }

        // A newer installed version was kept: an exact entry would name a
        // version the venv does not hold; a floor that allows it is fine.
        let actions = [InstallAction::Install, InstallAction::KeepNewer("2.0".into())];
        assert_eq!(save_entry_for("foo", &plan, &actions), kept());
        assert_eq!(save_entry_for("foo@1.0", &plan, &actions), kept());
        assert_eq!(save_entry_for("foo@>=0.5", &plan, &actions), save(">=0.5"));
        assert_eq!(save_entry_for("foo@*", &plan, &actions), save("*"));

        assert_eq!(save_entry_for("absent", &plan, &actions), None);
    }

    #[test]
    fn launch_check_reports_what_the_records_fall_short_of() {
        let record = |name: &str, version: &str, extra: &str| -> install::Manifest {
            serde_json::from_str(&format!(
                r#"{{"name":"{name}","version":"{version}","installed_at":0,"target":"t",
                    "files":["/v/{name}/a.vi","/v/{name}/b.vi"],"dependencies":[],"skipped_hooks":[]{extra}}}"#
            ))
            .unwrap()
        };
        let p = project::parse(
            "[dependencies]\nok = \">=1.0\"\nold = \"2.0\"\ngone = \"1\"\nhalf = \"1\"\n\
             rl = { version = \"1\", relink = true }\nhook = { version = \"1\", hooks = \"run\" }\n\
             broke = \"1\"\nquiet = \"1\"\nlegacy = \"1\"\nowed = \"1\"\n",
        )
        .unwrap();
        let read = |name: &str| -> Result<Option<install::Manifest>> {
            Ok(match name {
                "ok" => Some(record(name, "1.2", "")),
                "old" => Some(record(name, "1.9", "")),
                "half" => Some(record(name, "1", "")),
                "rl" => Some(record(name, "1", r#","relinked":false"#)),
                "hook" => Some(record(name, "1", r#","post_install_vi":"x","post_install":{"state":"skipped","at":1}"#)),
                "broke" => Some(record(
                    name,
                    "1",
                    r#","post_install_vi":"x","post_install":{"state":"failed","at":1,"error":"boom"}"#,
                )),
                // A record from an older lvpm: a hook, but no state.
                "legacy" => Some(record(name, "1", r#","post_install_vi":"x""#)),
                "owed" => Some(record(name, "1", r#","post_install_vi":"x","post_install":{"state":"pending"}"#)),
                // Skipped by policy, and the policy says so: fine.
                "quiet" => Some(record(name, "1", r#","post_install_vi":"x","post_install":{"state":"skipped","at":1}"#)),
                _ => None,
            })
        };
        let problems = venv_problems(
            &p.dependencies,
            &p.relink,
            |n| matches!(n, "hook" | "legacy" | "owed"),
            read,
            |path| path != Path::new("/v/half/b.vi"),
        )
        .unwrap();
        let install = |s: &str| (s.to_string(), Fix::Install);
        let hooks = |s: &str, n: &str| (s.to_string(), Fix::RunHooks(n.to_string()));
        assert_eq!(
            problems,
            vec![
                install("old: 1.9 installed, manifest asks for old@2.0"),
                install("gone: not installed"),
                install("half: 1 of 2 files missing, e.g. /v/half/b.vi"),
                install("rl: relink = true but the relink pass has not completed"),
                install("hook: PostInstall has not run"),
                hooks("broke: PostInstall failed: boom", "broke"),
                hooks("legacy: PostInstall has not run", "legacy"),
                install("owed: PostInstall has not run"),
            ]
        );
        assert_eq!(
            launch_check_hint(&problems),
            "run `lvpm install` to bring it in line; `lvpm run-hooks broke` reruns broke's PostInstall hook, \
             `lvpm run-hooks legacy` reruns legacy's PostInstall hook; or `lvpm launch --no-check` to start anyway"
        );
        assert_eq!(
            launch_check_hint(&[install("a: not installed")]),
            "run `lvpm install` to bring it in line; or `lvpm launch --no-check` to start anyway"
        );
        assert_eq!(
            launch_check_hint(&[hooks("a: PostInstall failed: x", "a")]),
            "`lvpm run-hooks a` reruns a's PostInstall hook; or `lvpm launch --no-check` to start anyway"
        );
    }

    #[test]
    fn a_failed_dependency_leaves_the_named_package_removed_from_the_manifest() {
        assert!(!named_package_removed_before(0));
        assert!(named_package_removed_before(1));
        assert!(named_package_removed_before(3));
    }

    #[test]
    fn dependency_confirmation_accepts_yes_and_defaults_to_no() {
        let names = vec!["dependency".to_string()];
        for (answer, expected) in [("y\n", true), ("YES\n", true), ("\n", false), ("no\n", false)] {
            let mut input = std::io::Cursor::new(answer.as_bytes());
            let mut output = Vec::new();
            assert_eq!(confirm_dependencies(&names, true, &mut input, &mut output).unwrap(), expected);
            assert!(String::from_utf8(output).unwrap().contains("dependency"));
        }
        assert!(confirm_dependencies(&names, false, &mut std::io::empty(), &mut Vec::new()).is_err());
    }

    #[test]
    fn uninstall_dependency_flags_conflict_with_each_other_and_all() {
        assert!(Cli::try_parse_from(["lvpm", "uninstall", "pkg", "--with-deps", "--keep-deps"]).is_err());
        assert!(Cli::try_parse_from(["lvpm", "uninstall", "--all", "--with-deps"]).is_err());
        assert!(Cli::try_parse_from(["lvpm", "uninstall", "pkg", "--with-deps"]).is_ok());
    }

    #[test]
    fn uninstall_roots_include_main_and_all_named_groups() {
        let dir = std::env::temp_dir().join(format!("lvpm-uninstall-roots-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(project::FILE_NAME);
        assert!(project_uninstall_roots(&path).is_err());
        std::fs::write(dir.join(project::FILE_NAME),
            "[dependencies]\nmain_package = \"*\"\n[dependencies.test]\ntest_package = \"*\"\n").unwrap();
        assert_eq!(project_uninstall_roots(&path).unwrap(), ["main_package", "test_package"]);
        std::fs::write(dir.join(project::FILE_NAME), "invalid = [").unwrap();
        assert!(project_uninstall_roots(&path).is_err());
        let vipm = dir.join("vipm.toml");
        std::fs::write(&vipm, "[dependencies]\ncommon = \"1.0\"\n[dev-dependencies]\ntest = \"1.0\"\n").unwrap();
        let err = format!("{:#}", project_uninstall_roots(&vipm).unwrap_err());
        assert!(err.contains("lvpm import"), "a vipm.toml is refused with the way in: {err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn venv_uses_its_bound_manifest_and_reports_when_it_is_missing() {
        let dir = std::env::temp_dir().join(format!("lvpm-bound-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bound = dir.join("app.toml");
        std::fs::write(&bound, "[dependencies]\npinned = \"1.0\"\n").unwrap();
        std::fs::write(dir.join("lvpm.toml"), "[dependencies]\nother = \"*\"\n").unwrap();
        let cli = Cli::try_parse_from(["lvpm", "install"]).unwrap();
        let mut v = venv::Venv {
            repo: dir.clone(), dir: dir.join(".lv-venv"),
            target: target::LvTarget { version: 26.3, bitness: 64, path: dir.join("LabVIEW") },
            port: 3456, manifest: Some(PathBuf::from("app.toml")),
        };
        let (path, project) = manifest_for(&cli, None, Some(&v)).unwrap().unwrap();
        assert_eq!(path, bound);
        assert_eq!(project.declared_dependency_names().collect::<Vec<_>>(), ["pinned"]);
        let (_, override_project) = manifest_for(&cli, Some(&dir.join("lvpm.toml")), Some(&v)).unwrap().unwrap();
        assert_eq!(override_project.declared_dependency_names().collect::<Vec<_>>(), ["other"]);
        std::fs::remove_file(&bound).unwrap();
        assert!(manifest_for(&cli, None, Some(&v)).is_err());

        // A venv created from a vipm.toml by an older lvpm: refused, with the way out.
        std::fs::write(dir.join("vipm.toml"), "[dependencies]\npinned = \"1.0\"\n").unwrap();
        v.manifest = Some(PathBuf::from("vipm.toml"));
        let err = format!("{:#}", manifest_for(&cli, None, Some(&v)).unwrap_err());
        assert!(err.contains("lvpm import"), "{err}");
        v.manifest = None;
        assert!(manifest_for(&cli, None, Some(&v)).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn install(args: &[&str]) -> Result<GroupArgs, clap::Error> {
        let cli = Cli::try_parse_from(["lvpm", "install"].iter().chain(args))?;
        match cli.cmd {
            Cmd::Install { groups, .. } => Ok(groups),
            _ => unreachable!(),
        }
    }

    #[test]
    fn group_flags_split_on_commas_and_accumulate() {
        let g = install(&["--with", "test,ci"]).unwrap();
        assert_eq!(g.with, ["test", "ci"]);
        let g = install(&["--with", "test", "--with", "ci"]).unwrap();
        assert_eq!(g.with, ["test", "ci"]);
        let g = install(&["--with", "a,b", "--with", "c", "--without", "b,d"]).unwrap();
        assert_eq!(g.with, ["a", "b", "c"]);
        assert_eq!(g.without, ["b", "d"]);
        let g = install(&["--only", "main,build"]).unwrap();
        assert_eq!(g.only, ["main", "build"]);
        assert!(g.with.is_empty() && g.without.is_empty());
    }

    #[test]
    fn group_flags_keep_spaces_for_select_to_trim() {
        let g = install(&["--with", "test, ci", "--without", "x,"]).unwrap();
        assert_eq!(g.with, ["test", " ci"]);
        assert_eq!(g.without, ["x", ""]);
    }

    #[test]
    fn group_flags_refuse_contradictory_combinations() {
        assert!(install(&["--only", "a", "--with", "b"]).is_err());
        assert!(install(&["--only", "a", "--without", "b"]).is_err());
        assert!(install(&["oglib_error", "--with", "a"]).is_err());
        assert!(install(&["oglib_error", "--without", "a"]).is_err());
        assert!(install(&["oglib_error", "--only", "a"]).is_err());
        assert!(install(&["--manifest", "app.toml", "--with", "dev"]).is_ok());
        assert!(install(&["--with", "a", "--without", "b"]).is_ok());
    }
}
