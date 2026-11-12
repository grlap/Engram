use super::*;
use crate::DevelopmentNoopRedactor;
use crate::domain::{WorkEvent, WorkLifecycle, WorkRunState, WorkTransition};
use crate::storage::work::test_support::{
    checkpoint, claim, evidence as native_evidence, root_request,
};
use crate::task_sync::evidence::inventory::plan_batch;
use std::collections::BTreeMap;

struct Fixture {
    pack: EvidencePack,
    event: RecordEnvelope,
    custody: Vec<ObjectId>,
    required: Vec<ObjectId>,
}

fn fixture(superseded: bool) -> Fixture {
    let mut source = SqliteStore::open_in_memory().unwrap();
    let work = source
        .create_work(
            &root_request(&project().0, "custody history", 1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let held = claim(&mut source, &work, "origin-holder", "claim", 2, 3600);
    let note = native_evidence(&mut source, &work, &held, "origin-holder", "note", 3);
    let checkpoint = checkpoint(
        &mut source,
        &work,
        &held,
        "origin-holder",
        "checkpoint",
        4,
        std::slice::from_ref(&note),
    );
    let bytes: Vec<u8> = source.connection.query_row(
        "SELECT o.canonical_json FROM work_feed_entries f JOIN objects o ON o.object_id=f.object_id WHERE f.feed_kind='run_execution' AND f.feed_id=?1 AND f.object_kind='work_event' ORDER BY f.position DESC LIMIT 1",
        [held.run_id.0.to_string()], |row| row.get(0),
    ).unwrap();
    let mut event: WorkEvent = serde_json::from_slice(&bytes).unwrap();
    let root = event.root_execution.as_ref().unwrap().head.clone();
    let custody_hold = ObjectId::mint();
    let mut custody = vec![custody_hold.clone()];
    event.transition = if superseded {
        let prepared = ObjectId::mint();
        let receipt = ObjectId::mint();
        custody.extend([prepared.clone(), receipt.clone()]);
        event.work.lifecycle = WorkLifecycle::Superseded;
        event.run.as_mut().unwrap().state = WorkRunState::SupersededByImport;
        WorkTransition::SupersededByImport {
            hold: custody_hold,
            prepared,
            receipt,
            reason: "original planning import retired this generation".into(),
        }
    } else {
        WorkTransition::ReconciliationHoldAbandoned {
            hold: custody_hold,
            reason: "original reconciliation was explicitly abandoned".into(),
        }
    };
    let (origin, enrollment_body) = origin(None);
    let event = envelope(&origin, &ObjectId::mint(), RecordKind::WorkEvent, &event);
    let mut records = BTreeMap::from([
        (origin.clone(), enrollment(&origin, &enrollment_body)),
        (event.object_id().clone(), event.clone()),
    ]);
    let mut pending = vec![event.clone()];
    while let Some(record) = pending.pop() {
        for dependency in crate::task_sync::evidence::inventory::dependencies(&record).unwrap() {
            if records.contains_key(&dependency.object_id) {
                continue;
            }
            let (kind, bytes): (String, Vec<u8>) = source
                .connection
                .query_row(
                    "SELECT object_kind,canonical_json FROM objects WHERE object_id=?1",
                    [dependency.object_id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            let linked = RecordEnvelope::from_canonical(
                origin.clone(),
                dependency.object_id.clone(),
                &kind,
                bytes,
            )
            .unwrap();
            records.insert(dependency.object_id, linked.clone());
            pending.push(linked);
        }
    }
    // These are actual native context objects, selected independently of the
    // collector's traversal; omitting any of them must fail the negative case.
    let required = vec![root, checkpoint, note];
    for id in &required {
        assert!(records.contains_key(id), "required context {id}");
    }
    for id in &custody {
        assert!(
            !records.contains_key(id),
            "custody is not an evidence original"
        );
    }
    Fixture {
        pack: pack(&origin, records.into_values().collect()),
        event,
        custody,
        required,
    }
}

fn runtime_counts(store: &SqliteStore) -> Vec<i64> {
    [
        "work_items",
        "work_runs",
        "work_claims",
        "work_feed_entries",
        "work_session_state",
        "work_task_sync_reconciliation_holds",
        "work_task_sync_import_heads",
        "work_task_sync_exchange_operations",
        "work_task_sync_exchange_heads",
    ]
    .map(|table| {
        store
            .connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    })
    .to_vec()
}

#[test]
fn custody_events_round_trip_originals_without_importing_local_custody_authority() {
    for superseded in [false, true] {
        let fixture = fixture(superseded);
        plan_batch(std::slice::from_ref(&fixture.pack), |_| Ok(None)).unwrap();
        let encoded = fixture.pack.to_bytes().unwrap();
        let mut a = SqliteStore::open_in_memory().unwrap();
        let before = runtime_counts(&a);
        retain(&mut a, &fixture.pack);
        assert_eq!(runtime_counts(&a), before);
        let (kind, bytes): (String, Vec<u8>) = a
            .connection
            .query_row(
                "SELECT object_kind,canonical_json FROM objects WHERE object_id=?1",
                [fixture.event.object_id().as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind, fixture.event.kind().as_str());
        assert_eq!(bytes, fixture.event.canonical_bytes());
        for id in &fixture.custody {
            let count: i64 = a
                .connection
                .query_row(
                    "SELECT count(*) FROM objects WHERE object_id=?1",
                    [id.as_str()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 0);
        }
        let after = snapshot(&a);
        assert!(retain(&mut a, &fixture.pack).replayed);
        assert_eq!(snapshot(&a), after);
        let exported = a
            .export_evidence_packs(&a.capture_evidence_packs(&project()).unwrap().unwrap())
            .unwrap();
        assert_eq!(exported.packs.len(), 1);
        assert_eq!(exported.packs[0].to_bytes().unwrap(), encoded);
        let mut b = SqliteStore::open_in_memory().unwrap();
        let other_before = runtime_counts(&b);
        retain(&mut b, &exported.packs[0]);
        assert_eq!(runtime_counts(&b), other_before);
        let returned = b
            .export_evidence_packs(&b.capture_evidence_packs(&project()).unwrap().unwrap())
            .unwrap();
        assert_eq!(returned.packs[0].to_bytes().unwrap(), encoded);
        assert!(retain(&mut a, &returned.packs[0]).replayed);
        assert_eq!(snapshot(&a), after);
        healthy(&a);
        healthy(&b);
    }
}

#[test]
fn custody_provenance_does_not_excuse_missing_or_wrongly_typed_execution_context() {
    for superseded in [false, true] {
        let fixture = fixture(superseded);
        for (index, missing) in fixture.required.iter().enumerate() {
            for wrong_kind in [false, true] {
                // Evidence citations are polymorphic. Root heads and run
                // checkpoints, unlike those citations, require one exact kind.
                if wrong_kind && index == 2 {
                    continue;
                }
                let records = fixture
                    .pack
                    .records()
                    .iter()
                    .filter_map(|record| {
                        if record.object_id() != missing {
                            return Some(record.clone());
                        }
                        wrong_kind.then(|| {
                            RecordEnvelope::from_canonical(
                                record.origin().clone(),
                                missing.clone(),
                                RecordKind::ProducingOrigin.as_str(),
                                fixture
                                    .pack
                                    .records()
                                    .iter()
                                    .find(|r| r.kind() == RecordKind::ProducingOrigin)
                                    .unwrap()
                                    .canonical_bytes()
                                    .to_vec(),
                            )
                            .unwrap()
                        })
                    })
                    .collect();
                let bad = pack(fixture.pack.publishing_origin(), records);
                // Import checks closure with this planner before retaining inert
                // originals; low-level retention does not admit their graph.
                let expected_problem = if wrong_kind {
                    "required original kind differs"
                } else {
                    "required original is unavailable"
                };
                assert!(
                    matches!(plan_batch(std::slice::from_ref(&bad), |_| Ok(None)),
                    Err(EvidenceError::Dependency { object, problem })
                        if &object == missing && problem == expected_problem)
                );
                if !wrong_kind {
                    let mut store = SqliteStore::open_in_memory().unwrap();
                    let before = runtime_counts(&store);
                    retain(&mut store, &bad);
                    assert_eq!(runtime_counts(&store), before);
                }
            }
        }
    }
}
