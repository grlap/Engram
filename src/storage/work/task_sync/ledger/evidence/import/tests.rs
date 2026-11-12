use super::*;
use crate::storage::work::test_support::{actor, at};
use crate::task_sync::{
    delta::OriginQualifiedCut,
    evidence::{EvidencePack, RecordKind},
    ledger::{BINDING_KIND, Direction, EvidencePackSelection, EvidenceRefusal, Transition},
    origin::Enrollment,
};
use crate::{FeedId, FeedPosition};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

fn pack(project: &crate::ProjectId, summary: &str) -> EvidencePack {
    let origin = ObjectId::mint();
    let enrollment = Enrollment {
        project_id: project.clone(),
        predecessor_origin_id: None,
        enrollment_operation_id: uuid::Uuid::new_v4(),
        actor: actor("foreign"),
        enrolled_at: at(1),
    };
    let enrollment = RecordEnvelope::from_canonical(
        origin.clone(),
        origin.clone(),
        RecordKind::ProducingOrigin.as_str(),
        crate::canonical::canonical_bytes(&enrollment).unwrap(),
    )
    .unwrap();
    let fact = RecordEnvelope::from_canonical(
        origin.clone(),
        ObjectId::mint(),
        RecordKind::WorkEvidence.as_str(),
        crate::canonical::canonical_bytes(&json!({
            "schema_version":crate::schema::SCHEMA_VERSION, "work_id":uuid::Uuid::new_v4(),
            "run_id":uuid::Uuid::new_v4(),"claim_id":uuid::Uuid::new_v4(),"claim_fence":1,
            "summary":summary,"refs":[],"actor":actor("foreign"),"created_at":at(1)
        }))
        .unwrap(),
    )
    .unwrap();
    EvidencePack::prepare(
        ObjectId::mint(),
        project.clone(),
        origin,
        vec![enrollment, fact],
    )
    .unwrap()
}

fn input(store: &SqliteStore, packs: Vec<EvidencePack>) -> InspectedEvidence {
    let mut binding = crate::storage::work::task_sync::tests::ledger::binding(
        &std::env::current_dir()
            .unwrap()
            .join("target/evidence-writer-fixture"),
    );
    binding.project = packs[0].project_id().clone();
    let origin = ObjectId::mint();
    binding.store.origin = Some(origin.clone());
    let binding_id = ObjectId::mint();
    let activated_id = ObjectId::mint();
    let activated = Event {
        schema_version: 1,
        binding: binding_id.clone(),
        sequence: 1,
        predecessor: None,
        actor: binding.actor.clone(),
        recorded_at: at(1),
        transition: Transition::Activated,
    };
    SqliteStore::insert_object(
        &store.connection,
        BINDING_KIND,
        &CanonicalObject::identified(&binding_id, &binding).unwrap(),
    )
    .unwrap();
    SqliteStore::insert_object(
        &store.connection,
        EVENT_KIND,
        &CanonicalObject::identified(&activated_id, &activated).unwrap(),
    )
    .unwrap();
    let ledger = super::super::super::Ledger {
        bindings: BTreeMap::from([(binding_id.clone(), binding.clone())]),
        events: BTreeMap::from([(activated_id.clone(), activated.clone())]),
        bytes: 0,
    };
    view::record(&store.connection, &ledger, &activated).unwrap();
    let operation = uuid::Uuid::new_v4();
    let fingerprint = CanonicalObject::freeze(&"fixture carrier")
        .unwrap()
        .key()
        .clone();
    let intent = EvidenceIntent {
        operation,
        key: operation.to_string(),
        direction: Direction::Incoming,
        actor: binding.actor.clone(),
        producer: fingerprint.clone(),
        expected_head: None,
        source: crate::task_sync::ledger::RetainedCommit {
            oid: fingerprint.to_string(),
            tree: fingerprint.to_string(),
            parents: Vec::new(),
            retention_ref: format!("refs/engram/task-sync/exchange/{operation}/commit"),
            raw_bytes_fingerprint: fingerprint,
        },
        local_cut: OriginQualifiedCut {
            binding: binding_id.clone(),
            origin,
            restore_occurrence: None,
            position: FeedPosition {
                feed: FeedId::Project(binding.project.clone()),
                position: 0,
            },
            anchor: None,
        },
        packs: packs
            .iter()
            .map(|pack| {
                let bytes = pack.to_bytes().unwrap();
                (
                    pack.pack_id().clone(),
                    EvidencePackSelection {
                        publishing_origin: pack.publishing_origin().clone(),
                        admission: ObjectId::mint(),
                        content_fingerprint: ObjectId::from_stored(format!(
                            "{:x}",
                            Sha256::digest(&bytes)
                        ))
                        .unwrap(),
                        encoded_bytes: bytes.len(),
                        records: pack.records().len(),
                    },
                )
            })
            .collect(),
    };
    let recipe =
        EvidenceImportRecipe::prepare(&binding_id, &binding, intent, &activated_id, 1, at(2))
            .unwrap();
    InspectedEvidence {
        binding_id,
        binding,
        recipe,
        packs,
    }
}

