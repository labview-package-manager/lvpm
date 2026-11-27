//! The venv workflow, end to end, through the lvpm binary: create, find,
//! install from the manifest and by name, upgrade, uninstall, groups, the
//! launch check, shadowing, hooks and machine locations, import, and the log.
//!
//! No LabVIEW is needed. A debug build of lvpm takes `LVPM_FAKE_LABVIEW` as
//! the one LabVIEW installed; each test makes a fake "LabVIEW 2099" of its own
//! (a year no real installation has, so nothing of the machine's own
//! LabVIEW is read), a local package folder, a cache, a log folder and a
//! project, all in a temp folder. What does need a running LabVIEW (relink,
//! hooks run in the venv's IDE, the overlay loading) is the container suite's
//! (ci/e2e/).
//!
//! Ported from upstream (ThatsTheEnd/lvpm daa141a). Upstream calls the hook
//! `LVPM_TEST_LABVIEW`, which the fork already uses to gate its @labview
//! scenarios. Where the fork words a result differently, the assertion takes
//! the fork's words; where the fork does not do the thing yet, the test is
//! ignored with the number of the slice it waits on.

// A release build compiles the hook out, so these would reach the real LabVIEW.
#![cfg(debug_assertions)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(windows)]
const EXE: &str = "LabVIEW.exe";
#[cfg(not(windows))]
const EXE: &str = "labview";

/// One test's world: a fake LabVIEW, packages, and a project.
struct Lab {
    root: PathBuf,
}

/// What a command printed (stdout, then stderr) and whether it succeeded.
struct Run {
    ok: bool,
    text: String,
    stderr: String,
}

