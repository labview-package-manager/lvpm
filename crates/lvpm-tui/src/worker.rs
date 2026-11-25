//! Runs core's operations off the UI thread, one job at a time, and hands
//! each result back over a channel the event loop polls.

use crate::destination::{EnvBuilder, Quiet};
use lvpm_core::cancel::CancelToken;
use lvpm_core::errors::Cancelled;
use lvpm_core::errors::EditError;
use lvpm_core::events::{Event, EventSink, Phase};
use lvpm_core::install::Manifest;
use lvpm_core::manifest_edit::DependencyTable;
use lvpm_core::ops::Destination;
use lvpm_core::ops::add_remove;
use lvpm_core::ops::install::{self, InstallOptions};
use lvpm_core::ops::query::{self, Catalog, PackageInfo};
use lvpm_core::target::{LvTarget, Roots};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Every job the worker can run.
#[derive(Debug, Clone)]
pub enum Job {
    /// Read the catalog and what the destination has installed; `refresh`
    /// revalidates the feeds as `--refresh` does.
    Load { refresh: bool },
    /// Read the details of one version of a package, as `lvpm info
    /// <name>@<version>` does.
    Info { name: String, version: String },
    /// Install `package`, `name@version`, as `lvpm install` does; a dry run
    /// only says what it would do. `cancel` stops it until files are written.
    /// In a project's venv, `exact` writes `name = "version"` to lvpm.toml,
    /// else the floor `name = ">=version"` at the version locked; elsewhere
    /// it is ignored.
    Install {
        package: String,
        exact: bool,
        dry_run: bool,
        allow_downgrade: bool,
        cancel: CancelToken,
    },
}

/// What a job sends: an install's events as they come, then its result.
#[derive(Debug)]
pub enum Done {
    /// One of core's events; the job runs on.
    Event(Event),
    /// The destination as resolved for this read, and the catalog with what
    /// that destination has installed, or core's error, whole.
    Loaded {
        destination: Result<Destination, String>,
        listing: Result<(Catalog, Vec<Manifest>), String>,
    },
    /// The details of `name` `version`, or core's error, whole.
    Info {
        name: String,
        version: String,
        info: Result<PackageInfo, String>,
    },
    /// An install, or its dry run, ended.
    Installed { dry_run: bool, outcome: Outcome },
}

/// How an operation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Cancelled,
    /// Core's error, whole.
    Failed(String),
}

/// Holds an install at the start of one phase until released, so a test sees
/// what the TUI shows meanwhile. Only a test sets one; a dry run passes it by.
#[derive(Debug)]
pub struct Park {
    at: Phase,
    state: Mutex<Gate>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct Gate {
    /// The install got to the phase.
    reached: bool,
    /// The install may go on.
    released: bool,
}

impl Park {
    pub fn new(at: Phase) -> Park {
        Park {
            at,
            state: Mutex::new(Gate::default()),
            changed: Condvar::new(),
        }
    }

    /// Block until the install is held; an install that never gets there
    /// fails the test instead of hanging it.
    pub fn wait_reached(&self) {
        let state = self.state.lock().expect("the park's lock");
        let limit = Duration::from_secs(30);
        let (held, waited) = (self.changed)
            .wait_timeout_while(state, limit, |gate| !gate.reached)
            .expect("the park's lock");
        // Unlocked first: a panic holding it would poison the park for teardown.
        drop(held);
        assert!(
            !waited.timed_out(),
            "the install never reached {:?}",
            self.at
        );
    }

    /// Let the install go on; it is not held again.
    pub fn release(&self) {
        self.state.lock().expect("the park's lock").released = true;
        self.changed.notify_all();
    }

    fn hold(&self, phase: Phase) {
        if phase != self.at {
            return;
        }
        let mut state = self.state.lock().expect("the park's lock");
        state.reached = true;
        self.changed.notify_all();
        let released = self.changed.wait_while(state, |gate| !gate.released);
        drop(released.expect("the park's lock"));
    }
}

/// Hands core's events to the UI as they come.
struct Forward<'a> {
    send: Sender<Done>,
    park: Option<&'a Park>,
}

impl EventSink for Forward<'_> {
    fn emit(&self, event: Event) {
        let phase = match &event {
            Event::PhaseStarted { phase } => Some(*phase),
            _ => None,
        };
        // Sent first, so the UI shows the phase the install is held at.
        self.send.send(Done::Event(event)).ok();
        if let (Some(phase), Some(park)) = (phase, self.park) {
            park.hold(phase);
        }
    }
}

/// Start `job` on its own thread; its result arrives on the receiver.
pub fn spawn(builder: &EnvBuilder, job: Job) -> Receiver<Done> {
    let (send, receive) = mpsc::channel();
    let builder = builder.clone();
    std::thread::spawn(move || {
        // The TUI has gone when nobody receives; the result is not needed then.
        send.send(run(&builder, job, &send)).ok();
    });
    receive
}

