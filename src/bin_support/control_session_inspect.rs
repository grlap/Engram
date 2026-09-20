//! Read-only presence receipt for host-owned stale-session reconciliation.

use super::doctor::{
    canonical_database_path,
    refusals::{emit, with_build},
};
use anyhow::{Result, bail};
use engram::storage::ControlSessionInspection;
use engram::{
    HostPathPolicy, MAX_SESSION_ID_BYTES, ProjectId, SqliteStore, describe_host_path_policy,
};
use serde_json::{Value, json};
use std::path::Path;

pub(crate) fn selector(value: &str) -> Result<String, String> {
    if value.trim().is_empty()
        || value.len() > MAX_SESSION_ID_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(format!(
            "expected a nonblank, control-free id of at most {MAX_SESSION_ID_BYTES} UTF-8 bytes"
        ));
    }
    Ok(value.into())
}

fn envelope(value: Value) -> Value {
    let mut value = value;
    value["schema_version"] = json!(1);
    value["scope"] = json!("control_session_inspect");
    value["mutation_enabled"] = json!(false);
    with_build(value)
}

pub(crate) fn run(
    database: &Path,
    identity: Option<HostPathPolicy>,
    project: &ProjectId,
    session_id: &str,
    retained_grant_id: &str,
    json_output: bool,
) -> Result<()> {
    let mut value = json!({
        "project_id": project, "database": null,
        "session_id": session_id, "retained_grant_id": retained_grant_id,
    });
    let result = (|| -> Result<_> {
        let path = canonical_database_path(database)?;
        value["database"] = json!(path);
        let report = SqliteStore::inspect_control_session(
            database,
            identity,
            session_id,
            retained_grant_id,
        )?;
        Ok((path, report))
    })();
    let value = report_receipt(database, identity, value, result);
    emit(&value, json_output)?;
    if let Some(reason) = value.get("reason") {
        bail!("control session inspection refused: {reason}");
    }
    Ok(())
}

// The successful result must come from inspect_control_session with the same
// database, identity and selectors as run() uses. Only after rechecking the
// canonical path may this function add any presence evidence to the envelope.
fn report_receipt(
    database: &Path,
    identity: Option<HostPathPolicy>,
    mut value: Value,
    result: Result<(String, ControlSessionInspection)>,
) -> Value {
    let result = result.and_then(|(path, report)| {
        // Path identity is evidence, not a lock. The host must hold its own
        // store/reset fence throughout the read and local reconciliation.
        if canonical_database_path(database)? != path {
            bail!("control inspection database path changed");
        }
        Ok(report)
    });
    match result {
        Ok(report) => {
            value["host_path_policy"] = json!({
                "stored": describe_host_path_policy(report.stored_host_path_policy),
                "resolved": identity.map(describe_host_path_policy), "status": "matched",
            });
            value["session_present"] = json!(report.session_present);
            value["session_grants_present"] = json!(report.session_grants_present);
            value["retained_grant_present"] = json!(report.retained_grant_present);
            envelope(value)
        }
        Err(error) => {
            // No default false booleans on ANY failed read.
            refusal_receipt(value, &error)
        }
    }
}

fn refusal_receipt(mut value: Value, error: &anyhow::Error) -> Value {
    value["code"] = json!("control_session_inspection_refused");
    value["reason"] = json!(format!("{error:#}"));
    envelope(value)
}

pub(crate) fn resolution_error(error: &anyhow::Error, json_output: bool) -> Result<()> {
    emit(
        &refusal_receipt(
            json!({
                "project_id": null, "database": null, "phase": "resolve",
            }),
            error,
        ),
        json_output,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_refusal(value: &Value) {
        assert_eq!(value["code"], "control_session_inspection_refused");
        assert_eq!(value["scope"], "control_session_inspect");
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["mutation_enabled"], false);
        for key in [
            "session_present",
            "session_grants_present",
            "retained_grant_present",
        ] {
            assert!(value.get(key).is_none(), "unexpected {key}");
        }
    }

    #[test]
    fn control_inspection_post_read_path_checks_refuse_without_presence() {
        let directory = crate::test_support::temp_home().unwrap();
        let database = directory.path().join("inspect.db");
        drop(SqliteStore::open(&database).unwrap());
        let identity = Some(HostPathPolicy::host_default());
        let path = canonical_database_path(&database).unwrap();
        let read = || {
            SqliteStore::inspect_control_session(&database, identity, "target", "grant").unwrap()
        };
        let report = read();
        let good = report_receipt(&database, identity, json!({}), Ok((path.clone(), report)));
        assert_eq!(good["session_present"], false);
        assert!(good.get("code").is_none());

        // A different pre-read canonical identity deterministically exercises
        // the inequality branch, without platform-dependent symlink setup.
        let other = directory.path().join("other.db");
        drop(SqliteStore::open(&other).unwrap());
        let other_path = canonical_database_path(&other).unwrap();
        let mismatch = report_receipt(&database, identity, json!({}), Ok((other_path, read())));
        assert_refusal(&mismatch);
        assert_eq!(
            mismatch["reason"],
            "control inspection database path changed"
        );

        let report = read();
        std::fs::remove_file(&database).unwrap();
        let cause = std::fs::canonicalize(&database).unwrap_err();
        let missing = report_receipt(&database, identity, json!({}), Ok((path, report)));
        assert_refusal(&missing);
        let reason = missing["reason"].as_str().unwrap();
        assert!(reason.contains("failed to canonicalize"));
        assert!(reason.contains(&cause.to_string()));
        assert!(!database.exists());
    }

    #[test]
    fn control_inspection_refusal_preserves_outer_context_and_cause() {
        let error = anyhow::anyhow!("underlying fixture cause").context("outer fixture context");
        let value = refusal_receipt(json!({"phase": "resolve"}), &error);
        assert_refusal(&value);
        assert_eq!(value["phase"], "resolve");
        assert_eq!(
            value["reason"],
            "outer fixture context: underlying fixture cause"
        );
    }

    #[test]
    fn control_inspection_selector_preserves_exact_ids_with_shared_byte_bound() {
        for exact in [
            "a".repeat(MAX_SESSION_ID_BYTES),
            format!(
                "{}{}",
                "é".repeat(MAX_SESSION_ID_BYTES / 2),
                "a".repeat(MAX_SESSION_ID_BYTES % 2)
            ),
            " padded ".into(),
        ] {
            assert_eq!(selector(&exact).unwrap(), exact);
            if exact.len() == MAX_SESSION_ID_BYTES {
                assert!(selector(&format!("{exact}a")).is_err());
            }
        }
        for invalid in ["", "   ", "bad\nid"] {
            assert!(selector(invalid).is_err());
        }
    }
}
