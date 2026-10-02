//! The store's own checks know the record: doctor accepts a recorded
//! observation and its receipt, refuses a forged one, and a store holding
//! old and new records round-trips through migration export and import.

use super::*;

fn healthy(store: &SqliteStore) -> bool {
    store.verify_all().is_ok_and(|report| report.is_healthy())
}

fn record_both_kinds(fixture: &mut Fixture) -> ExecutionObservationReceipt {
    let change = inter_turn_change(fixture, "doctor-change");
    observe(fixture, change, 7).expect("inter-turn change");
    let mut turn = inter_turn_change(fixture, "doctor-turn");
    turn.occurrence = ObservedOccurrence::UnadmittedTurn {
        host_turn_ref: "turn-9".into(),
        source_change: Some(content_change("rev-b", "rev-c")),
        observed_checks: vec![passed_check("cargo-test")],
    };
    turn.causality = ObservationCausality::HostAssertion {
        claimed_actor: Box::new(actor("runner")),
        basis: "terminal ownership".into(),
    };
    observe(fixture, turn, 8).expect("unadmitted turn")
}

#[test]
fn doctor_accepts_recorded_observations_and_their_receipts() {
    let mut fixture = fixture();
    record_both_kinds(&mut fixture);
    let report = fixture.store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

#[test]
fn doctor_refuses_a_forged_receipt() {
    let mut fixture = fixture();
    record_both_kinds(&mut fixture);
    // The stored receipt now claims the record was accounted for.
    fixture
        .store
        .connection
        .execute(
            "UPDATE control_operation_results
             SET result_json = CAST(replace(CAST(result_json AS TEXT),
                 '\"reason\":\"explicit_audit\"', '\"reason\":\"finished_run\"') AS BLOB)
             WHERE operation = 'execution_observe' AND idempotency_key = 'doctor-turn'",
            [],
        )
        .expect("forge the receipt");
    assert!(!healthy(&fixture.store));
}

#[test]
fn doctor_refuses_a_record_whose_cut_is_rewritten() {
    let mut fixture = fixture();
    let receipt = record_both_kinds(&mut fixture);
    // A capture cut at or after the record's own position names history the
    // record could not have seen.
    let forged = receipt.position.position;
    fixture
        .store
        .connection
        .execute(
            "UPDATE objects SET canonical_json = CAST(json_set(CAST(canonical_json AS TEXT),
                 '$.root_basis.capture_run_cut', ?2) AS BLOB)
             WHERE object_id = ?1",
            rusqlite::params![receipt.observation.as_str(), forged],
        )
        .expect("forge the record");
    assert!(!healthy(&fixture.store));
}

#[test]
fn a_store_with_old_and_new_records_round_trips_through_migration() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    {
        let mut fixture = fixture_on(SqliteStore::open(&source).expect("store"));
        record_both_kinds(&mut fixture);
        assert!(healthy(&fixture.store));
    }
    let file = directory.path().join("export.jsonl");
    crate::storage::migration::export_json(&source, &file).expect("export");
    let target = directory.path().join("target.db");
    crate::storage::migration::import_json(&file, &target).expect("import keeps the new kind");
    let imported = SqliteStore::open(&target).expect("imported store opens");
    assert!(healthy(&imported));
    let kinds: Vec<(String, i64)> = imported
        .connection
        .prepare(
            "SELECT object_kind, COUNT(*) FROM objects
             WHERE object_kind IN ('work_event', ?1) GROUP BY object_kind ORDER BY object_kind",
        )
        .expect("prepare")
        .query_map([UNADMITTED_OBSERVATION_KIND], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("kinds");
    assert_eq!(kinds[0].0, UNADMITTED_OBSERVATION_KIND);
    assert_eq!(kinds[0].1, 2);
    assert_eq!(kinds[1].0, "work_event");
}

