//! The operator `doctor` word: integrity, control, host-path, and graph
//! snapshot audit rendering plus projection repair.

use std::{fs, path::Path};

use anyhow::{Context, Result, bail};
use engram::{HostPathPolicy, ProjectId, SqliteStore, describe_host_path_policy};

use crate::{control_assurance_name, warn_if_action_gated};

const MAX_DOCTOR_GRAPH_SNAPSHOT_AUDITS: usize = 32;
const MAX_DOCTOR_GRAPH_SNAPSHOT_ACTOR_BYTES: usize = 256;

mod refusals;
use refusals::{Phase, report_error, with_build};

pub(crate) fn doctor(
    database: &Path,
    identity: Option<HostPathPolicy>,
    project_id: &ProjectId,
    json: bool,
    recover_policy: bool,
    repair_projections: bool,
) -> Result<()> {
    if recover_policy {
        return diagnose_policy_recovery(database, project_id, json);
    }
    if repair_projections {
        return repair_store_projections(database, project_id, json);
    }
    let store = match SqliteStore::open_with_host_path_identity(database, identity) {
        Ok(store) => store,
        Err(error) => return report_error(database, project_id, &error, json, Phase::Open),
    };
    let report = match store.verify_all() {
        Ok(report) => report,
        Err(error) => return report_error(database, project_id, &error, json, Phase::Verification),
    };
    if json {
        let json_report = build_doctor_json_report(&store, database, project_id, &report)?;
        println!("{}", serde_json::to_string_pretty(&json_report.value)?);
        match &json_report.control {
            Some(control) => emit_control_limitations(control),
            None => emit_unavailable_control_diagnostics_limitations(),
        }
        if let Some(failure) = json_report.failure {
            bail!("{failure}");
        }
        return Ok(());
    }
    if !report.is_healthy() {
        let json_report = build_doctor_json_report(&store, database, project_id, &report)?;
        refusals::emit(&json_report.value, false)?;
        bail!(
            "integrity check failed for {} object(s), {} graph snapshot audit(s), {} control record(s), and {} work record(s)",
            report.invalid_objects.len(),
            report.invalid_graph_snapshot_audits.len(),
            report.invalid_control_records.len(),
            report.invalid_work_records.len()
        );
    }
    let control = match store.control_diagnostics_at(chrono::Utc::now()) {
        Ok(control) => control,
        Err(error) => {
            return report_error(
                database,
                project_id,
                &error,
                json,
                Phase::ControlDiagnostics,
            );
        }
    };
    let stored_path_policy = store.stored_host_path_policy()?;
    println!(
        "Engram store is healthy ({} immutable object(s), {} graph snapshot audit(s), {} control record(s), {} work record(s) checked)",
        report.checked_objects,
        report.checked_graph_snapshot_audits,
        report.checked_control_records,
        report.checked_work_records
    );
    print_graph_snapshot_audits(&store, project_id)?;
    println!(
        "Control policy schema={} id={} epoch={} required={} obligation_rules={} supported={:?}; sessions={} issued={} begun={}",
        control.control_schema_version,
        control.active_policy,
        control.policy_epoch.0,
        control_assurance_name(control.required_assurance),
        control.obligation_rule_set,
        control.supported_effects,
        control.active_sessions,
        control.issued_turns,
        control.begun_turns,
    );
    match (stored_path_policy, identity) {
        (Some(stored), Some(_)) => println!(
            "Host path policy: {} (persisted; this opener resolved the same)",
            describe_host_path_policy(stored)
        ),
        (Some(stored), None) => println!(
            "Host path policy: {} (persisted; this opener could not resolve the project root, so path leases would be refused)",
            describe_host_path_policy(stored)
        ),
        (None, Some(resolved)) => println!(
            "Host path policy: {} (resolved now, persisted by this open)",
            describe_host_path_policy(resolved)
        ),
        (None, None) => println!(
            "Host path policy: unresolved; path leases are refused until --host-path-policy is supplied"
        ),
    }
    emit_control_limitations(&control);
    Ok(())
}

