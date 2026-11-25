//! [`OpEnv`]: everything an operation needs from its surroundings, handed to it
//! instead of read from the process.
//!
//! An operation reads no environment variable and no working directory. The
//! front end builds one `OpEnv` from its own flags and the process, and a test
//! builds one from nothing at all. Settling *where* a command's packages live
//! and *which indexes* it sees is the first thing every operation does, so it
//! is the first thing here.

use super::index_files::IndexFiles;
use crate::cancel::CancelToken;
use crate::config::Config;
use crate::events::{Event, EventSink};
use crate::github::GitHub;
use crate::index::{self, Index};
use crate::oci::Oci;
use crate::paths;
use crate::project::{self, Project};
use crate::target::{self, LvTarget, Roots};
use crate::venv::{self, Venv};
use anyhow::{Result, bail, ensure};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The flags that say where a command's packages live.
#[derive(Debug, Clone, Default)]
pub struct DestinationFlags {
    /// Pick this LabVIEW version, e.g. 2026. A write into the installation
    /// itself needs `--global` as well.
    pub labview_version: Option<String>,
    /// Pick the 32- or 64-bit LabVIEW when both of a version are installed.
    pub labview_bitness: Option<u8>,
    /// Install into a scratch directory instead of a LabVIEW installation.
    pub prefix: Option<PathBuf>,
    /// Use the LabVIEW installation itself, even from inside a project that
    /// has a venv.
    pub global: bool,
    /// The project whose venv to use, instead of looking upwards from `cwd`.
    pub project: Option<PathBuf>,
}