impl Lab {
    fn new(name: &str) -> Lab {
        let root =
            std::env::temp_dir().join(format!("lvpm-venv-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let labview = root.join("labview");
        std::fs::create_dir_all(labview.join("vi.lib")).unwrap();
        // Present, so the launch check finds it; empty, so nothing can ever start.
        std::fs::write(labview.join(EXE), b"").unwrap();
        std::fs::write(
            labview.join("LabVIEW.ini"),
            "[LabVIEW]\r\nRecentFiles.projectPathList=\"C:\\elsewhere\\x.lvproj\"\r\nserver.tcp.port=3363\r\n",
        )
        .unwrap();
        for dir in ["packages", "project", "home"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        Lab { root }
    }

    fn project(&self) -> PathBuf {
        self.root.join("project")
    }

    fn venv(&self) -> PathBuf {
        self.project().join(".lv-venv")
    }

    fn manifest_path(&self) -> PathBuf {
        self.project().join("lvpm.toml")
    }

    fn manifest(&self) -> String {
        std::fs::read_to_string(self.manifest_path()).unwrap()
    }

    /// `lvpm.toml` for LabVIEW 2099 and the local packages only, then `body`.
    fn write_manifest(&self, body: &str) {
        std::fs::write(
            self.manifest_path(),
            format!("[project]\nname = \"demo\"\nlabview = \"2099\"\n\n[sources]\ndefaults = false\n\n{body}"),
        )
        .unwrap();
    }

    fn record(&self, name: &str) -> PathBuf {
        self.venv().join("installed").join(format!("{name}.json"))
    }

    /// Where a package's file `<vi.lib>/<name>/<file>` lands in the venv.
    fn file(&self, name: &str, file: &str) -> PathBuf {
        self.venv()
            .join("packages")
            .join(name)
            .join("1")
            .join("vi.lib")
            .join(name)
            .join(file)
    }

    /// A `.vip` in the local package folder: `<vi.lib>/<name>/<name>.txt`,
    /// plus `extra` spec text and `members`.
    fn package_with(
        &self,
        name: &str,
        version: &str,
        requires: &str,
        extra: &str,
        members: &[(&str, &[u8])],
    ) {
        let file = std::fs::File::create(
            self.root
                .join("packages")
                .join(format!("{name}-{version}.vip")),
        )
        .unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("spec", options).unwrap();
        write!(
            zip,
            "[Package]\nName=\"{name}\"\nVersion=\"{version}\"\n[Dependencies]\nRequires=\"{requires}\"\n\n\
             [File Group 0]\nTarget Dir=\"<vi.lib>/{name}\"\nReplace Mode=\"Always\"\nNum Files=1\nFile 0=\"{name}.txt\"\n\n{extra}"
        )
        .unwrap();
        zip.start_file(format!("File Group 0/{name}.txt"), options)
            .unwrap();
        writeln!(zip, "{name} {version}").unwrap();
        for (path, bytes) in members {
            zip.start_file(*path, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }

    fn package(&self, name: &str, version: &str, requires: &str) {
        self.package_with(name, version, requires, "", &[]);
    }

    fn package_with_menus(&self, name: &str, menus: &[(&str, &[u8])]) {
        let mut extra = format!(
            "[File Group 1]\nTarget Dir=\"<menus>\"\nReplace Mode=\"Always\"\nNum Files={}\n",
            menus.len()
        );
        let mut paths = Vec::new();
        for (index, (path, _)) in menus.iter().enumerate() {
            extra.push_str(&format!("File {index}=\"{path}\"\n"));
            paths.push(format!("File Group 1/{path}"));
        }
        let members: Vec<(&str, &[u8])> = paths
            .iter()
            .zip(menus)
            .map(|(path, (_, bytes))| (path.as_str(), *bytes))
            .collect();
        self.package_with(name, "1.0.0.1", "", &extra, &members);
    }

    fn run_in(&self, dir: &Path, args: &[&str]) -> Run {
        let fake = format!("99.0,64,{}", self.root.join("labview").display());
        self.run_with_fake(dir, &fake, args)
    }

    /// `run_in`, with `LVPM_FAKE_LABVIEW` set to `fake`.
    fn run_with_fake(&self, dir: &Path, fake: &str, args: &[&str]) -> Run {
        self.run_program(Path::new(env!("CARGO_BIN_EXE_lvpm")), dir, fake, args)
    }

    /// `run_with_fake`, with the lvpm executable at `program`.
    fn run_program(&self, program: &Path, dir: &Path, fake: &str, args: &[&str]) -> Run {
        let home = self.root.join("home");
        let mut cmd = Command::new(program);
        // The developer's own lvpm settings stay out of the test.
        for (key, _) in std::env::vars_os() {
            // Config reads LVPM_CONFIG_<KEY>, else lvpm_config_<key>.
            if key
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("LVPM_CONFIG_")
            {
                cmd.env_remove(key);
            }
        }
        // The user config, the LabVIEW ini and the logins are read per user.
        for var in [
            "HOME",
            "USERPROFILE",
            "LOCALAPPDATA",
            "APPDATA",
            "XDG_CONFIG_HOME",
            "XDG_CACHE_HOME",
        ] {
            cmd.env(var, &home);
        }
        let out = cmd
            .current_dir(dir)
            .env("LVPM_FAKE_LABVIEW", fake)
            .env("LVPM_CONFIG_CACHE", self.root.join("cache"))
            .env("LVPM_CONFIG_SOURCES_LOCAL", self.root.join("packages"))
            .env("LVPM_CONFIG_VENV_DIR", ".lv-venv")
            .env("LVPM_CONFIG_LOG_DIR", self.root.join("logs"))
            .env_remove("LV_RTE_HEADLESS")
            .env_remove("LVPM_LOG")
            .env_remove("LVPM_TEST_LABVIEW")
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        Run {
            ok: out.status.success(),
            text: String::from_utf8_lossy(&out.stdout).into_owned()
                + &String::from_utf8_lossy(&out.stderr),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Run in the project folder; it must succeed.
    fn ok(&self, args: &[&str]) -> String {
        let r = self.run_in(&self.project(), args);
        assert!(r.ok, "lvpm {} failed:\n{}", args.join(" "), r.text);
        r.text
    }

    /// Run in the project folder; it must fail.
    fn fails(&self, args: &[&str]) -> String {
        let r = self.run_in(&self.project(), args);
        assert!(
            !r.ok,
            "lvpm {} succeeded, expected a failure:\n{}",
            args.join(" "),
            r.text
        );
        r.text
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[track_caller]
fn has(text: &str, part: &str) {
    assert!(text.contains(part), "expected {part:?} in:\n{text}");
}

#[track_caller]
fn lacks(text: &str, part: &str) {
    assert!(!text.contains(part), "did not expect {part:?} in:\n{text}");
}

#[test]
fn create_binds_the_project_and_remove_takes_it_away() {
    let lab = Lab::new("create");

    // No manifest yet: create asks which LabVIEW, then writes one bound to it.
    has(&lab.fails(&["venv", "create"]), "which LabVIEW?");
    let created = lab.ok(&["venv", "create", "--labview-version", "2099"]);
    has(&created, "bound to       LabVIEW 2099 (64-bit)");
    // fork: a new manifest takes the year as a minimum, so a newer LabVIEW
    // can stand in.
    has(&lab.manifest(), "labview = \">=2099\"");
    has(&lab.manifest(), "[dependencies]");
    assert_eq!(
        std::fs::read_to_string(lab.venv().join(".gitignore")).unwrap(),
        "*\n"
    );
    let binding: serde_json::Value =
        serde_json::from_slice(&std::fs::read(lab.venv().join("venv.json")).unwrap()).unwrap();
    // fork: a binding leaves `manifest` out when it is lvpm.toml.
    assert!(binding.get("manifest").is_none());
    assert_eq!(binding["target_key"], "LabVIEW-2099-64bit");

    // fork: the venv's own labview.ini is written at its first launch, not
    // by create, so settings made in the venv IDE survive (launch::write_ini).
    // Its keys are checked by launch.rs's own tests; upstream's ini checks go.

    let status = lab.ok(&["venv", "status"]);
    has(&status, "packages       0 installed");
    has(&status, "not running on this venv");
    has(&lab.fails(&["venv", "create"]), "already has a venv");

    lab.ok(&["venv", "remove", "-y"]);
    assert!(!lab.venv().exists());
    assert!(
        lab.manifest_path().is_file(),
        "the manifest is the project's, not the venv's"
    );
    // A project without its venv: status answers with the way to make one.
    // fork: it exits non-zero where upstream exits 0; only the answer is
    // checked here, and the exit code is an open question for the PR.
    let status = lab.run_in(&lab.project(), &["venv", "status"]);
    has(&status.text, "`lvpm venv create` makes one");
}

#[test]
fn a_venv_is_never_bound_to_a_labview_older_than_the_project() {
    let lab = Lab::new("older");
    std::fs::write(
        lab.manifest_path(),
        "[project]\nname = \"demo\"\nlabview = \"2100\"\n",
    )
    .unwrap();
    has(
        &lab.fails(&["venv", "create", "--labview-version", "2099"]),
        "older than the project's labview = 2100",
    );
    assert!(!lab.venv().exists());
}

#[test]
fn commands_find_the_venv_from_below_and_from_outside_and_a_bare_manifest_is_refused() {
    let lab = Lab::new("find");
    lab.package("app", "1.0.0.1", "");
    lab.write_manifest("[dependencies]\n");

    // A manifest with no venv beside it: never a silent install into LabVIEW.
    has(
        &lab.fails(&["install", "app"]),
        "`lvpm venv create` makes one",
    );

    lab.ok(&["venv", "create"]);
    let deep = lab.project().join("src").join("deep");
    std::fs::create_dir_all(&deep).unwrap();
    let r = lab.run_in(&deep, &["install", "app"]);
    assert!(r.ok, "{}", r.text);
    has(&r.text, "venv: ");
    assert!(lab.record("app").is_file());
    assert!(lab.file("app", "app.txt").is_file());
    assert!(
        lab.venv()
            .join("packages")
            .join("app")
            .join("1")
            .join("lvaddoninfo.json")
            .is_file()
    );

    // From outside the project, --project names it.
    let outside = lab.root.join("home");
    let project = lab.project().display().to_string();
    let r = lab.run_in(&outside, &["--project", &project, "list"]);
    assert!(r.ok, "{}", r.text);
    has(&r.text, "app");
    // And without it, outside is outside.
    has(
        &lab.run_in(&outside, &["venv", "status"]).text,
        "no venv at or above",
    );
}

#[test]
fn manifest_install_reads_a_group_written_under_dependencies() {
    let lab = Lab::new("old-group");
    lab.package("app", "1.0.0.1", "lib");
    lab.package("lib", "1.0.0.1", "");
    lab.package("tester", "1.0.0.1", "");
    lab.write_manifest(
        "[dependencies]
app = \"*\"

[dependencies.test]
tester = \"*\"
",
    );
    lab.ok(&["venv", "create"]);

    let installed = lab.ok(&["install"]);
    has(
        &installed,
        "is read as a group: write [dependency-groups.test]",
    );
    for (name, there) in [("app", true), ("lib", true), ("tester", false)] {
        assert_eq!(lab.record(name).is_file(), there, "{name}");
    }
    lab.ok(&["install", "--group", "test"]);
    assert!(lab.record("tester").is_file());
}

#[test]
fn manifest_install_brings_in_main_and_the_selected_groups_only() {
    let lab = Lab::new("groups");
    lab.package("app", "1.0.0.1", "lib");
    lab.package("lib", "1.0.0.1", "");
    lab.package("tester", "1.0.0.1", "");
    lab.package("builder", "1.0.0.1", "");
    lab.write_manifest(
        "[dependencies]\napp = \"*\"\n\n[dependencies.test]\ntester = \"*\"\n\n\
         [dependencies.ci]\ninclude-groups = [\"test\"]\nbuilder = \"*\"\n",
    );
    lab.ok(&["venv", "create"]);

    let installed = lab.ok(&["install"]);
    has(&installed, "groups not selected: test, ci");
    for (name, there) in [
        ("app", true),
        ("lib", true),
        ("tester", false),
        ("builder", false),
    ] {
        assert_eq!(lab.record(name).is_file(), there, "{name}");
    }
    lab.ok(&["install", "--with", "ci", "--without", "test"]);
    assert!(lab.record("builder").is_file());
    assert!(
        !lab.record("tester").is_file(),
        "--without applies after includes"
    );
    lab.ok(&["install", "--only", "test"]);
    assert!(lab.record("tester").is_file());
}

#[test]
#[ignore = "#55"]
fn manifest_install_keeps_what_is_no_longer_listed_and_refuses_an_empty_selection() {
    let lab = Lab::new("groups-additive");
    lab.package("app", "1.0.0.1", "lib");
    lab.package("lib", "1.0.0.1", "");
    lab.write_manifest(
        "[dependencies]
app = \"*\"
",
    );
    lab.ok(&["venv", "create"]);
    lab.ok(&["install"]);

    // Additive: a package no longer listed stays until uninstalled.
    lab.write_manifest("[dependencies]\nlib = \"*\"\n");
    lab.ok(&["install"]);
    assert!(lab.record("app").is_file());

    // An empty selection is an error, not a silent no-op.
    lab.write_manifest("[dependencies]\n");
    has(
        &lab.fails(&["install"]),
        "nothing to install: the selected groups (main) are empty",
    );
}

#[test]
fn venv_installs_merge_palette_trees_and_rebuild_them_after_uninstall() {
    let lab = Lab::new("palettes");
    let labview_menu = lab
        .root
        .join("labview/menus/Categories/Computer/LabVIEW.mnu");
    std::fs::create_dir_all(labview_menu.parent().unwrap()).unwrap();
    std::fs::write(&labview_menu, b"LabVIEW category").unwrap();
    lab.package_with_menus(
        "alpha",
        &[
            ("Categories/Programming/Shared/dir.mnu", b"alpha"),
            ("Categories/Programming/Shared/shared.mnu", b"shared"),
            ("Root.mnu", b"root"),
            ("Controls/alpha.mnu", b"control"),
        ],
    );
    lab.package_with_menus(
        "beta",
        &[
            ("Categories/Programming/Shared/dir.mnu", b"beta"),
            ("Categories/Programming/Shared/shared.mnu", b"shared"),
            ("Categories/Programming/Shared/beta.mnu", b"beta"),
            ("Categories/Computer/beta.mnu", b"computer"),
        ],
    );
    lab.write_manifest("[dependencies]\nalpha = \"*\"\nbeta = \"*\"\n");
    lab.ok(&["venv", "create"]);
    lab.ok(&["install", "--no-linkfix"]);

    let packages = lab.venv().join("packages");
    let menus = packages.join("venv-palettes/1/menus");
    assert_eq!(
        std::fs::read(menus.join("Categories/Programming/Shared/dir.mnu")).unwrap(),
        b"alpha"
    );
    assert_eq!(
        std::fs::read(menus.join("Categories/Programming/Shared/shared.mnu")).unwrap(),
        b"shared"
    );
    assert_eq!(std::fs::read(menus.join("Root.mnu")).unwrap(), b"root");
    assert_eq!(
        std::fs::read(menus.join("Controls/alpha.mnu")).unwrap(),
        b"control"
    );
    assert_eq!(
        std::fs::read(menus.join("Categories/Computer/beta.mnu")).unwrap(),
        b"computer"
    );
    assert_eq!(std::fs::read(&labview_menu).unwrap(), b"LabVIEW category");
    assert!(lab.file("alpha", "alpha.txt").is_file());
    assert!(lab.file("beta", "beta.txt").is_file());
    for package in ["alpha", "beta"] {
        assert!(!packages.join(package).join("1/menus").exists());
        assert!(
            lab.venv()
                .join("palettes/contributions")
                .join(package)
                .join("menus")
                .is_dir()
        );
    }
    let shared = menus.join("Categories/Programming/Shared/shared.mnu");
    std::fs::write(&shared, b"tampered").unwrap();
    let refused = lab.fails(&["launch"]);
    has(&refused, "palettes:");
    // fork: the venv is brought in line with `lvpm sync`.
    has(&refused, "run `lvpm sync`");
    lab.ok(&["install", "--no-linkfix"]);
    assert_eq!(std::fs::read(&shared).unwrap(), b"shared");
    let unexpected = packages.join("venv-palettes/1/unexpected.txt");
    std::fs::write(&unexpected, b"unexpected").unwrap();
    has(&lab.fails(&["launch"]), "palettes:");
    lab.ok(&["install", "--no-linkfix"]);
    assert!(!unexpected.exists());

    lab.ok(&["uninstall", "alpha", "--no-project"]);
    assert!(!menus.join("Root.mnu").exists());
    assert_eq!(
        std::fs::read(menus.join("Categories/Programming/Shared/dir.mnu")).unwrap(),
        b"beta"
    );
    assert_eq!(
        std::fs::read(menus.join("Categories/Programming/Shared/shared.mnu")).unwrap(),
        b"shared"
    );

    lab.ok(&["uninstall", "--all"]);
    assert!(!packages.join("venv-palettes/1").exists());
}

#[test]
fn install_by_name_keeps_the_manifest_in_step() {
    let lab = Lab::new("save");
    lab.package("foo", "1.0.0.1", "");
    lab.package("foo", "2.0.0.1", "");
    lab.package("bar", "1.0.0.1", "");
    lab.package("baz", "1.0.0.1", "");
    lab.write_manifest("# the project's packages\n[dependencies]\n");
    lab.ok(&["venv", "create"]);

    // The version asked for, as given.
    has(
        &lab.ok(&["install", "foo@1.0.0.1"]),
        "lvpm.toml: added foo = \"1.0.0.1\" to [dependencies]",
    );
    has(
        &std::fs::read_to_string(lab.file("foo", "foo.txt")).unwrap(),
        "foo 1.0.0.1",
    );
    // Asking for a newer version replaces the files and updates the entry in
    // place; asking for no version only rewrites the constraint, as `lvpm lock
    // --upgrade-package` is what moves a locked package.
    let upgraded = lab.ok(&["install", "foo@2.0.0.1"]);
    has(&upgraded, "foo 1.0.0.1 -> 2.0.0.1");
    has(
        &upgraded,
        "lvpm.toml was updated: foo \"1.0.0.1\" -> \"2.0.0.1\"",
    );
    has(
        &std::fs::read_to_string(lab.file("foo", "foo.txt")).unwrap(),
        "foo 2.0.0.1",
    );
    // A direct install keeps a newer installed version, and leaves the manifest
    // alone: in a project the lock decides, and a named install edits it.
    let kept = lab.ok(&["install", "foo@1.0.0.1", "--no-project"]);
    has(&kept, "not downgrading");
    has(&lab.manifest(), "foo = \"2.0.0.1\"");

    has(
        &lab.ok(&["install", "bar", "--group", "test"]),
        "lvpm.toml: added bar = \">=1.0.0.1\" to [dependency-groups.test]",
    );
    // --no-project installs without saving.
    lab.ok(&["install", "baz", "--no-project"]);
    assert!(lab.record("baz").is_file());
    let manifest = lab.manifest();
    assert!(manifest.starts_with("[project]"), "{manifest}");
    has(&manifest, "# the project's packages");
    has(&manifest, "[dependency-groups.test]\nbar = \">=1.0.0.1\"");
    assert!(!manifest.contains("baz"), "{manifest}");

    // Dry runs change nothing.
    let before = lab.manifest();
    has(
        &lab.ok(&["install", "foo@1.0.0.1", "--dry-run"]),
        "would add foo@1.0.0.1 to lvpm.toml",
    );
    assert_eq!(lab.manifest(), before);
    has(
        &std::fs::read_to_string(lab.file("foo", "foo.txt")).unwrap(),
        "foo 2.0.0.1",
    );
}

#[test]
fn uninstall_in_a_project_takes_the_package_out_of_the_manifest_and_all_leaves_it() {
    let lab = Lab::new("uninstall-project");
    lab.package("app", "1.0.0.1", "lib");
    lab.package("lib", "1.0.0.1", "core");
    lab.package("other", "1.0.0.1", "core");
    lab.package("core", "1.0.0.1", "");
    lab.write_manifest("[dependencies]\n");
    lab.ok(&["venv", "create"]);
    lab.ok(&["install", "app"]);
    lab.ok(&["install", "other"]);

    // A named uninstall is the project's: no prompt, so no --with-deps to give.
    let removed = lab.ok(&["uninstall", "app"]);
    has(&removed, "removing app from lvpm.toml");
    has(&removed, "lvpm.toml: removed app = \">=1.0.0.1\"");
    // What only app needed goes with it; core stays for `other`, which the
    // manifest declares.
    for (name, there) in [
        ("app", false),
        ("lib", false),
        ("core", true),
        ("other", true),
    ] {
        assert_eq!(lab.record(name).is_file(), there, "{name}");
    }
    assert!(!lab.file("app", "app.txt").exists());
    assert!(!lab.venv().join("packages").join("app").exists());
    lacks(&lab.manifest(), "app =");
    has(&lab.manifest(), "other =");

    // A dependency the manifest declares is the project's: kept, and the
    // --with-deps of the old uninstall is accepted.
    lab.ok(&["install", "app"]);
    std::fs::write(
        lab.manifest_path(),
        lab.manifest()
            .replace("[dependencies]\n", "[dependencies]\nlib = \"*\"\n"),
    )
    .unwrap();
    lab.ok(&["uninstall", "app", "--with-deps"]);
    assert!(lab.record("lib").is_file());
    assert!(!lab.record("app").is_file());
    has(&lab.manifest(), "lib = \"*\"");

    // --all empties the venv and never edits the manifest.
    let before = lab.manifest();
    lab.ok(&["uninstall", "--all"]);
    assert_eq!(lab.manifest(), before);
    has(
        &lab.ok(&["list"]),
        "lvpm has installed nothing into this target",
    );
}

#[test]
fn uninstall_refuses_a_package_another_one_requires_and_a_script_must_say_what_happens_to_dependencies()
 {
    let lab = Lab::new("uninstall");
    lab.package("app", "1.0.0.1", "lib");
    lab.package("lib", "1.0.0.1", "core");
    lab.package("other", "1.0.0.1", "core");
    lab.package("core", "1.0.0.1", "");
    lab.write_manifest(
        "[dependencies]
",
    );
    lab.ok(&["venv", "create"]);
    lab.ok(&["install", "app"]);
    lab.ok(&["install", "other"]);
    let manifest = lab.manifest();

    // In a project the lock decides: core is refused, and the next line names
    // the packages the project lists that need it.
    let refused = lab.fails(&["uninstall", "core"]);
    has(
        &refused,
        "core is required by installed packages: lib, other",
    );
    has(
        &refused,
        "next: run `lvpm uninstall app` and `lvpm uninstall other` first",
    );
    assert!(lab.record("core").is_file());
    assert_eq!(lab.manifest(), manifest);

    // A direct uninstall says it the same way, and can be forced.
    let refused = lab.fails(&["uninstall", "core", "--no-project", "--keep-deps"]);
    has(
        &refused,
        "core is required by installed packages: lib, other",
    );
    has(&refused, "or pass --force to remove core anyway");
    // A direct uninstall of what the manifest does not declare may take
    // dependencies with it, and a script never gets a prompt about that: it has
    // to say what happens to them.
    lab.write_manifest("[dependencies]\nother = \"*\"\n");
    has(
        &lab.fails(&["uninstall", "app", "--no-project"]),
        "--with-deps or --keep-deps",
    );
    assert!(lab.record("app").is_file());
}

// The fork puts lvpm.toml and lvpm.lock back when the sync stops partway
// (add_remove.rs), where upstream keeps the manifest edit; `lvpm sync` then
// puts the destination back in line with them.
#[test]
fn an_uninstall_whose_sync_stops_puts_lvpm_toml_and_lvpm_lock_back() {
    let lab = Lab::new("uninstall-fails");
    lab.package("app", "1.0.0.1", "lib");
    lab.package("lib", "1.0.0.1", "");
    lab.write_manifest(
        "[dependencies]
",
    );
    lab.ok(&["venv", "create"]);
    lab.ok(&["install", "app"]);
    let manifest = lab.manifest();
    let lock_path = lab.project().join("lvpm.lock");
    let lock = std::fs::read(&lock_path).unwrap();
    // A folder where lib recorded a file: removing it fails on any OS.
    let blocked = lab.file("lib", "lib.txt");
    std::fs::remove_file(&blocked).unwrap();
    std::fs::create_dir_all(blocked.join("in-the-way")).unwrap();

    let failed = lab.fails(&["uninstall", "app", "--with-deps"]);
    has(&failed, "lib.txt");
    has(
        &failed,
        "lvpm.toml and lvpm.lock were put back as they were",
    );
    assert_eq!(lab.manifest(), manifest);
    assert_eq!(std::fs::read(&lock_path).unwrap(), lock);
    has(&lab.manifest(), "app =");
    assert!(
        lab.record("lib").is_file(),
        "the failed dependency stays recorded, to retry"
    );
}

#[test]
#[ignore = "#55"]
fn an_unreadable_record_of_another_package_does_not_stop_an_install() {
    let lab = Lab::new("legacy-record");
    lab.package("app", "1.0.0.1", "");
    lab.write_manifest("[dependencies]\n");
    lab.ok(&["venv", "create"]);
    std::fs::create_dir_all(lab.venv().join("installed")).unwrap();
    std::fs::write(
        lab.record("legacy"),
        r#"{"name":"legacy","version":"1.0","display_name":null,"installed_at":0,"target":"t","files":[],"skipped_hooks":[]}"#,
    )
    .unwrap();

    let installed = lab.ok(&["install", "app"]);
    has(&installed, "legacy.json");
    has(&installed, "dependency list required");
    has(&installed, "linkfix: preparing installed files");
    has(&installed, "saved app = \"1.0.0.1\"");
    // What acts on every record still refuses it.
    has(&lab.fails(&["list"]), "dependency list required");
}

/// #49: with a relink-package.vi from another build beside lvpm, `relink --all`
/// fails naming that VI's path and the build lvpm expects, before any LabVIEW
/// starts (the fake one cannot).
#[test]
fn relink_refuses_a_relink_vi_from_another_build_beside_lvpm() {
    use md5::{Digest, Md5};
    let lab = Lab::new("stale-relink-vi");
    lab.package_with(
        "lib",
        "1.0.0.1",
        "",
        "[File Group 1]\nTarget Dir=\"<vi.lib>/lib\"\nReplace Mode=\"Always\"\nNum Files=1\nFile 0=\"lib.vi\"\n",
        &[("File Group 1/lib.vi", b"not really a vi")],
    );
    lab.write_manifest("[dependencies]\nlib = \"1.0.0.1\"\n");
    lab.ok(&["venv", "create"]);
    lab.ok(&["install"]);

    // A release layout: lvpm with its VI beside it, here one from another build.
    let bin = lab.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let lvpm = bin.join(Path::new(env!("CARGO_BIN_EXE_lvpm")).file_name().unwrap());
    std::fs::copy(env!("CARGO_BIN_EXE_lvpm"), &lvpm).unwrap();
    let stale = bin.join("relink-package.vi");
    std::fs::write(&stale, b"a relink VI from another build").unwrap();

    let fake = format!("99.0,64,{}", lab.root.join("labview").display());
    let r = lab.run_program(&lvpm, &lab.project(), &fake, &["relink", "--all"]);
    assert!(!r.ok, "relink --all succeeded:\n{}", r.text);
    let expected: String =
        Md5::digest(std::fs::read("crates/lvpm-core/src/lv-src/relink-package.vi").unwrap())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
    has(&r.stderr, &stale.display().to_string());
    has(&r.stderr, "is from another lvpm build");
    has(&r.stderr, &format!("needs the one with md5 {expected}"));
}

#[test]
fn launch_checks_the_venv_against_the_lock_before_starting_anything() {
    let lab = Lab::new("launch");
    lab.package("app", "1.0.0.1", "");
    lab.package("app", "2.0.0.1", "");
    lab.package("lib", "1.0.0.1", "");
    lab.write_manifest(
        "[dependencies]
app = \"1.0.0.1\"
lib = { version = \"*\", relink = true }
",
    );
    lab.ok(&["venv", "create"]);

    // fork: the check is against lvpm.lock, not the manifest.
    let refused = lab.fails(&["launch"]);
    has(&refused, "no lvpm.lock beside the manifest");
    has(&refused, "run `lvpm sync`");
    lacks(&refused, "Error: launching ");
    lab.ok(&["lock"]);
    let refused = lab.fails(&["launch"]);
    lacks(&refused, "Error: launching ");
    has(&refused, "app: not installed (lock: 1.0.0.1)");
    has(&refused, "lib: not installed");
    has(&refused, "run `lvpm sync`");
    has(&refused, "`lvpm launch --no-check` to start anyway");

    lab.ok(&["install", "--no-relink"]);
    // relink = true asks for LabVIEW's own load-and-save, which has not run.
    has(
        &lab.fails(&["launch"]),
        "lib: relink = true but the relink pass has not completed",
    );

    lab.write_manifest(
        "[dependencies]
app = \"2.0.0.1\"
lib = \"*\"
",
    );
    lab.ok(&["lock"]);
    has(
        &lab.fails(&["launch"]),
        "app: 1.0.0.1 installed, lock: 2.0.0.1",
    );
    lab.ok(&["install"]);
    std::fs::remove_file(lab.file("app", "app.txt")).unwrap();
    has(&lab.fails(&["launch"]), "app: 1 of 2 files missing");

    // In line again: the check passes, and only starting the fake LabVIEW fails.
    // fork: upstream kept app in the manifest with a flag the fork lacks;
    // write it back, so this holds whether or not uninstall edits it.
    lab.ok(&["uninstall", "app", "--no-project", "--keep-deps"]);
    lab.write_manifest(
        "[dependencies]
app = \"2.0.0.1\"
lib = \"*\"
",
    );
    lab.ok(&["install"]);
    let r = lab.run_in(&lab.project(), &["launch"]);
    has(&r.text, "Error: launching ");
    lacks(&r.text, "does not match");

    // A project from 0.3.1: every package installed, but no lvpm.lock. The
    // check is against the lock, so launch is refused until one sync writes it.
    let lock = lab.project().join("lvpm.lock");
    std::fs::remove_file(&lock).unwrap();
    let refused = lab.fails(&["launch"]);
    has(&refused, "no lvpm.lock beside the manifest");
    has(&refused, "run `lvpm sync`");
    lacks(&refused, "Error: launching ");
    lab.ok(&["sync"]);
    let written = std::fs::read_to_string(&lock).expect("lvpm sync writes lvpm.lock");
    has(&written, "2.0.0.1");
    has(&written, "lib");
    let r = lab.run_in(&lab.project(), &["launch"]);
    has(&r.text, "Error: launching ");
    lacks(&r.text, "does not match");
}

#[test]
fn a_package_labview_already_has_is_reported_as_shadowed() {
    let lab = Lab::new("shadow");
    // Shadowing is about LabVIEW files: app ships a VI.
    lab.package_with(
        "app",
        "1.0.0.1",
        "",
        "[File Group 1]\nTarget Dir=\"<vi.lib>/app\"\nReplace Mode=\"Always\"\nNum Files=1\nFile 0=\"app.vi\"\n",
        &[("File Group 1/app.vi", b"not a real VI")],
    );
    lab.package("lib", "1.0.0.1", "");
    lab.write_manifest("[dependencies]\n");
    lab.ok(&["venv", "create"]);
    // LabVIEW 2099 has its own copy of app, from VIPM or by hand.
    let own = lab.root.join("labview").join("vi.lib").join("app");
    std::fs::create_dir_all(&own).unwrap();
    std::fs::write(own.join("app.vi"), b"the installation's copy\n").unwrap();

    let installed = lab.ok(&["install", "app"]);
    has(
        &installed,
        "LabVIEW 2099 has its own copy of 1 package in this venv",
    );
    // fork: every install reports the venv's shadowed packages, not only
    // the ones it just installed, so upstream's `lacks` on this line goes.
    lab.ok(&["install", "lib"]);
    has(
        &lab.ok(&["venv", "status"]),
        "shadowed       1 also in LabVIEW 2099",
    );
    assert_eq!(
        std::fs::read(own.join("app.vi")).unwrap(),
        b"the installation's copy\n",
        "the installation is never written"
    );
}

#[test]
fn hooks_and_machine_locations_stay_out_of_a_venv() {
    let lab = Lab::new("hooks");
    lab.package_with(
        "tool",
        "1.0.0.1",
        "",
        "[Script VIs]\nPostInstall=\"PostInstall.vi\"\n\n\
         [File Group 1]\nTarget Dir=\"<OS User Documents>/tool\"\nReplace Mode=\"Always\"\nNum Files=1\nFile 0=\"readme.txt\"\n",
        &[("PostInstall.vi", b"not a real VI"), ("File Group 1/readme.txt", b"docs\n")],
    );
    lab.write_manifest("[dependencies]\n");
    lab.ok(&["venv", "create"]);

    let installed = lab.ok(&["install", "tool"]);
    // fork: one line per package names the hooks not run.
    has(&installed, "hooks not run: PostInstall");
    has(
        &installed,
        "skipped — a venv holds LabVIEW-tree content only",
    );
    assert!(lab.file("tool", "tool.txt").is_file());
    has(&lab.ok(&["list"]), "hooks skipped: PostInstall");

    // --no-linkfix leaves the files as copied, and says so.
    lab.ok(&["uninstall", "tool", "--no-project"]);
    has(
        &lab.ok(&["install", "tool", "--no-linkfix"]),
        "linkfix: skipped (--no-linkfix)",
    );
}

#[test]
#[ignore = "#48"]
fn import_writes_vipm_files_into_the_manifest_and_adopts_an_old_venv() {
    let lab = Lab::new("import");
    lab.package("app", "1.0.0.1", "");
    lab.package("tester", "1.0.0.1", "");
    lab.ok(&["venv", "create", "--labview-version", "2099"]);
    // As lvpm 0.3.0 left a venv created from a vipm.toml.
    std::fs::remove_file(lab.manifest_path()).unwrap();
    let binding_path = lab.venv().join("venv.json");
    let mut binding: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&binding_path).unwrap()).unwrap();
    binding["manifest"] = serde_json::json!("vipm.toml");
    std::fs::write(&binding_path, serde_json::to_vec_pretty(&binding).unwrap()).unwrap();
    let vipm = "[project]\nname = \"vipm-demo\"\nlabview-version = \"2099\"\n\n\
                [dependencies]\napp = \"1.0.0.1\"\n\n[dev-dependencies]\ntester = \"1.0.0.1\"\n";
    std::fs::write(lab.project().join("vipm.toml"), vipm).unwrap();

    has(&lab.fails(&["install"]), "lvpm import");
    lab.ok(&["import", "vipm.toml", "--dry-run"]);
    assert!(!lab.manifest_path().exists(), "a dry run writes nothing");

    let imported = lab.ok(&["import", "vipm.toml"]);
    has(&imported, "this project's venv now follows lvpm.toml");
    let binding: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&binding_path).unwrap()).unwrap();
    // fork: a binding leaves `manifest` out when it is lvpm.toml.
    assert!(binding.get("manifest").is_none());
    assert_eq!(
        std::fs::read_to_string(lab.project().join("vipm.toml")).unwrap(),
        vipm,
        "VIPM's file is never written"
    );
    let manifest = lab.manifest();
    has(&manifest, "app = \"1.0.0.1\"");
    has(&manifest, "[dependencies.dev]\ntester = \"1.0.0.1\"");
    has(&manifest, "labview = \"2099\"");

    std::fs::write(
        lab.manifest_path(),
        format!("{manifest}\n[sources]\ndefaults = false\n"),
    )
    .unwrap();
    lab.ok(&["install", "--with", "dev"]);
    assert!(lab.record("app").is_file() && lab.record("tester").is_file());
    has(
        &lab.ok(&["import", "vipm.toml"]),
        "already in [dependencies]",
    );
}

#[test]
// Upstream's test, on the fork's rules: a group here is
// `[dependency-groups.<name>]`, so a group named like a package is no failure,
// and a name with no version refuses the file instead of being set aside.
fn a_vipc_carries_its_packages_into_the_cache_and_a_failed_import_writes_nothing() {
    let lab = Lab::new("vipc");
    lab.write_manifest("[dependencies]\n");
    lab.ok(&["venv", "create"]);

    // A package the local folder does not have, for a .vipc to carry.
    let mut inner = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    inner.start_file("spec", options).unwrap();
    inner
        .write_all(
            b"[Package]\nName=\"carried\"\nVersion=\"3.0.0.1\"\n[Dependencies]\nRequires=\"\"\n\n\
              [File Group 0]\nTarget Dir=\"<vi.lib>/carried\"\nNum Files=1\nFile 0=\"carried.txt\"\n",
        )
        .unwrap();
    inner
        .start_file("File Group 0/carried.txt", options)
        .unwrap();
    inner.write_all(b"carried 3.0.0.1\n").unwrap();
    let carried = inner.finish().unwrap().into_inner();
    let package_xml = |name: &str, included: bool| {
        format!(
            "    <Package>\r\n      <Name>{name}</Name>\r\n      <Included>{included}</Included>\r\n      \
             <Dependency>false</Dependency>\r\n      <Pinned>false</Pinned>\r\n    </Package>\r\n"
        )
    };
    // The target as VIPM writes it, with the bitness.
    let write_vipc = |file: &str, packages: &str| {
        let config = format!(
            "<VI_Package_Configuration File_Type=\"xml\" Version=\"0.6\" Creator=\"x\">\r\n  <Target>\r\n    \
             <Name>LabVIEW</Name>\r\n    <Version>99.0 (64-bit)</Version>\r\n{packages}  \
             </Target>\r\n</VI_Package_Configuration>"
        );
        let vipc = lab.root.join(file);
        let mut z = zip::ZipWriter::new(std::fs::File::create(&vipc).unwrap());
        z.start_file("config.xml", options).unwrap();
        z.write_all(config.as_bytes()).unwrap();
        z.start_file("carried-3.0.0.1.vip", options).unwrap();
        z.write_all(&carried).unwrap();
        z.finish().unwrap();
        vipc.display().to_string()
    };

    // A name with no version fails the import, and the manifest is as it was.
    let bad_vipc = write_vipc(
        "bad.vipc",
        &(package_xml("carried-3.0.0.1", true) + &package_xml("mystery", false)),
    );
    let before = lab.manifest();
    has(
        &lab.fails(&["import", &bad_vipc]),
        "package mystery: no version (expected name-version)",
    );
    assert_eq!(lab.manifest(), before);

    let good_vipc = write_vipc("deps.vipc", &package_xml("carried-3.0.0.1", true));
    let imported = lab.ok(&["import", &good_vipc]);
    has(&imported, "imported carried 3.0.0.1");
    has(&imported, "saved carried = \">=3.0.0.1\"");
    // Installed from the cache: no index has it.
    has(&lab.ok(&["install", "carried"]), "+ carried 3.0.0.1");
    has(
        &std::fs::read_to_string(lab.file("carried", "carried.txt")).unwrap(),
        "carried 3.0.0.1",
    );
}

#[test]
fn every_command_on_the_venv_is_in_the_venvs_log() {
    let lab = Lab::new("log");
    lab.package("app", "1.0.0.1", "");
    lab.write_manifest("[dependencies]\n");
    lab.ok(&["venv", "create"]);
    lab.ok(&["install", "app"]);
    lab.fails(&["uninstall", "not_there", "--no-project", "--keep-deps"]);

    let logs: Vec<PathBuf> = std::fs::read_dir(lab.root.join("logs"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    let venv_log = logs
        .iter()
        .find(|p| p.file_name().unwrap() != "lvpm.log")
        .expect("a log of the venv's own");
    let text = std::fs::read_to_string(venv_log).unwrap();
    has(&text, "---- lvpm");
    has(&text, "install app");
    // fork: the log holds JSON events.
    assert!(
        text.lines()
            .any(|l| l.contains("{\"event\":\"manifest_edited\"") && l.contains("\"app\"")),
        "{text}"
    );
    has(&text, "---- done");
    has(&text, "---- failed:");
}

#[test]
fn a_malformed_fake_labview_is_refused_and_never_falls_back() {
    let lab = Lab::new("malformed");
    let dir = lab.root.join("labview").display().to_string();
    for spec in [
        "99.0,64".to_string(),
        format!("x,64,{dir}"),
        format!("99.0,7,{dir}"),
    ] {
        let r = lab.run_with_fake(
            &lab.project(),
            &spec,
            &["venv", "create", "--labview-version", "2099"],
        );
        assert!(!r.ok, "{spec:?} was accepted");
        has(&r.stderr, "<version>,<bitness>,<dir>");
    }
    assert!(!lab.venv().exists());
}