fn diagnose_policy_recovery(database: &Path, project_id: &ProjectId, json: bool) -> Result<()> {
    let report = match SqliteStore::diagnose_control_policy_recovery(database) {
        Ok(report) => report,
        Err(error) => {
            return report_error(
                database,
                project_id,
                &error,
                json,
                Phase::ControlPolicyRecovery,
            );
        }
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&with_build(serde_json::json!({
                "mode": "control_policy_recovery",
                "mutation_enabled": false,
                "project_id": project_id,
                "database": canonical_database_path(database)?,
                "control_policy": report,
            })))?
        );
    } else {
        println!(
            "Engram control-policy recovery diagnostics only ({} record(s) checked; mutation disabled)",
            report.checked_control_records
        );
        for finding in &report.invalid_control_records {
            println!("INVALID {}: {}", finding.record, finding.detail);
        }
        println!("Guidance: {}", report.guidance);
    }
    if !report.is_healthy() {
        bail!(
            "control-policy recovery diagnostics found {} invalid binding(s); the store remains fail-closed and unchanged",
            report.invalid_control_records.len()
        );
    }
    Ok(())
}

fn print_graph_snapshot_audits(store: &SqliteStore, project_id: &ProjectId) -> Result<()> {
    let (total, audits) = store
        .recent_work_graph_snapshot_save_audits(project_id, MAX_DOCTOR_GRAPH_SNAPSHOT_AUDITS)?;
    if total > audits.len() {
        println!(
            "Graph snapshot disclosure attempts: {total} total; showing the latest {}",
            audits.len()
        );
    }
    for audit in audits {
        println!(
            "Graph snapshot disclosure attempted at {}: cut work={} memory={}, widened={}, widening reason={}, redacted={}, body={}, destination={:?}, actor={}",
            audit.attempted_at,
            audit.as_of.work_feed,
            audit.as_of.project_memory,
            audit.widened,
            audit.widening_reason.as_deref().unwrap_or("none"),
            audit.redacted.items
                + audit.redacted.blockers
                + audit.redacted.sources
                + audit.redacted.records
                + audit.redacted.memories,
            audit.body_sha256,
            audit.destination_kind,
            bounded_doctor_snapshot_actor(&audit.actor.actor_id),
        );
    }
    let (total, audits) = store
        .recent_work_graph_snapshot_load_audits(project_id, MAX_DOCTOR_GRAPH_SNAPSHOT_AUDITS)?;
    if total > audits.len() {
        println!(
            "Graph snapshot loads: {total} total; showing the latest {}",
            audits.len()
        );
    }
    for audit in audits {
        println!(
            "Graph snapshot loaded at {}: cut work={} memory={}, widened={}, redacted={}, body={}, exporting build={}, actor={}",
            audit.loaded_at,
            audit.as_of.work_feed,
            audit.as_of.project_memory,
            audit.widened,
            audit.redacted.items
                + audit.redacted.blockers
                + audit.redacted.sources
                + audit.redacted.records
                + audit.redacted.memories,
            audit.body_sha256,
            audit.exporting_build,
            bounded_doctor_snapshot_actor(&audit.actor.actor_id),
        );
    }
    Ok(())
}

fn repair_store_projections(database: &Path, project_id: &ProjectId, json: bool) -> Result<()> {
    let report = match SqliteStore::repair_rebuildable_projections(database) {
        Ok(report) => report,
        Err(error) => {
            return report_error(database, project_id, &error, json, Phase::ProjectionRepair);
        }
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&with_build(serde_json::json!({
                "mode": "projection_repair",
                "mutation_enabled": true,
                "project_id": project_id,
                "database": canonical_database_path(database)?,
                "healthy": report.is_healthy(),
                "checked_objects": report.checked_objects,
                "checked_graph_snapshot_audits": report.checked_graph_snapshot_audits,
                "checked_control_records": report.checked_control_records,
                "checked_work_records": report.checked_work_records,
                "invalid_objects": report.invalid_objects,
                "invalid_graph_snapshot_audits": report.invalid_graph_snapshot_audits,
                "invalid_control_records": report.invalid_control_records,
                "invalid_work_records": report.invalid_work_records,
            })))?
        );
    } else {
        println!(
            "Engram rebuildable projections repaired and verified ({} immutable object(s), {} graph snapshot audit(s), {} control record(s), {} work record(s) checked)",
            report.checked_objects,
            report.checked_graph_snapshot_audits,
            report.checked_control_records,
            report.checked_work_records
        );
    }
    Ok(())
}

