use serde_json::Value;

pub(crate) fn remove_unsigned_integer_format(schema: &mut schemars::Schema) {
    let has_unsupported_format: bool = matches!(
        schema.get("format").and_then(Value::as_str),
        Some("uint" | "uint16" | "uint64")
    );

    if has_unsupported_format {
        schema.remove("format");
    }
}

#[cfg(test)]
mod tests {
    use crate::branch_deploy::{BranchDeleteManifest, BranchDeployManifest};
    use crate::deploy::DeployPlan;
    use crate::tools::{DownloadResponse, ProfilesResponse, UploadResponse};
    use schemars::{schema_for, JsonSchema};
    use serde_json::{json, Value};

    const UNSUPPORTED_FORMATS: [&str; 3] = ["uint", "uint16", "uint64"];

    #[test]
    fn mcp_output_schemas_use_compatible_unsigned_integers() {
        let deploy_schema: Value = schema::<DeployPlan>();
        let profiles_schema: Value = schema::<ProfilesResponse>();
        let upload_schema: Value = schema::<UploadResponse>();
        let download_schema: Value = schema::<DownloadResponse>();

        for schema in [
            &deploy_schema,
            &profiles_schema,
            &upload_schema,
            &download_schema,
        ] {
            assert_no_unsupported_formats(schema);
        }

        assert_integer_schema(&deploy_schema, "/properties/files_uploaded", None);
        assert_integer_schema(&deploy_schema, "/properties/bytes_uploaded", None);
        assert_integer_schema(&deploy_schema, "/properties/directories_created", None);
        assert_integer_schema(&deploy_schema, "/$defs/UploadedFile/properties/bytes", None);
        assert_integer_schema(
            &profiles_schema,
            "/$defs/ProfileSummary/properties/port",
            Some(65_535),
        );
        assert_integer_schema(&upload_schema, "/properties/bytes", None);
        assert_integer_schema(&download_schema, "/properties/bytes", None);

        assert_eq!(
            deploy_schema.pointer("/properties/dry_run/type"),
            Some(&json!("boolean"))
        );
        assert_eq!(
            deploy_schema.pointer("/properties/skipped/items/type"),
            Some(&json!("string"))
        );
        assert_eq!(
            deploy_schema.pointer("/properties/uploaded/type"),
            Some(&json!("array"))
        );
        assert_eq!(
            deploy_schema.pointer("/properties/uploaded/items/$ref"),
            Some(&json!("#/$defs/UploadedFile"))
        );
        assert_eq!(
            profiles_schema.pointer("/$defs/ProfileSummary/properties/host/type"),
            Some(&json!("string"))
        );
    }

    #[test]
    fn schema_customization_does_not_change_runtime_serialization() {
        let response: UploadResponse = UploadResponse {
            profile: "staging".to_string(),
            local_path: "index.html".to_string(),
            remote_path: "/public/index.html".to_string(),
            bytes: 42,
        };

        assert_eq!(
            serde_json::to_value(response).expect("upload response should serialize"),
            json!({
                "profile": "staging",
                "local_path": "index.html",
                "remote_path": "/public/index.html",
                "bytes": 42
            })
        );
    }

    #[test]
    fn transform_removes_only_the_three_unsupported_formats() {
        for format in UNSUPPORTED_FORMATS {
            let mut schema = schemars::json_schema!({
                "type": "integer",
                "format": format,
                "minimum": 0,
                "x-preserved": true
            });

            super::remove_unsigned_integer_format(&mut schema);

            assert_eq!(
                schema,
                schemars::json_schema!({
                    "type": "integer",
                    "minimum": 0,
                    "x-preserved": true
                })
            );
        }

        let mut schema = schemars::json_schema!({
            "type": "string",
            "format": "date-time",
            "x-preserved": true
        });
        let original = schema.clone();

        super::remove_unsigned_integer_format(&mut schema);

        assert_eq!(schema, original);
    }

    #[test]
    fn deploy_branch_contract_manifest_counts_have_compatible_integer_schemas() {
        let manifest_schema: Value = schema::<BranchDeployManifest>();

        assert_no_unsupported_formats(&manifest_schema);
        for count in [
            "commits",
            "touched_paths",
            "planned_uploads",
            "uploaded",
            "verified",
            "deleted_reported",
            "failures",
        ] {
            assert_integer_schema(
                &manifest_schema,
                &format!("/$defs/ManifestCounts/properties/{count}"),
                None,
            );
        }

        assert_eq!(
            manifest_schema.pointer("/properties/blocked_by_conflicts/type"),
            Some(&json!("boolean"))
        );
        assert_eq!(
            manifest_schema.pointer("/$defs/DeployMode/enum"),
            Some(&json!(["overwrite", "merge"]))
        );
        assert_optional_integer_schema(
            &manifest_schema,
            "/$defs/UploadResult/properties/remote_bytes_read",
            "/$defs/UploadResult/required",
        );
    }

    #[test]
    fn deletion_contract_manifest_counts_have_compatible_integer_schemas() {
        let manifest_schema: Value = schema::<BranchDeleteManifest>();

        assert_no_unsupported_formats(&manifest_schema);
        assert_eq!(
            manifest_schema.pointer("/properties/repository_root/type"),
            Some(&json!("string"))
        );
        assert!(manifest_schema.pointer("/properties/repository").is_none());
        for count in ["planned", "deleted", "failed", "not_attempted", "blocked"] {
            assert_integer_schema(
                &manifest_schema,
                &format!("/$defs/DeleteManifestCounts/properties/{count}"),
                None,
            );
        }
    }

    fn schema<T: JsonSchema>() -> Value {
        serde_json::to_value(schema_for!(T)).expect("schema should serialize")
    }

    fn assert_no_unsupported_formats(value: &Value) {
        match value {
            Value::Array(values) => {
                for value in values {
                    assert_no_unsupported_formats(value);
                }
            }
            Value::Object(properties) => {
                if let Some(format) = properties.get("format").and_then(Value::as_str) {
                    assert!(
                        !UNSUPPORTED_FORMATS.contains(&format),
                        "unsupported format {format} in {value}"
                    );
                }
                for value in properties.values() {
                    assert_no_unsupported_formats(value);
                }
            }
            _ => {}
        }
    }

    fn assert_integer_schema(schema: &Value, pointer: &str, maximum: Option<u64>) {
        let property: &Value = schema
            .pointer(pointer)
            .unwrap_or_else(|| panic!("missing schema property at {pointer}"));

        assert_eq!(property.get("type"), Some(&json!("integer")));
        assert_eq!(property.get("minimum"), Some(&json!(0)));
        assert_eq!(property.get("format"), None);
        if let Some(maximum) = maximum {
            assert_eq!(property.get("maximum"), Some(&json!(maximum)));
        }
    }

    fn assert_optional_integer_schema(schema: &Value, pointer: &str, required_pointer: &str) {
        let property = schema
            .pointer(pointer)
            .unwrap_or_else(|| panic!("missing schema property at {pointer}"));
        let types = property
            .get("type")
            .and_then(Value::as_array)
            .expect("optional integer should permit integer and null");

        assert!(types.contains(&json!("integer")));
        assert!(types.contains(&json!("null")));
        assert_eq!(property.get("minimum"), Some(&json!(0)));
        assert_eq!(property.get("format"), None);
        assert!(
            !schema
                .pointer(required_pointer)
                .and_then(Value::as_array)
                .is_some_and(|required| required.contains(&json!("remote_bytes_read"))),
            "remote byte count should remain optional"
        );
    }
}
