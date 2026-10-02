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
    accounted.accounting = ObservationAccounting::NoSourceChange;
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