struct DoctorJsonReport {
    value: serde_json::Value,
    control: Option<engram::storage::ControlDiagnostics>,
    failure: Option<String>,
}

fn build_doctor_json_report(
    store: &SqliteStore,
    database: &Path,
    project_id: &ProjectId,
    report: &engram::storage::IntegrityReport,
) -> Result<DoctorJsonReport> {
    let control = store.control_diagnostics_at(chrono::Utc::now());
    assemble_doctor_json_report(store, database, project_id, report, control)
}

fn assemble_doctor_json_report(
    store: &SqliteStore,
    database: &Path,
    project_id: &ProjectId,
    report: &engram::storage::IntegrityReport,
    control: std::result::Result<engram::storage::ControlDiagnostics, engram::StoreError>,
) -> Result<DoctorJsonReport> {
    // Preserve the typed error before any best-effort integrity rendering.
    // A transient diagnostic failure does not prove that the store is corrupt.
    if let Err(error) = &control
        && report.is_healthy()
        && engram::storage::store_open_refusal_kind(error)
            != engram::storage::StoreOpenRefusalKind::CorruptStore
    {
        return Ok(DoctorJsonReport {
            value: refusals::refusal(database, project_id, error, Phase::ControlDiagnostics),
            control: None,
            failure: Some(format!("control diagnostics failed: {error}")),
        });
    }
    let stored_path_policy = store.stored_host_path_policy()?;
    let canonical_database = canonical_database_path(database)?;
    let (control, control_value, control_error) = match control {
        Ok(control) => {
            let value = control_diagnostics_json(&control);
            (Some(control), value, None)
        }
        Err(error) => (None, serde_json::Value::Null, Some(error.to_string())),
    };
    let graph_snapshot_disclosures = if report.invalid_graph_snapshot_audits.is_empty() {
        let (total, items) = store
            .recent_work_graph_snapshot_save_audits(project_id, MAX_DOCTOR_GRAPH_SNAPSHOT_AUDITS)?;
        let items = items
            .iter()
            .map(|item| compact_graph_snapshot_audit_json(item, &item.actor.actor_id))
            .collect::<Result<Vec<_>>>()?;
        serde_json::json!({ "total": total, "items": items })
    } else {
        serde_json::Value::Null
    };
    let graph_snapshot_loads = if report.invalid_graph_snapshot_audits.is_empty() {
        let (total, items) = store
            .recent_work_graph_snapshot_load_audits(project_id, MAX_DOCTOR_GRAPH_SNAPSHOT_AUDITS)?;
        let items = items
            .iter()
            .map(|item| compact_graph_snapshot_audit_json(item, &item.actor.actor_id))
            .collect::<Result<Vec<_>>>()?;
        serde_json::json!({ "total": total, "items": items })
    } else {
        serde_json::Value::Null
    };
    let mut value = with_build(serde_json::json!({
        "healthy": report.is_healthy(),
        "project_id": project_id,
        "database": canonical_database,
        "work_schema_version": store.work_schema_version(),
        "checked": {
            "objects": report.checked_objects,
            "graph_snapshot_audits": report.checked_graph_snapshot_audits,
            "control_records": report.checked_control_records,
            "work_records": report.checked_work_records,
        },
        "invalid": {
            "objects": report.invalid_objects,
            "graph_snapshot_audits": report.invalid_graph_snapshot_audits,
            "control_records": report.invalid_control_records,
            "work_records": report.invalid_work_records,
        },
        "graph_snapshot_disclosure_attempts": graph_snapshot_disclosures,
        "graph_snapshot_loads": graph_snapshot_loads,
        "host_path_policy": stored_path_policy.map(describe_host_path_policy),
        "control": control_value,
    }));
    if let Some(error) = &control_error {
        let serde_json::Value::Object(object) = &mut value else {
            bail!("doctor JSON report is not an object");
        };
        object.insert(
            "control_error".into(),
            serde_json::Value::String(error.clone()),
        );
    }
    let failure = if report.is_healthy() {
        control_error.map(|error| format!("control diagnostics failed: {error}"))
    } else {
        Some(format!(
            "integrity check failed for {} object(s), {} graph snapshot audit(s), {} control record(s), and {} work record(s)",
            report.invalid_objects.len(),
            report.invalid_graph_snapshot_audits.len(),
            report.invalid_control_records.len(),
            report.invalid_work_records.len()
        ))
    };
    if failure.is_some() {
        value["healthy"] = serde_json::json!(false);
        value["code"] = serde_json::json!("corrupt_store");
        value["phase"] = serde_json::json!(if report.is_healthy() {
            Phase::ControlDiagnostics
        } else {
            Phase::Verification
        });
        value["findings"] = corruption_findings(&value);
    }
    Ok(DoctorJsonReport {
        value,
        control,
        failure,
    })
}