fn run(builder: &EnvBuilder, job: Job, send: &Sender<Done>) -> Done {
    match job {
        Job::Load { refresh } => {
            let sink = Forward {
                send: send.clone(),
                park: None,
            };
            let env = builder.env(&sink, refresh);
            let destination = builder.destination();
            let listing = query::catalog(&env, None).and_then(|catalog| {
                let installed = match destination.is_ok() {
                    true => query::list(&env)?.0,
                    false => Vec::new(),
                };
                Ok((catalog, installed))
            });
            Done::Loaded {
                destination,
                listing: listing.map_err(|error| format!("{error:#}")),
            }
        }
        Job::Info { name, version } => {
            let env = builder.env(&Quiet, false);
            let info = query::info(&env, &format!("{name}@{version}"));
            Done::Info {
                name,
                version,
                info: info.map_err(|error| format!("{error:#}")),
            }
        }
        Job::Install {
            package,
            exact,
            dry_run,
            allow_downgrade,
            cancel,
        } => {
            let park = builder.park.as_deref().filter(|_| !dry_run);
            let sink = Forward {
                send: send.clone(),
                park,
            };
            let mut env = builder.env(&sink, false);
            env.cancel = cancel;
            let opts = InstallOptions {
                dry_run,
                allow_downgrade,
                ..InstallOptions::default()
            };
            // In a project's venv the install is an add, as `lvpm install <name>`
            // is. The TUI sets the newest LabVIEW in the flags, so
            // `route::install_mode` would answer additive: test the venv instead.
            let destination = env.destination();
            // The TUI names the newest LabVIEW in its flags, which writes into
            // it only with --global: the TUI has no such flag, so with no venv
            // it refuses, naming the command that does.
            let refused = match &destination {
                Err(_) => match env.read_destination() {
                    Ok(Destination::Global(Roots {
                        target: Some(target),
                        ..
                    })) => Some(format!(
                        "outside a venv the TUI installs nothing into LabVIEW itself; \
                         run lvpm install {package} {}",
                        global_flags(&target, &builder.targets)
                    )),
                    _ => None,
                },
                Ok(_) => None,
            };
            let outcome = match (destination, refused) {
                (Ok(Destination::Venv(_)), _) => {
                    // A bare name: core writes the floor at the version it locks.
                    let spec = match exact {
                        true => package.as_str(),
                        false => package
                            .split_once('@')
                            .map_or(package.as_str(), |(name, _)| name),
                    };
                    match add_remove::add(&env, spec, DependencyTable::Dependencies, None, &opts) {
                        Ok(()) => Outcome::Done,
                        Err(EditError::Cancelled) => Outcome::Cancelled,
                        // `Other` may be a file that could not be put back: claim nothing.
                        Err(error @ EditError::Other(_)) => Outcome::Failed(format!("{error:#}")),
                        Err(error) => Outcome::Failed(format!(
                            "{error:#}\nlvpm.toml and lvpm.lock are unchanged"
                        )),
                    }
                }
                (_, Some(refusal)) => Outcome::Failed(refusal),
                (_, None) => match install::install(&env, Some(&package), &opts) {
                    Ok(()) => Outcome::Done,
                    Err(error) if error.chain().any(|cause| cause.is::<Cancelled>()) => {
                        Outcome::Cancelled
                    }
                    Err(error) => Outcome::Failed(format!("{error:#}")),
                },
            };
            Done::Installed { dry_run, outcome }
        }
    }
}

/// The flags that make `lvpm install` write into `target`: its year, and its
/// bitness when another detected installation has the same year.
fn global_flags(target: &LvTarget, detected: &[LvTarget]) -> String {
    let year = target.year();
    let shared = detected
        .iter()
        .any(|other| other.year() == year && other.bitness != target.bitness);
    match shared {
        true => format!(
            "--global --labview-version {year} --labview-bitness {}",
            target.bitness
        ),
        false => format!("--global --labview-version {year}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lvpm_core::config::Config;
    use lvpm_core::target::LvTarget;

    /// The install job's answer where the TUI has no venv, and whether it
    /// started anything, with `targets` detected (the newest first).
    fn install_without_a_venv(targets: &[(f64, u8)]) -> (Outcome, bool) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            log_dir: Default::default(),
            venv_title: Default::default(),
            cache: dir.path().join("cache"),
            hooks_run: Vec::new(),
            local_sources: Vec::new(),
            venv_dir: ".lv-venv".into(),
        };
        let targets = targets
            .iter()
            .map(|&(version, bitness)| LvTarget {
                version,
                bitness,
                path: dir.path().join(format!("LabVIEW {version} {bitness}")),
            })
            .collect::<Vec<_>>();
        let builder = EnvBuilder::new(config, targets.clone(), dir.path().join("work"));
        let job = Job::Install {
            package: "alpha".into(),
            exact: false,
            dry_run: false,
            allow_downgrade: false,
            cancel: CancelToken::default(),
        };
        let (send, receive) = mpsc::channel();
        let Done::Installed { outcome, .. } = run(&builder, job, &send) else {
            panic!("an install job ends with its result");
        };
        let wrote = receive.try_recv().is_ok()
            || dir.path().join("cache").exists()
            || targets.iter().any(|target| target.path.exists());
        (outcome, wrote)
    }

    #[test]
    fn an_install_with_no_venv_is_refused_and_writes_nothing_into_labview() {
        let (outcome, wrote) = install_without_a_venv(&[(26.3, 64)]);
        assert_eq!(
            outcome,
            Outcome::Failed(
                "outside a venv the TUI installs nothing into LabVIEW itself; \
                 run lvpm install alpha --global --labview-version 2026"
                    .into()
            )
        );
        assert!(!wrote, "nothing was started or created");
    }

    #[test]
    fn the_refusal_names_the_bitness_when_another_installation_shares_the_year() {
        let (outcome, _) = install_without_a_venv(&[(26.3, 64), (26.3, 32), (25.3, 64)]);
        assert_eq!(
            outcome,
            Outcome::Failed(
                "outside a venv the TUI installs nothing into LabVIEW itself; \
                 run lvpm install alpha --global --labview-version 2026 --labview-bitness 64"
                    .into()
            )
        );
    }
}