// The integrity check alone, apart from the receipt: a record is consistent
// only with the history its own cut names.
#[test]
fn integrity_holds_a_record_to_its_cut_and_claim_epoch() {
    let mut fixture = fixture();
    let receipt = record_both_kinds(&mut fixture);
    let consistent = |observation: &crate::domain::UnadmittedExecutionObservation| {
        super::super::unadmitted_observation_is_consistent_on(
            &fixture.store.connection,
            observation,
            &receipt.observation,
        )
        .expect("check")
    };
    let recorded = stored(&fixture.store, &receipt.observation);
    assert!(consistent(&recorded));

    let mut late_cut = recorded.clone();
    late_cut.root_basis.capture_run_cut = receipt.position.position;
    assert!(!consistent(&late_cut), "a cut at its own position");

    let mut invented_epoch = recorded.clone();
    invented_epoch.claim_epoch_event = crate::ObjectId::from_canonical_bytes(b"no such event");
    assert!(
        !consistent(&invented_epoch),
        "an invented claim epoch event"
    );

    let mut accounted = recorded;
    accounted.accounting = ObservationAccounting::NoSourceChange {};
    assert!(
        !consistent(&accounted),
        "an accounting this build never records"
    );
}

// Doctor holds a record to exactly what the write path stores: the newest
// event recording the claim epoch at the cut, and a closing sighting that
// agrees with the root basis.
#[test]
fn integrity_holds_the_newest_epoch_event_and_the_sighting_to_the_root_basis() {
    use super::refusals::{name_root, sighting_under};
    use crate::domain::{NamedRootBindingKind, SourceRootState};
    let mut fixture = fixture();
    let first_epoch_event: String = fixture
        .store
        .connection
        .query_row(
            "SELECT entry.object_id FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
             WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
               AND entry.object_kind = 'work_event'
               AND json_extract(object.canonical_json, '$.claim.claim_id') = ?2
             ORDER BY entry.position LIMIT 1",
            rusqlite::params![
                fixture.claim.run_id.0.to_string(),
                fixture.claim.claim_id.0.to_string()
            ],
            |row| row.get(0),
        )
        .expect("the claim's first event");
    // A renewal records the same epoch again, later.
    claim(
        &mut fixture.store,
        &fixture.work,
        "runner",
        "observed-claim-renew",
        3,
        300,
    );
    let named = name_root(&mut fixture, 2, NamedRootBindingKind::Bound, 4, 4);
    let input = sighting_under(
        &fixture,
        "integrity-sighting",
        NamedRootState::Bound {
            workspace_id: "workspace-A".into(),
            generation: 2,
            named_at: at(4),
        },
        named.event,
        (2, SourceRootState::Named),
    );
    let receipt = observe(&mut fixture, input, 7).expect("recorded");
    let consistent = |observation: &crate::domain::UnadmittedExecutionObservation| {
        super::super::unadmitted_observation_is_consistent_on(
            &fixture.store.connection,
            observation,
            &receipt.observation,
        )
        .expect("check")
    };
    let recorded = stored(&fixture.store, &receipt.observation);
    assert!(consistent(&recorded));
    assert_ne!(recorded.claim_epoch_event.as_str(), first_epoch_event);

    let mut older_epoch = recorded.clone();
    older_epoch.claim_epoch_event =
        crate::ObjectId::from_stored(first_epoch_event).expect("record id");
    assert!(!consistent(&older_epoch), "an older valid epoch event");

    let mut stale_sighting = recorded;
    let RecordedOccurrence::InterTurnChange {
        source_change: ObservedSourceChange::ContentComparison { sighting, .. },
    } = &mut stale_sighting.occurrence
    else {
        unreachable!()
    };
    sighting.source_basis.source_root_generation = Some(1);
    assert!(
        !consistent(&stale_sighting),
        "a sighting under another generation"
    );
}

/// Accounted records of every kind on one run: a new change with its
/// obligation, a repeat of it, a report with no change, and one the lifecycle
/// kept as audit-only after a newer claim.
fn record_accounted_kinds(fixture: &mut Fixture) -> ExecutionObservationReceipt {
    use super::accounting::{accounted, observe_accounted};
    let change = observe_accounted(fixture, "doctor-account", "rev-a", "rev-b", 7).expect("change");
    observe_accounted(fixture, "doctor-repeat", "rev-x", "rev-b", 8).expect("repeat");
    let mut quiet = accounted(fixture, "doctor-quiet", "rev-a", "rev-b");
    quiet.occurrence = ObservedOccurrence::UnadmittedTurn {
        host_turn_ref: "turn-quiet".into(),
        source_change: None,
        observed_checks: vec![passed_check("cargo-test")],
    };
    observe(fixture, quiet, 9).expect("no change");
    let historical = accounted(fixture, "doctor-historical", "rev-b", "rev-c");
    claim(
        &mut fixture.store,
        &fixture.work,
        "runner-2",
        "doctor-reclaim",
        400,
        300,
    );
    observe(fixture, historical, 401).expect("historical");
    change
}