fn control_diagnostics_json(control: &engram::storage::ControlDiagnostics) -> serde_json::Value {
    serde_json::json!({
        "schema_version": control.control_schema_version,
        "policy": control.active_policy,
        "epoch": control.policy_epoch.0,
        "required_assurance": control.required_assurance,
        "obligation_rules": control.obligation_rule_set,
        "supported_effects": control.supported_effects,
        "sessions": control.active_sessions,
        "issued": control.issued_turns,
        "begun": control.begun_turns,
    })
}

fn corruption_findings(value: &serde_json::Value) -> serde_json::Value {
    let mut findings = Vec::new();
    if let Some(families) = value["invalid"].as_object() {
        for (family, records) in families {
            if let Some(records) = records.as_array() {
                findings.extend(records.iter().map(|record| format!("{family}: {record}")));
            }
        }
    }
    if let Some(error) = value
        .get("control_error")
        .and_then(serde_json::Value::as_str)
    {
        findings.push(format!("control diagnostics: {error}"));
    }
    serde_json::json!(findings)
}

fn compact_graph_snapshot_audit_json<T: serde::Serialize>(
    audit: &T,
    actor_id: &str,
) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(audit)?;
    let serde_json::Value::Object(fields) = &mut value else {
        bail!("graph snapshot audit did not serialize as an object");
    };
    fields.insert(
        "actor".into(),
        serde_json::json!({
            "actor_id": bounded_doctor_snapshot_actor(actor_id),
            "details_omitted": true,
        }),
    );
    Ok(value)
}

fn bounded_doctor_snapshot_actor(actor_id: &str) -> String {
    if actor_id.len() <= MAX_DOCTOR_GRAPH_SNAPSHOT_ACTOR_BYTES {
        return actor_id.to_owned();
    }
    let suffix = "…";
    let mut end = MAX_DOCTOR_GRAPH_SNAPSHOT_ACTOR_BYTES.saturating_sub(suffix.len());
    while !actor_id.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &actor_id[..end])
}

fn canonical_database_path(database: &Path) -> Result<String> {
    let canonical = fs::canonicalize(database)
        .with_context(|| format!("failed to canonicalize {}", database.display()))?;
    Ok(path_without_windows_verbatim_prefix(&canonical))
}

#[cfg(windows)]
fn path_without_windows_verbatim_prefix(path: &Path) -> String {
    let path = path.to_string_lossy();
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        path.into_owned()
    }
}

#[cfg(not(windows))]
fn path_without_windows_verbatim_prefix(path: &Path) -> String {
    path.display().to_string()
}

