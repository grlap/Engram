//! Diagnostic rendering of existing refusals; never repairs or reopens a store.

use engram::{
    ProjectId, StoreError, build_identity,
    storage::{StoreOpenRefusalKind, store_open_refusal_kind, store_schema_reference},
};
use serde_json::{Value, json};
use std::path::Path;

#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Phase {
    Open,
    Verification,
    ControlDiagnostics,
    ControlPolicyRecovery,
    ProjectionRepair,
}

pub(super) fn with_build(mut value: Value) -> Value {
    let identity = build_identity::current();
    value["build"] = json!(identity.build);
    value["build_fingerprint"] = json!(identity.build_fingerprint);
    value
}

pub(super) fn refusal(
    database: &Path,
    project: &ProjectId,
    error: &StoreError,
    phase: Phase,
) -> Value {
    let mut value = with_build(json!({
        "healthy": false,
        "mutation_enabled": false,
        "phase": phase,
        "project_id": project,
        "database": super::canonical_database_path(database)
            .unwrap_or_else(|_| super::path_without_windows_verbatim_prefix(database)),
    }));
    match store_open_refusal_kind(error) {
        StoreOpenRefusalKind::ProjectionRepairRequired => {
            value["code"] = json!("projection_repair_required");
            value["remedy"] = json!("engram doctor --repair-projections");
            value["scope"] = json!(["indexes", "triggers", "fts"]);
        }
        StoreOpenRefusalKind::DifferentBuildSchema => {
            value["code"] = json!("different_build_schema");
            let schema = store_schema_reference(database).ok();
            value["store_schema_reference"] = json!(schema);
            if schema.is_none() {
                value["store_schema"] = json!("unavailable");
            }
            value["running"] = json!(build_identity::current().build);
            value["remedy"] = json!(
                "Use the Engram build that created this store, or perform a compatible portable handoff; do not repair a different durable schema"
            );
        }
        StoreOpenRefusalKind::CorruptStore => {
            value["code"] = json!("corrupt_store");
            value["findings"] = json!([error.to_string()]);
        }
        StoreOpenRefusalKind::Busy => operational_refusal(
            &mut value,
            error,
            "busy",
            "Retry after the other process holding the store releases it".into(),
        ),
        StoreOpenRefusalKind::Permission => operational_refusal(
            &mut value,
            error,
            "permission",
            format!("Check access to {}: {error}", database.display()),
        ),
        StoreOpenRefusalKind::Io => operational_refusal(
            &mut value,
            error,
            "io",
            format!(
                "Check the path and storage for {}: {error}",
                database.display()
            ),
        ),
        StoreOpenRefusalKind::PathPolicy {
            recorded,
            requested,
        } => {
            let (flag, recorded_aliases) = recorded.split_once(',').unwrap_or((&recorded, ""));
            let (_, requested_aliases) = requested.split_once(',').unwrap_or((&requested, ""));
            let action = if recorded_aliases == requested_aliases {
                format!("use --host-path-policy {flag}")
            } else {
                "Use a host compatible with the recorded alias rules, or initialize a fresh store at a new location".into()
            };
            operational_refusal(
                &mut value,
                error,
                "path_policy",
                format!("Recorded policy: {recorded}; requested policy: {requested}; {action}"),
            );
        }
    }
    value
}

fn operational_refusal(value: &mut Value, error: &StoreError, kind: &str, remedy: String) {
    value["code"] = json!("store_open_refused");
    value["reason"] = json!(error.to_string());
    value["kind"] = json!(kind);
    value["remedy"] = Value::String(remedy);
}

/// Text escapes every value as JSON, so attacker-authored findings cannot
/// impersonate top-level operator guidance. JSON and text expose the same fields.
pub(super) fn emit(value: &Value, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else if let Some(fields) = value.as_object() {
        for (key, value) in fields {
            println!("{key}: {}", serde_json::to_string(value)?);
        }
    }
    Ok(())
}