impl DestinationFlags {
    /// The flags that picked the target, as typed, each with its value: first
    /// the one that decided it, `--prefix`, then `--labview-version`, then
    /// `--global`, then the `--labview-bitness` that narrowed a LabVIEW pick.
    /// `--labview-version` picks nothing when `venv_wins` and `--global` is not
    /// there to say otherwise (see [`OpEnv::destination`]).
    pub fn picked(&self, venv_wins: bool) -> Vec<(&'static str, Option<String>)> {
        let version = self
            .labview_version
            .as_ref()
            .filter(|_| !venv_wins || self.global);
        let bitness = (self.labview_bitness)
            .filter(|_| self.prefix.is_none() && (version.is_some() || self.global));
        [
            self.prefix
                .as_ref()
                .map(|p| ("--prefix", Some(p.display().to_string()))),
            version.map(|v| ("--labview-version", Some(v.clone()))),
            self.global.then_some(("--global", None)),
            bitness.map(|b| ("--labview-bitness", Some(b.to_string()))),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

/// `word` as one word of a command line a user is told to run: in double quotes
/// when it holds white space or a character a shell reads (`<>|&;()`).
pub fn shell_word(word: &str) -> String {
    match word.contains(|c: char| c.is_whitespace() || "<>|&;()".contains(c)) {
        true => format!("\"{word}\""),
        false => word.to_string(),
    }
}

/// What an operation runs in.
pub struct OpEnv<'a> {
    pub flags: DestinationFlags,
    /// Revalidate remote feeds and re-read local packages instead of trusting
    /// cached stats.
    pub refresh: bool,
    /// The directory the command was run from.
    pub cwd: PathBuf,
    /// Is LabVIEW headless on this machine? `LV_RTE_HEADLESS` is NI's own
    /// global override (LabVIEW 2026+): every LabVIEW start becomes
    /// non-interactive — no activation, no dialogs. Nobody sets it on a
    /// workstation, since the IDE would stop opening, so its presence says
    /// "automation machine, no developer present" more reliably than any flag
    /// of ours could. lvpm reads it for the three decisions that hinge on
    /// exactly that: whether a manifest without a venv means the machine,
    /// whether to relink, whether to rebuild palettes. The front end reads the
    /// variable; operations only read this.
    pub headless: bool,
    pub config: Config,
    /// The LabVIEW installations detected on this machine: the detected list;
    /// choose from [`OpEnv::pickable`].
    pub targets: Vec<LvTarget>,
    pub sink: &'a dyn EventSink,
    /// Does nothing unless the front end keeps a clone and cancels it.
    pub cancel: CancelToken,
    /// Put the machine's own places (ProgramData and the like) under this
    /// directory for a LabVIEW installation. Only a test of an installation
    /// that is not really there sets it; see [`Roots::labview_on`].
    pub machine: Option<PathBuf>,
    /// The API GitHub dependencies are read from, and the token it is told.
    pub github: GitHub,
    /// The OCI registry client and the logins it holds. Anonymous until the
    /// front end installs credentials.
    pub oci: Arc<Oci>,
    /// A command can read its manifest more than once; the bare-year warning
    /// is said once per command, for the first manifest that has one.
    bare_year_warned: AtomicBool,
    /// The same for the old-form group warning.
    old_form_groups_warned: AtomicBool,
}

/// What a command does at its destination: reading needs no more than naming
/// it, writing into LabVIEW itself needs `--global`.
#[derive(Clone, Copy, PartialEq)]
enum Intent {
    Read,
    Write,
}

/// Where a command's packages live.
#[derive(Debug)]
pub enum Destination {
    /// The LabVIEW installation itself.
    Global(Roots),
    /// A `--prefix` sandbox.
    Scratch(Roots),
    /// A project's venv.
    Venv(Venv),
}

impl Destination {
    /// Roots for what reads the store or removes from it. Venv-wide for a
    /// venv — each package has roots of its own for the files themselves.
    pub fn store(&self) -> Roots {
        match self {
            Destination::Global(r) | Destination::Scratch(r) => r.clone(),
            Destination::Venv(v) => v.store(),
        }
    }

    /// The LabVIEW the destination is for, if any: a scratch prefix has none.
    pub fn target(&self) -> Option<&LvTarget> {
        match self {
            Destination::Venv(v) => Some(&v.target),
            Destination::Global(r) | Destination::Scratch(r) => r.target.as_ref(),
        }
    }
}

impl<'a> OpEnv<'a> {
    /// An environment with no destination flags, not headless, not refreshing
    /// and never cancelled; the caller sets what differs.
    pub fn new(
        sink: &'a dyn EventSink,
        config: Config,
        targets: Vec<LvTarget>,
        cwd: PathBuf,
    ) -> OpEnv<'a> {
        OpEnv {
            flags: DestinationFlags::default(),
            refresh: false,
            cwd,
            headless: false,
            config,
            targets,
            sink,
            cancel: CancelToken::default(),
            machine: None,
            github: GitHub::default(),
            oci: Arc::new(Oci::default()),
            bare_year_warned: AtomicBool::new(false),
            old_form_groups_warned: AtomicBool::new(false),
        }
    }

    fn labview_roots(&self, t: &LvTarget) -> Roots {
        Roots::labview_on(t, self.machine.as_deref())
    }

    /// The installations a pick may choose from: the detected ones, narrowed to
    /// `--labview-bitness` when it is given. Not for finding a venv's own LabVIEW.
    pub fn pickable(&self) -> Result<Vec<LvTarget>> {
        target::of_bitness(&self.targets, self.flags.labview_bitness)
    }

    /// Resolve `--labview-version` / `--prefix` into a set of roots.
    pub fn roots(&self) -> Result<Roots> {
        match (&self.flags.labview_version, &self.flags.prefix) {
            (Some(_), Some(_)) => bail!("--labview-version and --prefix are mutually exclusive"),
            (Some(want), None) => {
                if self.targets.is_empty() {
                    bail!("no LabVIEW installations detected");
                }
                Ok(self.labview_roots(&target::select(&self.pickable()?, want)?))
            }
            (None, Some(p)) => Ok(Roots::scratch(p)),
            (None, None) => bail!(
                "pick a destination: --global [--labview-version <YYYY>] for LabVIEW itself, --prefix \
                 <DIR> for a scratch tree, or run inside a project that has a venv (`lvpm venv create`)\n\
                 hint: `lvpm targets` lists detected LabVIEW installations"
            ),
        }
    }

    /// The destination flag that decided the target, as typed: the one a named
    /// install or uninstall in a project tells the user only picks where the
    /// project syncs. A venv lookup that fails fails the command after it.
    pub fn destination_flag(&self) -> Option<&'static str> {
        self.picked().first().map(|(flag, _)| *flag)
    }

    /// Every destination flag that picked the target, with its value, as one
    /// command-line fragment (`--prefix dest`): what a suggested command needs
    /// to act on the same target, and the `--project` that picked the project.
    pub fn destination_flags_as_typed(&self) -> Option<String> {
        self.flags_as_typed(true)
    }

    /// [`Self::destination_flags_as_typed`] without `--project`: for a command
    /// that cannot take it, as `--no-project` cannot.
    pub fn target_flags_as_typed(&self) -> Option<String> {
        self.flags_as_typed(false)
    }

    fn flags_as_typed(&self, with_project: bool) -> Option<String> {
        let project = (self.flags.project.as_ref())
            .filter(|_| with_project)
            .map(|p| ("--project", Some(p.display().to_string())));
        let typed: Vec<String> = (self.picked().into_iter().chain(project))
            .map(|(flag, value)| match value {
                Some(v) => format!("{flag} {}", shell_word(&v)),
                None => flag.to_string(),
            })
            .collect();
        (!typed.is_empty()).then(|| typed.join(" "))
    }

    fn picked(&self) -> Vec<(&'static str, Option<String>)> {
        let venv_wins = self.find_venv().is_ok_and(|venv| venv.is_some());
        self.flags.picked(venv_wins)
    }

