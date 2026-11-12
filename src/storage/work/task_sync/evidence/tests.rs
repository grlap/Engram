use super::*;
use crate::storage::work::test_support::{actor, at};
use crate::task_sync::evidence::witnesses::{
    FeedMember, OriginFeed, OriginFeedCut, OriginFeedSegment,
};
use crate::task_sync::origin::Enrollment;
use serde_json::json;

mod batch;
mod custody;
mod native_reads;
mod witness_scope;

fn project() -> ProjectId {
    ProjectId("evidence-project".into())
}
fn origin(predecessor: Option<ObjectId>) -> (ObjectId, Enrollment) {
    (
        ObjectId::mint(),
        Enrollment {
            project_id: project(),
            predecessor_origin_id: predecessor,
            enrollment_operation_id: uuid::Uuid::new_v4(),
            actor: actor("evidence-test"),
            enrolled_at: at(1),
        },
    )
}
fn envelope(
    origin: &ObjectId,
    id: &ObjectId,
    kind: RecordKind,
    value: &impl Serialize,
) -> RecordEnvelope {
    RecordEnvelope::from_canonical(
        origin.clone(),
        id.clone(),
        kind.as_str(),
        serde_json_canonicalizer::to_vec(value).unwrap(),
    )
    .unwrap()
}
fn enrollment(id: &ObjectId, value: &Enrollment) -> RecordEnvelope {
    envelope(id, id, RecordKind::ProducingOrigin, value)
}
fn evidence(origin: &ObjectId, summary: &str) -> RecordEnvelope {
    envelope(
        origin,
        &ObjectId::mint(),
        RecordKind::WorkEvidence,
        &json!({
            "schema_version":crate::schema::SCHEMA_VERSION,"work_id":uuid::Uuid::new_v4(),
            "run_id":uuid::Uuid::new_v4(),"claim_id":uuid::Uuid::new_v4(),"claim_fence":1,
            "summary":summary,"refs":[],"actor":actor("evidence-test"),"created_at":at(1)
        }),
    )
}
fn pack(origin: &ObjectId, records: Vec<RecordEnvelope>) -> EvidencePack {
    EvidencePack::prepare(ObjectId::mint(), project(), origin.clone(), records).unwrap()
}
fn retain(store: &mut SqliteStore, pack: &EvidencePack) -> Retention {
    store
        .retain_evidence_pack(&project(), pack, &ObjectId::mint())
        .unwrap()
}
fn snapshot(store: &SqliteStore) -> crate::storage::TestDatabaseShapeSnapshot {
    crate::storage::test_database_shape_snapshot(&store.connection).unwrap()
}
fn healthy(store: &SqliteStore) {
    let report = store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
}

fn originals(store: &SqliteStore) -> Vec<(String, String, Vec<u8>)> {
    store
        .connection
        .prepare("SELECT object_id,object_kind,canonical_json FROM objects ORDER BY object_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap()
}

#[test]
fn immutable_replay_and_return_exchange_preserve_bytes_without_native_authority() {
    let mut a = SqliteStore::open_in_memory().unwrap();
    let mut b = SqliteStore::open_in_memory().unwrap();
    let (id, origin) = origin(None);
    let fact = evidence(&id, "reported evidence, no destination credit");
    let input = pack(&id, vec![enrollment(&id, &origin), fact.clone()]);
    let encoded = input.to_bytes().unwrap();
    let result = retain(&mut a, &input);
    assert_eq!(result.imported_objects, 2);
    let after = snapshot(&a);
    let replay = retain(&mut a, &input);
    assert!(replay.replayed);
    assert_eq!(replay.admission, result.admission);
    assert_eq!(snapshot(&a), after);
    assert!(
        super::super::producing_origin::load_on(&a.connection, &project())
            .unwrap()
            .is_none()
    );
    assert!(
        crate::storage::work::feeds::load_typed_work_object::<crate::WorkEvidence>(
            &a.connection,
            fact.object_id(),
            "work_evidence"
        )
        .is_err()
    );
    let cursor = a.capture_evidence_packs(&project()).unwrap().unwrap();
    let exported = a.export_evidence_packs(&cursor).unwrap();
    assert!(exported.next.is_none());
    assert_eq!(exported.packs[0].to_bytes().unwrap(), encoded);
    retain(&mut b, &exported.packs[0]);
    let back = b
        .export_evidence_packs(&b.capture_evidence_packs(&project()).unwrap().unwrap())
        .unwrap();
    assert!(retain(&mut a, &back.packs[0]).replayed);
    assert_eq!(snapshot(&a), after);
    healthy(&a);
    healthy(&b);
}

#[test]
fn conflict_rolls_back_even_when_caller_commits_the_outer_writer() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let (id, origin) = origin(None);
    let fact = evidence(&id, "original");
    retain(
        &mut store,
        &pack(&id, vec![enrollment(&id, &origin), fact.clone()]),
    );
    let mut changed: serde_json::Value = serde_json::from_slice(fact.canonical_bytes()).unwrap();
    changed["summary"] = json!("different bytes");
    let collision = envelope(&id, fact.object_id(), fact.kind(), &changed);
    let input = pack(&id, vec![evidence(&id, "must roll back"), collision]);
    let before = snapshot(&store);
    let mut tx = store.connection.transaction().unwrap();
    assert!(matches!(
        retain_pack_on(&mut tx, &project(), &input, &ObjectId::mint()),
        Err(PersistenceError::Evidence(EvidenceError::RecordConflict(_)))
    ));
    tx.commit().unwrap();
    assert_eq!(snapshot(&store), before);
    // Same UUID with another valid kind also refuses.
    let other = envelope(&id, fact.object_id(), RecordKind::ProducingOrigin, &origin);
    assert!(
        store
            .retain_evidence_pack(&project(), &pack(&id, vec![other]), &ObjectId::mint())
            .is_err()
    );
    assert_eq!(snapshot(&store), before);
}

