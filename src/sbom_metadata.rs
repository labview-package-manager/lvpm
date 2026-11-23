//! Package identifiers and provenance shared by the SBOM exporters.

use std::fmt::Write;

pub(crate) const DIGEST_SUBJECT: &str = "lvpm:digest:subject";
pub(crate) const ARTIFACT_FILE: &str = "artifact-file";

/// lvpm's VIPM package keys are ASCII case-insensitive. The generic PURL type
/// has no registered VIPM namespace or default repository, so this namespace
/// identifies the local convention without claiming to identify a feed.
pub(crate) fn vipm_purl(name: &str, version: &str) -> Option<String> {
    if name.trim().is_empty()
        || matches!(name, "." | "..")
        || name.starts_with('/')
        || name.ends_with('/')
        || name.chars().any(char::is_control)
    {
        return None;
    }
    let mut purl = format!(
        "pkg:generic/vipm/{}",
        purl_segment(&name.to_ascii_lowercase())
    );
    if !version.trim().is_empty() {
        if version.chars().any(char::is_control) {
            return None;
        }
        purl.push('@');
        purl.push_str(&purl_segment(version));
    }
    Some(purl)
}

/// Encode UTF-8 bytes using the PURL character rules in ECMA-427 §5.4.
pub(crate) fn purl_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b".-_~:".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    encoded
}

/// Keep an unresolved external reference distinct from namesakes while
/// publishing only its opaque identity and basename.
pub(crate) fn portable_path_identity(path: &str) -> String {
    let portable = crate::sbom_paths::portable_reference(path);
    if portable == path {
        return portable;
    }
    let basename = portable.strip_prefix("external/").unwrap_or(&portable);
    let digest = crate::cache::sha256_hex(path.as_bytes());
    format!("external/{digest}/{basename}")
}

/// Provenance for metadata actually obtained from the package archive.
pub(crate) fn archive_properties(
    has_supplier: bool,
    has_license: bool,
) -> impl Iterator<Item = (&'static str, &'static str)> {
    [
        Some((DIGEST_SUBJECT, "package-archive")),
        has_supplier.then_some((
            "lvpm:package:supplier-source",
            "archive-spec:[Description].Vendor",
        )),
        has_license.then_some((
            "lvpm:package:license-source",
            "archive-spec:[Description].License",
        )),
    ]
    .into_iter()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vipm_purl_keeps_existing_keys_and_encodes_utf8_and_separators() {
        assert_eq!(
            vipm_purl("NI_Foo-Bar.1", "1.0.0-2").as_deref(),
            Some("pkg:generic/vipm/ni_foo-bar.1@1.0.0-2")
        );
        assert_eq!(
            vipm_purl("Data&Ö", "Rev:2 +@?#").as_deref(),
            Some("pkg:generic/vipm/data%26%C3%96@Rev:2%20%2B%40%3F%23")
        );
        assert_eq!(
            vipm_purl("Source/Package", "1").as_deref(),
            Some("pkg:generic/vipm/source%2Fpackage@1")
        );
        assert_eq!(
            vipm_purl("Package", "").as_deref(),
            Some("pkg:generic/vipm/package")
        );
    }

    #[test]
    fn missing_or_invalid_identity_does_not_produce_a_purl() {
        for name in ["", " \t", ".", "..", "/pkg", "pkg/", "pkg\0rest"] {
            assert_eq!(vipm_purl(name, "1"), None);
        }
        assert_eq!(vipm_purl("package", "1\t2"), None);
    }

    #[test]
    fn opaque_path_labels_distinguish_namesakes_without_disclosing_directories() {
        assert_eq!(
            portable_path_identity("../support/helper.lvlibp"),
            "../support/helper.lvlibp"
        );
        let windows = portable_path_identity(r"C:\Private Folder\helper.lvlibp");
        let posix = portable_path_identity("/home/private/helper.lvlibp");
        for label in [&windows, &posix] {
            assert!(label.starts_with("external/"));
            assert!(label.ends_with("/helper.lvlibp"));
            assert!(!label.to_ascii_lowercase().contains("private"));
        }
        assert_ne!(windows, posix);
    }

    #[test]
    fn provenance_describes_known_archive_subjects_and_spec_fields() {
        assert_eq!(
            archive_properties(false, false).collect::<Vec<_>>(),
            [(DIGEST_SUBJECT, "package-archive")]
        );
        assert_eq!(
            archive_properties(true, true).collect::<Vec<_>>(),
            [
                (DIGEST_SUBJECT, "package-archive"),
                (
                    "lvpm:package:supplier-source",
                    "archive-spec:[Description].Vendor"
                ),
                (
                    "lvpm:package:license-source",
                    "archive-spec:[Description].License"
                ),
            ],
        );
        assert_ne!("package-archive", ARTIFACT_FILE);
    }
}
