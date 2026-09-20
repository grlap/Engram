//! Read-only host readiness, deliberately separate from the full doctor audit.

use super::doctor::refusals::{self, Phase, with_build};
use anyhow::{Result, bail};
use engram::storage::StoreReadiness;
use engram::{HostPathPolicy, ProjectId, SqliteStore, describe_host_path_policy};
use serde_json::{Value, json};
use std::path::Path;

fn envelope(mut value: Value, ready: bool) -> Value {
    if let Some(fields) = value.as_object_mut() {
        fields.remove("healthy");
    }
    value["schema_version"] = json!(1);
    value["scope"] = json!("readiness");
    value["ready"] = json!(ready);
    value["full_audit"] = json!("not_run");
    value["mutation_enabled"] = json!(false);
    with_build(value)
}

fn refusal_envelope(mut detail: Value) -> Value {
    // Only the routing fields are promoted. Doctor owns the remaining diagnostic
    // namespace, including its projection-repair `scope`, not our envelope.
    if let Some(fields) = detail.as_object_mut() {
        fields.remove("healthy");
    }
    let mut value = json!({});
    for field in [
        "project_id",
        "database",
        "code",
        "kind",
        "phase",
        "reason",
        "remedy",
    ] {
        if let Some(field_value) = detail.get(field) {
            value[field] = field_value.clone();
        }
    }
    value["detail"] = detail;
    envelope(value, false)
}

pub(crate) fn run(
    database: &Path,
    identity: Option<HostPathPolicy>,
    project: &ProjectId,
    json_output: bool,
) -> Result<()> {
    let report = match SqliteStore::readiness(database, identity) {
        Ok(report) => report,
        Err(error) => {
            let mut value = refusals::refusal(database, project, &error, Phase::Open);
            value["reason"] = json!(error.to_string());
            if value.get("remedy").is_none() {
                value["remedy"] = json!(
                    "Inspect the refused policy/store or restore a verified backup; readiness never repairs it."
                );
            }
            refusals::emit(&refusal_envelope(value), json_output)?;
            bail!("readiness refused: {error}");
        }
    };
    let value = report_receipt(database, identity, project, &report);
    refusals::emit(&value, json_output)?;
    if value["ready"] != true {
        bail!("readiness database identity unavailable");
    }
    Ok(())
}

// `report` must come from SqliteStore::readiness with this same identity, as in
// run(). Its snapshot preflight owns mismatch refusal; this formatter may then
// describe two present policies as matched without repeating admission.
fn report_receipt(
    database: &Path,
    identity: Option<HostPathPolicy>,
    project: &ProjectId,
    report: &StoreReadiness,
) -> Value {
    let path = match super::doctor::canonical_database_path(database) {
        Ok(path) => path,
        Err(error) => {
            return envelope(
                json!({
                    "project_id": project, "database": null,
                    "code": "store_open_refused", "kind": "io", "phase": "path",
                    "reason": format!("{error:#}"), "remedy": "Check the selected database path and retry."
                }),
                false,
            );
        }
    };
    let status = match (report.stored_host_path_policy, identity) {
        (_, None) => "unresolved",
        (None, Some(_)) => "unbound",
        (Some(_), Some(_)) => "matched",
    };
    envelope(
        json!({
            "project_id": project,
            "database": path,
            "work_schema_version": report.work_schema_version,
            "host_path_policy": {
                "stored": report.stored_host_path_policy.map(describe_host_path_policy),
                "resolved": identity.map(describe_host_path_policy),
                "status": status,
            },
            "control": report.control,
        }),
        true,
    )
}

pub(crate) fn resolution_error(error: &anyhow::Error, json_output: bool) -> Result<()> {
    // resolve_project currently fails only while reading the project marker or
    // requiring an explicit home. Keep this mapping paired with that function.
    let code = if error
        .downcast_ref::<super::project::ProjectFileRefusal>()
        .is_some()
    {
        "project_resolution_failed"
    } else {
        "home_required"
    };
    let value = envelope(
        json!({
            "project_id": null, "database": null, "code": code, "phase": "resolve",
            "reason": error.to_string(),
            "remedy": "Supply the intended --project-file and explicit --home (or ENGRAM_HOME); readiness never selects or creates a replacement."
        }),
        false,
    );
    refusals::emit(&value, json_output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_borrowed_diagnostics_cannot_replace_reserved_fields() {
        let mut detail = json!({
            "project_id": "fixture", "database": "diagnostic.db",
            "code": "projection_repair_required", "phase": "open",
            "reason": "fixture", "remedy": "explicit repair",
            "scope": ["indexes", "triggers", "fts"],
            "schema_version": 99, "ready": true, "full_audit": "run",
            "mutation_enabled": true, "healthy": true,
            "build": "foreign", "build_fingerprint": "foreign"
        });
        let value = refusal_envelope(detail.clone());
        detail.as_object_mut().unwrap().remove("healthy");
        assert_eq!(value["detail"], detail);
        assert_eq!(value["scope"], "readiness");
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["ready"], false);
        assert_eq!(value["full_audit"], "not_run");
        assert_eq!(value["mutation_enabled"], false);
        assert!(value.get("healthy").is_none());
        let build = engram::build_identity::current();
        assert_eq!(value["build"], json!(build.build));
        assert_eq!(value["build_fingerprint"], json!(build.build_fingerprint));
        for field in [
            "project_id",
            "database",
            "code",
            "phase",
            "reason",
            "remedy",
        ] {
            assert_eq!(value[field], detail[field]);
        }
    }

    #[test]
    fn readiness_unresolved_identity_is_disclosed_without_binding() {
        let directory = crate::test_support::temp_home().unwrap();
        let database = directory.path().join("readiness.db");
        drop(SqliteStore::open_unresolved(&database).unwrap());
        let before = std::fs::read(&database).unwrap();
        let report = SqliteStore::readiness(&database, None).unwrap();
        let value = report_receipt(&database, None, &ProjectId("fixture".into()), &report);
        assert_eq!(value["ready"], true);
        assert_eq!(value["host_path_policy"]["status"], "unresolved");
        assert!(value["host_path_policy"]["stored"].is_null());
        assert!(value["host_path_policy"]["resolved"].is_null());
        assert_eq!(std::fs::read(database).unwrap(), before);
    }

    #[test]
    fn readiness_missing_post_open_identity_is_a_path_refusal() {
        let directory = crate::test_support::temp_home().unwrap();
        let database = directory.path().join("readiness.db");
        drop(SqliteStore::open_unresolved(&database).unwrap());
        let report = SqliteStore::readiness(&database, None).unwrap();
        // Deterministically model disappearance after the successful store read.
        std::fs::remove_file(&database).unwrap();
        let cause = std::fs::canonicalize(&database).unwrap_err();
        let value = report_receipt(&database, None, &ProjectId("fixture".into()), &report);
        assert_eq!(value["ready"], false);
        assert_eq!(value["phase"], "path");
        assert_eq!(value["code"], "store_open_refused");
        assert_eq!(value["kind"], "io");
        assert!(
            value["reason"]
                .as_str()
                .unwrap()
                .contains(&cause.to_string())
        );
        assert!(value["database"].is_null());
        assert!(value.get("control").is_none());
        assert!(value.get("build").is_some());
        assert!(value.get("build_fingerprint").is_some());
        assert!(!database.exists());
    }
}