fn source(origin: &ObjectId) -> OriginFeed {
    OriginFeed {
        origin: origin.clone(),
        project_id: project(),
        feed: crate::FeedId::Project(project()),
    }
}
fn member(position: i64, fact: &RecordEnvelope) -> FeedMember {
    FeedMember {
        position,
        object_id: fact.object_id().clone(),
        object_kind: fact.kind().as_str().into(),
    }
}

#[test]
fn feed_witness_gap_beyond_cut_and_changed_membership_are_atomic_refusals() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let (id, origin) = origin(None);
    let fact = evidence(&id, "first");
    retain(
        &mut store,
        &pack(&id, vec![enrollment(&id, &origin), fact.clone()]),
    );
    let segment_id = ObjectId::mint();
    let segment = OriginFeedSegment {
        source: source(&id),
        from_exclusive: 0,
        through: 1,
        predecessor: None,
        anchor: None,
        entries: vec![member(1, &fact)],
    };
    let first = envelope(&id, &segment_id, RecordKind::OriginFeedSegment, &segment);
    let cut = OriginFeedCut {
        source: source(&id),
        through: 1,
        endpoint: Some(member(1, &fact)),
        segments: vec![segment_id.clone()],
    };
    retain(
        &mut store,
        &pack(
            &id,
            vec![
                first,
                envelope(&id, &ObjectId::mint(), RecordKind::OriginFeedCut, &cut),
            ],
        ),
    );
    let before = snapshot(&store);
    let mut gap = segment.clone();
    gap.entries[0].position = 2;
    let mut beyond = cut.clone();
    beyond.through = 0;
    beyond.endpoint = None;
    for record in [
        envelope(&id, &ObjectId::mint(), RecordKind::OriginFeedSegment, &gap),
        envelope(&id, &ObjectId::mint(), RecordKind::OriginFeedCut, &beyond),
    ] {
        let input = pack(&id, vec![record]);
        let mut tx = store.connection.transaction().unwrap();
        assert!(retain_pack_on(&mut tx, &project(), &input, &ObjectId::mint()).is_err());
        tx.commit().unwrap();
        assert_eq!(snapshot(&store), before);
    }
    let changed = evidence(&id, "competing membership");
    let mut conflict = segment;
    conflict.entries = vec![member(1, &changed)];
    assert!(
        store
            .retain_evidence_pack(
                &project(),
                &pack(
                    &id,
                    vec![
                        changed,
                        envelope(
                            &id,
                            &ObjectId::mint(),
                            RecordKind::OriginFeedSegment,
                            &conflict
                        )
                    ]
                ),
                &ObjectId::mint()
            )
            .is_err()
    );
    assert_eq!(snapshot(&store), before);
    healthy(&store);
}

#[test]
fn origin_lineage_gap_cycle_and_project_mismatch_refuse_without_retention() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let before = snapshot(&store);
    let (id, missing) = origin(Some(ObjectId::mint()));
    let (cycle_id, mut cycle) = origin(None);
    cycle.predecessor_origin_id = Some(cycle_id.clone());
    let (wrong_id, mut wrong) = origin(None);
    wrong.project_id = ProjectId("other".into());
    for (id, record) in [(id, missing), (cycle_id, cycle), (wrong_id, wrong)] {
        assert!(
            store
                .retain_evidence_pack(
                    &project(),
                    &pack(&id, vec![enrollment(&id, &record)]),
                    &ObjectId::mint()
                )
                .is_err()
        );
        assert_eq!(snapshot(&store), before);
    }
}