#[test]
fn doctor_accepts_accounted_records_and_their_obligations() {
    let mut fixture = fixture();
    record_both_kinds(&mut fixture);
    record_accounted_kinds(&mut fixture);
    let report = fixture.store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

// An accounting the history before the record does not decide is refused:
// a new change rewritten as a repeat, a record that names its own id as the
// anchor, or a repeat rewritten as a new change.
#[test]
fn integrity_holds_the_accounting_to_the_history_before_the_record() {
    let mut fixture = fixture();
    let change = record_accounted_kinds(&mut fixture);
    let consistent = |observation: &crate::domain::UnadmittedExecutionObservation| {
        super::super::unadmitted_observation_is_consistent_on(
            &fixture.store.connection,
            observation,
            &change.observation,
        )
        .expect("check")
    };
    let recorded = stored(&fixture.store, &change.observation);
    assert!(consistent(&recorded));
    for (forged, why) in [
        (
            ObservationAccounting::Repeat {
                source_change: crate::ObjectId::from_canonical_bytes(b"an anchor"),
            },
            "a change rewritten as a repeat",
        ),
        (
            ObservationAccounting::SourceChange {
                source_change: Some(change.observation.clone()),
            },
            "an anchor naming the record itself",
        ),
        (
            ObservationAccounting::NoSourceChange {},
            "a change rewritten as no change",
        ),
        (
            ObservationAccounting::AuditOnly {
                reason: ObservationAuditReason::ExplicitAudit,
            },
            "an accounted request kept as explicit audit",
        ),
    ] {
        let mut tampered = recorded.clone();
        tampered.accounting = forged;
        assert!(!consistent(&tampered), "{why}");
    }
    // A policy basis the project never held at the record's time.
    let mut other_policy = recorded;
    other_policy.policy_basis = ObservationPolicyBasis::AccountIfEligible {
        project_policy_epoch: ProjectPolicyEpoch(9),
        policy: crate::ObjectId::from_canonical_bytes(b"another policy"),
        obligation_rule_set: crate::ObjectId::from_canonical_bytes(b"another rule set"),
    };
    assert!(!consistent(&other_policy), "an unheld policy basis");
}

#[test]
fn doctor_refuses_a_lost_obligation_of_an_accounted_change() {
    let mut fixture = fixture();
    let change = record_accounted_kinds(&mut fixture);
    fixture
        .store
        .connection
        .execute(
            "DELETE FROM work_run_obligations WHERE obligation_id = (
                 SELECT json_extract(object.canonical_json, '$.obligation_id')
                 FROM objects object WHERE object.object_id = ?1)",
            [change.opened_obligations[0].as_str()],
        )
        .expect("drop the projection row");
    assert!(!healthy(&fixture.store));
}

#[test]
fn a_store_with_accounted_records_round_trips_through_migration() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    {
        let mut fixture = fixture_on(SqliteStore::open(&source).expect("store"));
        record_both_kinds(&mut fixture);
        record_accounted_kinds(&mut fixture);
        assert!(healthy(&fixture.store));
    }
    let file = directory.path().join("export.jsonl");
    crate::storage::migration::export_json(&source, &file).expect("export");
    let target = directory.path().join("target.db");
    crate::storage::migration::import_json(&file, &target).expect("import");
    let imported = SqliteStore::open(&target).expect("imported store opens");
    assert!(healthy(&imported));
    let accounting: Vec<String> = imported
        .connection
        .prepare(
            "SELECT json_extract(canonical_json, '$.accounting.kind') FROM objects
             WHERE object_kind = ?1 ORDER BY 1",
        )
        .expect("prepare")
        .query_map([UNADMITTED_OBSERVATION_KIND], |row| row.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("kinds");
    assert_eq!(
        accounting,
        [
            "audit_only",
            "audit_only",
            "audit_only",
            "no_source_change",
            "repeat",
            "source_change"
        ]
    );
}
