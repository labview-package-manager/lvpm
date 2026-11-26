//! The load check after linkfix. Linkfix marks a VI source-only by setting the
//! flag and dropping its compiled code; on some files saved in an older
//! LabVIEW the venv's LabVIEW then loads them broken (issue #77). So a venv
//! install opens the VIs linkfix made source-only once in the venv's LabVIEW,
//! saves nothing, and warns per package of the broken ones.

use super::env::OpEnv;
use super::resave::{self, is_lv_file};
use super::session::InstallSession;
use crate::events::Event;
use std::path::{Path, PathBuf};

/// Check `separated`, the files this install's linkfix made source-only. Only
/// a venv has its own LabVIEW to check them in. A LabVIEW that does not start
/// or a check that stops is a warning: the install stands.
pub(super) fn after_linkfix(env: &OpEnv, session: &InstallSession, separated: &[PathBuf]) {
    let Some(venv) = session.venv() else { return };
    let files: Vec<PathBuf> = separated
        .iter()
        .filter(|f| is_lv_file(f))
        .cloned()
        .collect();
    if files.is_empty() {
        return;
    }
    env.sink
        .emit(Event::LoadCheckStarting { files: files.len() });
    let broken = resave::launcher()
        .start(venv, env.headless, env.sink)
        .and_then(|mut labview| labview.broken(&env.work_dir(), &files));
    let broken = match broken {
        Ok(broken) => broken,
        Err(why) => {
            return env.sink.emit(Event::LoadCheckIncomplete {
                error: format!("{why:#}"),
            });
        }
    };
    let packages = venv.packages_dir();
    let mut reported = 0;
    for (package, checked) in by_package(&packages, &files) {
        let broken: Vec<PathBuf> = broken
            .iter()
            .filter(|b| package_of(&packages, b).as_deref() == Some(package.as_str()))
            .cloned()
            .collect();
        if !broken.is_empty() {
            reported += 1;
            env.sink.emit(Event::LoadCheckBroken {
                package,
                broken,
                checked,
            });
        }
    }
    env.sink.emit(Event::LoadCheckDone {
        checked: files.len(),
        broken: reported,
    });
}

/// The package a file under `<venv>/packages/<package>/...` belongs to.
fn package_of(packages: &Path, file: &Path) -> Option<String> {
    let rest = file.strip_prefix(packages).ok()?;
    Some(
        rest.components()
            .next()?
            .as_os_str()
            .to_string_lossy()
            .into_owned(),
    )
}

