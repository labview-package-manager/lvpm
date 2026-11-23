//! The venv workflow, end to end, through the lvpm binary: create, find,
//! install from the manifest and by name, upgrade, uninstall, groups, the
//! launch check, shadowing, hooks and machine locations, import, and the log.
//!
//! No LabVIEW is needed. A debug build of lvpm takes `LVPM_TEST_LABVIEW` as
//! the one LabVIEW installed; each test makes a fake "LabVIEW 2099" of its own
//! (a year no real installation has, so nothing of the machine's own
//! LabVIEW is read), a local package folder, a cache, a log folder and a
//! project, all in a temp folder. What does need a running LabVIEW (relink,
//! hooks run in the venv's IDE, the overlay loading) is the container suite's
//! (e2e/).

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
}

impl Lab {
    fn new(name: &str) -> Lab {
        let root = std::env::temp_dir().join(format!("lvpm-venv-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let labview = root.join("labview");
        std::fs::create_dir_all(labview.join("vi.lib")).unwrap();
        // Present, so detection takes it; empty, so nothing can ever start.
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
        self.venv().join("packages").join(name).join("1").join("vi.lib").join(name).join(file)
    }

    /// A `.vip` in the local package folder: `<vi.lib>/<name>/<name>.txt`,
    /// plus `extra` spec text and `members`.
    fn package_with(&self, name: &str, version: &str, requires: &str, extra: &str, members: &[(&str, &[u8])]) {
        let file = std::fs::File::create(self.root.join("packages").join(format!("{name}-{version}.vip"))).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("spec", options).unwrap();
        write!(
            zip,
            "[Package]\nName=\"{name}\"\nVersion=\"{version}\"\n[Dependencies]\nRequires=\"{requires}\"\n\n\
             [File Group 0]\nTarget Dir=\"<vi.lib>/{name}\"\nReplace Mode=\"Always\"\nNum Files=1\nFile 0=\"{name}.txt\"\n\n{extra}"
        )
        .unwrap();
        zip.start_file(format!("File Group 0/{name}.txt"), options).unwrap();
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
        let out = Command::new(env!("CARGO_BIN_EXE_lvpm"))
            .current_dir(dir)
            .env("LVPM_TEST_LABVIEW", format!("99.0,64,{}", self.root.join("labview").display()))
            .env("LVPM_CONFIG_CACHE", self.root.join("cache"))
            .env("LVPM_CONFIG_SOURCES_LOCAL", self.root.join("packages"))
            .env("LVPM_CONFIG_VENV_DIR", ".lv-venv")
            .env("LVPM_CONFIG_LOG_DIR", self.root.join("logs"))
            // Linux reads the LabVIEW ini per user; keep it inside the test.
            .env("HOME", self.root.join("home"))
            .env_remove("LV_RTE_HEADLESS")
            .env_remove("LVPM_LOG")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        Run {
            ok: out.status.success(),
            text: String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr),
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
        assert!(!r.ok, "lvpm {} succeeded, expected a failure:\n{}", args.join(" "), r.text);
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
    has(&lab.manifest(), "labview = \"2099\"");
    has(&lab.manifest(), "[dependencies]");
    assert_eq!(std::fs::read_to_string(lab.venv().join(".gitignore")).unwrap(), "*\n");
    let binding: serde_json::Value = serde_json::from_slice(&std::fs::read(lab.venv().join("venv.json")).unwrap()).unwrap();
    assert_eq!(binding["manifest"], "lvpm.toml");
    assert_eq!(binding["target_key"], "LabVIEW-2099-64bit");

    // The venv's own ini: the overlay mounted, VI Server on, no recent files
    // from outside the venv.
    let ini = std::fs::read_to_string(lab.venv().join("labview.ini")).unwrap();
    has(&ini, "LVAddons.AdditionalLocations=");
    has(&ini, "server.tcp.enabled=True");
    lacks(&ini, "RecentFiles.projectPathList");

    let status = lab.ok(&["venv", "status"]);
    has(&status, "packages       0 installed");
    has(&status, "not running on this venv");
    has(&lab.fails(&["venv", "create"]), "already has a venv");

    lab.ok(&["venv", "remove", "-y"]);
    assert!(!lab.venv().exists());
    assert!(lab.manifest_path().is_file(), "the manifest is the project's, not the venv's");
    // A project without its venv: status answers, it does not refuse.
    has(&lab.ok(&["venv", "status"]), "`lvpm venv create` makes one");
}

#[test]
fn sbom_exports_the_selected_installed_package_graph() {
    use sha2::Digest;

    let lab = Lab::new("sbom");
    lab.package_with(
        "app",
        "1.0.0.1",
        "lib",
        "[Description]\nVendor=\"Example Supplier\"\nLicense=\"BSD\"\n",
        &[],
    );
    lab.package("lib", "2.0.0.1", "");
    lab.package("tester", "3.0.0.1", "");
    lab.write_manifest(
        "[dependencies]\napp = \"*\"\n\n[dependencies.test]\ntester = \"*\"\n\n\
         [nipm.dependencies]\nni-visa = \"24.0\"\n",
    );
    lab.ok(&["venv", "create"]);
    lab.ok(&["install", "--with", "test"]);

    let output = lab.project().join("bom.cdx.json");
    lab.ok(&["sbom", "--output", "bom.cdx.json"]);
    let sbom: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
    assert_eq!(sbom["specVersion"], "1.5");
    assert_eq!(sbom["metadata"]["component"]["name"], "demo");
    assert_eq!(sbom["components"].as_array().unwrap().len(), 2);
    let app = &sbom["components"][0];
    assert_eq!(app["purl"], "pkg:generic/vipm/app@1.0.0.1");
    assert_eq!(app["supplier"]["name"], "Example Supplier");
    assert_eq!(app["licenses"][0]["license"]["name"], "BSD");
    let archive = std::fs::read(lab.root.join("packages/app-1.0.0.1.vip")).unwrap();
    let hex = |bytes: &[u8]| bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    assert_eq!(app["hashes"], serde_json::json!([
        {"alg": "MD5", "content": hex(&md5::Md5::digest(&archive))},
        {"alg": "SHA-256", "content": hex(&sha2::Sha256::digest(&archive))}
    ]));
    let lib = &sbom["components"][1];
    assert_eq!(lib["version"], "2.0.0.1");
    assert!(lib.get("supplier").is_none());
    assert!(lib.get("licenses").is_none());
    assert_eq!(sbom["dependencies"], serde_json::json!([
        {"ref": "lvpm:project", "dependsOn": ["lvpm:package:app"]},
        {"ref": "lvpm:package:app", "dependsOn": ["lvpm:package:lib"]},
        {"ref": "lvpm:package:lib", "dependsOn": []}
    ]));

    let sqlite = serde_json::json!({
        "name": "SQLite", "type": "library", "version": "3.50.4",
        "bom-ref": "pkg:generic/sqlite@3.50.4", "purl": "pkg:generic/sqlite@3.50.4"
    });
    let manual_path = lab.project().join("manual.cdx.json");
    let mut manual = serde_json::json!({
        "bomFormat": "CycloneDX", "specVersion": "1.5", "version": 1,
        "components": [sqlite, sqlite]
    });
    std::fs::write(&manual_path, serde_json::to_vec(&manual).unwrap()).unwrap();
    let include = ["sbom", "--include", "manual.cdx.json", "--output", "bom.cdx.json"];
    lab.ok(&include);
    let included = std::fs::read(&output).unwrap();
    let merged: serde_json::Value = serde_json::from_slice(&included).unwrap();
    assert_eq!(merged["metadata"], sbom["metadata"]);
    assert_eq!(merged["components"], serde_json::json!([app, lib, sqlite]));
    assert_eq!(merged["dependencies"], serde_json::json!([
        {"ref": "lvpm:project", "dependsOn": ["lvpm:package:app", "pkg:generic/sqlite@3.50.4"]},
        sbom["dependencies"][1], sbom["dependencies"][2]
    ]));
    manual["components"][1]["bom-ref"] = serde_json::json!("different-ref");
    std::fs::write(&manual_path, serde_json::to_vec(&manual).unwrap()).unwrap();
    has(&lab.fails(&include), "conflicting component");
    assert_eq!(std::fs::read(&output).unwrap(), included);

    // Installed records written by older lvpm versions lack archive metadata.
    let tester_record = lab.record("tester");
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&tester_record).unwrap()).unwrap();
    record.as_object_mut().unwrap().remove("package_metadata").unwrap();
    std::fs::write(&tester_record, serde_json::to_vec(&record).unwrap()).unwrap();

    lab.ok(&["sbom", "--only", "test", "--output", "bom.cdx.json"]);
    let previous = std::fs::read(&output).unwrap();
    let test_sbom: serde_json::Value = serde_json::from_slice(&previous).unwrap();
    assert_eq!(test_sbom["components"].as_array().unwrap().len(), 1);
    let tester = &test_sbom["components"][0];
    assert_eq!(tester["purl"], "pkg:generic/vipm/tester@3.0.0.1");
    assert!(tester.get("hashes").is_none());

    std::fs::remove_file(lab.record("lib")).unwrap();
    has(
        &lab.fails(&["sbom", "--output", "bom.cdx.json"]),
        "dependency \"lib\" of app is not installed",
    );
    assert_eq!(std::fs::read(output).unwrap(), previous);
}

#[test]
fn sbom_merge_attaches_the_child_and_preserves_both_dependency_graphs() {
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    let lab = Lab::new("sbom-merge");
    let primary = json!({
        "bomFormat": "CycloneDX", "specVersion": "1.5", "version": 1,
        "metadata": {"component": {"type": "application", "name": "Package", "bom-ref": "lvpm:project"}},
        "components": [
            {"type": "library", "name": "NI Runtime", "bom-ref": "runtime"},
            {"type": "library", "name": "Shared", "bom-ref": "shared", "purl": "pkg:generic/shared"}
        ],
        "dependencies": [{"ref": "lvpm:project", "dependsOn": ["runtime", "shared"]}]
    });
    let mut child = json!({
        "bomFormat": "CycloneDX", "specVersion": "1.5", "version": 2,
        "serialNumber": "urn:uuid:28a04ec4-f6ce-4ae4-8b6c-10e3ec431754",
        "metadata": {
            "timestamp": "2026-11-20T12:00:00Z",
            "tools": [{"vendor": "Fixture Vendor", "name": "Fixture Generator", "version": "1"}],
            "lifecycles": [{"phase": "post-build"}],
            "component": {"type": "application", "name": "EXE", "bom-ref": "lvpm:project"},
            "properties": [{"name": "lvpm:scan", "value": "partial"}]
        },
        "components": [
            {"type": "library", "name": "VIPM Dependency", "bom-ref": "runtime"},
            {"type": "library", "name": "Shared", "bom-ref": "child-shared", "purl": "pkg:generic/shared"}
        ],
        "dependencies": [
            {"ref": "lvpm:project", "dependsOn": ["runtime"]},
            {"ref": "runtime", "dependsOn": ["child-shared"]}
        ],
        "compositions": [{"aggregate": "incomplete", "assemblies": ["lvpm:project"]}]
    });
    let save = |filename, bom: &serde_json::Value| {
        std::fs::write(lab.project().join(filename), serde_json::to_vec(bom).unwrap()).unwrap();
    };
    save("nipkg.cdx.json", &primary);
    save("exe.cdx.json", &child);
    let merge = ["sbom", "merge", "--primary", "nipkg.cdx.json", "--child", "exe.cdx.json", "--output", "merged.cdx.json"];
    lab.ok(&merge);
    let output = lab.project().join("merged.cdx.json");
    let bytes = std::fs::read(&output).unwrap();
    let bom: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let source_document = |node: &Value, filename| {
        let encoded = node["properties"].as_array().unwrap().iter()
            .find(|property| property["name"] == "lvpm:sbom:source-document")
            .unwrap()["value"].as_str().unwrap();
        let source: Value = serde_json::from_str(encoded).unwrap();
        let sha256 = Sha256::digest(std::fs::read(lab.project().join(filename)).unwrap())
            .iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        assert_eq!(source["sha256"], sha256);
        source
    };
    let root = &bom["metadata"]["component"];
    for (key, before) in primary["metadata"]["component"].as_object().unwrap() {
        assert_eq!(&root[key], before);
    }
    assert_eq!(source_document(root, "nipkg.cdx.json")["version"], 1);
    let components = bom["components"].as_array().unwrap();
    assert_eq!(components.len(), 4);
    let named = |name: &str| components.iter().find(|component| component["name"] == name).unwrap();
    let exe_ref = named("EXE")["bom-ref"].as_str().unwrap();
    let lib_ref = named("VIPM Dependency")["bom-ref"].as_str().unwrap();
    assert_ne!(exe_ref, "lvpm:project");
    assert_ne!(lib_ref, "runtime");
    assert!(named("EXE")["properties"].as_array().unwrap().contains(&child["metadata"]["properties"][0]));
    let source = source_document(named("EXE"), "exe.cdx.json");
    assert_eq!(source["serialNumber"], child["serialNumber"]);
    assert_eq!(source["version"], 2);
    for key in ["timestamp", "tools", "lifecycles"] {
        assert_eq!(source["metadata"][key], child["metadata"][key]);
    }
    let edges = |reference: &str| bom["dependencies"].as_array().unwrap().iter()
        .find(|row| row["ref"] == reference).unwrap()["dependsOn"].clone();
    assert_eq!(edges("lvpm:project"), json!([exe_ref, "runtime", "shared"]));
    assert_eq!(edges(exe_ref), json!([lib_ref]));
    assert_eq!(edges(lib_ref), json!(["shared"]));
    assert_eq!(bom["compositions"], json!([{"aggregate": "incomplete", "assemblies": [exe_ref]}]));

    let plans = lab.project().join("plans");
    std::fs::create_dir(&plans).unwrap();
    let plan = json!({
        "children": {"app": "../exe.cdx.json"},
        "relationships": [{
            "parent": "primary", "child": "app", "kind": "contains", "evidence": "installer layout"
        }]
    });
    save("plans/compose.json", &plan);
    let compose = |primary, output| [
        "sbom", "merge", "--primary", primary, "--relationships", "plans/compose.json",
        "--output", output,
    ];
    lab.ok(&compose("nipkg.cdx.json", "composed.cdx.json"));
    let composed = std::fs::read(lab.project().join("composed.cdx.json")).unwrap();
    let delivered: Value = serde_json::from_slice(&composed).unwrap();
    let installer = &delivered["metadata"]["component"];
    assert_eq!(installer["components"][0]["name"], "EXE");
    assert_eq!(installer["components"][0]["bom-ref"], exe_ref);
    assert_eq!(delivered["dependencies"][0]["dependsOn"], json!(["runtime", "shared"]));
    assert_eq!(delivered["compositions"], bom["compositions"]);
    lab.ok(&compose("composed.cdx.json", "composed-again.cdx.json"));
    assert_eq!(std::fs::read(lab.project().join("composed-again.cdx.json")).unwrap(), composed);
    has(&lab.fails(&compose("nipkg.cdx.json", "plans/compose.json")), "would overwrite input");
    assert_eq!(std::fs::read(lab.project().join("plans/compose.json")).unwrap(), serde_json::to_vec(&plan).unwrap());

    child["components"][1]["name"] = json!("Conflicting shared package");
    save("exe.cdx.json", &child);
    has(&lab.fails(&merge), "conflicting components with PURL");
    child["dependencies"][1]["dependsOn"] = json!(["missing-ref"]);
    save("exe.cdx.json", &child);
    has(&lab.fails(&merge), "unknown dependency or composition reference");
    assert_eq!(std::fs::read(output).unwrap(), bytes);
}

#[test]
fn sbom_diff_compares_nipkg_declarations_by_package_name() {
    use serde_json::{Value, json};

    let lab = Lab::new("nipkg-sbom-diff");
    let component = |name: &str, relationship: &str, fields: &[(&str, &str)]| {
        let mut properties = vec![json!({"name": "lvpm:nipkg:relationship", "value": relationship})];
        properties.extend(fields.iter().map(|(key, value)| json!({"name": format!("lvpm:nipkg:{key}"), "value": value})));
        json!({"type": "library", "name": name, "bom-ref": format!("lvpm:nipkg:{name}"), "properties": properties})
    };
    let bom = |source, version, components, required| json!({
        "bomFormat": "CycloneDX", "specVersion": "1.5", "version": 1,
        "metadata": {
            "component": {"type": "application", "name": "template", "version": version, "bom-ref": "lvpm:project"},
            "properties": [{"name": "lvpm:nipkg:dependency-source", "value": source}]
        },
        "components": components,
        "dependencies": [{"ref": "lvpm:project", "dependsOn": required}]
    });
    let before = bom(
        "lvproj-explicit", "1.0.0",
        [
            component("unchanged", "Required Dependency", &[("min-version", "26.3"), ("min-version-type", "Inclusive")]),
            component("pin", "Required Dependency", &[("min-version", "1"), ("min-version-type", "Inclusive")]),
            component("optional", "Recommended Dependency", &[]),
            component("removed", "Required Dependency", &[]),
        ],
        vec!["lvpm:nipkg:unchanged", "lvpm:nipkg:pin", "lvpm:nipkg:removed"],
    );
    let mut after = bom(
        "nipkg-control", "1.0.0-8",
        [
            component("unchanged", "Required Dependency", &[("version-requirement", ">=26.3")]),
            component("pin", "Required Dependency", &[("version-requirement", "= 2")]),
            component("optional", "Suggested Dependency", &[]),
            component("extra", "Alternative Dependency", &[("alternative", "extra  | another (>>1.0)")]),
        ],
        vec!["lvpm:nipkg:unchanged", "lvpm:nipkg:pin"],
    );
    let save = |name, document: &Value| {
        std::fs::write(lab.project().join(name), serde_json::to_vec(document).unwrap()).unwrap();
    };
    save("buildspec.cdx.json", &before);
    save("built.cdx.json", &after);
    let args = ["sbom", "diff", "--buildspec", "buildspec.cdx.json", "--built", "built.cdx.json"];
    let output = lab.ok(&args);
    let report: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(report["package"], "template");
    assert_eq!(report["built_version"], "1.0.0-8");
    assert_eq!(report["added"].as_array().unwrap().len(), 1);
    assert_eq!(report["added"][0]["package"], "extra");
    assert_eq!(report["added"][0]["requirement"]["alternatives"], json!(["extra | another (>> 1.0)"]));
    assert_eq!(report["missing"].as_array().unwrap().len(), 1);
    assert_eq!(report["missing"][0]["package"], "removed");
    let changed = report["changed"].as_array().unwrap();
    assert_eq!(changed.len(), 2);
    assert_eq!(changed[0]["before"]["relationships"], json!(["Recommended Dependency"]));
    assert_eq!(changed[0]["after"]["relationships"], json!(["Suggested Dependency"]));
    assert_eq!(changed[1]["before"]["constraints"], json!([">= 1"]));
    assert_eq!(changed[1]["after"]["constraints"], json!(["= 2"]));

    after["metadata"]["component"]["name"] = json!("different");
    save("built.cdx.json", &after);
    has(&lab.fails(&args), "cannot compare different packages");
}

fn packed_library_fixture(has_dependency: bool) -> Vec<u8> {
    // A Windows resource-only PE with the same directory layout as App Builder's
    // output. Generate it here so the CLI test needs neither LabVIEW nor a binary fixture.
    let mut imports = u32::from(has_dependency).to_be_bytes().to_vec();
    if has_dependency {
        let mut path = vec![0, 1, 0, 3, 0]; // Relative PTH0, starting in this folder.
        path.extend_from_slice(b"\x07support\x0dhelper.lvlibp");
        imports.extend_from_slice(b"PTH0");
        imports.extend_from_slice(&(path.len() as u32).to_be_bytes());
        imports.extend_from_slice(&path);
        let id = b"{12345678-ABCD-1234-ABCD-123456789ABC}";
        imports.extend_from_slice(&(id.len() as u32).to_be_bytes());
        imports.extend_from_slice(id);
    }
    assert!(imports.len() < 128);
    let table = [vec![imports.len() as u8], imports].concat();
    let mut leif = b"LEIF\x00\x00\x00\x22\x12\x09\x00\x00\x00\x00\x20\x00\x80\x00\x00\x00\x00\x22\x00\x00\x00\x03\x00\x00\x00\x00\x00\x00\x00\x01".to_vec();
    let library_id = if has_dependency {
        b"{AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE}"
    } else {
        b"{12345678-ABCD-1234-ABCD-123456789ABC}"
    };
    let mut metadata = vec![(1 + 4 + library_id.len()) as u8, 0];
    metadata.extend_from_slice(&(library_id.len() as u32).to_be_bytes());
    metadata.extend_from_slice(library_id);
    for chunk in [b"export metadata".as_slice(), &metadata, &table] {
        leif.extend_from_slice(&(chunk.len() as u32).to_be_bytes());
        leif.extend_from_slice(chunk);
    }
    let leif_len = leif.len() as u32;
    leif[10..14].copy_from_slice(&leif_len.to_be_bytes());

    fn put16(bytes: &mut [u8], start: usize, value: u16) {
        bytes[start..start + 2].copy_from_slice(&value.to_le_bytes());
    }
    fn put32(bytes: &mut [u8], start: usize, value: u32) {
        bytes[start..start + 4].copy_from_slice(&value.to_le_bytes());
    }
    let mut version = vec![0; 92];
    put16(&mut version, 0, 92);
    put16(&mut version, 2, 52);
    for (index, unit) in "VS_VERSION_INFO\0".encode_utf16().enumerate() {
        put16(&mut version, 6 + index * 2, unit);
    }
    put32(&mut version, 40, 0xFEEF04BD);
    put32(&mut version, 44, 0x00010000);
    put32(&mut version, 48, 0x00010002); // File version 1.2.3.4.
    put32(&mut version, 52, 0x00030004);

    let mut pe = vec![0; 0x400];
    pe[..2].copy_from_slice(b"MZ");
    put32(&mut pe, 0x3c, 0x80);
    pe[0x80..0x84].copy_from_slice(b"PE\0\0");
    put16(&mut pe, 0x84, 0x14c); // COFF machine.
    put16(&mut pe, 0x86, 1); // One section.
    put16(&mut pe, 0x94, 224); // PE32 optional header size.
    put16(&mut pe, 0x96, 0x2103);
    put16(&mut pe, 0x98, 0x10b); // PE32 magic.
    put32(&mut pe, 0x98 + 56, 0x2000); // SizeOfImage.
    put32(&mut pe, 0x98 + 60, 0x200); // SizeOfHeaders.
    put32(&mut pe, 0x98 + 92, 16); // NumberOfRvaAndSizes.
    put32(&mut pe, 0x98 + 112, 0x1000); // Resource directory RVA.
    put32(&mut pe, 0x98 + 116, 0x200); // Resource directory size.
    let section = 0x98 + 224;
    pe[section..section + 5].copy_from_slice(b".rsrc");
    put32(&mut pe, section + 8, 0x200); // Virtual size.
    put32(&mut pe, section + 12, 0x1000); // Section RVA.
    put32(&mut pe, section + 16, 0x200); // File size.
    put32(&mut pe, section + 20, 0x200); // File offset.

    let resources = &mut pe[0x200..];
    put16(resources, 14, 2);
    let directory = 0x80000000;
    for (entry, id, next) in [
        (16, 10, directory | 32),
        (24, 16, directory | 56),
        (48, 2, directory | 80),
        (72, 1, directory | 104),
        (96, 0, 128),
        (120, 1033, 144),
    ] {
        put32(resources, entry, id);
        put32(resources, entry + 4, next);
    }
    for offset in [32, 56, 80, 104] {
        put16(resources, offset + 14, 1);
    }
    let version_offset = 160 + leif.len().next_multiple_of(4);
    put32(resources, 128, 0x1000 + 160);
    put32(resources, 132, leif_len);
    put32(resources, 144, 0x1000 + version_offset as u32);
    put32(resources, 148, version.len() as u32);
    resources[160..160 + leif.len()].copy_from_slice(&leif);
    resources[version_offset..version_offset + version.len()].copy_from_slice(&version);
    pe
}

#[test]
fn sbom_exports_declared_dependencies_from_a_built_packed_library() {
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    let lab = Lab::new("ppl-sbom");
    let project = lab.project();
    let archive = project.join("demo.lvlibp");
    let output = project.join("bom.cdx.json");
    let args = ["sbom", "--lvlibp", "demo.lvlibp", "--output", "bom.cdx.json"];
    let read = || serde_json::from_slice::<Value>(&std::fs::read(&output).unwrap()).unwrap();

    let bytes = packed_library_fixture(true);
    std::fs::write(&archive, &bytes).unwrap();
    lab.ok(&args);
    let bom = read();
    assert_eq!(bom["specVersion"], "1.5");
    assert!(bom["metadata"]["properties"].as_array().unwrap().contains(&json!({
        "name": "lvpm:ppl:dependency-source", "value": "leif-direct-packed-library-references"
    })));
    let subject = &bom["metadata"]["component"];
    assert_eq!(subject["type"], "library");
    assert_eq!(subject["name"], "demo");
    assert_eq!(subject["version"], "1.2.3.4");
    let digest: String = Sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect();
    assert_eq!(subject["hashes"], json!([{"alg": "SHA-256", "content": digest}]));
    assert_eq!(bom["compositions"][0]["aggregate"], "incomplete");

    let components = bom["components"].as_array().unwrap();
    assert_eq!(components.len(), 1);
    let helper = &components[0];
    assert_eq!(helper["type"], "library");
    assert_eq!(helper["name"], "helper.lvlibp");
    assert!(helper.get("version").is_none());
    let properties = helper["properties"].as_array().unwrap();
    assert!(properties.contains(&json!({"name": "lvpm:ppl:reference-path", "value": "support/helper.lvlibp"})));
    assert!(properties.contains(&json!({"name": "lvpm:ppl:library-id", "value": "{12345678-ABCD-1234-ABCD-123456789ABC}"})));
    let root = bom["dependencies"].as_array().unwrap().iter()
        .find(|row| row["ref"] == subject["bom-ref"]).unwrap();
    assert_eq!(root["dependsOn"], json!([helper["bom-ref"]]));
    assert_eq!(std::fs::read(&archive).unwrap(), bytes);

    let without_imports = packed_library_fixture(false);
    std::fs::write(&archive, &without_imports).unwrap();
    lab.ok(&args);
    let bom = read();
    assert_eq!(bom["components"], json!([]));
    assert_eq!(bom["dependencies"][0]["dependsOn"], json!([]));
    assert_eq!(bom["compositions"][0]["aggregate"], "incomplete");

    let previous = std::fs::read(&output).unwrap();
    std::fs::write(&archive, &bytes[..0x240]).unwrap();
    lab.fails(&args);
    assert_eq!(std::fs::read(output).unwrap(), previous);
}

#[test]
fn project_sbom_merges_a_separate_sidecar_into_a_transitive_packed_library() {
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    let lab = Lab::new("project-ppl-sbom");
    let project = lab.project();
    let bytes = packed_library_fixture(true);
    std::fs::write(project.join("demo.lvlibp"), &bytes).unwrap();
    std::fs::write(project.join("helper.lvlib"), b"<Library LVVersion=\"24008000\"></Library>").unwrap();
    std::fs::write(project.join("test.lvproj"), r#"
<Project>
  <Item Name="My Computer">
    <Item Name="demo.lvlibp" Type="LVLibp" URL="../demo.lvlibp"/>
    <Item Name="helper.lvlib" Type="Library" URL="../helper.lvlib"/>
    <Item Name="Build Specifications" Type="Build">
      <Item Name="Program" Type="EXE">
        <Property Name="Source[0].itemID">/My Computer/demo.lvlibp</Property>
        <Property Name="Source[0].sourceInclusion">Include</Property>
      </Item>
      <Item Name="Sources" Type="Source Distribution">
        <Property Name="Source[0].itemID">/My Computer/demo.lvlibp</Property>
        <Property Name="Source[0].sourceInclusion">Include</Property>
      </Item>
      <Item Name="Helper" Type="Packed Library">
        <Property Name="Source[0].itemID">/My Computer/helper.lvlib</Property>
        <Property Name="Source[0].Library.LVLIBPtopLevel">true</Property>
        <Property Name="TgtF_productName">Helper SDK</Property>
        <Property Name="TgtF_targetfileName">helper.lvlibp</Property>
      </Item>
    </Item>
  </Item>
</Project>
"#).unwrap();
    let read = |name: &str| serde_json::from_slice::<Value>(&std::fs::read(project.join(name)).unwrap()).unwrap();
    let artifact_name = |name: &str| json!({"name": "lvpm:ppl:artifact-name", "value": name});
    let digest: String = Sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect();
    fn artifact<'a>(bom: &'a Value, property: &Value) -> &'a Value {
        bom["components"].as_array().unwrap().iter()
            .find(|component| component["properties"].as_array().is_some_and(|properties| properties.contains(property)))
            .unwrap_or_else(|| panic!("missing packed library component for {property} in {bom}"))
    }
    fn direct<'a>(bom: &'a Value, reference: &Value) -> &'a Value {
        &bom["dependencies"].as_array().unwrap().iter().find(|edge| &edge["ref"] == reference)
            .unwrap_or_else(|| panic!("missing dependency row for {reference} in {bom}"))["dependsOn"]
    }
    for (spec, output) in [("Program", "exe.cdx.json"), ("Sources", "src.cdx.json")] {
        lab.ok(&[
            "sbom", "--lvproj", "test.lvproj", "--lvproj-target", "My Computer",
            "--lvproj-build-spec", spec, "--output", output,
        ]);
        let bom = read(output);
        let demo = artifact(&bom, &artifact_name("demo.lvlibp"));
        let helper = artifact(&bom, &artifact_name("helper.lvlibp"));
        assert_eq!(demo["version"], "1.2.3.4");
        assert_eq!(demo["hashes"], json!([{"alg": "SHA-256", "content": digest}]));
        assert!(!helper.get("hashes").is_some_and(|hashes| hashes.as_array().is_some_and(|hashes| !hashes.is_empty())));
        assert_eq!(bom["components"].as_array().unwrap().len(), 2);
        assert_eq!(direct(&bom, &bom["metadata"]["component"]["bom-ref"]), &json!([demo["bom-ref"]]));
        assert_eq!(direct(&bom, &demo["bom-ref"]), &json!([helper["bom-ref"]]));
        assert!(bom["compositions"].as_array().unwrap().iter().any(|composition| composition["aggregate"] == "incomplete"));
    }

    let sidecars = project.join("inventory");
    std::fs::create_dir(&sidecars).unwrap();
    let manual = json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "components": [{"type": "library", "name": "Support Package", "bom-ref": "support-package"}]
    });
    std::fs::write(sidecars.join("manual.cdx.json"), serde_json::to_vec(&manual).unwrap()).unwrap();
    lab.ok(&[
        "sbom", "--lvproj", "test.lvproj", "--lvproj-target", "My Computer",
        "--lvproj-build-spec", "Helper", "--include", "inventory/manual.cdx.json",
        "--output", "inventory/sidecar.cdx.json",
    ]);
    let sidecar = read("inventory/sidecar.cdx.json");
    assert_eq!(sidecar["metadata"]["component"]["name"], "Helper SDK");
    assert!(sidecar["metadata"]["component"]["properties"].as_array().unwrap().contains(&artifact_name("helper.lvlibp")));

    let merge = |primary, output| {
        lab.ok(&[
            "sbom", "merge", "--primary", primary, "--child", "inventory/sidecar.cdx.json", "--output", output,
        ]);
        read(output)
    };
    let original = read("exe.cdx.json");
    let demo_ref = &artifact(&original, &artifact_name("demo.lvlibp"))["bom-ref"];
    let helper_ref = &artifact(&original, &artifact_name("helper.lvlibp"))["bom-ref"];
    let root_ref = &original["metadata"]["component"]["bom-ref"];
    let merged = merge("exe.cdx.json", "merged.cdx.json");
    assert_eq!(&artifact(&merged, &artifact_name("helper.lvlibp"))["bom-ref"], helper_ref);
    assert_eq!(merged["components"].as_array().unwrap().len(), 3);
    assert_eq!(direct(&merged, root_ref), &json!([demo_ref]));
    assert_eq!(direct(&merged, demo_ref), &json!([helper_ref]));
    assert_eq!(direct(&merged, helper_ref), &json!(["support-package"]));
    assert_eq!(merge("merged.cdx.json", "repeated.cdx.json"), merged);

    lab.ok(&["sbom", "--lvlibp", "demo.lvlibp", "--output", "inventory/demo.cdx.json"]);
    lab.ok(&[
        "sbom", "merge", "--primary", "exe.cdx.json",
        "--child", "inventory/sidecar.cdx.json", "inventory/demo.cdx.json", "--output", "batch.cdx.json",
    ]);
    let batch = read("batch.cdx.json");
    assert_eq!(batch["components"].as_array().unwrap().len(), 3);
    assert_eq!(direct(&batch, root_ref), &json!([demo_ref]));
    assert_eq!(direct(&batch, demo_ref), &json!([helper_ref]));
    assert_eq!(direct(&batch, helper_ref), &json!(["support-package"]));
    lab.fails(&[
        "sbom", "merge", "--primary", "exe.cdx.json",
        "--child", "inventory/sidecar.cdx.json", "inventory/missing.cdx.json", "--output", "batch.cdx.json",
    ]);
    assert_eq!(read("batch.cdx.json"), batch);

    assert_eq!(std::fs::read(project.join("demo.lvlibp")).unwrap(), bytes);
    assert!(!project.join("support/helper.lvlibp").exists());
}