pub(super) fn report_error(
    database: &Path,
    project: &ProjectId,
    error: &StoreError,
    json: bool,
    phase: Phase,
) -> anyhow::Result<()> {
    let value = refusal(database, project, error, phase);
    emit(&value, json)?;
    anyhow::bail!(
        "doctor refused: {}",
        value["code"].as_str().unwrap_or("corrupt_store")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_preserves_canonical_database_identity() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("diagnostic.db");
        drop(engram::SqliteStore::open(&database).unwrap());
        let alias = directory.path().join("alias");
        std::fs::create_dir(&alias).unwrap();
        let spelling = alias.join("..").join("diagnostic.db");
        let error = StoreError::InvalidControlProjection("damaged fixture".into());
        assert_ne!(spelling, database);
        let value = refusal(
            &spelling,
            &ProjectId("diagnostics".into()),
            &error,
            Phase::Open,
        );
        assert_eq!(
            value["database"],
            super::super::canonical_database_path(&database).unwrap()
        );
    }

    #[test]
    fn path_policy_alias_mismatch_requires_a_compatible_host() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("policy.db");
        for requested_case in [false, true] {
            let recorded = engram::HostPathPolicy::host_default();
            drop(
                engram::SqliteStore::open_with_host_path_identity(&database, Some(recorded))
                    .unwrap(),
            );
            let requested = engram::HostPathPolicy {
                case_fold_paths: requested_case,
                windows_alias_rules: !recorded.windows_alias_rules,
            };
            let error =
                engram::SqliteStore::open_with_host_path_identity(&database, Some(requested))
                    .err()
                    .unwrap();
            let before = std::fs::read(&database).unwrap();
            let value = refusal(
                &database,
                &ProjectId("diagnostics".into()),
                &error,
                Phase::Open,
            );
            assert_eq!(value["kind"], "path_policy");
            assert_eq!(value["reason"], error.to_string());
            let remedy = value["remedy"].as_str().unwrap();
            assert!(remedy.contains("host compatible with the recorded alias rules"));
            assert!(remedy.contains("fresh store at a new location"));
            assert!(!remedy.contains("--host-path-policy"));
            assert_eq!(std::fs::read(&database).unwrap(), before);
        }
    }

    #[test]
    fn unavailable_store_schema_is_explicit_and_does_not_create_a_file() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("different.db");
        drop(engram::SqliteStore::open(&database).unwrap());
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch("ALTER TABLE objects ADD COLUMN incompatible TEXT")
            .unwrap();
        drop(connection);
        let error = engram::SqliteStore::open(&database).err().unwrap();
        // Model a diagnostic target that is no longer readable after refusal.
        let absent = directory.path().join("absent.db");
        let value = refusal(
            &absent,
            &ProjectId("diagnostics".into()),
            &error,
            Phase::Open,
        );
        assert_eq!(value["code"], "different_build_schema");
        assert!(value["store_schema_reference"].is_null());
        assert_eq!(value["store_schema"], "unavailable");
        assert_eq!(
            value["database"],
            super::super::path_without_windows_verbatim_prefix(&absent)
        );
        assert!(!absent.exists());
    }

    #[test]
    fn refused_reports_distinguish_the_invoking_phase() {
        let error = StoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        ));
        for (phase, name) in [
            (Phase::Open, "open"),
            (Phase::Verification, "verification"),
            (Phase::ControlDiagnostics, "control_diagnostics"),
            (Phase::ControlPolicyRecovery, "control_policy_recovery"),
            (Phase::ProjectionRepair, "projection_repair"),
        ] {
            let value = refusal(
                Path::new("diagnostic.db"),
                &ProjectId("diagnostics".into()),
                &error,
                phase,
            );
            assert_eq!(value["phase"], name);
            assert_eq!(value["code"], "store_open_refused");
            assert_eq!(value["mutation_enabled"], false);
        }
    }

    #[test]
    fn operational_refusals_preserve_the_exact_reason_without_claiming_corruption() {
        for (code, kind) in [
            (rusqlite::ffi::SQLITE_BUSY, "busy"),
            (rusqlite::ffi::SQLITE_LOCKED, "busy"),
            (rusqlite::ffi::SQLITE_PERM, "permission"),
            (rusqlite::ffi::SQLITE_READONLY, "permission"),
            (rusqlite::ffi::SQLITE_IOERR, "io"),
            (rusqlite::ffi::SQLITE_CANTOPEN, "io"),
        ] {
            let error = StoreError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                Some("Exact OS detail\nwith continuation".into()),
            ));
            let value = refusal(
                Path::new("diagnostic.db"),
                &ProjectId("diagnostics".into()),
                &error,
                Phase::Open,
            );
            assert_eq!(value["code"], "store_open_refused");
            assert_eq!(value["kind"], kind);
            assert_eq!(value["healthy"], false);
            assert_eq!(value["reason"], error.to_string());
            assert!(value.get("findings").is_none());
            if kind != "busy" {
                assert!(value["remedy"].as_str().unwrap().contains("diagnostic.db"));
                assert!(
                    value["remedy"]
                        .as_str()
                        .unwrap()
                        .contains(&error.to_string())
                );
            }
        }
    }

    #[test]
    fn existing_open_refusals_have_typed_diagnostics_without_ddl() {
        for (damage, code) in [
            (
                "DROP INDEX memory_heads_scope",
                "projection_repair_required",
            ),
            (
                "ALTER TABLE objects ADD COLUMN incompatible TEXT",
                "different_build_schema",
            ),
            (
                "UPDATE control_policy_versions SET policy_json = X'7B7D'",
                "corrupt_store",
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("refusal.db");
            drop(engram::SqliteStore::open(&path).unwrap());
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection.execute_batch(damage).unwrap();
            drop(connection);
            let before = std::fs::read(&path).unwrap();
            let error = engram::SqliteStore::open(&path)
                .err()
                .expect("ordinary open must refuse");
            let value = refusal(&path, &ProjectId("diagnostics".into()), &error, Phase::Open);
            assert_eq!(value["code"], code, "{error}");
            assert_eq!(value["healthy"], false);
            assert_eq!(
                value["build_fingerprint"],
                json!(build_identity::current().build_fingerprint)
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
            match code {
                "projection_repair_required" => {
                    assert_eq!(value["remedy"], "engram doctor --repair-projections");
                    assert_eq!(value["scope"], json!(["indexes", "triggers", "fts"]));
                }
                "different_build_schema" => {
                    assert_eq!(
                        value["store_schema_reference"],
                        json!(store_schema_reference(&path).unwrap())
                    );
                    assert_eq!(value["running"], value["build"]);
                    assert_ne!(
                        value["store_schema_reference"],
                        value["running"]["schema_reference"]
                    );
                }
                _ => assert!(!value["findings"].as_array().unwrap().is_empty()),
            }
        }
    }
}
