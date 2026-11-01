use super::*;
use crate::domain::ObservedTurnOutcome;
use crate::storage::HostSessionAuthority;

#[test]
fn work_session_terminal_outcomes_preserve_absence_identity_and_migration() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let exported = directory.path().join("export.json");
    let destination = directory.path().join("imported.db");
    let mut f = fixture_on(SqliteStore::open(&source).unwrap());
    let session = SessionId("ordinary-outcomes".into());
    let token = f.store.resume_work_connection(&session, at(3)).unwrap();
    let mut observer = actor(&session.0);
    observer.run_id = Some(f.binding.run_id.0.to_string());
    let mut retained = Vec::new();
    for outcome in [
        None,
        Some(ObservedTurnOutcome::Succeeded),
        Some(ObservedTurnOutcome::Failed),
        Some(ObservedTurnOutcome::Cancelled),
        Some(ObservedTurnOutcome::Unknown),
    ] {
        let label = outcome.map_or("absent", ObservedTurnOutcome::as_str);
        let mut input = inter_turn_change(&f, label);
        input.occurrence = ObservedOccurrence::UnadmittedTurn {
            host_turn_ref: format!("turn-{label}"),
            outcome,
            source_change: None,
            observed_checks: vec![passed_check("observed-test")],
        };
        let wire = serde_json::to_value(&input).unwrap();
        let mut frame = wire.clone();
        frame["operation"] = serde_json::json!("execution_observe");
        let parsed: crate::host::HostControlRequest = serde_json::from_value(frame).unwrap();
        assert_eq!(parsed.operation(), "execution_observe");
        let receipt = f
            .store
            .record_unadmitted_execution_observation(
                &f.work.project_id,
                &session,
                &token,
                HostSessionAuthority::Work,
                &observer,
                input.clone(),
                at(7),
            )
            .unwrap();
        let record = stored(&f.store, &receipt.observation);
        record.validate_recorded_shape().unwrap();
        assert_eq!(record.occurrence.reported(), input.occurrence);
        assert_eq!(
            receipt.observed_checks[0].credit,
            ObservedCheckCredit::Uncredited
        );
        let saved = serde_json::to_value(&record.occurrence).unwrap();
        let words = crate::work_service::unadmitted::occurrence_words(&record.occurrence);
        if let Some(outcome) = outcome {
            assert_eq!(wire["occurrence"]["outcome"], outcome.as_str());
            assert_eq!(saved["outcome"], outcome.as_str());
            assert!(words.contains(&format!("host-reported outcome: {}", outcome.as_str())));
        } else {
            assert!(wire["occurrence"].get("outcome").is_none());
            assert!(saved.get("outcome").is_none());
            assert!(!words.contains("outcome"));
            // Decoding a historical absent member preserves its canonical bytes.
            let bytes = crate::canonical::canonical_bytes(&record).unwrap();
            let old: UnadmittedExecutionObservation = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(crate::canonical::canonical_bytes(&old).unwrap(), bytes);
        }
        let before = footprint(&f.store);
        assert_eq!(
            f.store
                .record_unadmitted_execution_observation(
                    &f.work.project_id,
                    &session,
                    &token,
                    HostSessionAuthority::Work,
                    &observer,
                    input.clone(),
                    at(8),
                )
                .unwrap(),
            receipt
        );
        let mut changed = input.clone();
        if let ObservedOccurrence::UnadmittedTurn { outcome, .. } = &mut changed.occurrence {
            *outcome = if outcome.is_none() {
                Some(ObservedTurnOutcome::Unknown)
            } else {
                None
            };
        }
        assert!(matches!(
            f.store.record_unadmitted_execution_observation(
                &f.work.project_id,
                &session,
                &token,
                HostSessionAuthority::Work,
                &observer,
                changed,
                at(8),
            ),
            Err(StoreError::ControlOperationIdempotencyConflict { .. })
        ));
        assert_eq!(footprint(&f.store), before);
        retained.push((input, receipt, record));
    }
    let report = f.store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
    drop(f.store);
    crate::storage::migration::export_json(&source, &exported).unwrap();
    crate::storage::migration::import_json(&exported, &destination).unwrap();
    let mut store = SqliteStore::open(&destination).unwrap();
    let token = store.resume_work_connection(&session, at(9)).unwrap();
    for (input, receipt, record) in retained {
        assert_eq!(stored(&store, &receipt.observation), record);
        assert_eq!(
            store
                .record_unadmitted_execution_observation(
                    &f.work.project_id,
                    &session,
                    &token,
                    HostSessionAuthority::Work,
                    &observer,
                    input,
                    at(10),
                )
                .unwrap(),
            receipt
        );
    }
    let report = store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
}

#[test]
fn work_session_terminal_outcome_wire_refuses_null_types_and_wrong_occurrence() {
    let f = fixture();
    let mut input = inter_turn_change(&f, "wire-outcome");
    input.occurrence = ObservedOccurrence::UnadmittedTurn {
        host_turn_ref: "turn-wire".into(),
        outcome: None,
        source_change: None,
        observed_checks: vec![],
    };
    let mut wire = serde_json::to_value(&input).unwrap();
    wire["operation"] = serde_json::json!("execution_observe");
    for invalid in [
        serde_json::Value::Null,
        serde_json::json!(17),
        serde_json::json!(true),
        serde_json::json!("stopped"),
        serde_json::json!("passed"),
    ] {
        wire["occurrence"]["outcome"] = invalid;
        assert!(serde_json::from_value::<crate::host::HostControlRequest>(wire.clone()).is_err());
        assert!(serde_json::from_value::<RecordedOccurrence>(wire["occurrence"].clone()).is_err());
    }
    for occurrence in [
        ObservedOccurrence::InterTurnChange {
            source_change: content_change("a", "b"),
        },
        ObservedOccurrence::ObservedCheck {
            host_turn_ref: "turn".into(),
            check: passed_check("test"),
        },
    ] {
        wire["occurrence"] = serde_json::to_value(occurrence).unwrap();
        wire["occurrence"]["outcome"] = serde_json::json!("succeeded");
        assert!(serde_json::from_value::<crate::host::HostControlRequest>(wire.clone()).is_err());
    }
}