#[test]
fn sbom_scans_saved_project_items_across_targets() {
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    let lab = Lab::new("whole-project-sbom");
    lab.package_with(
        "source-dependency",
        "1.0.0.1",
        "",
        "[File Group 1]\nTarget Dir=\"<vi.lib>/source-dependency\"\nReplace Mode=\"Always\"\nNum Files=1\nFile 0=\"dependency.lvlib\"\n",
        &[("File Group 1/dependency.lvlib", b"<Library/>")],
    );
    lab.write_manifest("[dependencies]\nsource-dependency = \"*\"\n");
    lab.ok(&["venv", "create"]);
    lab.ok(&["install", "--no-linkfix"]);

    let project = lab.project();
    let packed = packed_library_fixture(true);
    let leaf = packed_library_fixture(false);
    std::fs::create_dir(project.join("support")).unwrap();
    std::fs::create_dir(project.join("native")).unwrap();
    std::fs::write(project.join("demo.lvlibp"), &packed).unwrap();
    std::fs::write(project.join("support/helper.lvlibp"), &leaf).unwrap();
    std::fs::write(project.join("native/folder.dll"), &leaf).unwrap();
    let mut dll = leaf;
    // Add a regular import descriptor and its module name in unused PE section bytes.
    dll[0x100..0x104].copy_from_slice(&0x11a0u32.to_le_bytes());
    dll[0x104..0x108].copy_from_slice(&40u32.to_le_bytes());
    dll[0x3ac..0x3b0].copy_from_slice(&0x11e0u32.to_le_bytes());
    dll[0x3e0..0x3eb].copy_from_slice(b"System.dll\0");
    std::fs::write(project.join("explicit.dll"), &dll).unwrap();
    std::fs::write(
        project.join("workspace.lvlib"),
        br#"<Library><Item Type="Library" URL="&lt;vilib&gt;/source-dependency/dependency.lvlib"/></Library>"#,
    )
    .unwrap();
    std::fs::write(
        project.join("Demo.lvproj"),
        r#"<Project>
  <Item Name="My Computer" Type="My Computer">
    <Item Name="demo.lvlibp" Type="LVLibp" URL="../demo.lvlibp"/>
    <Item Name="explicit.dll" Type="Document" URL="../explicit.dll"/>
    <Item Name="Dependencies" Type="Dependencies">
      <Item Name="cached.dll" Type="Document" URL="../cached.dll"/>
    </Item>
    <Item Name="Build Specifications" Type="Build">
      <Item Name="Program" Type="EXE">
        <Property Name="Source[0].itemID">/My Computer/demo.lvlibp</Property>
        <Property Name="Source[0].sourceInclusion">Include</Property>
        <Item Name="build-only.dll" Type="Document" URL="../build-only.dll"/>
      </Item>
    </Item>
  </Item>
  <Item Name="Other Target" Type="RT Target">
    <Item Name="Packed Library Alias.lvlibp" Type="LVLibp" URL="../demo.lvlibp"/>
    <Item Name="Development" Type="Folder">
      <Item Name="workspace.lvlib" Type="Library" URL="../workspace.lvlib"/>
    </Item>
    <Item Name="Native" Type="Folder" URL="../native">
      <Property Name="NI.DISK" Type="Bool">true</Property>
    </Item>
  </Item>
</Project>"#,
    )
    .unwrap();

    fn property<'a>(node: &'a Value, name: &str) -> Option<&'a str> {
        node["properties"]
            .as_array()?
            .iter()
            .find(|item| item["name"] == name)?["value"]
            .as_str()
    }

    fn dependency_row<'a>(bom: &'a Value, reference: &Value) -> &'a [Value] {
        bom["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| &row["ref"] == reference)
            .unwrap()["dependsOn"]
            .as_array()
            .unwrap()
    }

    let raw = project.join("project.cdx.json");
    lab.ok(&[
        "sbom",
        "--lvproj",
        "Demo.lvproj",
        "--output",
        "project.cdx.json",
    ]);
    let raw_bytes = std::fs::read(&raw).unwrap();
    let bom: Value = serde_json::from_slice(&raw_bytes).unwrap();
    let metadata = &bom["metadata"];
    assert_eq!(property(metadata, "lvpm:labview:scan-scope"), Some("project"));
    assert_eq!(
        property(metadata, "lvpm:labview:ownership-source"),
        Some("installed-file-ownership")
    );
    let subject = &metadata["component"];
    assert_eq!(subject["name"], "Demo");
    assert!(subject.get("version").is_none());
    let components = bom["components"].as_array().unwrap();
    let component = |name: &str| {
        let found: Vec<_> = components
            .iter()
            .filter(|node| node["name"] == name)
            .collect();
        assert_eq!(found.len(), 1, "expected one component named {name}: {bom}");
        found[0]
    };
    let roots = dependency_row(&bom, &subject["bom-ref"]);
    let package = component("source-dependency");
    assert_eq!(package["purl"], "pkg:generic/vipm/source-dependency@1.0.0.1");
    assert!(roots.contains(&package["bom-ref"]));

    let parent_ppl = component("demo.lvlibp");
    let child_ppl = component("helper.lvlibp");
    assert!(roots.contains(&parent_ppl["bom-ref"]));
    assert_eq!(
        dependency_row(&bom, &parent_ppl["bom-ref"]),
        &[child_ppl["bom-ref"].clone()]
    );
    assert_eq!(property(parent_ppl, "lvpm:ppl:shipment"), None);

    for (name, key) in [
        ("explicit.dll", "explicit.dll"),
        ("folder.dll", "native/folder.dll"),
    ] {
        let binary = component(name);
        assert_eq!(binary["type"], "library");
        assert!(
            binary["bom-ref"]
                .as_str()
                .unwrap()
                .starts_with("lvpm:labview:project-file:")
        );
        assert!(
            property(binary, "lvpm:labview:project-path")
                .unwrap()
                .ends_with(key)
        );
        let bytes = std::fs::read(project.join(key)).unwrap();
        let digest: String = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(binary["hashes"], json!([{"alg": "SHA-256", "content": digest}]));
        assert!(roots.contains(&binary["bom-ref"]));
    }
    let explicit = component("explicit.dll");
    assert_eq!(property(explicit, "lvpm:pe:machine"), Some("0x014c"));
    let import: Value = serde_json::from_str(property(explicit, "lvpm:pe:import").unwrap()).unwrap();
    assert_eq!(import["name"], "System.dll");
    assert_eq!(import["status"], "unresolved");
    assert!(
        dependency_row(&bom, &explicit["bom-ref"])
            .contains(&json!("lvpm:pe:import:014c:system.dll"))
    );

    let sqlite = "pkg:generic/sqlite@3.46.1";
    let manual = json!({
        "bomFormat": "CycloneDX", "specVersion": "1.5", "version": 1,
        "components": [{"type": "library", "name": "SQLite", "version": "3.46.1", "bom-ref": sqlite, "purl": sqlite}],
    });
    std::fs::write(
        project.join("manual.cdx.json"),
        serde_json::to_vec(&manual).unwrap(),
    )
    .unwrap();
    lab.ok(&[
        "sbom",
        "--lvproj",
        "Demo.lvproj",
        "--include",
        "manual.cdx.json",
        "--output",
        "project-with-manual.cdx.json",
    ]);
    let with_manual: Value =
        serde_json::from_slice(&std::fs::read(project.join("project-with-manual.cdx.json")).unwrap())
            .unwrap();
    assert_eq!(&with_manual["metadata"]["component"], subject);
    let manual_roots = dependency_row(&with_manual, &subject["bom-ref"]);
    let sqlite_ref = json!(sqlite);
    assert!(roots.iter().all(|reference| manual_roots.contains(reference)));
    assert_eq!(manual_roots.len(), roots.len() + 1);
    assert!(manual_roots.contains(&sqlite_ref));
    assert_eq!(
        with_manual["components"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|node| node["bom-ref"] == sqlite_ref)
            .count(),
        1
    );
    assert_eq!(std::fs::read(&raw).unwrap(), raw_bytes);

    lab.fails(&["sbom", "--lvproj", "Demo.lvproj", "--output", "explicit.dll"]);
    assert_eq!(std::fs::read(project.join("explicit.dll")).unwrap(), dll);
}