    /// Settle where a command that writes packages puts them, the way cargo
    /// settles which crate you mean: `--prefix` and `--global` say so outright;
    /// otherwise a venv at or above the current directory wins, and only when
    /// there is none does `--labview-version` mean the installation itself, and
    /// then only on a headless machine: a container's LabVIEW *is* the sandbox
    /// and nobody is there to type `--global`. Anywhere else it is refused,
    /// because a write into an installation changes every project that uses it.
    ///
    /// A manifest with no venv is refused too, headless excepted for the same
    /// reason: `lvpm install` in the project means the installation.
    pub fn destination(&self) -> Result<Destination> {
        self.resolve(Intent::Write)
    }

    /// [`Self::destination`] for a command that only reads, which needs no
    /// `--global`: `--labview-version` alone names the installation.
    pub fn read_destination(&self) -> Result<Destination> {
        self.resolve(Intent::Read)
    }

    fn resolve(&self, intent: Intent) -> Result<Destination> {
        let explicit = |r: Roots| match r.target.is_some() {
            true => Destination::Global(r),
            false => Destination::Scratch(r),
        };
        if self.flags.prefix.is_some() || self.flags.global {
            return self.flagged_roots().map(explicit);
        }
        if self.headless
            && self.flags.project.is_none()
            && let venv::Found::ManifestOnly(repo) = venv::probe(&self.cwd, &self.config.venv_dir)
        {
            let roots = self.manifest_roots(&repo)?;
            self.sink.emit(Event::HeadlessInstallationChosen {
                repo,
                labview: roots
                    .target
                    .as_ref()
                    .map(LvTarget::label)
                    .unwrap_or_default(),
            });
            return Ok(Destination::Global(roots));
        }
        if let Some(v) = self.find_venv()? {
            self.sink.emit(Event::VenvSelected {
                dir: v.dir.clone(),
                labview: v.target.label(),
            });
            return Ok(Destination::Venv(v));
        }
        if intent == Intent::Write && self.flags.labview_version.is_some() && !self.headless {
            return Err(self.needs_global());
        }
        self.roots().map(explicit)
    }

    /// What `--prefix`, `--global` and `--labview-version` say, with no venv
    /// looked for unless `--global` is given alone: what a command with an
    /// explicit destination settles on, and the roots `run-hooks` acts in.
    pub fn flagged_roots(&self) -> Result<Roots> {
        let flags = &self.flags;
        ensure!(
            !(flags.global && flags.prefix.is_some()),
            "--global and --prefix are mutually exclusive"
        );
        match (flags.global, &flags.labview_version) {
            (true, None) => self.global_roots(),
            _ => self.roots(),
        }
    }

    /// [`Self::flagged_roots`] for a command that writes into the installation
    /// it finds with no venv (`run-hooks`): a real one needs `--global`,
    /// headless excepted.
    pub(super) fn write_roots(&self) -> Result<Roots> {
        let roots = self.flagged_roots()?;
        match roots.target.is_some() && !self.flags.global && !self.headless {
            true => Err(self.needs_global()),
            false => Ok(roots),
        }
    }

    /// The refusal of a write into LabVIEW itself that `--labview-version`
    /// alone asked for.
    fn needs_global(&self) -> anyhow::Error {
        let year = self.flags.labview_version.as_deref().unwrap_or("YYYY");
        anyhow::anyhow!(
            "--labview-version {year} alone does not write into LabVIEW {year} itself: that changes \
             every project that uses it\n\
             next: add --global, or --prefix <DIR> for a scratch tree"
        )
    }