#[test]
fn missing_or_forged_partition_never_grants_a_native_read_or_append() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let (id, origin) = origin(None);
    let fact = evidence(&id, "foreign");
    let input = pack(&id, vec![enrollment(&id, &origin), fact.clone()]);
    let result = retain(&mut store, &input);
    store
        .connection
        .execute(
            "DELETE FROM work_task_sync_evidence_imported WHERE object_id=?1",
            [fact.object_id().as_str()],
        )
        .unwrap();
    assert!(require_native(&store.connection, fact.object_id()).is_err());
    assert!(!store.verify_all().unwrap().is_healthy());
    schema::rebuild(&store.connection).unwrap();
    let object =
        CanonicalObject::stored(fact.object_id(), fact.canonical_bytes().to_vec()).unwrap();
    let tx = store.connection.transaction().unwrap();
    assert!(
        crate::storage::work::feeds::append_to_project_feed(
            &tx,
            &project(),
            "work_evidence",
            &object
        )
        .is_err()
    );
    drop(tx);
    let native = CanonicalObject::mint(&json!({"native":"never imported"})).unwrap();
    SqliteStore::insert_object(&store.connection, "native_fixture", &native).unwrap();
    store
        .connection
        .execute(
            "INSERT INTO work_task_sync_evidence_imported(object_id,admission_id) VALUES (?1,?2)",
            params![native.key().as_str(), result.admission.as_str()],
        )
        .unwrap();
    assert!(require_native(&store.connection, native.key()).is_err());
    schema::rebuild(&store.connection).unwrap();
    assert!(!is_imported_only(&store.connection, native.key()).unwrap());
    healthy(&store);
}

#[test]
fn bounded_cursor_keeps_its_capture_and_rejects_foreign_or_future_endpoints() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let (id, origin) = origin(None);
    let first = pack(&id, vec![enrollment(&id, &origin)]);
    retain(&mut store, &first);
    let captured = store.capture_evidence_packs(&project()).unwrap().unwrap();
    let second = pack(&id, vec![evidence(&id, "later")]);
    retain(&mut store, &second);
    let original = store.export_evidence_packs(&captured).unwrap();
    assert_eq!(original.packs.len(), 1);
    assert_eq!(original.packs[0].pack_id(), first.pack_id());
    let bad = PackCursor {
        after: Some(second.pack_id().clone()),
        ..captured.clone()
    };
    assert!(store.export_evidence_packs(&bad).is_err());
    let bad = PackCursor {
        project_id: ProjectId("other".into()),
        ..captured
    };
    assert!(store.export_evidence_packs(&bad).is_err());
}

#[test]
fn foreign_same_project_and_identical_native_origin_survive_repair_and_migration() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("source.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let (native_id, native) = origin(None);
    let native_object = CanonicalObject::identified(&native_id, &native).unwrap();
    let tx = store.begin_work_mutation().unwrap();
    super::super::producing_origin::insert_enrollment_on(&tx, &native_object).unwrap();
    tx.commit().unwrap();
    let (foreign_id, foreign) = origin(None);
    let input = pack(
        &foreign_id,
        vec![
            enrollment(&foreign_id, &foreign),
            enrollment(&native_id, &native),
            evidence(&foreign_id, "retained"),
        ],
    );
    retain(&mut store, &input);
    assert!(!is_imported_only(&store.connection, &native_id).unwrap());
    assert_eq!(
        super::super::producing_origin::load_on(&store.connection, &project())
            .unwrap()
            .unwrap()
            .origin_id,
        native_id
    );
    let before = originals(&store);
    healthy(&store);
    drop(store);
    assert!(
        SqliteStore::repair_rebuildable_projections(&database)
            .unwrap()
            .is_healthy()
    );
    let store = SqliteStore::open(&database).unwrap();
    assert_eq!(originals(&store), before);
    healthy(&store);
    drop(store);
    let export = directory.path().join("migration.json");
    let imported = directory.path().join("imported.db");
    crate::storage::migration::export_json(&database, &export).unwrap();
    crate::storage::migration::import_json(&export, &imported).unwrap();
    let migrated = SqliteStore::open(&imported).unwrap();
    healthy(&migrated);
    assert_eq!(originals(&migrated), before);
    assert_eq!(
        super::super::producing_origin::load_on(&migrated.connection, &project())
            .unwrap()
            .unwrap()
            .origin_id,
        native_id
    );
}