#[test]
fn project_sbom_keeps_existing_output_when_a_required_source_is_missing() {
    let lab = Lab::new("whole-project-missing-source");
    let project = lab.project();
    std::fs::write(project.join("present.lvlib"), b"<Library/>").unwrap();
    std::fs::write(
        project.join("Demo.lvproj"),
        r#"<Project>
  <Item Name="My Computer" Type="My Computer">
    <Item Name="present.lvlib" Type="Library" URL="../present.lvlib"/>
    <Item Name="missing.lvlib" Type="Library" URL="../missing.lvlib"/>
  </Item>
</Project>"#,
    )
    .unwrap();
    let output = project.join("bom.cdx.json");
    std::fs::write(&output, b"prior output").unwrap();
    let strict = lab.fails(&[
        "sbom",
        "--lvproj",
        "Demo.lvproj",
        "--diagnostics-json",
        "--output",
        "bom.cdx.json",
    ]);
    has(&strict, "sbom.incomplete");
    has(&strict, "missing.lvlib");
    assert_eq!(std::fs::read(&output).unwrap(), b"prior output");

    let partial_args = [
        "sbom",
        "--lvproj",
        "Demo.lvproj",
        "--allow-incomplete",
        "--output",
        "bom.cdx.json",
    ];
    lab.ok(&partial_args);
    let partial = std::fs::read(&output).unwrap();
    let bom: serde_json::Value = serde_json::from_slice(&partial).unwrap();
    assert!(
        bom["compositions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|scope| scope["aggregate"] == "incomplete")
    );
    let mut require_runtime = partial_args.to_vec();
    require_runtime.push("--fail-on-incomplete=runtime");
    lab.fails(&require_runtime);
    assert_eq!(std::fs::read(output).unwrap(), partial);
}

#[test]
fn distribution_sboms_include_only_configured_packed_library_sources() {
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    let lab = Lab::new("distribution-ppl-sbom");
    let project = lab.project();
    let packed = packed_library_fixture(false);
    std::fs::create_dir(project.join("stage")).unwrap();
    std::fs::write(project.join("stage/demo.lvlibp"), &packed).unwrap();
    std::fs::write(project.join("stage/unselected.lvlibp"), &packed).unwrap();
    std::fs::write(project.join("test.lvproj"), r#"
<Project>
  <Item Name="My Computer">
    <Item Name="demo.lvlibp" Type="LVLibp" URL="../stage/demo.lvlibp"/>
    <Item Name="unselected.lvlibp" Type="LVLibp" URL="../stage/unselected.lvlibp"/>
    <Item Name="Build Specifications" Type="Build">
      <Item Name="Installer" Type="Installer">
        <Property Name="INST_productName">Demo Installer</Property>
        <Property Name="INST_productVersion">2.0.0</Property>
        <Property Name="SourceCount">1</Property>
        <Property Name="Source[0].tag">/My Computer/demo.lvlibp</Property>
        <Property Name="Source[0].type">File</Property>
      </Item>
      <Item Name="NI Package" Type="{E661DAE2-7517-431F-AC41-30807A3BDA38}">
        <Property Name="PKG_packageName">demo</Property>
        <Property Name="PKG_version">2.0.0</Property>
        <Property Name="PKG_dependencies.Count">1</Property>
        <Property Name="PKG_dependencies[0].Package.Name">ni-runtime</Property>
        <Property Name="PKG_dependencies[0].Relationship">Required Dependency</Property>
        <Property Name="PKG_sources.Count">1</Property>
        <Property Name="PKG_sources[0].ID">/My Computer/Build Specifications/Packed</Property>
        <Property Name="PKG_sources[0].Type">Packed Library Build</Property>
      </Item>
      <Item Name="Packed" Type="Packed Library">
        <Property Name="Destination[0].path">stage/demo.lvlibp</Property>
      </Item>
      <Item Name="Archive" Type="Zip File">
        <Property Name="Source[0].itemID">/My Computer/demo.lvlibp</Property>
        <Property Name="Source[0].sourceInclusion">Include</Property>
        <Property Name="Source[1].itemID">/My Computer/unselected.lvlibp</Property>
        <Property Name="Source[1].sourceInclusion">Exclude</Property>
      </Item>
    </Item>
  </Item>
</Project>
"#).unwrap();
    let digest: String = Sha256::digest(&packed).iter().map(|byte| format!("{byte:02x}")).collect();
    for spec in ["Installer", "NI Package", "Archive"] {
        lab.ok(&[
            "sbom", "--lvproj", "test.lvproj", "--lvproj-target", "My Computer",
            "--lvproj-build-spec", spec, "--output", "bom.cdx.json",
        ]);
        let bom: Value = serde_json::from_slice(&std::fs::read(project.join("bom.cdx.json")).unwrap()).unwrap();
        let components = bom["components"].as_array().unwrap();
        let direct = bom["dependencies"][0]["dependsOn"].as_array().unwrap();
        let ppl = components.iter().find(|component| {
            component["properties"].as_array().is_some_and(|props| {
                props.contains(&json!({"name": "lvpm:ppl:artifact-name", "value": "demo.lvlibp"}))
            })
        }).unwrap_or_else(|| panic!("{spec} did not export the selected PPL: {bom}"));
        assert_eq!(ppl["hashes"], json!([{"alg": "SHA-256", "content": digest}]));
        assert!(direct.contains(&ppl["bom-ref"]));
        assert_eq!(components.len(), if spec == "NI Package" { 2 } else { 1 });
        if spec == "NI Package" {
            assert!(direct.contains(&json!("lvpm:nipkg:ni-runtime")));
        }
    }
}

#[test]
fn sbom_exports_requirements_and_packed_libraries_from_a_built_nipkg() {
    use serde_json::json;
    use sha2::{Digest, Sha256};

    let lab = Lab::new("nipkg-artifact-sbom");
    let control = b"\xef\xbb\xbfArchitecture: windows_x64\r\nPackage: template\r\nVersion: 1.0.0-8\r\n\
        Depends: ni-runtime (>= 26.3),\r\n ni-visa (<=26.5), choice-a | choice-b (>>1.0)\r\n\
        Recommends: ni-certs\r\nSuggests: ni-help\r\n";
    let gzip_tar = |entries: &[(&str, &[u8])]| {
        let mut tar = Vec::new();
        for (name, body) in entries {
            let mut header = [0; 512];
            header[..name.len()].copy_from_slice(name.as_bytes());
            header[124..135].copy_from_slice(format!("{:011o}", body.len()).as_bytes());
            header[148..156].fill(b' ');
            header[156] = b'0';
            header[257..263].copy_from_slice(b"ustar\0");
            let checksum: u32 = header.iter().map(|&byte| u32::from(byte)).sum();
            header[148..154].copy_from_slice(format!("{checksum:06o}").as_bytes());
            header[154] = 0;
            tar.extend(header);
            tar.extend(*body);
            tar.resize(tar.len().next_multiple_of(512), 0);
        }
        tar.resize(tar.len() + 1024, 0);
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    };

    let control = gzip_tar(&[("./control", control)]);
    let demo = packed_library_fixture(true);
    let helper = packed_library_fixture(false);
    let payload = gzip_tar(&[
        ("./Program Files/demo.lvlibp", &demo),
        ("./Program Files/support/helper.lvlibp", &helper),
        ("./Program Files/README.txt", b"not a packed library"),
    ]);
    let mut pkg = b"!<arch>\n".to_vec();
    for (name, data) in [("data.tar.gz/", &payload), ("control.tar.gz/", &control)] {
        writeln!(pkg, "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}\x60", 0, 0, 0, 644, data.len()).unwrap();
        pkg.extend(data);
        if data.len() % 2 != 0 {
            pkg.push(b'\n');
        }
    }
    let file = lab.project().join("template.nipkg");
    std::fs::write(&file, &pkg).unwrap();
    let args = ["sbom", "--nipkg", "template.nipkg", "--output", "bom.cdx.json"];
    lab.ok(&args);
    let output = lab.project().join("bom.cdx.json");
    let bytes = std::fs::read(&output).unwrap();
    let bom: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(bom["metadata"]["component"]["version"], "1.0.0-8");
    let components = bom["components"].as_array().unwrap();
    assert_eq!(components.len(), 8);
    let find = |name: &str| components.iter().find(|c| c["name"] == name).unwrap();
    let parent = find("demo.lvlibp");
    let child = find("helper.lvlibp");
    let root_edges = bom["dependencies"][0]["dependsOn"].as_array().unwrap();
    assert_eq!(root_edges.len(), 4);
    for reference in [json!("lvpm:nipkg:ni-runtime"), json!("lvpm:nipkg:ni-visa"), parent["bom-ref"].clone(), child["bom-ref"].clone()] {
        assert!(root_edges.contains(&reference));
    }
    for (component, path, binary) in [
        (parent, "Program Files/demo.lvlibp", &demo),
        (child, "Program Files/support/helper.lvlibp", &helper),
    ] {
        assert_eq!(component["version"], "1.2.3.4");
        let digest: String = Sha256::digest(binary).iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(component["hashes"], json!([{"alg": "SHA-256", "content": digest}]));
        let properties = component["properties"].as_array().unwrap();
        assert!(properties.contains(&json!({"name": "lvpm:ppl:payload-path", "value": path})));
        assert!(properties.contains(&json!({"name": "lvpm:ppl:shipment", "value": "package-payload"})));
    }
    let parent_edge = bom["dependencies"].as_array().unwrap().iter()
        .find(|row| row["ref"] == parent["bom-ref"]).unwrap();
    assert_eq!(parent_edge["dependsOn"], json!([child["bom-ref"]]));
    assert_eq!(bom["compositions"][0]["aggregate"], "incomplete");
    assert!(find("choice-b")["properties"].as_array().unwrap().contains(
        &json!({"name": "lvpm:nipkg:version-requirement", "value": ">> 1.0"})
    ));
    assert!(find("ni-visa")["properties"].as_array().unwrap().contains(
        &json!({"name": "lvpm:nipkg:max-version-inclusive", "value": "true"})
    ));
    assert!(find("ni-runtime").get("version").is_none());

    // The post-build path combines the package graph and the direct inventory.
    lab.ok(&["sbom", "--artifact", "template.nipkg", "--output", "artifact.cdx.json"]);
    let artifact = lab.project().join("artifact.cdx.json");
    let combined_bytes = std::fs::read(&artifact).unwrap();
    let combined: serde_json::Value = serde_json::from_slice(&combined_bytes).unwrap();
    let shipped = combined["metadata"]["component"]["components"].as_array().unwrap();
    assert_eq!(shipped.len(), 3);
    for packed in [parent, child] {
        let same_file: Vec<_> = shipped.iter().filter(|item| item["bom-ref"] == packed["bom-ref"]).collect();
        assert_eq!(same_file.len(), 1);
        assert_eq!(same_file[0]["hashes"], packed["hashes"]);
    }
    let readme = shipped.iter().find(|item| item["name"] == "README.txt").unwrap();
    assert_eq!(readme["hashes"][0]["alg"], "SHA-256");
    assert!(!combined["components"].as_array().unwrap().iter().any(|item| {
        item["bom-ref"] == parent["bom-ref"] || item["bom-ref"] == child["bom-ref"]
    }));
    assert!(combined["dependencies"].as_array().unwrap().contains(parent_edge));
    assert!(combined["dependencies"][0]["dependsOn"].as_array().unwrap()
        .contains(&json!("lvpm:nipkg:ni-runtime")));

    std::fs::write(&file, &pkg[..pkg.len() - 8]).unwrap();
    has(&lab.fails(&["sbom", "--artifact", "template.nipkg", "--output", "artifact.cdx.json"]),
        "truncated .nipkg member");
    assert_eq!(std::fs::read(artifact).unwrap(), combined_bytes);
    has(&lab.fails(&args), "truncated .nipkg member");
    assert_eq!(std::fs::read(output).unwrap(), bytes);
}

#[test]
fn sbom_exports_explicit_nipkg_requirements_from_the_selected_build_spec() {
    let lab = Lab::new("nipkg-sbom");
    std::fs::write(lab.project().join("test.lvproj"), r#"
<Project>
  <Item Name="Other Target">
    <Item Name="Build Specifications" Type="Build">
      <Item Name="NIPKG" Type="EXE"/>
    </Item>
  </Item>
  <Item Name="Machine &amp; IO">
    <Item Name="Build Specifications" Type="Build">
      <Item Name="Unsupported" Type="DLL"/>
      <Item Name="NIPKG" Type="{E661DAE2-7517-431F-AC41-30807A3BDA38}">
        <Property Name="PKG_packageName">example</Property>
        <Property Name="PKG_version">1.2.3</Property>
        <Property Name="PKG_autoSelectDeps">false</Property>
        <Property Name="PKG_dependencies.Count">2</Property>
        <Property Name="PKG_dependencies[0].Package.Name">ni-runtime</Property>
        <Property Name="PKG_dependencies[0].Relationship">Required Dependency</Property>
        <Property Name="PKG_dependencies[0].MinVersion">26.3.0-0+f1</Property>
        <Property Name="PKG_dependencies[0].MinVersionType">Inclusive</Property>
        <Property Name="PKG_dependencies[0].MaxVersion"/>
        <Property Name="PKG_dependencies[0].MaxVersionInclusive">false</Property>
        <Property Name="PKG_dependencies[1].Package.Name">ni-tools</Property>
        <Property Name="PKG_dependencies[1].Relationship">Recommended Dependency</Property>
      </Item>
    </Item>
  </Item>
</Project>
"#).unwrap();
    let export = |build_spec| [
        "sbom", "--lvproj", "test.lvproj", "--lvproj-target", "Machine & IO",
        "--lvproj-build-spec", build_spec, "--output", "bom.cdx.json",
    ];
    lab.ok(&export("NIPKG"));
    let output = lab.project().join("bom.cdx.json");
    let bytes = std::fs::read(&output).unwrap();
    let bom: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(bom["metadata"]["component"]["name"], "example");
    assert_eq!(bom["metadata"]["component"]["version"], "1.2.3");
    assert_eq!(bom["components"].as_array().unwrap().len(), 2);
    let runtime = &bom["components"][0];
    assert_eq!(runtime["name"], "ni-runtime");
    assert!(runtime.get("version").is_none());
    assert_eq!(runtime["properties"], serde_json::json!([
        {"name": "lvpm:nipkg:relationship", "value": "Required Dependency"},
        {"name": "lvpm:nipkg:min-version", "value": "26.3.0-0+f1"},
        {"name": "lvpm:nipkg:min-version-type", "value": "Inclusive"}
    ]));
    assert_eq!(bom["components"][1]["name"], "ni-tools");
    assert_eq!(bom["dependencies"][0]["dependsOn"], serde_json::json!(["lvpm:nipkg:ni-runtime"]));

    has(&lab.fails(&export("Unsupported")), "has unsupported type");
    has(&lab.fails(&export("missing")), "not found on target");
    assert_eq!(std::fs::read(output).unwrap(), bytes);
}

fn add_offline_vip(lab: &Lab, name: &str, requires: &str, installed_file: &Path) {
    let version = "1.2.3.4";
    lab.package(name, version, requires);
    let record = lab.project().join("vipm-db").join(name);
    std::fs::create_dir_all(&record).unwrap();
    std::fs::write(
        record.join("files-installed"),
        format!("{}\n", installed_file.canonicalize().unwrap().display()),
    )
    .unwrap();
    let archive = format!("{name}-{version}.vip");
    std::fs::copy(lab.root.join("packages").join(&archive), record.join(&archive)).unwrap();
}

#[test]
fn project_and_build_sboms_resolve_transitive_requirements_from_the_offline_vipm_database() {
    use serde_json::{Value, json};

    let lab = Lab::new("offline-vipm-sbom");
    let project = lab.project();
    let vi_lib = lab.root.join("labview/vi.lib");
    for (package, requires) in [
        ("used-vip", "bridge-vip>=1.2.0.0"),
        ("bridge-vip", "leaf-vip>=1.2.3.0,leaf-vip=1.2.3.4"),
        ("leaf-vip", ""),
        // A bad requirement in an unrelated package has no effect on this SBOM.
        ("unrelated-vip", "uninstalled-vip>=8.0.0.0"),
    ] {
        let library = vi_lib.join(format!("{package}.lvlib"));
        std::fs::write(&library, b"<Library/>").unwrap();
        add_offline_vip(&lab, package, requires, &library);
    }
    std::fs::write(
        project.join("application.lvlib"),
        br#"<Library><Item Type="Library" URL="&lt;vilib&gt;/used-vip.lvlib"/></Library>"#,
    )
    .unwrap();
    std::fs::write(
        project.join("test.lvproj"),
        r#"<Project>
  <Item Name="My Computer" Type="My Computer">
    <Item Name="application.lvlib" Type="Library" URL="../application.lvlib"/>
    <Item Name="Build Specifications" Type="Build">
      <Item Name="Program" Type="EXE">
        <Property Name="Source[0].itemID">/My Computer/application.lvlib</Property>
        <Property Name="Source[0].sourceInclusion">TopLevel</Property>
      </Item>
      <Item Name="Source" Type="Source Distribution">
        <Property Name="Source[0].itemID">/My Computer/application.lvlib</Property>
        <Property Name="Source[0].sourceInclusion">Include</Property>
      </Item>
      <Item Name="NIPKG" Type="{E661DAE2-7517-431F-AC41-30807A3BDA38}">
        <Property Name="PKG_packageName">example</Property>
        <Property Name="PKG_version">1.0.0</Property>
        <Property Name="PKG_dependencies.Count">1</Property>
        <Property Name="PKG_dependencies[0].Package.Name">ni-runtime</Property>
        <Property Name="PKG_dependencies[0].Relationship">Required Dependency</Property>
        <Property Name="PKG_sources.Count">1</Property>
        <Property Name="PKG_sources[0].ID">/My Computer/Build Specifications/Program</Property>
        <Property Name="PKG_sources[0].Type">EXE Build</Property>
      </Item>
    </Item>
  </Item>
</Project>"#,
    )
    .unwrap();
    let vip = json!("pkg:generic/vipm/used-vip@1.2.3.4");
    let bridge = json!("pkg:generic/vipm/bridge-vip@1.2.3.4");
    let leaf = json!("pkg:generic/vipm/leaf-vip@1.2.3.4");
    let sqlite = json!("pkg:generic/sqlite@3.46.1");
    let manual = json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "components": [
            {"type": "library", "name": "SQLite", "version": "3.46.1", "bom-ref": sqlite, "purl": sqlite}
        ]
    });
    std::fs::write(
        project.join("manual.cdx.json"),
        serde_json::to_vec(&manual).unwrap(),
    )
    .unwrap();

    let export = |spec| -> Value {
        let mut args = vec![
            "sbom", "--lvproj", "test.lvproj", "--vipm-database", "vipm-db",
            "--include", "manual.cdx.json", "--output", "scan.cdx.json",
        ];
        if let Some(spec) = spec {
            args.extend(["--lvproj-target", "My Computer", "--lvproj-build-spec", spec]);
        }
        lab.ok(&args);
        serde_json::from_slice(&std::fs::read(project.join("scan.cdx.json")).unwrap()).unwrap()
    };
    fn direct<'a>(bom: &'a Value, reference: &Value) -> &'a [Value] {
        bom["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| &row["ref"] == reference)
            .unwrap()["dependsOn"]
            .as_array()
            .unwrap()
    }

    fn component<'a>(items: &'a [Value], purl: &Value) -> &'a Value {
        let mut matching = items.iter().filter(|item| &item["purl"] == purl);
        let item = matching.next().expect("expected exactly one component");
        assert!(matching.next().is_none());
        item
    }

    fn property<'a>(component: &'a Value, name: &str) -> &'a str {
        component["properties"]
            .as_array()
            .unwrap()
            .iter()
            .find(|property| property["name"] == name)
            .unwrap()["value"]
            .as_str()
            .unwrap()
    }

    for spec in [None, Some("Program"), Some("Source"), Some("NIPKG")] {
        let bom = export(spec);
        let product = &bom["metadata"]["component"];
        let root = direct(&bom, &product["bom-ref"]);
        let components = bom["components"].as_array().unwrap();
        let used = component(components, &vip);
        let bridge_package = component(components, &bridge);
        let leaf_package = component(components, &leaf);
        component(components, &sqlite);
        assert!(components.iter().all(|item| item["name"] != "unrelated-vip"));
        assert_eq!(root.iter().filter(|item| *item == &sqlite).count(), 1);
        assert_eq!(direct(&bom, &vip), std::slice::from_ref(&bridge));
        assert_eq!(direct(&bom, &bridge), std::slice::from_ref(&leaf));
        assert_eq!(property(bridge_package, "lvpm:labview:matched-files"), "0");
        assert_eq!(property(leaf_package, "lvpm:labview:matched-files"), "0");
        let decode_requires = |component: &Value| {
            serde_json::from_str::<Value>(property(component, "lvpm:package:requires")).unwrap()
        };
        assert_eq!(
            decode_requires(used),
            json!({"name":"bridge-vip","version":"1.2.3.4","minimum":"1.2.0.0"}),
        );
        assert_eq!(
            decode_requires(bridge_package),
            json!({
                "name": "leaf-vip",
                "version": "1.2.3.4",
                "constraints": [
                    {"operator": "=", "version": "1.2.3.4"},
                    {"operator": ">=", "version": "1.2.3.0"}
                ]
            }),
        );
        if spec == Some("NIPKG") {
            assert_eq!(product["name"], "example");
            let child = components
                .iter()
                .chain(product["components"].as_array().into_iter().flatten())
                .find(|item| item["name"] == "Program")
                .unwrap();
            assert!(child["properties"].as_array().unwrap().contains(
                &json!({"name": "lvpm:nipkg:configured-source", "value": "true"})
            ));
            assert!(root.contains(&child["bom-ref"]));
            assert!(root.contains(&json!("lvpm:nipkg:ni-runtime")));
            assert_eq!(root.len(), 3);
            assert_eq!(direct(&bom, &child["bom-ref"]), std::slice::from_ref(&vip));
        } else {
            assert_eq!(root.len(), 2);
            assert!(root.contains(&vip));
        }
    }

    let output = project.join("scan.cdx.json");
    let previous = std::fs::read(&output).unwrap();
    let args = [
        "sbom", "--lvproj", "test.lvproj", "--vipm-database", "vipm-db",
        "--diagnostics-json", "--output", "scan.cdx.json",
    ];
    for (requirement, diagnostic) in [
        ("leaf-vip>=2.0.0.0", "leaf-vip>=2.0.0.0"),
        ("leaf-vip!=1.2.3.4", "invalid package requirement"),
        ("leaf-vip=1.2.3.0", "leaf-vip=1.2.3.0"),
    ] {
        add_offline_vip(
            &lab,
            "bridge-vip",
            requirement,
            &vi_lib.join("bridge-vip.lvlib"),
        );
        let failed = lab.fails(&args);
        has(&failed, "sbom.incomplete");
        has(&failed, "leaf-vip");
        has(&failed, diagnostic);
        assert_eq!(std::fs::read(&output).unwrap(), previous);
    }

    let mut partial_args = args.to_vec();
    partial_args.push("--allow-incomplete");
    let partial = lab.ok(&partial_args);
    has(&partial, "leaf-vip");
    let bom: Value = serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
    assert!(!bom["components"].as_array().unwrap().iter().any(|item| item["purl"] == leaf));
    assert!(!bom["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["ref"] == bridge && row["dependsOn"].as_array().is_some_and(|deps| deps.contains(&leaf))));
}

#[test]
fn project_sbom_follows_dispatch_methods_of_a_class_reached_from_a_binary_vi() {
    let lab = Lab::new("sbom-dispatch-class");
    let project = lab.project();
    let vi_lib = lab.root.join("labview/vi.lib");
    let backend = vi_lib.join("backend");
    std::fs::create_dir(&backend).unwrap();
    let driver = vi_lib.join("driver-vip.lvlib");
    std::fs::write(&driver, b"<Library/>").unwrap();
    add_offline_vip(&lab, "driver-vip", "", &driver);

    std::fs::write(
        project.join("entry.vi"),
        token_link_fixture(b"VIPI", &[b"<vilib>", b"backend", b"SQLite.lvclass"]),
    )
    .unwrap();
    std::fs::write(
        backend.join("Connect.vi"),
        token_link_fixture(b"VIVI", &[b"<vilib>", b"driver-vip.lvlib"]),
    )
    .unwrap();
    std::fs::write(
        backend.join("SQLite.lvclass"),
        br#"<Library>
  <Item Name="Parent.lvclass" Type="Parent" URL="../Parent.lvclass"/>
  <Item Name="Connect.vi" Type="VI" URL="../Connect.vi">
    <Property Name="NI.ClassItem.IsStaticMethod" Type="Bool">false</Property>
  </Item>
  <Item Name="Optional.vi" Type="VI" URL="../Optional.vi">
    <Property Name="NI.ClassItem.IsStaticMethod" Type="Bool">true</Property>
  </Item>
</Library>"#,
    )
    .unwrap();
    std::fs::write(
        backend.join("Parent.lvclass"),
        br#"<Library>
  <Item Name="Unselected.vi" Type="VI" URL="../Unselected.vi">
    <Property Name="NI.ClassItem.IsStaticMethod" Type="Bool">false</Property>
  </Item>
</Library>"#,
    )
    .unwrap();
    std::fs::write(
        project.join("test.lvproj"),
        br#"<Project><Item Name="My Computer" Type="My Computer">
  <Item Name="entry.vi" Type="VI" URL="../entry.vi"/>
</Item></Project>"#,
    )
    .unwrap();

    lab.ok(&[
        "sbom",
        "--lvproj",
        "test.lvproj",
        "--vipm-database",
        "vipm-db",
        "--output",
        "scan.cdx.json",
    ]);
    let bom: serde_json::Value =
        serde_json::from_slice(&std::fs::read(project.join("scan.cdx.json")).unwrap()).unwrap();
    let vip = "pkg:generic/vipm/driver-vip@1.2.3.4";
    let components = bom["components"].as_array().unwrap();
    assert_eq!(components.len(), 1);
    assert_eq!(components[0]["purl"], vip);
    assert!(components[0]["properties"].as_array().unwrap().contains(
        &serde_json::json!({"name": "lvpm:labview:matched-files", "value": "1"})
    ));
    let root = &bom["metadata"]["component"]["bom-ref"];
    let row = bom["dependencies"].as_array().unwrap().iter()
        .find(|row| &row["ref"] == root).unwrap();
    assert_eq!(row["dependsOn"], serde_json::json!([vip]));
}

#[test]
fn a_venv_is_never_bound_to_a_labview_older_than_the_project() {
    let lab = Lab::new("older");
    std::fs::write(lab.manifest_path(), "[project]\nname = \"demo\"\nlabview = \"2100\"\n").unwrap();
    has(&lab.fails(&["venv", "create", "--labview-version", "2099"]), "older than the project's labview = 2100");
    assert!(!lab.venv().exists());
}

#[test]
fn commands_find_the_venv_from_below_and_from_outside_and_a_bare_manifest_is_refused() {
    let lab = Lab::new("find");
    lab.package("app", "1.0.0.1", "");
    lab.write_manifest("[dependencies]\n");

    // A manifest with no venv beside it: never a silent install into LabVIEW.
    has(&lab.fails(&["install", "app"]), "`lvpm venv create` makes one");

    lab.ok(&["venv", "create"]);
    let deep = lab.project().join("src").join("deep");
    std::fs::create_dir_all(&deep).unwrap();
    let r = lab.run_in(&deep, &["install", "app"]);
    assert!(r.ok, "{}", r.text);
    has(&r.text, "venv: ");
    assert!(lab.record("app").is_file());
    assert!(lab.file("app", "app.txt").is_file());
    assert!(lab.venv().join("packages").join("app").join("1").join("lvaddoninfo.json").is_file());

    // From outside the project, --project names it.
    let outside = lab.root.join("home");
    let project = lab.project().display().to_string();
    let r = lab.run_in(&outside, &["--project", &project, "list"]);
    assert!(r.ok, "{}", r.text);
    has(&r.text, "app");
    // And without it, outside is outside.
    has(&lab.run_in(&outside, &["venv", "status"]).text, "no venv at or above");
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
    for (name, there) in [("app", true), ("lib", true), ("tester", false), ("builder", false)] {
        assert_eq!(lab.record(name).is_file(), there, "{name}");
    }
    lab.ok(&["install", "--with", "ci", "--without", "test"]);
    assert!(lab.record("builder").is_file());
    assert!(!lab.record("tester").is_file(), "--without applies after includes");
    lab.ok(&["install", "--only", "test"]);
    assert!(lab.record("tester").is_file());

    // Additive: a package no longer listed stays until uninstalled.
    lab.write_manifest("[dependencies]\nlib = \"*\"\n");
    lab.ok(&["install"]);
    assert!(lab.record("app").is_file());

    // An empty selection is an error, not a silent no-op.
    lab.write_manifest("[dependencies]\n");
    has(&lab.fails(&["install"]), "nothing to install: the selected groups (main) are empty");
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
    assert_eq!(std::fs::read(menus.join("Controls/alpha.mnu")).unwrap(), b"control");
    assert_eq!(std::fs::read(menus.join("Categories/Computer/beta.mnu")).unwrap(), b"computer");
    assert_eq!(std::fs::read(&labview_menu).unwrap(), b"LabVIEW category");
    assert!(lab.file("alpha", "alpha.txt").is_file());
    assert!(lab.file("beta", "beta.txt").is_file());
    for package in ["alpha", "beta"] {
        assert!(!packages.join(package).join("1/menus").exists());
        assert!(lab.venv().join("palettes/contributions").join(package).join("menus").is_dir());
    }
    let shared = menus.join("Categories/Programming/Shared/shared.mnu");
    std::fs::write(&shared, b"tampered").unwrap();
    let refused = lab.fails(&["launch"]);
    has(&refused, "palettes:");
    has(&refused, "run `lvpm install`");
    lab.ok(&["install", "--no-linkfix"]);
    assert_eq!(std::fs::read(&shared).unwrap(), b"shared");
    let unexpected = packages.join("venv-palettes/1/unexpected.txt");
    std::fs::write(&unexpected, b"unexpected").unwrap();
    has(&lab.fails(&["launch"]), "palettes:");
    lab.ok(&["install", "--no-linkfix"]);
    assert!(!unexpected.exists());

    lab.ok(&["uninstall", "alpha"]);
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
    has(&lab.ok(&["install", "foo@1.0.0.1"]), "saved foo = \"1.0.0.1\" to lvpm.toml [dependencies]");
    has(&std::fs::read_to_string(lab.file("foo", "foo.txt")).unwrap(), "foo 1.0.0.1");
    // An upgrade replaces the files and updates the entry in place.
    let upgraded = lab.ok(&["install", "foo"]);
    has(&upgraded, "foo 1.0.0.1 -> 2.0.0.1");
    has(&upgraded, "updated foo in lvpm.toml [dependencies]: \"1.0.0.1\" -> \"2.0.0.1\"");
    has(&std::fs::read_to_string(lab.file("foo", "foo.txt")).unwrap(), "foo 2.0.0.1");
    // A newer installed version is kept, and the manifest never names one
    // the venv does not hold.
    let kept = lab.ok(&["install", "foo@1.0.0.1"]);
    has(&kept, "not downgrading");
    has(&kept, "lvpm.toml not changed: foo 2.0.0.1 stays installed");
    has(&lab.manifest(), "foo = \"2.0.0.1\"");

    has(&lab.ok(&["install", "bar", "--group", "test"]), "[dependencies.test]");
    lab.ok(&["install", "baz", "--no-save"]);
    let manifest = lab.manifest();
    assert!(manifest.starts_with("[project]"), "{manifest}");
    has(&manifest, "# the project's packages");
    has(&manifest, "[dependencies.test]\nbar = \"1.0.0.1\"");
    lacks(&manifest, "baz");
    assert!(lab.record("baz").is_file());

    // Dry runs change nothing.
    let before = lab.manifest();
    has(&lab.ok(&["install", "foo@1.0.0.1", "--allow-downgrade", "--dry-run"]), "would save foo = \"1.0.0.1\"");
    assert_eq!(lab.manifest(), before);
    has(&std::fs::read_to_string(lab.file("foo", "foo.txt")).unwrap(), "foo 2.0.0.1");
}

#[test]
fn uninstall_keeps_packages_dependencies_and_the_manifest_in_step() {
    let lab = Lab::new("uninstall");
    lab.package("app", "1.0.0.1", "lib");
    lab.package("lib", "1.0.0.1", "core");
    lab.package("other", "1.0.0.1", "core");
    lab.package("core", "1.0.0.1", "");
    lab.write_manifest("[dependencies]\n");
    lab.ok(&["venv", "create"]);
    lab.ok(&["install", "app"]);
    lab.ok(&["install", "other"]);

    // What another package needs is refused, unless forced.
    let refused = lab.fails(&["uninstall", "core", "--keep-deps"]);
    has(&refused, "core is required by installed packages");
    has(&refused, "--force");
    // A script never gets a prompt: it has to say what happens to dependencies.
    has(&lab.fails(&["uninstall", "app"]), "--with-deps or --keep-deps");
    assert!(lab.record("app").is_file());

    // Unshared dependencies go with it, shared ones stay, and so does the manifest.
    let removed = lab.ok(&["uninstall", "app", "--with-deps"]);
    // `other` was installed by name, so the manifest declares it.
    has(&removed, "keeping core: required by a package declared in lvpm.toml");
    has(&removed, "removed app from lvpm.toml [dependencies]");
    for (name, there) in [("app", false), ("lib", false), ("core", true), ("other", true)] {
        assert_eq!(lab.record(name).is_file(), there, "{name}");
    }
    assert!(!lab.file("app", "app.txt").exists());
    assert!(!lab.venv().join("packages").join("app").exists());
    lacks(&lab.manifest(), "app =");

    // A dependency the manifest declares is the project's: kept.
    lab.ok(&["install", "app"]);
    std::fs::write(lab.manifest_path(), lab.manifest().replace("[dependencies]\n", "[dependencies]\nlib = \"*\"\n")).unwrap();
    has(&lab.ok(&["uninstall", "app", "--with-deps"]), "keeping lib: declared in lvpm.toml");
    assert!(lab.record("lib").is_file());

    // --no-save leaves the entry, and says so.
    has(&lab.ok(&["uninstall", "lib", "--no-save", "--keep-deps"]), "lib is declared in lvpm.toml");
    has(&lab.manifest(), "lib = \"*\"");

    // --all empties the venv and never edits the manifest.
    let before = lab.manifest();
    lab.ok(&["uninstall", "--all"]);
    assert_eq!(lab.manifest(), before);
    has(&lab.ok(&["list"]), "lvpm has installed nothing into this target");
}

#[test]
fn uninstall_takes_the_named_package_out_of_the_manifest_even_when_a_dependency_fails() {
    let lab = Lab::new("uninstall-fails");
    lab.package("app", "1.0.0.1", "lib");
    lab.package("lib", "1.0.0.1", "");
    lab.write_manifest("[dependencies]\n");
    lab.ok(&["venv", "create"]);
    lab.ok(&["install", "app"]);
    // A folder where lib recorded a file: removing it fails on any OS.
    let blocked = lab.file("lib", "lib.txt");
    std::fs::remove_file(&blocked).unwrap();
    std::fs::create_dir_all(blocked.join("in-the-way")).unwrap();

    let failed = lab.fails(&["uninstall", "app", "--with-deps"]);
    has(&failed, "removed app from lvpm.toml [dependencies]");
    has(&failed, "lib.txt");
    lacks(&lab.manifest(), "app =");
    assert!(!lab.record("app").exists());
    assert!(lab.record("lib").is_file(), "the failed dependency stays recorded, to retry");
}

#[test]
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

/// A synthetic VI with two stored help-directory links. Only Known.vi exists.
fn help_link_fixture(qualify_known: bool) -> Vec<u8> {
    let mut links = b"\0\x01LVIN\0\0\0\x02".to_vec();
    let known: &[&[u8]] = if qualify_known {
        &[b"Known.lvlib", b"Known.vi"]
    } else {
        &[b"Known.vi"]
    };
    append_token_link(&mut links, b"VIVI", known, &[b"<helpdir>", b"help-links", b"Known.vi"]);
    append_token_link(
        &mut links, b"VIVI", &[b"Missing.vi"], &[b"<helpdir>", b"help-links", b"Missing.vi"],
    );
    link_resource(links)
}

fn token_link_fixture(ident: &[u8; 4], path: &[&[u8]]) -> Vec<u8> {
    let mut links = b"\0\x01LVIN\0\0\0\x01".to_vec();
    append_token_link(&mut links, ident, &path[path.len() - 1..], path);
    link_resource(links)
}

fn append_token_link(links: &mut Vec<u8>, ident: &[u8; 4], qualifiers: &[&[u8]], path: &[&[u8]]) {
    fn push_name(output: &mut Vec<u8>, name: &[u8]) {
        output.push(name.len() as u8);
        output.extend_from_slice(name);
    }

    links.extend_from_slice(b"\0\x02");
    links.extend_from_slice(ident);
    links.resize(links.len().next_multiple_of(4), 0);
    links.extend_from_slice(&(qualifiers.len() as u32).to_be_bytes());
    for qualifier in qualifiers {
        push_name(links, qualifier);
    }
    links.resize(links.len().next_multiple_of(2), 0);

    let length = 4 + path.iter().map(|name| 1 + name.len()).sum::<usize>();
    links.extend_from_slice(b"PTH0");
    links.extend_from_slice(&(length as u32).to_be_bytes());
    links.extend_from_slice(&0u16.to_be_bytes());
    links.extend_from_slice(&(path.len() as u16).to_be_bytes());
    for component in path {
        push_name(links, component);
    }
    links.extend_from_slice(&[0; 4]);
}

fn link_resource(mut links: Vec<u8>) -> Vec<u8> {
    links.extend_from_slice(b"\0\x03");
    // RSRC data and block-info tables for the saved-version and VI link blocks.
    let sections = [(b"LVSR", [0x20, 0, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0].as_slice()),
                    (b"LIvi", links.as_slice())];
    let mut data = Vec::new();
    let mut info = vec![0; 20];
    info[12..16].copy_from_slice(&52u32.to_be_bytes());
    let block_table = info.len();
    info.extend_from_slice(&1u32.to_be_bytes());
    info.resize(info.len() + 12 * sections.len(), 0);
    for (index, (ident, bytes)) in sections.into_iter().enumerate() {
        let block = block_table + 4 + index * 12;
        info[block..block + 4].copy_from_slice(ident);
        let section_offset = (info.len() - block_table) as u32;
        info[block + 8..block + 12].copy_from_slice(&section_offset.to_be_bytes());
        info.extend_from_slice(&[0; 12]);
        info.extend_from_slice(&(data.len() as u32).to_be_bytes());
        info.extend_from_slice(&[0; 4]);
        data.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        data.extend_from_slice(bytes);
        data.resize(data.len().next_multiple_of(4), 0);
    }

    let mut header = [0; 32];
    header[..6].copy_from_slice(b"RSRC\r\n");
    header[8..12].copy_from_slice(b"LVIN");
    header[16..20].copy_from_slice(&(32 + data.len() as u32).to_be_bytes());
    header[20..24].copy_from_slice(&(32 + info.len() as u32).to_be_bytes());
    header[24..28].copy_from_slice(&32u32.to_be_bytes());
    header[28..32].copy_from_slice(&(data.len() as u32).to_be_bytes());
    [header.as_slice(), &data, &header, &info].concat()
}

#[test]
fn install_and_linkfix_resolve_helpdir_links() {
    let lab = Lab::new("helpdir-links");
    let caller = help_link_fixture(false);
    let expected = help_link_fixture(true);
    lab.package_with(
        "help-links",
        "1.0.0.1",
        "",
        "[File Group 1]\nTarget Dir=\"<vi.lib>/help-links\"\nReplace Mode=\"Always\"\nNum Files=1\nFile 0=\"Caller.vi\"\n\n\
         [File Group 2]\nTarget Dir=\"<help>/help-links\"\nReplace Mode=\"Always\"\nNum Files=2\nFile 0=\"Known.vi\"\nFile 1=\"Known.lvlib\"\n",
        &[
            ("File Group 1/Caller.vi", &caller),
            ("File Group 2/Known.vi", b"placeholder target"),
            (
                "File Group 2/Known.lvlib",
                br#"<Library><Item Name="Known.vi" Type="VI" URL="../Known.vi"/></Library>"#,
            ),
        ],
    );
    lab.write_manifest("[dependencies]\n");
    lab.ok(&["venv", "create"]);
    let installed = lab.ok(&["install", "help-links"]);
    has(&installed, "linkfix: preparing installed files");

    let installed_caller = lab.file("help-links", "Caller.vi");
    let installed_help = lab.venv().join("packages/help-links/1/help/help-links/Known.vi");
    assert_eq!(std::fs::read(installed_help).unwrap(), b"placeholder target");
    assert_eq!(std::fs::read(&installed_caller).unwrap(), expected);

    // The explicit rewrite must recover the same known link and leave the
    // missing target's link alone.
    std::fs::write(&installed_caller, caller).unwrap();
    lab.ok(&["linkfix", "--fix", "qualifiers"]);
    assert_eq!(std::fs::read(&installed_caller).unwrap(), expected);
}

#[test]
fn launch_checks_the_venv_against_the_manifest_before_starting_anything() {
    let lab = Lab::new("launch");
    lab.package("app", "1.0.0.1", "");
    lab.package("app", "2.0.0.1", "");
    lab.package("lib", "1.0.0.1", "");
    lab.write_manifest("[dependencies]\napp = \"1.0.0.1\"\nlib = { version = \"*\", relink = true }\n");
    lab.ok(&["venv", "create"]);

    let refused = lab.fails(&["launch"]);
    has(&refused, "app: not installed");
    has(&refused, "lib: not installed");
    has(&refused, "run `lvpm install`");
    has(&refused, "`lvpm launch --no-check` to start anyway");

    lab.ok(&["install", "--no-relink"]);
    // relink = true asks for LabVIEW's own load-and-save, which has not run.
    has(&lab.fails(&["launch"]), "lib: relink = true but the relink pass has not completed");

    lab.write_manifest("[dependencies]\napp = \"2.0.0.1\"\nlib = \"*\"\n");
    has(&lab.fails(&["launch"]), "app: 1.0.0.1 installed, manifest asks for app");
    lab.ok(&["install"]);
    std::fs::remove_file(lab.file("app", "app.txt")).unwrap();
    has(&lab.fails(&["launch"]), "app: 1 of 2 files missing");

    // In line again: the check passes, and only starting the fake LabVIEW fails.
    lab.ok(&["uninstall", "app", "--no-save", "--keep-deps"]);
    lab.ok(&["install"]);
    let r = lab.run_in(&lab.project(), &["launch"]);
    has(&r.text, "checked 2 package(s)");
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
    has(&installed, "LabVIEW 2099 has its own copy of 1 package in this venv");
    lacks(&lab.ok(&["install", "lib"]), "has its own copy");
    has(&lab.ok(&["venv", "status"]), "shadowed       1 also in LabVIEW 2099");
    assert_eq!(std::fs::read(own.join("app.vi")).unwrap(), b"the installation's copy\n", "the installation is never written");
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
    has(&installed, "1 post-install hook(s) not run (a venv runs a package's hooks only when lvpm.toml lists it as <pkg> = { version = \"…\", hooks = \"run\" }, or with --hooks); `lvpm list` shows which");
    has(&installed, "skipped — a venv holds LabVIEW-tree content only");
    assert!(lab.file("tool", "tool.txt").is_file());
    has(&lab.ok(&["list"]), "post-install skipped");

    // --no-linkfix leaves the files as copied, and says so.
    lab.ok(&["uninstall", "tool", "--no-save"]);
    has(&lab.ok(&["install", "tool", "--no-save", "--no-linkfix"]), "linkfix: skipped (--no-linkfix)");
}

#[test]
fn import_writes_vipm_files_into_the_manifest_and_adopts_an_old_venv() {
    let lab = Lab::new("import");
    lab.package("app", "1.0.0.1", "");
    lab.package("tester", "1.0.0.1", "");
    lab.ok(&["venv", "create", "--labview-version", "2099"]);
    // As lvpm 0.3.0 left a venv created from a vipm.toml.
    std::fs::remove_file(lab.manifest_path()).unwrap();
    let binding_path = lab.venv().join("venv.json");
    let mut binding: serde_json::Value = serde_json::from_slice(&std::fs::read(&binding_path).unwrap()).unwrap();
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
    let binding: serde_json::Value = serde_json::from_slice(&std::fs::read(&binding_path).unwrap()).unwrap();
    assert_eq!(binding["manifest"], "lvpm.toml");
    assert_eq!(std::fs::read_to_string(lab.project().join("vipm.toml")).unwrap(), vipm, "VIPM's file is never written");
    let manifest = lab.manifest();
    has(&manifest, "app = \"1.0.0.1\"");
    has(&manifest, "[dependencies.dev]\ntester = \"1.0.0.1\"");
    has(&manifest, "labview = \"2099\"");

    std::fs::write(lab.manifest_path(), format!("{manifest}\n[sources]\ndefaults = false\n")).unwrap();
    lab.ok(&["install", "--with", "dev"]);
    assert!(lab.record("app").is_file() && lab.record("tester").is_file());
    has(&lab.ok(&["import", "vipm.toml"]), "already in [dependencies]");
}

#[test]
fn a_vipc_carries_its_packages_into_the_cache_and_a_failed_import_writes_nothing() {
    let lab = Lab::new("vipc");
    lab.write_manifest("[dependencies]\nbundled = \"*\"\n");
    lab.ok(&["venv", "create"]);

    // A .vipc carrying one package the local folder does not have, and
    // naming one with no version.
    let mut inner = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    inner.start_file("spec", options).unwrap();
    inner
        .write_all(
            b"[Package]\nName=\"carried\"\nVersion=\"3.0.0.1\"\n[Dependencies]\nRequires=\"\"\n\n\
              [File Group 0]\nTarget Dir=\"<vi.lib>/carried\"\nNum Files=1\nFile 0=\"carried.txt\"\n",
        )
        .unwrap();
    inner.start_file("File Group 0/carried.txt", options).unwrap();
    inner.write_all(b"carried 3.0.0.1\n").unwrap();
    let carried = inner.finish().unwrap().into_inner();
    let config = "<VI_Package_Configuration File_Type=\"xml\" Version=\"0.6\" Creator=\"x\">\r\n  <Target>\r\n    \
                  <Name>LabVIEW</Name>\r\n    <Version>99.0 (64-bit)</Version>\r\n    \
                  <Package>\r\n      <Name>carried-3.0.0.1</Name>\r\n      <Included>true</Included>\r\n    </Package>\r\n    \
                  <Package>\r\n      <Name>mystery</Name>\r\n      <Included>false</Included>\r\n    </Package>\r\n  \
                  </Target>\r\n</VI_Package_Configuration>";
    let vipc = lab.root.join("deps.vipc");
    let mut z = zip::ZipWriter::new(std::fs::File::create(&vipc).unwrap());
    z.start_file("config.xml", options).unwrap();
    z.write_all(config.as_bytes()).unwrap();
    z.start_file("carried-3.0.0.1.vip", options).unwrap();
    z.write_all(&carried).unwrap();
    z.finish().unwrap();
    let vipc = vipc.display().to_string();

    // `bundled` is a package already, so it cannot also be a group: the
    // import fails, and the manifest is as it was.
    let before = lab.manifest();
    has(&lab.fails(&["import", &vipc, "--group", "bundled"]), "cannot also be a group");
    assert_eq!(lab.manifest(), before);

    let imported = lab.ok(&["import", &vipc]);
    has(&imported, "\"mystery\" in config.xml has no version in its name; not imported");
    has(&imported, "saved carried = \"3.0.0.1\"");
    // Installed from the cache: no index has it.
    has(&lab.ok(&["install", "carried"]), "+ carried 3.0.0.1");
    has(&std::fs::read_to_string(lab.file("carried", "carried.txt")).unwrap(), "carried 3.0.0.1");
}

#[test]
fn ordinary_commands_on_the_venv_are_logged() {
    let lab = Lab::new("log");
    lab.package("app", "1.0.0.1", "");
    lab.write_manifest("[dependencies]\n");
    lab.ok(&["venv", "create"]);
    lab.ok(&["install", "app"]);
    lab.fails(&["uninstall", "not_there", "--keep-deps"]);

    let logs: Vec<PathBuf> = std::fs::read_dir(lab.root.join("logs")).unwrap().flatten().map(|e| e.path()).collect();
    let venv_log = logs.iter().find(|p| p.file_name().unwrap() != "lvpm.log").expect("a log of the venv's own");
    let text = std::fs::read_to_string(venv_log).unwrap();
    has(&text, "---- lvpm");
    has(&text, "install app");
    has(&text, "saved app = \"1.0.0.1\"");
    has(&text, "---- done");
    has(&text, "---- failed:");
}

#[test]
fn sbom_merge_leaves_log_named_inputs_and_json_outputs_intact() {
    let lab = Lab::new("sbom-log-alias");
    let logs = lab.root.join("logs");
    std::fs::create_dir(&logs).unwrap();
    let log = logs.join("lvpm.log");
    let primary = serde_json::json!({
        "bomFormat": "CycloneDX", "specVersion": "1.5", "version": 1,
        "metadata": {"component": {"type": "application", "bom-ref": "product", "name": "Product"}},
    });
    let child = serde_json::json!({
        "bomFormat": "CycloneDX", "specVersion": "1.5", "version": 1,
        "metadata": {"component": {"type": "library", "bom-ref": "library", "name": "Library"}},
    });
    let original = serde_json::to_vec(&primary).unwrap();
    std::fs::write(&log, &original).unwrap();
    std::fs::write(lab.project().join("primary.json"), &original).unwrap();
    std::fs::write(lab.project().join("child.json"), child.to_string()).unwrap();

    let log_path = log.to_str().unwrap();
    lab.ok(&["sbom", "merge", "--primary", log_path, "--child", "child.json", "--output", "release.json"]);
    assert_eq!(std::fs::read(&log).unwrap(), original);

    lab.ok(&["sbom", "merge", "--diagnostics-json", "--primary", "primary.json", "--child", "child.json", "--output", log_path]);
    let release: serde_json::Value = serde_json::from_slice(&std::fs::read(&log).unwrap()).unwrap();
    assert_eq!(release["metadata"]["component"]["name"], "Product");
    assert_eq!(release["components"][0]["name"], "Library");
}

#[test]
fn sbom_structured_diagnostics_exclude_logging_notes_and_venv_banners() {
    let lab = Lab::new("sbom-log-diagnostics");
    let logs = lab.root.join("logs");
    std::fs::write(&logs, "this path is a regular file").unwrap();
    let args = ["sbom", "--diagnostics-json", "--artifact", "missing", "--output", "release.json"];
    let response = lab.fails(&args);
    let diagnostic: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(diagnostic["code"], "sbom.io");
    assert_eq!(std::fs::read_to_string(&logs).unwrap(), "this path is a regular file");

    let response = lab.fails(&[
        "--log-level", "invalid", "sbom", "--diagnostics-json", "--artifact", "missing", "--output", "release.json",
    ]);
    let diagnostic: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(diagnostic["code"], "sbom.invalid-usage");
    assert_eq!(diagnostic["location"], "--log-level");

    lab.write_manifest("[dependencies]\n");
    let headless = Command::new(env!("CARGO_BIN_EXE_lvpm"))
        .current_dir(lab.project())
        .env("LV_RTE_HEADLESS", "1")
        .env("LVPM_TEST_LABVIEW", format!("99.0,64,{}", lab.root.join("labview").display()))
        .env("LVPM_CONFIG_LOG_DIR", &logs)
        .env("LVPM_CONFIG_VENV_DIR", ".lv-venv")
        .env_remove("LVPM_LOG")
        .stdin(Stdio::null())
        .args(["sbom", "--diagnostics-json", "--include", "missing.cdx.json", "--output", "release.json"])
        .output().unwrap();
    assert_eq!(headless.status.code(), Some(6));
    let diagnostic: serde_json::Value = serde_json::from_slice(&headless.stderr).unwrap();
    assert_eq!(diagnostic["code"], "sbom.io");
    assert_eq!(std::fs::read_to_string(&logs).unwrap(), "this path is a regular file");

    std::fs::remove_file(&logs).unwrap();
    lab.ok(&["venv", "create"]);
    let existing_logs: Vec<_> = std::fs::read_dir(&logs).unwrap().map(|entry| {
        let path = entry.unwrap().path();
        let bytes = std::fs::read(&path).unwrap();
        (path, bytes)
    }).collect();
    let response = lab.fails(&["sbom", "--diagnostics-json", "--include", "missing.cdx.json", "--output", "release.json"]);
    let diagnostic: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(diagnostic["code"], "sbom.io");
    assert_eq!(std::fs::read_dir(&logs).unwrap().count(), existing_logs.len());
    for (path, bytes) in existing_logs {
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}
