//! Exercise the post-build artifact CLI without NI software or prebuilt fixtures.

use serde_json::{Value, json};
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        loop {
            let root = std::env::temp_dir().join(format!(
                "lvpm-artifact-cli-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&root) {
                Ok(()) => return Self(root),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("creating {}: {error}", root.display()),
            }
        }
    }

    fn put(&self, name: &str, bytes: &[u8]) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn run(&self, args: &[&str], code: i32) -> Output {
        let (command, args) = match args {
            ["merge", rest @ ..] => (&["sbom", "merge", "--diagnostics-json"][..], rest),
            _ => (&["sbom", "--diagnostics-json"][..], args),
        };
        let out = Command::new(env!("CARGO_BIN_EXE_lvpm"))
            .current_dir(&self.0)
            .env("LVPM_NO_UPDATE_CHECK", "1")
            .env("LVPM_CONFIG_LOG_DIR", self.0.join("logs"))
            .env("LVPM_LOG", "off")
            .stdin(Stdio::null())
            .args(command)
            .args(args)
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(code),
            "lvpm sbom {args:?}\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn diagnostic(output: &Output, code: &str, severity: &str) {
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let found = stderr.lines().filter(|line| !line.is_empty()).any(|line| {
        let value: Value = serde_json::from_str(line).unwrap();
        value["code"] == code
            && value["severity"] == severity
            && value["location"].as_str().is_some_and(|s| !s.is_empty())
            && value["message"].as_str().is_some_and(|s| !s.is_empty())
    });
    assert!(found, "missing {severity} {code} in:\n{stderr}");
}

fn property<'a>(node: &'a Value, name: &str) -> Option<&'a str> {
    node["properties"]
        .as_array()?
        .iter()
        .find(|item| item["name"] == name)?["value"]
        .as_str()
}

#[test]
fn artifact_export_is_deterministic_for_portable_output_keys() {
    let scratch = Scratch::new();
    scratch.put("delivery/Gerät A/example.dat", b"abc");
    scratch.put("delivery/Gerät A/output-only.cfg", b"settings");
    scratch.put("delivery/variant B/example.dat", b"abcd");
    scratch.put("outside/only-source.vi", b"not delivered");

    for (left, right, result) in [
        (
            "delivery/Gerät A",
            "delivery/variant B/example.dat",
            "first.cdx.json",
        ),
        (
            "delivery/variant B/example.dat",
            "delivery/Gerät A",
            "second.cdx.json",
        ),
    ] {
        let output = scratch.run(
            &[
                "--artifact",
                left,
                "--artifact",
                right,
                "--artifact-root",
                "delivery",
                "--primary-artifact",
                "Gerät A/example.dat",
                "--product-name",
                "Example release",
                "--product-version",
                "1.2.3",
                "--fail-on-incomplete=files",
                "--output",
                result,
            ],
            0,
        );
        diagnostic(&output, "sbom.coverage", "warning");
        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        assert!(stderr.lines().any(|line| {
            let diagnostic: Value = serde_json::from_str(line).unwrap();
            diagnostic["code"] == "sbom.coverage" && diagnostic["coverage_scope"] == "runtime"
        }));
    }

    let first = fs::read(scratch.0.join("first.cdx.json")).unwrap();
    let second = fs::read(scratch.0.join("second.cdx.json")).unwrap();
    assert_eq!(first, second);
    let bom: Value = serde_json::from_slice(&first).unwrap();
    assert_eq!(bom["bomFormat"], "CycloneDX");
    assert_eq!(bom["specVersion"], "1.5");
    let compositions = bom["compositions"].as_array().unwrap();
    assert!(compositions.iter().any(|scope| {
        scope["aggregate"] == "complete"
            && scope["bom-ref"]
                .as_str()
                .unwrap_or_default()
                .starts_with("lvpm:sbom:file-inventory:")
    }));
    assert!(
        compositions
            .iter()
            .any(|scope| { scope["aggregate"] == "unknown" && scope["dependencies"].is_array() })
    );

    let product = &bom["metadata"]["component"];
    assert_eq!(product["name"], "Example release");
    assert_eq!(product["version"], "1.2.3");
    assert_eq!(
        property(product, "lvpm:artifact:path"),
        Some("Gerät A/example.dat")
    );
    assert_eq!(property(product, "lvpm:artifact:byte-size"), Some("3"));
    assert_eq!(product["hashes"][0]["alg"], "SHA-256");
    assert_eq!(
        product["hashes"][0]["content"],
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let files = product["components"].as_array().unwrap();
    let paths: Vec<_> = files
        .iter()
        .map(|file| property(file, "lvpm:artifact:path").unwrap())
        .collect();
    assert_eq!(paths, ["Gerät A/output-only.cfg", "variant B/example.dat"]);
    assert_eq!(
        property(&files[0], "lvpm:artifact:origin"),
        Some("unresolved")
    );
    assert_eq!(files[1]["name"], "example.dat");
    assert_ne!(files[1]["hashes"], product["hashes"]);
    assert!(!String::from_utf8(first).unwrap().contains("only-source.vi"));
}

#[test]
fn artifact_export_rejects_invalid_requests_before_replacing_output() {
    let scratch = Scratch::new();
    scratch.put("delivery/file.dat", b"unchanged input");
    scratch.put("result.cdx.json", b"prior output");
    scratch.put(
        "invalid.cdx.json",
        br#"{
        "bomFormat":"CycloneDX", "specVersion":"1.5",
        "components":[{"type":"invented", "name":"invalid", "bom-ref":"fixture:invalid"}]
    }"#,
    );

    for (arguments, exit, expected) in [
        (
            &[
                "--artifact",
                "delivery",
                "--primary-artifact",
                "outside.dat",
            ][..],
            3,
            "sbom.invalid-input",
        ),
        (&["--artifact", "missing.dat"][..], 6, "sbom.io"),
        (
            &["--artifact", "delivery", "--artifact", "delivery/file.dat"][..],
            3,
            "sbom.invalid-input",
        ),
        (
            &[
                "--artifact",
                "delivery/file.dat",
                "--include",
                "invalid.cdx.json",
            ][..],
            3,
            "sbom.invalid-schema",
        ),
        (
            &["--artifact", "delivery/file.dat", "--fail-on-incomplete"][..],
            4,
            "sbom.incomplete",
        ),
        (
            &[
                "--artifact",
                "delivery/file.dat",
                "--fail-on-incomplete=runtime",
            ][..],
            4,
            "sbom.incomplete",
        ),
    ] {
        let mut args = arguments.to_vec();
        args.extend(["--output", "result.cdx.json"]);
        diagnostic(&scratch.run(&args, exit), expected, "error");
        assert_eq!(
            fs::read(scratch.0.join("result.cdx.json")).unwrap(),
            b"prior output"
        );
    }

    let output = scratch.run(
        &[
            "--artifact",
            "delivery",
            "--output",
            "delivery/new.cdx.json",
        ],
        3,
    );
    diagnostic(&output, "sbom.invalid-input", "error");
    assert!(!scratch.0.join("delivery/new.cdx.json").exists());

    let output = scratch.run(
        &[
            "--artifact",
            "delivery/file.dat",
            "--output",
            "delivery/file.dat",
        ],
        3,
    );
    diagnostic(&output, "sbom.output-conflict", "error");
    assert_eq!(
        fs::read(scratch.0.join("delivery/file.dat")).unwrap(),
        b"unchanged input"
    );
}