#[test]
fn whole_pack_object_budget_resumes_without_reloading_the_captured_prefix() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let (id, origin) = origin(None);
    retain(&mut store, &pack(&id, vec![enrollment(&id, &origin)]));
    let mut last = None;
    for index in 0..3 {
        let batch = pack(
            &id,
            (0..3000)
                .map(|_| evidence(&id, &format!("batch {index}")))
                .collect(),
        );
        last = Some(batch.pack_id().clone());
        retain(&mut store, &batch);
    }
    let capture = store.capture_evidence_packs(&project()).unwrap().unwrap();
    let first = store.export_evidence_packs(&capture).unwrap();
    assert_eq!(
        first
            .packs
            .iter()
            .map(|pack| pack.records().len())
            .sum::<usize>(),
        6001
    );
    let next = first.next.unwrap();
    // A resumed page must not reload any earlier body, even for decoding.
    let old = first.packs[0].records()[0].object_id();
    store
        .connection
        .execute(
            "UPDATE objects SET canonical_json=?1 WHERE object_id=?2",
            params![b"{}".as_slice(), old.as_str()],
        )
        .unwrap();
    let tail = store.export_evidence_packs(&next).unwrap();
    assert_eq!(tail.packs.len(), 1);
    assert_eq!(Some(tail.packs[0].pack_id()), last.as_ref());
    assert!(tail.next.is_none());
}

#[test]
fn governing_witness_is_bound_to_the_original_evaluation_and_complete_snapshot() {
    use crate::task_sync::evidence::witnesses::{
        GoverningBasis, GoverningSnapshotWitness, SnapshotAvailability,
    };
    let mut store = SqliteStore::open_in_memory().unwrap();
    let (id, origin) = origin(None);
    let work = uuid::Uuid::new_v4();
    let snapshot: crate::WorkItem=serde_json::from_value(json!({
        "schema_version":crate::schema::SCHEMA_VERSION,"project_id":project(),"work_id":work,"root_id":work,
        "short_ref":"snapshot","parent_id":null,"child_requirement":"required","title":"complete snapshot",
        "outcome":"retained governing context","acceptance":["criterion"],"kind":"task","priority":1,
        "labels":[],"assigned_to":null,"deferred_until":null,"origin":"local","source_snapshot_id":null,
        "lifecycle":"open","revision":1,"active_run_id":null,"created_by":actor("fixture"),"created_at":at(1),"updated_at":at(1)
    })).unwrap();
    let revision =
        ObjectId::from_canonical_bytes(&serde_json_canonicalizer::to_vec(&snapshot).unwrap());
    let evaluation_id = ObjectId::mint();
    let run = uuid::Uuid::new_v4();
    let evaluation = envelope(
        &id,
        &evaluation_id,
        RecordKind::AcceptanceEvaluation,
        &json!({
            "schema_version":crate::schema::SCHEMA_VERSION,"project_id":project(),"work_id":work,"root_id":work,
            "run_id":run,"work_revision":1,"work_revision_hash":revision,"criteria":["criterion"],
            "evaluated_cut":{"feed":{"kind":"run_execution","id":run},"position":0},"evidence_basis":[],
            "mode":"same_session","evaluator":actor("fixture"),"verdicts":[],"attempt_key":"historical","created_at":at(1)
        }),
    );
    let basis = GoverningBasis {
        evaluation: evaluation_id,
        project_id: project(),
        work_id: snapshot.work_id,
        work_revision: 1,
        content_fingerprint: revision,
    };
    let witness = GoverningSnapshotWitness {
        basis,
        snapshot: SnapshotAvailability::Exact {
            snapshot: Box::new(snapshot),
            source_snapshot: None,
        },
    };
    retain(
        &mut store,
        &pack(
            &id,
            vec![
                enrollment(&id, &origin),
                evaluation,
                envelope(
                    &id,
                    &ObjectId::mint(),
                    RecordKind::GoverningSnapshotWitness,
                    &witness,
                ),
            ],
        ),
    );
    let before = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
    let mut wrong = witness;
    if let SnapshotAvailability::Exact { snapshot, .. } = &mut wrong.snapshot {
        snapshot.title = "different complete content".into();
    }
    assert!(
        store
            .retain_evidence_pack(
                &project(),
                &pack(
                    &id,
                    vec![envelope(
                        &id,
                        &ObjectId::mint(),
                        RecordKind::GoverningSnapshotWitness,
                        &wrong
                    )]
                ),
                &ObjectId::mint()
            )
            .is_err()
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).unwrap(),
        before
    );
    healthy(&store);
}
