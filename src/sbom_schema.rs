//! Validation of CycloneDX 1.5 JSON against the embedded official schema.

use jsonschema::{Draft, Registry, Validator};
use serde_json::Value;
use std::error::Error;
use std::fmt;
use std::sync::OnceLock;

const BOM_SCHEMA: &str = include_str!("../schemas/cyclonedx-1.5/bom-1.5.schema.json");
const SPDX_SCHEMA: &str = include_str!("../schemas/cyclonedx-1.5/spdx.schema.json");
const JSF_SCHEMA: &str = include_str!("../schemas/cyclonedx-1.5/jsf-0.82.schema.json");

static VALIDATOR: OnceLock<Result<Validator, String>> = OnceLock::new();

#[derive(Debug)]
pub(crate) enum SchemaError {
    Invalid { location: String, message: String },
    Initialization(String),
}

impl fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { location, message } => {
                write!(
                    formatter,
                    "CycloneDX 1.5 schema violation at {location}: {message}"
                )
            }
            Self::Initialization(message) => {
                write!(
                    formatter,
                    "unable to initialize the CycloneDX 1.5 schema: {message}"
                )
            }
        }
    }
}

impl Error for SchemaError {}

pub(crate) fn validate(document: &Value) -> Result<(), SchemaError> {
    let validator = VALIDATOR
        .get_or_init(|| build_validator().map_err(|error| format!("{error:#}")))
        .as_ref()
        .map_err(|error| SchemaError::Initialization(error.clone()))?;
    validator.validate(document).map_err(|error| {
        let location = error.instance_path().to_string();
        SchemaError::Invalid {
            location: if location.is_empty() {
                "/".to_owned()
            } else {
                location
            },
            message: error.masked().to_string(),
        }
    })?;
    // The official schema declares specVersion as a string without a value constraint.
    if document["specVersion"] != "1.5" {
        return Err(SchemaError::Invalid {
            location: "/specVersion".to_owned(),
            message: "expected version 1.5".to_owned(),
        });
    }
    validate_digest_lengths(document, "")
}

fn validate_digest_lengths(value: &Value, location: &str) -> Result<(), SchemaError> {
    match value {
        Value::Object(properties) => {
            for (key, child) in properties {
                let key_pointer = key.replace('~', "~0").replace('/', "~1");
                let child_location = format!("{location}/{key_pointer}");
                if key == "hashes" {
                    // CycloneDX uses this field only for arrays of hash objects.
                    if let Some(hashes) = child.as_array() {
                        for (index, hash) in hashes.iter().enumerate() {
                            let algorithm = hash["alg"].as_str().unwrap_or_default();
                            let expected = match algorithm {
                                "MD5" => 32,
                                "SHA-1" => 40,
                                "SHA-256" | "SHA3-256" | "BLAKE2b-256" => 64,
                                "SHA-384" | "SHA3-384" | "BLAKE2b-384" => 96,
                                "SHA-512" | "SHA3-512" | "BLAKE2b-512" => 128,
                                // BLAKE3 supports variable output length.
                                _ => continue,
                            };
                            if hash["content"]
                                .as_str()
                                .is_none_or(|content| content.len() != expected)
                            {
                                return Err(SchemaError::Invalid {
                                    location: format!("{child_location}/{index}/content"),
                                    message: format!(
                                        "{algorithm} requires {expected} hexadecimal digits"
                                    ),
                                });
                            }
                        }
                    }
                } else {
                    validate_digest_lengths(child, &child_location)?;
                }
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                validate_digest_lengths(child, &format!("{location}/{index}"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn build_validator() -> anyhow::Result<Validator> {
    let schema = serde_json::from_str(BOM_SCHEMA)?;
    let spdx = serde_json::from_str::<Value>(SPDX_SCHEMA)?;
    let jsf = serde_json::from_str::<Value>(JSF_SCHEMA)?;
    let registry = Registry::new()
        .add("http://cyclonedx.org/schema/spdx.schema.json", spdx)?
        .add("http://cyclonedx.org/schema/jsf-0.82.schema.json", jsf)?
        .prepare()?;
    Ok(jsonschema::options()
        .with_draft(Draft::Draft7)
        .with_registry(&registry)
        .offline()
        .should_validate_formats(true)
        .should_ignore_unknown_formats(false)
        .build(&schema)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_embedded_schema_and_reports_instance_paths() {
        let mut document = json!({
            "bomFormat": "CycloneDX",
            "specVersion": "1.5",
            "metadata": {"timestamp": "2026-11-20T15:00:00Z"},
            "components": [{
                "type": "library",
                "name": "component",
                "licenses": [{"license": {"id": "MIT"}}],
                "hashes": [{"alg": "SHA-256", "content": "0".repeat(64)}]
            }]
        });
        validate(&document).unwrap();

        document["components"][0]["type"] = json!("invented");
        assert_invalid_at(&document, "/components/0/type");
        document["components"][0]["type"] = json!("library");

        document["components"][0]["hashes"][0]["content"] = json!("not hexadecimal");
        assert_invalid_at(&document, "/components/0/hashes/0/content");
        document["components"][0]["hashes"][0]["content"] = json!("0".repeat(32));
        assert_invalid_at(&document, "/components/0/hashes/0/content");
        document["components"][0]["hashes"][0]["content"] = json!("0".repeat(64));

        document["metadata"]["timestamp"] = json!("not a timestamp");
        assert_invalid_at(&document, "/metadata/timestamp");
        document["metadata"]["timestamp"] = json!("2026-11-20T15:00:00Z");

        document["specVersion"] = json!("1.6");
        assert_invalid_at(&document, "/specVersion");
    }

    #[test]
    fn enforces_referenced_license_and_signature_definitions() {
        let mut document = json!({
            "bomFormat": "CycloneDX",
            "specVersion": "1.5",
            "components": [{
                "type": "library",
                "name": "component",
                "licenses": [{"license": {"id": "Invented-License"}}]
            }]
        });
        assert!(matches!(
            validate(&document),
            Err(SchemaError::Invalid { .. })
        ));
        document["components"][0]["licenses"][0]["license"]["id"] = json!("MIT");
        document["signature"] = json!({"algorithm": "invented", "value": "xxx"});
        assert!(matches!(
            validate(&document),
            Err(SchemaError::Invalid { .. })
        ));
    }

    fn assert_invalid_at(document: &Value, expected: &str) {
        match validate(document) {
            Err(SchemaError::Invalid { location, .. }) => assert_eq!(location, expected),
            result => panic!("expected a schema error at {expected}, got {result:?}"),
        }
    }
}