#[test]
fn strict_file_coverage_requires_verified_containment() {
    let scratch = Scratch::new();
    scratch.put("delivery/release.dat", b"release");
    scratch.put("delivery/member.dat", b"shipped bytes");
    scratch.put("other/member.dat", b"different bytes");

    scratch.run(
        &[
            "--artifact",
            "delivery",
            "--artifact-root",
            "delivery",
            "--primary-artifact",
            "release.dat",
            "--fail-on-incomplete=files",
            "--output",
            "primary.cdx.json",
        ],
        0,
    );
    for (file, output) in [
        ("delivery/member.dat", "shipped.cdx.json"),
        ("other/member.dat", "unverified.cdx.json"),
    ] {
        scratch.run(
            &[
                "--artifact",
                file,
                "--fail-on-incomplete=files",
                "--output",
                output,
            ],
            0,
        );
    }

    for (name, child, kind) in [
        ("contained", "unverified.cdx.json", "contains"),
        ("delivered", "shipped.cdx.json", "contains"),
        ("external", "unverified.cdx.json", "requires"),
    ] {
        let plan = json!({
            "children": {"app": child},
            "relationships": [{"parent": "primary", "child": "app", "kind": kind,
                               "evidence": "release manifest"}],
        });
        scratch.put(&format!("{name}.json"), &serde_json::to_vec(&plan).unwrap());
    }

    let merge = |primary: &str, plan: &str, dest: &str, options: &[&str], status| {
        let mut args = vec![
            "merge",
            "--primary",
            primary,
            "--relationships",
            plan,
            "--output",
            dest,
        ];
        args.extend(options);
        scratch.run(&args, status)
    };
    let strict = ["--fail-on-incomplete=files"];
    let positive = "delivered.cdx.json";
    merge("primary.cdx.json", "delivered.json", positive, &strict, 0);
    merge(positive, "delivered.json", "repeated.cdx.json", &strict, 0);
    assert_eq!(
        fs::read(scratch.0.join(positive)).unwrap(),
        fs::read(scratch.0.join("repeated.cdx.json")).unwrap()
    );
    merge(
        "primary.cdx.json",
        "external.json",
        "external.cdx.json",
        &strict,
        0,
    );

    let output = merge(
        "primary.cdx.json",
        "contained.json",
        "declared.cdx.json",
        &[],
        0,
    );
    let notice = String::from_utf8(output.stderr).unwrap();
    assert!(
        notice.contains("file inventory has no verified containment"),
        "{notice}"
    );
    let prior = b"keep earlier output";
    scratch.put("strict.cdx.json", prior);
    for primary in ["primary.cdx.json", "declared.cdx.json"] {
        let output = merge(primary, "contained.json", "strict.cdx.json", &strict, 4);
        diagnostic(&output, "sbom.incomplete", "error");
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("no verified containment")
        );
        assert_eq!(fs::read(scratch.0.join("strict.cdx.json")).unwrap(), prior);
    }

    scratch.put("outer/wrapper.dat", b"wrapper");
    scratch.put("outer/release.dat", b"release");
    scratch.run(
        &[
            "--artifact",
            "outer",
            "--artifact-root",
            "outer",
            "--primary-artifact",
            "wrapper.dat",
            "--fail-on-incomplete=files",
            "--output",
            "outer.cdx.json",
        ],
        0,
    );
    let nested = json!({
        "children": {"release": "declared.cdx.json"},
        "relationships": [{"parent": "primary", "child": "release", "kind": "contains",
                           "evidence": "wrapper manifest"}],
    });
    scratch.put("nested.json", &serde_json::to_vec(&nested).unwrap());
    let output = merge(
        "outer.cdx.json",
        "nested.json",
        "strict.cdx.json",
        &strict,
        4,
    );
    diagnostic(&output, "sbom.incomplete", "error");
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("no verified containment")
    );
    assert_eq!(fs::read(scratch.0.join("strict.cdx.json")).unwrap(), prior);
}