    /// The installation `--global` means when `--labview-version` does not
    /// say: a project's own, from its manifest or its venv, else the only one
    /// installed.
    fn global_roots(&self) -> Result<Roots> {
        if self.flags.project.is_none()
            && let venv::Found::ManifestOnly(repo) = venv::probe(&self.cwd, &self.config.venv_dir)
        {
            return self.manifest_roots(&repo);
        }
        if let Some(v) = self.find_venv()? {
            return Ok(self.labview_roots(&v.target));
        }
        match self.pickable()?.as_slice() {
            [] => bail!("no LabVIEW installations detected"),
            [only] => Ok(self.labview_roots(only)),
            several => bail!(
                "which LabVIEW? pass --labview-version <YYYY> (installed: {})\n\
                 hint: `lvpm targets` lists what is installed",
                several
                    .iter()
                    .map(LvTarget::label)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// The installation a venv-less project means: `--labview-version` if
    /// given, else the manifest's `labview` — the same choice `lvpm venv
    /// create` makes, with the same rule that the manifest's version is a
    /// minimum a newer LabVIEW may satisfy and an older one may not.
    fn manifest_roots(&self, repo: &Path) -> Result<Roots> {
        let manifest_path = repo.join(project::FILE_NAME);
        let proj = project::read(&manifest_path)?;
        let min_lv = proj.labview.as_deref();
        // Read before anything is chosen, so a `labview` that is no year is
        // refused whichever way the installation is picked.
        let min_year = min_lv.map(project::labview_year).transpose()?;
        let requested = self.flags.labview_version.as_deref();
        ensure!(
            requested.or(min_lv).is_some(),
            "which LabVIEW? pass --labview-version <YYYY>, or set labview in {}\n\
             hint: `lvpm targets` lists what is installed",
            manifest_path.display()
        );
        ensure!(
            !self.targets.is_empty(),
            "no LabVIEW installations detected"
        );
        let t = target::select_for_project(&self.pickable()?, requested, min_year)?
            .expect("checked above: a flag or a manifest year is set");
        Ok(self.labview_roots(&t))
    }

    /// The venv a command means, if any: `--project` names the repo outright,
    /// otherwise it is looked up from `cwd`.
    pub fn find_venv(&self) -> Result<Option<Venv>> {
        self.find_venv_with(false)
    }

    /// [`Self::find_venv`] that also takes a venv of the old layout, for removing it.
    pub fn find_venv_for_removal(&self) -> Result<Option<Venv>> {
        self.find_venv_with(true)
    }

    fn find_venv_with(&self, legacy: bool) -> Result<Option<Venv>> {
        let find = match legacy {
            true => venv::find_for_removal,
            false => venv::find,
        };
        let project = self.flags.project.as_deref().map(|p| self.cwd.join(p));
        let found = find(
            project.as_deref(),
            &self.cwd,
            &self.config.venv_dir,
            &self.targets,
        )?;
        if let Some(v) = &found {
            self.check_bitness(&v.target)?;
        }
        Ok(found)
    }

    /// Refuses a venv bound to another bitness than `--labview-bitness` names.
    pub fn check_bitness(&self, bound_to: &LvTarget) -> Result<()> {
        if let Some(bits) = self.flags.labview_bitness
            && bound_to.bitness != bits
        {
            bail!(
                "this venv is bound to {}, not the {bits}-bit LabVIEW --labview-bitness asks for\n\
                 hint: drop --labview-bitness, or run `lvpm venv remove` without it, then \
                 `lvpm venv create --labview-bitness {bits}`",
                bound_to.label()
            );
        }
        Ok(())
    }

    /// Where the relink VI and a staged PostUninstall hook are written. `%TEMP%`
    /// on Windows; on Linux LabVIEW cannot open files under the private
    /// `/tmp` a sandboxed shell may see, so the cache instead.
    pub fn work_dir(&self) -> PathBuf {
        match cfg!(windows) {
            true => std::env::temp_dir(),
            false => self.config.cache.clone(),
        }
    }

    /// The manifest a command belongs to: `explicit` (`--manifest`) names it;
    /// else `bound`, the manifest of the venv in effect (missing: none); else `--project`'s; else
    /// the nearest `lvpm.toml` at or above the working directory. None is fine
    /// — a package can be installed without a project.
    pub fn manifest_for(
        &self,
        explicit: Option<&Path>,
        bound: Option<&Path>,
    ) -> Result<Option<(PathBuf, Project)>> {
        let path = match (explicit, bound, &self.flags.project) {
            (Some(p), _, _) => Some(self.cwd.join(p)),
            (None, Some(m), _) => Some(m.to_path_buf()).filter(|p| p.is_file()),
            (None, None, Some(dir)) => {
                Some(self.cwd.join(dir).join(project::FILE_NAME)).filter(|p| p.is_file())
            }
            (None, None, None) => project::find_manifest(&self.cwd),
        };
        match path {
            Some(p) => Ok(Some((
                paths::resolve(&self.cwd, &p),
                self.read_manifest(&p)?,
            ))),
            None => Ok(None),
        }
    }

    /// Read a manifest file. A `vipm.toml` goes through the VIPM conversion; it
    /// is reached through `--manifest` or a venv bound to it, never discovered.
    pub fn read_manifest(&self, path: &Path) -> Result<Project> {
        match crate::vipm_interop::is_toml(path) {
            true => crate::vipm_interop::read_toml(path, self.sink),
            false => {
                let project = project::read(path)?;
                if let Some(year) = project
                    .labview
                    .as_ref()
                    .filter(|_| project.labview_bare_year)
                    && !self.bare_year_warned.swap(true, Ordering::Relaxed)
                {
                    self.sink.emit(Event::LabviewBareYear {
                        path: path.to_path_buf(),
                        year: year.clone(),
                    });
                }
                if !project.old_form_groups.is_empty()
                    && !self.old_form_groups_warned.swap(true, Ordering::Relaxed)
                {
                    for group in &project.old_form_groups {
                        self.sink.emit(Event::OldFormGroup {
                            path: path.to_path_buf(),
                            group: group.clone(),
                        });
                    }
                }
                Ok(project)
            }
        }
    }

    /// The configured local package folders, a relative one taken from `cwd`.
    pub(super) fn local_sources(&self) -> Vec<PathBuf> {
        let folders = self.config.local_sources.iter();
        folders.map(|dir| paths::resolve(&self.cwd, dir)).collect()
    }

    /// Every index the command should see: global local folders plus hosted
    /// manifest sources, on top of the public indexes unless disabled. The
    /// community indexes are asked for `wanted` (see [`wanted_names`]) and what
    /// those require, and for nothing else.
    pub fn load_index(
        &self,
        manifest: Option<&(PathBuf, Project)>,
        wanted: &[String],
    ) -> Result<Index> {
        let loaded = self.load_index_each(manifest, wanted, |_, error| Err(error))?;
        Ok(loaded.index)
    }

    /// [`Self::load_index`], handing a source that cannot be read to `on_fail`
    /// as [`index::load_each`] does.
    pub(super) fn load_index_each(
        &self,
        manifest: Option<&(PathBuf, Project)>,
        wanted: &[String],
        on_fail: impl FnMut(&str, anyhow::Error) -> Result<()>,
    ) -> Result<index::Loaded> {
        let mut remotes = Vec::new();
        let mut registries = Vec::new();
        let mut defaults = true;
        let mut owners = HashMap::new();
        if let Some((_, proj)) = manifest {
            let feeds = proj.resolved_sources()?;
            owners = index::owners(&feeds, &proj.source_pins);
            (remotes, registries) = index::split_sources(feeds, &proj.source_pins);
            defaults = proj.default_sources;
            self.warn_unpinned(&registries);
        }
        // Opened for a project that has a community index, and for no other.
        let has_index = remotes
            .iter()
            .any(|remote| matches!(remote, index::Remote::Index(_)));
        let files = has_index
            .then(|| IndexFiles::open(&self.config.cache, &self.github, self.refresh))
            .transpose()?;
        let fetch = |feed: &index::IndexFeed, key: &str| match &files {
            Some(files) => files.read(feed, key),
            None => Ok(None),
        };
        let sources = index::SourceSet {
            extra: &remotes,
            local: &self.local_sources(),
            defaults,
            oci: &registries,
            wanted,
            index_fetch: &fetch,
            index_parse: crate::community::read_entries,
        };
        index::load_each(
            &self.config.cache,
            self.refresh,
            &sources,
            &owners,
            &self.oci,
            self.sink,
            &self.cancel,
            on_fail,
        )
    }

    /// Says, once, of each OCI source nothing is pinned to: it is asked for
    /// no name, so it would sit in `[sources]` and do nothing.
    pub(super) fn warn_unpinned(&self, registries: &[index::OciFeed]) {
        for feed in registries.iter().filter(|feed| feed.pinned.is_empty()) {
            self.sink.emit(Event::OciSourceUnpinned {
                name: feed.name.clone(),
            });
        }
    }
}

/// The names the community indexes are asked for: every dependency of
/// `project`, in any table, and `own`, the names the command was given
/// (`lvpm install foo`).
pub(super) fn wanted_names(project: Option<&Project>, own: &[String]) -> Vec<String> {
    let listed = project.into_iter().flat_map(|p| p.tables());
    let listed = listed.flat_map(|(_, lines)| lines.iter().map(|(name, _)| name.clone()));
    listed.chain(own.iter().cloned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::NullSink;

    #[test]
    fn the_destination_flags_that_picked_the_target_are_listed_with_their_values_in_the_order_that_decides()
     {
        let picked = |flags: &DestinationFlags, venv_wins| flags.picked(venv_wins);
        let version = DestinationFlags {
            labview_version: Some("2026".into()),
            ..Default::default()
        };
        assert_eq!(
            picked(&version, false),
            [("--labview-version", Some("2026".to_string()))]
        );
        // A venv wins over --labview-version, so it picked nothing.
        assert_eq!(picked(&version, true), []);
        let prefix_and_version = DestinationFlags {
            prefix: Some("dest".into()),
            labview_version: Some("2026".into()),
            ..Default::default()
        };
        assert_eq!(
            picked(&prefix_and_version, true),
            [("--prefix", Some("dest".to_string()))]
        );
        let global = DestinationFlags {
            global: true,
            ..Default::default()
        };
        assert_eq!(picked(&global, true), [("--global", None)]);
        // --prefix picked the roots, whatever else was typed.
        let prefix_and_global = DestinationFlags {
            prefix: Some("dest".into()),
            global: true,
            ..Default::default()
        };
        assert_eq!(
            picked(&prefix_and_global, true),
            [("--prefix", Some("dest".to_string())), ("--global", None)]
        );
        // With --global a venv does not win, so the version it names is the target.
        let global_and_version = DestinationFlags {
            global: true,
            labview_version: Some("2026".into()),
            ..Default::default()
        };
        assert_eq!(
            picked(&global_and_version, true),
            [
                ("--labview-version", Some("2026".to_string())),
                ("--global", None)
            ]
        );
        // --labview-bitness narrowed the LabVIEW pick, so it goes with it.
        let version_and_bitness = DestinationFlags {
            labview_version: Some("2026".into()),
            labview_bitness: Some(32),
            ..Default::default()
        };
        assert_eq!(
            picked(&version_and_bitness, false),
            [
                ("--labview-version", Some("2026".to_string())),
                ("--labview-bitness", Some("32".to_string()))
            ]
        );
        // With --prefix, or a venv that won, no LabVIEW was picked to narrow.
        assert_eq!(picked(&version_and_bitness, true), []);
        let prefix_and_bitness = DestinationFlags {
            prefix: Some("dest".into()),
            labview_bitness: Some(32),
            ..Default::default()
        };
        assert_eq!(
            picked(&prefix_and_bitness, false),
            [("--prefix", Some("dest".to_string()))]
        );
        assert_eq!(picked(&DestinationFlags::default(), false), []);
    }

    #[test]
    fn the_destination_flags_are_one_command_line_fragment_with_a_spaced_value_quoted() {
        let mut env = OpEnv::new(&NullSink, config(), Vec::new(), PathBuf::from("."));
        assert_eq!(env.destination_flag(), None);
        assert_eq!(env.destination_flags_as_typed(), None);
        env.flags.prefix = Some(PathBuf::from("dest"));
        env.flags.global = true;
        assert_eq!(env.destination_flag(), Some("--prefix"));
        assert_eq!(
            env.destination_flags_as_typed().as_deref(),
            Some("--prefix dest --global")
        );
        env.flags.prefix = Some(PathBuf::from("my dest"));
        assert_eq!(
            env.destination_flags_as_typed().as_deref(),
            Some("--prefix \"my dest\" --global")
        );
        // --project picked the project, so a repeated command needs it too, but
        // it is not the flag that picked the target.
        env.flags.project = Some(PathBuf::from("my proj"));
        assert_eq!(env.destination_flag(), Some("--prefix"));
        assert_eq!(
            env.destination_flags_as_typed().as_deref(),
            Some("--prefix \"my dest\" --global --project \"my proj\"")
        );
        // The target alone, for a command that cannot take --project.
        assert_eq!(
            env.target_flags_as_typed().as_deref(),
            Some("--prefix \"my dest\" --global")
        );
    }

    #[test]
    fn a_value_a_shell_reads_is_quoted_in_every_flag_a_hint_repeats() {
        for (value, said) in [
            ("a&b", "\"a&b\""),
            ("a;b", "\"a;b\""),
            ("a b", "\"a b\""),
            ("a(b)", "\"a(b)\""),
            ("a>b", "\"a>b\""),
            ("a|b", "\"a|b\""),
            ("plain", "plain"),
        ] {
            assert_eq!(shell_word(value), said);
        }
        let mut env = OpEnv::new(&NullSink, config(), Vec::new(), PathBuf::from("."));
        env.flags.prefix = Some(PathBuf::from("a&b"));
        env.flags.project = Some(PathBuf::from("p;q"));
        assert_eq!(
            env.destination_flags_as_typed().as_deref(),
            Some("--prefix \"a&b\" --project \"p;q\"")
        );
    }

    #[test]
    fn the_wanted_names_are_every_dependency_in_any_table_then_the_commands_own() {
        use crate::project::Constraint::Any;
        let project = Project {
            dependencies: vec![("a".to_string(), Any)],
            dev_dependencies: vec![("d".to_string(), Any)],
            groups: vec![("ci".to_string(), vec![("g".to_string(), Any)])],
            ..Project::default()
        };
        let own = ["new".to_string()];
        assert_eq!(wanted_names(Some(&project), &own), ["a", "d", "g", "new"]);
        assert_eq!(wanted_names(None, &own), ["new"]);
        assert_eq!(wanted_names(Some(&project), &[]), ["a", "d", "g"]);
    }

    #[test]
    fn a_relative_local_source_is_taken_from_the_directory_the_command_ran_in() {
        let cwd = std::env::temp_dir().join("lvpm-env-local-sources");
        let elsewhere = std::env::temp_dir().join("lvpm-env-elsewhere");
        let mut config = config();
        config.local_sources = vec![PathBuf::from("./packages"), elsewhere.clone()];
        let env = OpEnv::new(&NullSink, config, Vec::new(), cwd.clone());
        assert_eq!(env.local_sources(), [cwd.join("packages"), elsewhere]);
    }

    #[test]
    fn an_oci_source_with_no_pinned_package_is_warned_about_and_one_with_a_pin_is_not() {
        let feed = |name: &str, pinned: &[&str]| index::OciFeed {
            name: name.to_string(),
            url: crate::oci::FeedUrl::parse(&format!("oci://r:5000/{name}")).unwrap(),
            pinned: pinned.iter().map(|p| p.to_string()).collect(),
        };
        let recorded = crate::ops::testing::Recorded::default();
        let env = OpEnv::new(&recorded, config(), Vec::new(), PathBuf::from("."));
        env.warn_unpinned(&[feed("idle", &[]), feed("busy", &["alpha"])]);
        assert_eq!(
            recorded.events(),
            [Event::OciSourceUnpinned {
                name: "idle".to_string()
            }]
        );
    }

    fn config() -> Config {
        Config {
            cache: PathBuf::from("cache"),
            hooks_run: Vec::new(),
            local_sources: Vec::new(),
            venv_dir: PathBuf::from(".lv-venv"),
            log_dir: Default::default(),
            venv_title: Default::default(),
        }
    }

    fn labview_2026() -> LvTarget {
        LvTarget {
            version: 26.3,
            bitness: 64,
            path: PathBuf::from("LabVIEW 2026"),
        }
    }

    fn labview(year: u32, bitness: u8) -> LvTarget {
        LvTarget {
            version: f64::from(year - 2000),
            bitness,
            path: PathBuf::from(format!("LabVIEW {year} {bitness}")),
        }
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lvpm-env-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_command_that_names_no_destination_is_told_how_to_pick_one() {
        let env = OpEnv::new(
            &NullSink,
            config(),
            vec![labview_2026()],
            PathBuf::from("."),
        );
        let why = env.roots().unwrap_err().to_string();
        assert!(why.starts_with("pick a destination:"), "{why}");
    }

    #[test]
    fn a_global_with_no_version_picks_the_only_installation_and_none_is_an_error() {
        let mut env = OpEnv::new(
            &NullSink,
            config(),
            vec![labview_2026()],
            PathBuf::from("."),
        );
        env.flags.global = true;
        let Ok(Destination::Global(roots)) = env.destination() else {
            panic!("--global alone picks the one installation");
        };
        assert_eq!(roots.target.unwrap().path, labview_2026().path);

        env.targets.clear();
        let why = env.destination().unwrap_err().to_string();
        assert_eq!(why, "no LabVIEW installations detected");
    }

    #[test]
    fn a_headless_install_reads_the_manifests_labview_the_way_locking_does() {
        for (labview, said) in [
            ("soon", Some("not a minimum LabVIEW year")),
            ("26", Some("not a minimum LabVIEW year")),
            ("2030", Some("is older than")),
            ("2024", None),
        ] {
            let repo = scratch_dir(&format!("headless-{labview}"));
            std::fs::write(
                repo.join(project::FILE_NAME),
                format!("[project]\nlabview = \">={labview}\"\n"),
            )
            .unwrap();
            let mut env = OpEnv::new(&NullSink, config(), vec![labview_2026()], repo.clone());
            env.flags.labview_version = Some("2026".into());
            match (env.manifest_roots(&repo), said) {
                (Ok(_), None) => {}
                (Err(why), Some(said)) => {
                    assert!(format!("{why:#}").contains(said), "{labview}: {why:#}")
                }
                (outcome, _) => panic!("{labview}: {:?}", outcome.map(|_| ())),
            }
            let _ = std::fs::remove_dir_all(&repo);
        }
    }

    #[test]
    fn a_bitness_the_venv_is_not_bound_to_is_refused_and_a_matching_or_absent_one_is_not() {
        let repo = scratch_dir("venv-bitness");
        let bound = LvTarget {
            version: 26.3,
            ..labview(2026, 64)
        };
        let installed = vec![bound.clone(), labview(2026, 32)];
        venv::create(
            &repo,
            Some("2026"),
            None,
            Path::new(".lv-venv"),
            &installed[..1],
            &NullSink,
        )
        .unwrap();
        let mut env = OpEnv::new(&NullSink, config(), installed, repo.clone());

        env.flags.labview_bitness = Some(32);
        assert_eq!(
            env.find_venv().unwrap_err().to_string(),
            "this venv is bound to LabVIEW 2026 (64-bit)  v26.3, not the 32-bit LabVIEW \
             --labview-bitness asks for\n\
             hint: drop --labview-bitness, or run `lvpm venv remove` without it, then \
             `lvpm venv create --labview-bitness 32`"
        );

        for bits in [Some(64), None] {
            env.flags.labview_bitness = bits;
            let venv = env.find_venv().unwrap().expect("the venv is found");
            assert_eq!(venv.target.path, bound.path);
        }
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn a_labview_version_picks_among_the_injected_installations() {
        let mut env = OpEnv::new(
            &NullSink,
            config(),
            vec![labview_2026()],
            PathBuf::from("."),
        );
        env.flags.labview_version = Some("2026".into());
        assert_eq!(
            env.roots().unwrap().target.unwrap().path,
            labview_2026().path
        );

        env.targets.clear();
        let why = env.roots().unwrap_err().to_string();
        assert_eq!(why, "no LabVIEW installations detected");
    }

    #[test]
    fn a_manifests_minimum_still_lets_a_newer_year_of_the_requested_bitness_stand_in() {
        let repo = scratch_dir("headless-bitness");
        std::fs::write(
            repo.join(project::FILE_NAME),
            "[project]\nlabview = \">=2025\"\n",
        )
        .unwrap();
        let installed = vec![labview(2025, 64), labview(2026, 32)];
        let mut env = OpEnv::new(&NullSink, config(), installed, repo.clone());
        env.flags.labview_bitness = Some(32);
        let chosen = env.manifest_roots(&repo).unwrap().target.unwrap();
        assert_eq!(chosen.path, labview(2026, 32).path);

        // Without the flag the manifest's own year is the pick.
        env.flags.labview_bitness = None;
        let chosen = env.manifest_roots(&repo).unwrap().target.unwrap();
        assert_eq!(chosen.path, labview(2025, 64).path);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn a_bitness_picks_between_the_installations_of_a_version_through_roots() {
        let installed = vec![labview(2026, 64), labview(2026, 32)];
        let mut env = OpEnv::new(&NullSink, config(), installed, PathBuf::from("."));
        env.flags.labview_version = Some("2026".into());
        env.flags.labview_bitness = Some(32);
        assert_eq!(
            env.roots().unwrap().target.unwrap().path,
            labview(2026, 32).path
        );

        env.targets = vec![labview(2026, 64)];
        let why = env.roots().unwrap_err().to_string();
        assert_eq!(
            why,
            format!(
                "no 32-bit LabVIEW installation detected (detected: {})\n\
                 hint: `lvpm targets` lists detected LabVIEW installations",
                labview(2026, 64).label()
            )
        );
    }

    #[test]
    fn a_bitness_the_named_path_does_not_have_matches_no_target() {
        let installed = vec![labview(2026, 64), labview(2026, 32)];
        let mut env = OpEnv::new(&NullSink, config(), installed, PathBuf::from("."));
        env.flags.labview_version = Some(labview(2026, 64).path.display().to_string());
        env.flags.labview_bitness = Some(32);
        let why = env.roots().unwrap_err().to_string();
        assert!(why.starts_with("no LabVIEW target matching"), "{why}");
    }

    #[test]
    fn a_bitness_with_nothing_installed_still_says_nothing_is_installed() {
        let mut env = OpEnv::new(&NullSink, config(), Vec::new(), PathBuf::from("."));
        env.flags.labview_version = Some("2026".into());
        env.flags.labview_bitness = Some(32);
        let why = env.roots().unwrap_err().to_string();
        assert_eq!(why, "no LabVIEW installations detected");
    }

    #[test]
    fn the_manifest_is_looked_for_from_the_given_directory_not_the_process_one() {
        let root = scratch_dir("manifest");
        let deep = root.join("src").join("deep");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(
            root.join(project::FILE_NAME),
            "[project]\nname = \"found\"\n",
        )
        .unwrap();

        let mut env = OpEnv::new(&NullSink, config(), Vec::new(), deep.clone());
        let (path, found) = env.manifest_for(None, None).unwrap().unwrap();
        assert_eq!(path, root.join(project::FILE_NAME));
        assert_eq!(found.name.as_deref(), Some("found"));

        // A relative `--manifest` is relative to the given directory too.
        std::fs::write(deep.join("other.toml"), "[project]\nname = \"other\"\n").unwrap();
        let explicit = Path::new("other.toml");
        let (_, other) = env.manifest_for(Some(explicit), None).unwrap().unwrap();
        assert_eq!(other.name.as_deref(), Some("other"));

        // `--project` names the directory, and a directory without one has none.
        env.flags.project = Some(PathBuf::from("..").join(".."));
        let (via_project, _) = env.manifest_for(None, None).unwrap().unwrap();
        assert_eq!(
            via_project.canonicalize().unwrap(),
            root.join(project::FILE_NAME).canonicalize().unwrap()
        );
        env.flags.project = Some(PathBuf::from("src"));
        assert!(env.manifest_for(None, None).unwrap().is_none());

        let _ = std::fs::remove_dir_all(&root);
    }
}