/// How many of `files` each package has, in the order the packages come.
fn by_package(packages: &Path, files: &[PathBuf]) -> Vec<(String, usize)> {
    let mut out: Vec<(String, usize)> = Vec::new();
    for package in files.iter().filter_map(|f| package_of(packages, f)) {
        match out.iter_mut().find(|(p, _)| *p == package) {
            Some((_, n)) => *n += 1,
            None => out.push((package, 1)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::resave::fake::Fake;
    use crate::config::Config;
    use crate::events::Event;
    use crate::ops::env::OpEnv;
    use crate::ops::install::{InstallOptions, install};
    use crate::ops::testing::{self, Recorded};
    use std::path::{Path, PathBuf};

    fn env_on<'a>(sink: &'a Recorded, repo: &Path) -> OpEnv<'a> {
        let config = Config {
            log_dir: Default::default(),
            venv_title: Default::default(),
            cache: repo.join("cache"),
            hooks_run: Vec::new(),
            local_sources: vec![repo.join("packages")],
            venv_dir: PathBuf::from(".lv-venv"),
        };
        OpEnv::new(
            sink,
            config,
            vec![testing::labview_2026()],
            repo.to_path_buf(),
        )
    }

    /// A repo whose `packages` hold `alpha` (a.vi with compiled code, which
    /// linkfix separates, and clean.vi without) and `beta` (b.vi with).
    fn repo(tag: &str) -> PathBuf {
        let repo = testing::scratch_dir(tag);
        let packages = repo.join("packages");
        std::fs::create_dir_all(&packages).unwrap();
        testing::venv_in(&repo);
        let (compiled, clean) = (crate::linkfix::vi_with_compiled_code(), testing_clean_vi());
        testing::vip_of(
            &packages,
            "alpha",
            "1.0",
            "",
            &[("a/a.vi", compiled.clone()), ("a/clean.vi", clean)],
        );
        testing::vip_of(
            &packages,
            "beta",
            "1.0",
            "alpha >= 1.0",
            &[("b/b.vi", compiled)],
        );
        repo
    }

    fn testing_clean_vi() -> Vec<u8> {
        std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lv-src/relink-package.vi"))
            .unwrap()
    }

    fn of<T>(events: &[Event], pick: impl Fn(&Event) -> Option<T>) -> Vec<T> {
        events.iter().filter_map(pick).collect()
    }

    fn broken(events: &[Event]) -> Vec<(String, Vec<String>, usize)> {
        of(events, |e| match e {
            Event::LoadCheckBroken {
                package,
                broken,
                checked,
            } => Some((
                package.clone(),
                broken
                    .iter()
                    .map(|b| b.file_name().unwrap().to_string_lossy().into_owned())
                    .collect(),
                *checked,
            )),
            _ => None,
        })
    }

    #[test]
    fn a_venv_install_checks_only_the_vis_linkfix_made_source_only_in_one_labview() {
        let repo = repo("load-check-clean");
        let fake = Fake::saving().install();
        let sink = Recorded::default();

        install(
            &env_on(&sink, &repo),
            Some("beta"),
            &InstallOptions::default(),
        )
        .unwrap();

        let events = sink.events();
        assert!(
            events.contains(&Event::LoadCheckStarting { files: 2 }),
            "{events:?}"
        );
        assert!(events.contains(&Event::LoadCheckDone {
            checked: 2,
            broken: 0
        }));
        assert!(broken(&events).is_empty());
        let log = fake.log();
        assert_eq!((log.starts, log.closes), (1, 1));
        let mut checked = log.checks.concat();
        checked.sort();
        assert_eq!(checked, ["a.vi", "b.vi"]);
    }

    #[test]
    fn a_broken_vi_is_one_warning_for_its_package_and_the_install_stands() {
        let repo = repo("load-check-broken");
        Fake::broken(&["b.vi"]).install();
        let sink = Recorded::default();

        install(
            &env_on(&sink, &repo),
            Some("beta"),
            &InstallOptions::default(),
        )
        .unwrap();

        let events = sink.events();
        assert_eq!(
            broken(&events),
            [("beta".to_string(), vec!["b.vi".to_string()], 1)]
        );
        assert!(events.contains(&Event::LoadCheckDone {
            checked: 2,
            broken: 1
        }));
    }

    #[test]
    fn a_labview_that_cannot_start_leaves_the_check_incomplete_not_clean() {
        let repo = repo("load-check-nostart");
        Fake::cannot_start().install();
        let sink = Recorded::default();

        install(
            &env_on(&sink, &repo),
            Some("beta"),
            &InstallOptions::default(),
        )
        .unwrap();

        let events = sink.events();
        assert!(events.contains(&Event::LoadCheckIncomplete {
            error: "no LabVIEW.exe in LabVIEW 2026".into()
        }));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::LoadCheckDone { .. }))
        );
    }

    #[test]
    fn a_check_that_stops_part_way_is_incomplete_and_closes_the_labview() {
        let repo = repo("load-check-stops");
        let fake = Fake::check_fails().install();
        let sink = Recorded::default();

        install(
            &env_on(&sink, &repo),
            Some("beta"),
            &InstallOptions::default(),
        )
        .unwrap();

        let events = sink.events();
        assert!(
            of(&events, |e| match e {
                Event::LoadCheckIncomplete { error } => Some(error.clone()),
                _ => None,
            })
            .iter()
            .any(|e| e.contains("error 56"))
        );
        assert!(broken(&events).is_empty());
        assert_eq!(fake.log().closes, 1);
    }

    #[test]
    fn no_check_without_linkfix_on_a_dry_run_or_with_relink() {
        let cases = [
            InstallOptions {
                no_linkfix: true,
                ..InstallOptions::default()
            },
            InstallOptions {
                dry_run: true,
                ..InstallOptions::default()
            },
        ];
        for (i, opts) in cases.into_iter().enumerate() {
            let repo = repo(&format!("load-check-off-{i}"));
            let fake = Fake::saving().install();
            let sink = Recorded::default();

            install(&env_on(&sink, &repo), Some("beta"), &opts).unwrap();

            let events = sink.events();
            assert!(
                !events
                    .iter()
                    .any(|e| matches!(e, Event::LoadCheckStarting { .. })),
                "case {i}"
            );
            assert_eq!(fake.log().starts, 0, "case {i}");
        }
    }

    #[test]
    fn an_install_into_a_prefix_has_no_labview_to_check_in() {
        let repo = repo("load-check-prefix");
        let fake = Fake::saving().install();
        let sink = Recorded::default();
        let mut env = env_on(&sink, &repo);
        env.flags.prefix = Some(repo.join("dest"));

        install(&env, Some("beta"), &InstallOptions::default()).unwrap();

        assert!(
            !sink
                .events()
                .iter()
                .any(|e| matches!(e, Event::LoadCheckStarting { .. }))
        );
        assert_eq!(fake.log().starts, 0);
    }

    #[test]
    fn files_are_grouped_by_their_package_folder() {
        let packages = Path::new("v").join("packages");
        let files = [
            packages.join("a").join("1").join("x.vi"),
            packages.join("b").join("1").join("y.vi"),
            packages.join("a").join("1").join("z.vi"),
            PathBuf::from("elsewhere.vi"),
        ];
        assert_eq!(
            super::by_package(&packages, &files),
            [("a".to_string(), 2), ("b".to_string(), 1)]
        );
    }
}
