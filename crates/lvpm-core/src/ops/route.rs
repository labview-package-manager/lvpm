//! Which operation an `install` request means. One pure function, so that the
//! CLI and any other front end route it the same way.
//!
//! In a project, `install` with no package is a sync of the project to its
//! lock, and with a package it is an `add`: the destination flags only pick
//! where the project syncs. With `--no-project`, or outside a project, it puts
//! that package where it was asked to and leaves the project alone.

use super::env::OpEnv;
use crate::errors::{LockIncompatible, RouteError};
use anyhow::anyhow;
use std::path::Path;

/// What an install request means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMode {
    /// Make the destination equal `lvpm.lock`: bare install, in a project.
    Sync,
    /// Put the named package in `lvpm.toml`, lock and sync, as `add` does: a
    /// named install in a project, unless `--no-project`.
    Add,
    /// Put the named package where it was asked to go, and what it needs.
    Additive,
}

/// What an install request asks, before any flag is read as anything else.
#[derive(Debug, Clone, Copy, Default)]
pub struct InstallRequest<'a> {
    pub package: Option<&'a str>,
    /// `--manifest`: the dependencies of another manifest.
    pub manifest: Option<&'a Path>,
    /// `--no-deps`.
    pub no_deps: bool,
    /// `--no-project`: install directly, leaving the project's files alone.
    pub no_project: bool,
}

/// The mode of `request`, in the project `env` runs in, if any.
///
/// A named package is an [`InstallMode::Add`] in a project, whatever the
/// destination flags (`--global`, `--prefix`, `--labview-version`) say about
/// where the project syncs, unless `--no-project`; it is additive otherwise.
/// With no package, in a project (with or without a destination flag), it is a
/// sync. Add and sync both take the lock's full set, so `--no-deps` and
/// `--manifest` are refused for them. With no package outside a project there
/// is nothing to install.
pub fn install_mode(env: &OpEnv, request: &InstallRequest) -> Result<InstallMode, RouteError> {
    let mode = match request.package {
        Some(_) => {
            if request.no_project || env.manifest_for(None, None)?.is_none() {
                // The one package, installed where it was asked to go, has no
                // use for another manifest's dependencies.
                return match request.manifest {
                    Some(_) => Err(RouteError::Other(anyhow!(
                        "--manifest cannot be used with a package name"
                    ))),
                    None => Ok(InstallMode::Additive),
                };
            }
            InstallMode::Add
        }
        None => {
            if env.manifest_for(request.manifest, None)?.is_none() {
                return Err(RouteError::NoPackage {
                    searched_from: env.cwd.clone(),
                });
            }
            InstallMode::Sync
        }
    };
    let unsupported = match (request.no_deps, request.manifest) {
        (true, _) => Some(LockIncompatible::NoDeps),
        (false, Some(_)) => Some(LockIncompatible::Manifest),
        (false, None) => None,
    };
    match unsupported {
        Some(option) => Err(RouteError::NotWithLock {
            option,
            adding: mode == InstallMode::Add,
        }),
        None => Ok(mode),
    }
}

/// What an uninstall request means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UninstallMode {
    /// Take the named package out of `lvpm.toml`, lock and sync, as `remove`
    /// does: a named uninstall in a project, unless `--no-project`.
    Project,
    /// Take the named package, or everything, out of the destination and leave
    /// the project's files alone.
    Direct,
}

/// What an uninstall request asks, before any flag is read as anything else.
/// The `*_flag` fields hold the flag as typed, for the error that names it.
#[derive(Debug, Clone, Copy, Default)]
pub struct UninstallRequest<'a> {
    pub package: Option<&'a str>,
    /// `--all`.
    pub all: bool,
    /// `--no-project`: uninstall directly, leaving the project's files alone.
    pub no_project: bool,
    /// `--keep-deps`.
    pub keep_deps: bool,
    /// `--force`.
    pub force: bool,
    /// `--dev` or `--group`: the table a project uninstall takes it out of.
    pub table_flag: Option<&'static str>,
    /// One of `--dry-run`, `--no-linkfix`, `--relink`, `--hooks`: how a project
    /// uninstall syncs.
    pub apply_flag: Option<&'static str>,
}