fn objects(store: &SqliteStore) -> Vec<(String, String, Vec<u8>)> {
    store
        .connection
        .prepare("SELECT object_id,object_kind,canonical_json FROM objects ORDER BY object_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn evidence_writer_admits_cross_pack_origin_context_in_either_delivery_order() {
    let project = crate::ProjectId("evidence-writer".into());
    let original = pack(&project, "context is in another selected pack");
    let origin = original.publishing_origin();
    let enrollment = original
        .records()
        .iter()
        .find(|record| record.kind() == RecordKind::ProducingOrigin)
        .unwrap()
        .clone();
    let fact = original
        .records()
        .iter()
        .find(|record| record.kind() == RecordKind::WorkEvidence)
        .unwrap()
        .clone();
    let facts = EvidencePack::prepare(
        ObjectId::mint(),
        project.clone(),
        origin.clone(),
        vec![fact.clone()],
    )
    .unwrap();
    let context = EvidencePack::prepare(
        ObjectId::mint(),
        project.clone(),
        origin.clone(),
        vec![enrollment.clone()],
    )
    .unwrap();
    let fact_bytes = facts.to_bytes().unwrap();
    let context_bytes = context.to_bytes().unwrap();
    for encoded in [
        [fact_bytes.as_slice(), context_bytes.as_slice()],
        [context_bytes.as_slice(), fact_bytes.as_slice()],
    ] {
        let packs = encoded
            .into_iter()
            .map(|bytes| EvidencePack::read(bytes).unwrap())
            .collect();
        let mut store = SqliteStore::open_in_memory().unwrap();
        let input = input(&store, packs);
        let mut transaction = store.connection.transaction().unwrap();
        let outcome = apply_on(&mut transaction, &input).unwrap();
        transaction.commit().unwrap();
        assert_eq!(outcome.event, input.recipe.outcome);
        assert_eq!(
            store
                .completed_task_sync_evidence(&input.binding_id, input.recipe.intent().unwrap())
                .unwrap(),
            Some(outcome)
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT count(*) FROM work_task_sync_evidence_packs",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            2
        );
        for record in [&enrollment, &fact] {
            let actual: Vec<u8> = store
                .connection
                .query_row(
                    "SELECT canonical_json FROM objects WHERE object_id=?1",
                    [record.object_id().as_str()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(actual, record.canonical_bytes());
        }
        assert!(
            producing_origin::load_on(&store.connection, &project)
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn evidence_writer_commits_inert_records_and_original_outcome_together() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let project = crate::ProjectId("evidence-writer".into());
    let input = input(&store, vec![pack(&project, "inert original")]);
    let feeds: i64 = store
        .connection
        .query_row("SELECT count(*) FROM work_feed_entries", [], |row| {
            row.get(0)
        })
        .unwrap();
    let transaction = store.connection.transaction().unwrap();
    let mut transaction = transaction;
    let outcome = apply_on(&mut transaction, &input).unwrap();
    transaction.commit().unwrap();
    assert_eq!(outcome.event, input.recipe.outcome);
    assert_eq!(
        store
            .completed_task_sync_evidence(&input.binding_id, input.recipe.intent().unwrap())
            .unwrap(),
        Some(outcome)
    );
    for record in input.packs[0].records() {
        let actual: Vec<u8> = store
            .connection
            .query_row(
                "SELECT canonical_json FROM objects WHERE object_id=?1",
                [record.object_id().as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(actual, record.canonical_bytes());
    }
    assert_eq!(
        store
            .connection
            .query_row("SELECT count(*) FROM work_feed_entries", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        feeds
    );
    assert!(
        producing_origin::load_on(&store.connection, &project)
            .unwrap()
            .is_none()
    );
    let mut budget = view::read::ReadBudget::default();
    let tail = view::read::tail_on(
        &store.connection,
        &input.binding_id,
        &input.binding,
        &mut budget,
    )
    .unwrap();
    assert_eq!(tail.id, input.recipe.outcome_id);
    assert!(super::super::super::settled_tail(&tail.event));
}

#[test]
fn late_pack_collision_rolls_back_payload_but_settles_original_request() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let project = crate::ProjectId("evidence-writer".into());
    let first = pack(&project, "must roll back");
    let second = pack(&project, "collision");
    let collision = &second.records()[1];
    SqliteStore::insert_object(
        &store.connection,
        "fixture_native_original",
        &CanonicalObject::identified(collision.object_id(), &"native different bytes").unwrap(),
    )
    .unwrap();
    let input = input(&store, vec![first, second]);
    let before = objects(&store);
    let mut transaction = store.connection.transaction().unwrap();
    let result = apply_on(&mut transaction, &input).unwrap();
    transaction.commit().unwrap();
    assert!(matches!(
        result.event.transition,
        Transition::EvidenceStopped {
            outcome: EvidenceStop::Refused {
                reason: EvidenceRefusal::RecordConflict
            },
            ..
        }
    ));
    let mut after = objects(&store);
    after.retain(|(id, _, _)| {
        id != input.recipe.prepared_id.as_str() && id != input.recipe.outcome_id.as_str()
    });
    assert_eq!(after, before);
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT count(*) FROM work_task_sync_evidence_packs",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .completed_task_sync_evidence(&input.binding_id, input.recipe.intent().unwrap())
            .unwrap(),
        Some(result.clone())
    );
    let recovered = EvidenceImportRecipe::recover(
        input.recipe.prepared_id.clone(),
        input.recipe.prepared.clone(),
        result.outcome.clone(),
        &result.event,
    )
    .unwrap();
    assert_eq!(recovered, input.recipe);
    assert_eq!(
        task_sync::retention::evidence::request_fingerprint(&recovered).unwrap(),
        task_sync::retention::evidence::request_fingerprint(&input.recipe).unwrap()
    );
    let mut changed = input.recipe.intent().unwrap().clone();
    changed.key.push_str("changed");
    assert!(
        store
            .completed_task_sync_evidence(&input.binding_id, &changed)
            .is_err()
    );
}

#[test]
fn sqlite_failure_rolls_back_preparation_and_does_not_claim_a_known_refusal() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let project = crate::ProjectId("evidence-writer".into());
    let input = input(&store, vec![pack(&project, "storage failure")]);
    store.connection.execute_batch("CREATE TEMP TRIGGER refuse_evidence_manifest BEFORE INSERT ON objects WHEN NEW.object_kind='task_sync_evidence_pack_manifest' BEGIN SELECT RAISE(ABORT,'fixture storage refusal'); END;").unwrap();
    let before = objects(&store);
    {
        let mut transaction = store.connection.transaction().unwrap();
        assert!(apply_on(&mut transaction, &input).is_err());
    }
    assert_eq!(objects(&store), before);
    assert!(
        store
            .completed_task_sync_evidence(&input.binding_id, input.recipe.intent().unwrap())
            .unwrap()
            .is_none()
    );
}