fn emit_control_limitations(control: &engram::storage::ControlDiagnostics) {
    warn_if_action_gated(control.required_assurance);
    if !control.action_gating_available {
        eprintln!(
            "CONTROL LIMITATION: action gating is unavailable; unsupported effects fail closed: {:?}",
            control.unenforced_effects
        );
    }
    if !control.authority_mediation_available {
        eprintln!(
            "CONTROL LIMITATION: organizational authority mediation is not wired; the built-in policy admits only effects that require no external authority decision"
        );
    }
    if !control.action_outcome_tracking_available {
        eprintln!(
            "CONTROL LIMITATION: action-outcome reconciliation is not wired; effects requiring action gating remain unsupported"
        );
    }
    emit_redactor_limitation();
}

fn emit_unavailable_control_diagnostics_limitations() {
    eprintln!(
        "CONTROL LIMITATION: control diagnostics are unavailable; this integrity report provides no enforcement assurance"
    );
    emit_redactor_limitation();
}

fn emit_redactor_limitation() {
    eprintln!("WARNING: development no-op redactor is active; no secret or PII protection");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_open_operational_control_failure_keeps_typed_refusal() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("healthy.db");
        let store = SqliteStore::open(&database).unwrap();
        let report = store.verify_all().unwrap();
        assert!(report.is_healthy());
        let before = fs::read(&database).unwrap();
        for (code, kind) in [
            (rusqlite::ffi::SQLITE_BUSY, "busy"),
            (rusqlite::ffi::SQLITE_READONLY, "permission"),
            (rusqlite::ffi::SQLITE_IOERR, "io"),
        ] {
            // Inject the control query result at the production report-assembly
            // boundary: no lock timing, and ordinary open/verification succeeded.
            let error = engram::StoreError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                Some("control query failed\nexact OS detail".into()),
            ));
            let reason = error.to_string();
            let result = assemble_doctor_json_report(
                &store,
                &database,
                &ProjectId("doctor-operation".into()),
                &report,
                Err(error),
            )
            .unwrap();
            assert_eq!(result.value["healthy"], false);
            assert_eq!(result.value["code"], "store_open_refused");
            assert_eq!(result.value["phase"], "control_diagnostics");
            assert_eq!(result.value["kind"], kind);
            assert_eq!(result.value["reason"], reason);
            assert!(result.value.get("findings").is_none());
            let remedy = result.value["remedy"].as_str().unwrap();
            if kind == "busy" {
                assert!(remedy.starts_with("Retry"));
            } else {
                assert!(remedy.contains(&database.display().to_string()));
                assert!(remedy.contains(&reason));
            }
            assert_eq!(
                result.failure,
                Some(format!("control diagnostics failed: {reason}"))
            );
            assert!(result.control.is_none());
            assert_eq!(fs::read(&database).unwrap(), before);
        }
    }

    #[test]
    fn unhealthy_integrity_is_not_hidden_by_operational_control_failure() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("corrupt.db");
        let store = SqliteStore::open(&database).unwrap();
        let corruptor = rusqlite::Connection::open(&database).unwrap();
        corruptor
            .execute_batch("UPDATE control_policy_versions SET policy_json = X'7B7D'")
            .unwrap();
        drop(corruptor);
        let report = store.verify_all().unwrap();
        assert!(!report.is_healthy());
        let project = ProjectId("doctor-combined-failure".into());
        let baseline = build_doctor_json_report(&store, &database, &project, &report).unwrap();
        let before = fs::read(&database).unwrap();
        let expected_failure = format!(
            "integrity check failed for {} object(s), {} graph snapshot audit(s), {} control record(s), and {} work record(s)",
            report.invalid_objects.len(),
            report.invalid_graph_snapshot_audits.len(),
            report.invalid_control_records.len(),
            report.invalid_work_records.len()
        );
        for code in [
            rusqlite::ffi::SQLITE_BUSY,
            rusqlite::ffi::SQLITE_READONLY,
            rusqlite::ffi::SQLITE_IOERR,
        ] {
            // Verification has already proved corruption. A later operational
            // control error must not discard those findings or change the cause
            // reported by the text path's integrity-failure exit.
            let error = engram::StoreError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                Some("later control read failed".into()),
            ));
            let reason = error.to_string();
            let result =
                assemble_doctor_json_report(&store, &database, &project, &report, Err(error))
                    .unwrap();
            assert_eq!(result.value["healthy"], false);
            assert_eq!(result.value["code"], "corrupt_store");
            assert_eq!(result.value["phase"], "verification");
            assert_eq!(result.value["invalid"], baseline.value["invalid"]);
            assert_eq!(result.value["checked"], baseline.value["checked"]);
            assert_eq!(result.value["control_error"], reason);
            let findings = result.value["findings"].as_array().unwrap();
            for finding in baseline.value["findings"].as_array().unwrap() {
                if !finding
                    .as_str()
                    .unwrap()
                    .starts_with("control diagnostics:")
                {
                    assert!(findings.contains(finding));
                }
            }
            assert!(
                findings.contains(&serde_json::json!(format!("control diagnostics: {reason}")))
            );
            assert_eq!(result.failure.as_deref(), Some(expected_failure.as_str()));
            assert!(result.value.get("kind").is_none());
            assert!(result.value.get("remedy").is_none());
            assert!(result.control.is_none());
            assert_eq!(fs::read(&database).unwrap(), before);
        }
    }

    #[test]
    fn doctor_snapshot_actor_rendering_is_utf8_safe_and_bounded() {
        let actor = format!("{}é", "x".repeat(300));
        let rendered = bounded_doctor_snapshot_actor(&actor);
        assert!(rendered.len() <= MAX_DOCTOR_GRAPH_SNAPSHOT_ACTOR_BYTES);
        assert!(rendered.ends_with('…'));
        assert!(std::str::from_utf8(rendered.as_bytes()).is_ok());
    }

    #[test]
    fn unhealthy_doctor_json_survives_a_corrupt_policy_projection() {
        let directory = tempfile::tempdir().expect("temporary doctor store");
        let database = directory.path().join("engram.db");
        let store = SqliteStore::open(&database).expect("healthy store");
        let corruptor = rusqlite::Connection::open(&database).expect("corruption connection");
        corruptor
            .execute(
                "UPDATE control_policy_versions SET policy_json = X'7B7D'
                 WHERE policy_hash = (
                     SELECT policy_hash FROM control_policy_state WHERE singleton = 1
                 )",
                [],
            )
            .expect("corrupt active policy projection after open");
        drop(corruptor);

        let report = store.verify_all().expect("integrity report");
        assert!(!report.is_healthy());
        let json_report = build_doctor_json_report(
            &store,
            &database,
            &ProjectId("doctor-corrupt-policy".into()),
            &report,
        )
        .expect("best-effort doctor JSON");

        assert_eq!(json_report.value["healthy"], false);
        assert_eq!(json_report.value["code"], "corrupt_store");
        assert_eq!(json_report.value["phase"], "verification");
        let findings = json_report.value["findings"].as_array().unwrap();
        assert!(!findings.is_empty());
        assert!(findings.iter().all(serde_json::Value::is_string));
        assert!(
            findings
                .iter()
                .any(|finding| finding.as_str().unwrap().starts_with("control_records:"))
        );
        assert!(findings.iter().any(|finding| {
            finding
                .as_str()
                .unwrap()
                .starts_with("control diagnostics:")
        }));
        assert_eq!(json_report.value["control"], serde_json::Value::Null);
        assert!(json_report.value["control_error"].is_string());
        assert!(
            json_report.value["invalid"]["control_records"]
                .as_array()
                .is_some_and(|records| records.iter().any(|record| {
                    record
                        .as_str()
                        .is_some_and(|record| record.starts_with("control_policy_"))
                }))
        );
        assert!(json_report.control.is_none());
        assert!(json_report.failure.is_some());
        serde_json::to_string(&json_report.value).expect("printable doctor JSON");
    }
}