/// The mode of `request`, in the project `env` runs in, if any.
///
/// A named package is [`UninstallMode::Project`] in a project, whatever the
/// destination flags say about where the project syncs, unless `--no-project`
/// or `--all`; everything else is direct, as it always was. A project uninstall
/// takes `--with-deps` (what happens anyway) but refuses `--keep-deps` and
/// `--force`: the lock decides what stays, and a sync would undo either. A
/// direct one refuses the flags only a project uninstall has.
pub fn uninstall_mode(
    env: &OpEnv,
    request: &UninstallRequest,
) -> Result<UninstallMode, RouteError> {
    let in_a_project = request.package.is_some()
        && !request.all
        && !request.no_project
        && env.manifest_for(None, None)?.is_some();
    if in_a_project {
        let refused = [
            (request.keep_deps, "--keep-deps"),
            (request.force, "--force"),
        ];
        return match refused.into_iter().find(|(given, _)| *given) {
            Some((_, flag)) => Err(RouteError::UninstallNotWithLock { flag }),
            None => Ok(UninstallMode::Project),
        };
    }
    match request.table_flag.or(request.apply_flag) {
        Some(flag) => Err(RouteError::OnlyInAProject { flag }),
        None => Ok(UninstallMode::Direct),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::events::NullSink;
    use std::path::PathBuf;

    /// A directory that is a project when `manifest` says so.
    struct Dir {
        root: PathBuf,
    }

    impl Dir {
        fn new(tag: &str, manifest: Option<&str>) -> Dir {
            let root =
                std::env::temp_dir().join(format!("lvpm-route-test-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            if let Some(text) = manifest {
                std::fs::write(root.join(crate::project::FILE_NAME), text).unwrap();
            }
            Dir { root }
        }

        fn env<'a>(&self, sink: &'a NullSink) -> OpEnv<'a> {
            let config = Config {
                log_dir: Default::default(),
                venv_title: Default::default(),
                cache: self.root.join("cache"),
                hooks_run: Vec::new(),
                local_sources: Vec::new(),
                venv_dir: PathBuf::from(".lv-venv"),
            };
            OpEnv::new(sink, config, Vec::new(), self.root.clone())
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    const PROJECT: &str = "[project]\nlabview = \">=2024\"\n";

    fn mode(dir: &Dir, request: InstallRequest) -> Result<InstallMode, RouteError> {
        install_mode(&dir.env(&NullSink), &request)
    }

    #[test]
    fn a_bare_install_in_a_project_is_a_sync() {
        let dir = Dir::new("bare", Some(PROJECT));
        assert_eq!(
            mode(&dir, InstallRequest::default()).unwrap(),
            InstallMode::Sync
        );
    }

    #[test]
    fn a_bare_install_is_a_sync_whatever_the_destination_flag() {
        let dir = Dir::new("flags", Some(PROJECT));
        let sink = NullSink;
        for (global, prefix) in [(true, None), (false, Some(PathBuf::from("dest")))] {
            let mut env = dir.env(&sink);
            env.flags.global = global;
            env.flags.prefix = prefix;
            assert_eq!(
                install_mode(&env, &InstallRequest::default()).unwrap(),
                InstallMode::Sync
            );
        }
    }

    const GAMMA: InstallRequest = InstallRequest {
        package: Some("gamma"),
        manifest: None,
        no_deps: false,
        no_project: false,
    };

    #[test]
    fn a_named_package_in_a_project_is_an_add() {
        let dir = Dir::new("named-in", Some(PROJECT));
        assert_eq!(mode(&dir, GAMMA).unwrap(), InstallMode::Add);
    }

    #[test]
    fn a_named_package_outside_a_project_is_additive() {
        let dir = Dir::new("named-out", None);
        assert_eq!(mode(&dir, GAMMA).unwrap(), InstallMode::Additive);
    }

    /// A named package in a project (a directory of its own, named by `tag`)
    /// with `set` done to the environment's flags.
    fn named_in_a_project_with(tag: &str, set: fn(&mut OpEnv)) -> InstallMode {
        let dir = Dir::new(tag, Some(PROJECT));
        let sink = NullSink;
        let mut env = dir.env(&sink);
        set(&mut env);
        install_mode(&env, &GAMMA).unwrap()
    }

    #[test]
    fn named_in_a_project_with_global_is_add() {
        let mode = named_in_a_project_with("named-global", |env| env.flags.global = true);
        assert_eq!(mode, InstallMode::Add);
    }

    #[test]
    fn named_in_a_project_with_labview_version_is_add() {
        let mode = named_in_a_project_with("named-labview-version", |env| {
            env.flags.labview_version = Some("2026".into())
        });
        assert_eq!(mode, InstallMode::Add);
    }

    #[test]
    fn named_in_a_project_with_prefix_is_add() {
        let mode = named_in_a_project_with("named-prefix", |env| {
            env.flags.prefix = Some(PathBuf::from("dest"))
        });
        assert_eq!(mode, InstallMode::Add);
    }

    #[test]
    fn no_project_is_additive_in_a_project_and_outside_one() {
        let request = InstallRequest {
            no_project: true,
            ..GAMMA
        };
        let in_a_project = Dir::new("no-project-in", Some(PROJECT));
        assert_eq!(mode(&in_a_project, request).unwrap(), InstallMode::Additive);
        let outside = Dir::new("no-project-out", None);
        assert_eq!(mode(&outside, request).unwrap(), InstallMode::Additive);
    }

    #[test]
    fn a_manifest_is_refused_with_a_named_package_that_would_install_directly() {
        let request = InstallRequest {
            manifest: Some(Path::new("other.toml")),
            ..GAMMA
        };
        let in_a_project = Dir::new("manifest-opted-out", Some(PROJECT));
        let opted_out = InstallRequest {
            no_project: true,
            ..request
        };
        let outside = Dir::new("manifest-outside", None);
        for (dir, request) in [(&in_a_project, opted_out), (&outside, request)] {
            let why = mode(dir, request).unwrap_err();
            assert_eq!(
                why.to_string(),
                "--manifest cannot be used with a package name"
            );
        }
    }

    #[test]
    fn a_manifest_with_a_named_package_in_a_project_is_refused_as_a_lock_cannot_honour_it() {
        let dir = Dir::new("manifest-named", Some(PROJECT));
        let request = InstallRequest {
            manifest: Some(Path::new("other.toml")),
            ..GAMMA
        };
        assert!(matches!(
            mode(&dir, request),
            Err(RouteError::NotWithLock {
                option: LockIncompatible::Manifest,
                adding: true
            })
        ));
    }

    #[test]
    fn a_project_flag_is_no_destination_so_a_named_package_is_still_an_add() {
        let dir = Dir::new("named-project-flag", Some(PROJECT));
        let sink = NullSink;
        let mut env = dir.env(&sink);
        env.flags.project = Some(PathBuf::from("."));
        assert_eq!(install_mode(&env, &GAMMA).unwrap(), InstallMode::Add);
    }

    #[test]
    fn a_bare_install_outside_a_project_has_nothing_to_install() {
        let dir = Dir::new("outside", None);
        let why = mode(&dir, InstallRequest::default()).unwrap_err();
        assert!(
            matches!(&why, RouteError::NoPackage { searched_from } if *searched_from == dir.root),
            "{why:?}"
        );
        assert_eq!(why.to_string(), "give a package name");
    }

    #[test]
    fn no_deps_and_manifest_are_refused_for_a_sync() {
        let dir = Dir::new("refused", Some(PROJECT));
        let no_deps = InstallRequest {
            no_deps: true,
            ..InstallRequest::default()
        };
        assert!(matches!(
            mode(&dir, no_deps),
            Err(RouteError::NotWithLock {
                option: LockIncompatible::NoDeps,
                adding: false
            })
        ));

        let manifest = InstallRequest {
            manifest: Some(Path::new(crate::project::FILE_NAME)),
            ..InstallRequest::default()
        };
        assert!(matches!(
            mode(&dir, manifest),
            Err(RouteError::NotWithLock {
                option: LockIncompatible::Manifest,
                adding: false
            })
        ));
    }

    #[test]
    fn no_deps_is_refused_for_an_add_and_fine_for_a_named_package_that_stays_additive() {
        let request = InstallRequest {
            no_deps: true,
            ..GAMMA
        };
        let in_a_project = Dir::new("named-no-deps", Some(PROJECT));
        assert!(matches!(
            mode(&in_a_project, request),
            Err(RouteError::NotWithLock {
                option: LockIncompatible::NoDeps,
                adding: true
            })
        ));
        let outside = Dir::new("named-no-deps-out", None);
        assert_eq!(mode(&outside, request).unwrap(), InstallMode::Additive);
        let opted_out = InstallRequest {
            no_project: true,
            ..request
        };
        assert_eq!(
            mode(&in_a_project, opted_out).unwrap(),
            InstallMode::Additive
        );
    }

    const ALPHA: UninstallRequest = UninstallRequest {
        package: Some("alpha"),
        all: false,
        no_project: false,
        keep_deps: false,
        force: false,
        table_flag: None,
        apply_flag: None,
    };

    fn uninstall(dir: &Dir, request: UninstallRequest) -> Result<UninstallMode, RouteError> {
        uninstall_mode(&dir.env(&NullSink), &request)
    }

    #[test]
    fn a_named_uninstall_in_a_project_is_project_with_or_without_a_destination_flag() {
        let dir = Dir::new("uninstall-named", Some(PROJECT));
        assert_eq!(uninstall(&dir, ALPHA).unwrap(), UninstallMode::Project);
        let sink = NullSink;
        let flagged: [fn(&mut OpEnv); 3] = [
            |env| env.flags.global = true,
            |env| env.flags.prefix = Some(PathBuf::from("dest")),
            |env| env.flags.labview_version = Some("2026".into()),
        ];
        for set in flagged {
            let mut env = dir.env(&sink);
            set(&mut env);
            assert_eq!(
                uninstall_mode(&env, &ALPHA).unwrap(),
                UninstallMode::Project,
                "{:?}",
                env.flags
            );
        }
    }

    #[test]
    fn no_project_all_bare_and_no_project_at_all_are_direct() {
        let in_a_project = Dir::new("uninstall-direct", Some(PROJECT));
        let outside = Dir::new("uninstall-direct-out", None);
        let direct = [
            UninstallRequest {
                no_project: true,
                ..ALPHA
            },
            UninstallRequest {
                package: None,
                all: true,
                ..ALPHA
            },
            UninstallRequest {
                package: None,
                ..ALPHA
            },
        ];
        for request in direct {
            assert_eq!(
                uninstall(&in_a_project, request).unwrap(),
                UninstallMode::Direct
            );
            assert_eq!(uninstall(&outside, request).unwrap(), UninstallMode::Direct);
        }
        assert_eq!(uninstall(&outside, ALPHA).unwrap(), UninstallMode::Direct);
    }

    #[test]
    fn keep_deps_and_force_are_refused_in_a_project_and_fine_for_a_direct_uninstall() {
        let in_a_project = Dir::new("uninstall-lock", Some(PROJECT));
        for (request, flag) in [
            (
                UninstallRequest {
                    keep_deps: true,
                    ..ALPHA
                },
                "--keep-deps",
            ),
            (
                UninstallRequest {
                    force: true,
                    ..ALPHA
                },
                "--force",
            ),
        ] {
            let why = uninstall(&in_a_project, request).unwrap_err();
            assert!(
                matches!(why, RouteError::UninstallNotWithLock { flag: refused } if refused == flag),
                "{why:?}"
            );
            let opted_out = UninstallRequest {
                no_project: true,
                ..request
            };
            assert_eq!(
                uninstall(&in_a_project, opted_out).unwrap(),
                UninstallMode::Direct
            );
        }
    }

    #[test]
    fn a_project_only_flag_is_refused_on_a_direct_uninstall_and_taken_in_a_project() {
        let in_a_project = Dir::new("uninstall-project-flag", Some(PROJECT));
        let outside = Dir::new("uninstall-project-flag-out", None);
        for flag in [
            "--dev",
            "--group",
            "--dry-run",
            "--no-linkfix",
            "--relink",
            "--hooks",
        ] {
            let table = flag == "--dev" || flag == "--group";
            let request = UninstallRequest {
                table_flag: table.then_some(flag),
                apply_flag: (!table).then_some(flag),
                ..ALPHA
            };
            assert_eq!(
                uninstall(&in_a_project, request).unwrap(),
                UninstallMode::Project,
                "{flag}"
            );
            let direct = [
                (
                    &in_a_project,
                    UninstallRequest {
                        no_project: true,
                        ..request
                    },
                ),
                (
                    &in_a_project,
                    UninstallRequest {
                        package: None,
                        all: true,
                        ..request
                    },
                ),
                (
                    &in_a_project,
                    UninstallRequest {
                        package: None,
                        ..request
                    },
                ),
                (&outside, request),
            ];
            for (dir, request) in direct {
                let why = uninstall(dir, request).unwrap_err();
                assert!(
                    matches!(why, RouteError::OnlyInAProject { flag: refused } if refused == flag),
                    "{flag}: {why:?}"
                );
            }
        }
    }
}
