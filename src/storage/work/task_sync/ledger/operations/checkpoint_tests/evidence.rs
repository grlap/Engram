use super::*;
use crate::task_sync::{
    evidence::{EvidencePack, RecordEnvelope, RecordKind},
    exchange::evidence::InspectedEvidence,
    ledger::{
        EvidenceIntent, EvidencePackSelection, EvidenceRefusal, EvidenceStop, RetainedCommit,
    },
};
use std::collections::BTreeMap;

mod capacity;
mod committed;
mod historical;

/// The fetch's retained carrier selection, as it stands before construction.
fn retain_carrier(binding: &Binding, operation: uuid::Uuid, source: &RetainedCommit) {
    let commits = binding
        .repository
        .git_directory
        .join("engram-task-sync-commits");
    std::fs::create_dir_all(&commits).unwrap();
    std::fs::write(
        commits.join(format!("{operation}.json")),
        crate::canonical::canonical_bytes(source).unwrap(),
    )
    .unwrap();
}

fn retained_input(store: &mut SqliteStore, binding: &Binding, id: &ObjectId) -> InspectedEvidence {
    let (intent, pack) = fresh_intent(store, binding, id);
    retain_carrier(binding, intent.operation, &intent.source);
    let recipe = store
        .retain_task_sync_evidence_recipe(id, binding, &intent, at(70))
        .unwrap();
    InspectedEvidence {
        binding_id: id.clone(),
        binding: binding.clone(),
        recipe,
        packs: vec![pack],
    }
}

/// A fresh incoming evidence request whose carrier is not yet retained.
fn fresh_intent(
    store: &SqliteStore,
    binding: &Binding,
    id: &ObjectId,
) -> (EvidenceIntent, EvidencePack) {
    let origin = ObjectId::mint();
    let enrollment = crate::task_sync::origin::Enrollment {
        project_id: binding.project.clone(),
        predecessor_origin_id: None,
        enrollment_operation_id: uuid::Uuid::new_v4(),
        actor: binding.actor.clone(),
        enrolled_at: at(2),
    };
    let record = RecordEnvelope::from_canonical(
        origin.clone(),
        origin.clone(),
        RecordKind::ProducingOrigin.as_str(),
        crate::canonical::canonical_bytes(&enrollment).unwrap(),
    )
    .unwrap();
    let pack = EvidencePack::prepare(
        ObjectId::mint(),
        binding.project.clone(),
        origin.clone(),
        vec![record],
    )
    .unwrap();
    let bytes = pack.to_bytes().unwrap();
    let operation = uuid::Uuid::new_v4();
    let fingerprint = CanonicalObject::freeze(&operation).unwrap().key().clone();
    let source = RetainedCommit {
        oid: fingerprint.to_string(),
        tree: fingerprint.to_string(),
        parents: Vec::new(),
        retention_ref: format!("refs/engram/task-sync/exchange/{operation}/commit"),
        raw_bytes_fingerprint: fingerprint.clone(),
    };
    let cut = producing_origin::cut_on(&store.connection, &binding.project).unwrap();
    let intent = EvidenceIntent {
        operation,
        key: operation.to_string(),
        direction: Direction::Incoming,
        actor: binding.actor.clone(),
        producer: fingerprint,
        expected_head: None,
        source,
        local_cut: OriginQualifiedCut {
            binding: id.clone(),
            origin: binding.store.origin.clone().unwrap(),
            restore_occurrence: None,
            position: FeedPosition {
                feed: FeedId::Project(binding.project.clone()),
                position: cut.position,
            },
            anchor: cut.anchor,
        },
        packs: BTreeMap::from([(
            pack.pack_id().clone(),
            EvidencePackSelection {
                publishing_origin: origin,
                admission: ObjectId::mint(),
                content_fingerprint: {
                    use sha2::{Digest, Sha256};
                    ObjectId::from_stored(format!("{:x}", Sha256::digest(&bytes))).unwrap()
                },
                encoded_bytes: bytes.len(),
                records: pack.records().len(),
            },
        )]),
    };
    (intent, pack)
}

/// Every exchange writer holds the origin reservation for its whole effect, so
/// one in flight may have observed custody before this request's carrier was
/// retained. A fresh construction then refuses with nothing retained instead
/// of fixing a predecessor that writer is about to replace, and once the
/// reservation is free it is constructed at the current tail and applies.
#[test]
fn evidence_construction_refuses_beside_an_in_flight_writer_and_retains_nothing() {
    let (_home, mut store, binding, _) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let (intent, pack) = fresh_intent(&store, &binding, &id);
    let database = std::path::PathBuf::from(store.connection.path().unwrap());
    let in_flight = crate::task_sync::origin::continuity::PreparedContinuity::acquire(
        &database,
        &binding.project,
    )
    .unwrap();
    retain_carrier(&binding, intent.operation, &intent.source);
    let files = retention::inventory(&binding.repository.git_directory).unwrap();
    let before = state(&store);
    let error = store
        .retain_task_sync_evidence_recipe(&id, &binding, &intent, at(70))
        .unwrap_err();
    assert!(error.to_string().contains("busy"), "{error}");
    assert_eq!(
        retention::inventory(&binding.repository.git_directory).unwrap(),
        files
    );
    assert!(
        retention::evidence::read_recipe(&id, &binding, &intent)
            .unwrap()
            .is_none()
    );
    assert_eq!(state(&store), before);
    drop(in_flight);
    let recipe = store
        .retain_task_sync_evidence_recipe(&id, &binding, &intent, at(70))
        .unwrap();
    let result = store
        .apply_task_sync_evidence_import(&InspectedEvidence {
            binding_id: id.clone(),
            binding: binding.clone(),
            recipe: recipe.clone(),
            packs: vec![pack],
        })
        .unwrap();
    assert_eq!(result.outcome, recipe.outcome_id);
    audit_healthy(&mut store, &binding, &id);
}

/// The race's own ordering: another writer observed custody before this
/// request's carrier was retained, and its SQL commit came first. Its later
/// custody observation then refuses, and this request's construction refuses
/// with nothing retained rather than naming the predecessor that writer
/// replaced. This proves prevention after that ordering only; it does not
/// claim the other writer's interrupted private acknowledgement recovers.
#[test]
fn evidence_construction_retains_nothing_after_a_writer_committed_beside_its_carrier() {
    let (_home, mut store, binding, _) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let (intent, _) = fresh_intent(&store, &binding, &id);
    let writer = retained_input(&mut store, &binding, &id);
    let (carrier_binding, operation, source) =
        (binding.clone(), intent.operation, intent.source.clone());
    crate::storage::work::task_sync::ledger::evidence::after_commit::set(move || {
        retain_carrier(&carrier_binding, operation, &source);
    });
    let error = store.apply_task_sync_evidence_import(&writer).unwrap_err();
    assert!(
        store
            .completed_task_sync_evidence(&id, writer.recipe.intent().unwrap())
            .unwrap()
            .is_some(),
        "the writer committed before its custody observation refused: {error}"
    );
    let files = retention::inventory(&binding.repository.git_directory).unwrap();
    let before = state(&store);
    assert!(
        store
            .retain_task_sync_evidence_recipe(&id, &binding, &intent, at(72))
            .is_err()
    );
    assert!(
        retention::evidence::read_recipe(&id, &binding, &intent)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        retention::inventory(&binding.repository.git_directory).unwrap(),
        files
    );
    assert_eq!(state(&store), before);
}

/// Construction fixes its predecessor only when it is retained, so a request
/// selected after another writer advanced the binding names that writer's
/// settled outcome and applies, rather than a predecessor read earlier.
#[test]
fn evidence_construction_takes_the_tail_another_writer_advanced() {
    let (_home, mut store, binding, _) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let (intent, pack) = fresh_intent(&store, &binding, &id);
    let earlier = retained_input(&mut store, &binding, &id);
    let advanced = store.apply_task_sync_evidence_import(&earlier).unwrap();
    retain_carrier(&binding, intent.operation, &intent.source);
    let recipe = store
        .retain_task_sync_evidence_recipe(&id, &binding, &intent, at(71))
        .unwrap();
    assert_eq!(
        recipe.prepared.predecessor.as_ref(),
        Some(&advanced.outcome)
    );
    let result = store
        .apply_task_sync_evidence_import(&InspectedEvidence {
            binding_id: id.clone(),
            binding: binding.clone(),
            recipe,
            packs: vec![pack],
        })
        .unwrap();
    assert!(matches!(
        result.event.transition,
        Transition::EvidenceImportApplied { .. }
    ));
    audit_healthy(&mut store, &binding, &id);
}

#[test]
fn retained_evidence_applies_or_stops_after_unrelated_feed_movement() {
    for stop in [false, true] {
        let (_home, mut store, binding, _) = initial_fixture();
        let id = store.activate_task_sync_binding(&binding).unwrap();
        let input = retained_input(&mut store, &binding, &id);
        let inventory = retention::inventory(&binding.repository.git_directory).unwrap();
        unrelated(&mut store, &binding.project);
        let result = if stop {
            store
                .stop_task_sync_evidence_import(
                    &input,
                    &EvidenceStop::Refused {
                        reason: EvidenceRefusal::Dependency,
                    },
                )
                .unwrap()
        } else {
            store.apply_task_sync_evidence_import(&input).unwrap()
        };
        assert_eq!(result.prepared, input.recipe.prepared_id);
        assert_eq!(result.outcome, input.recipe.outcome_id);
        assert_eq!(
            store
                .completed_task_sync_evidence(&id, input.recipe.intent().unwrap())
                .unwrap(),
            Some(result)
        );
        assert_eq!(
            retention::inventory(&binding.repository.git_directory).unwrap(),
            inventory
        );
        audit_healthy(&mut store, &binding, &id);
    }
}

/// A settled evidence outcome, applied or stopped, admits an otherwise valid
/// legacy incoming preparation through the common settlement contract, while
/// that preparation, still unresolved, refuses the next new exchange.
#[test]
fn legacy_incoming_preparation_follows_a_settled_evidence_outcome() {
    for stop in [false, true] {
        let (_home, mut store, binding, tree) = initial_fixture();
        let id = store.activate_task_sync_binding(&binding).unwrap();
        let input = retained_input(&mut store, &binding, &id);
        let result = if stop {
            store
                .stop_task_sync_evidence_import(
                    &input,
                    &EvidenceStop::Refused {
                        reason: EvidenceRefusal::InvalidPayload,
                    },
                )
                .unwrap()
        } else {
            store.apply_task_sync_evidence_import(&input).unwrap()
        };
        assert!(
            matches!(
                result.event.transition,
                Transition::EvidenceImportApplied { .. } | Transition::EvidenceStopped { .. }
            ),
            "{stop}"
        );
        let next = super::super::tests::input(&store, &id, &binding, tree.clone());
        store.prepare_task_sync_exchange(&next, at(80)).unwrap();
        let other = super::super::tests::input(&store, &id, &binding, tree);
        let error = store
            .prepare_task_sync_exchange(&other, at(81))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("new exchange bypasses unresolved operation"),
            "{error}"
        );
    }
}

#[test]
fn evidence_custody_audit_refuses_changed_carrier_without_replacing_it() {
    let (_home, mut store, binding, _) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let input = retained_input(&mut store, &binding, &id);
    store.apply_task_sync_evidence_import(&input).unwrap();
    let intent = input.recipe.intent().unwrap();
    let mut changed = intent.source.clone();
    changed.tree = CanonicalObject::freeze(&"different evidence carrier tree")
        .unwrap()
        .key()
        .to_string();
    let path = binding
        .repository
        .git_directory
        .join("engram-task-sync-commits")
        .join(format!("{}.json", intent.operation));
    let bytes = crate::canonical::canonical_bytes(&changed).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    let before = state(&store);
    let ledger = Ledger::load(&store.connection).unwrap();
    let roots = std::collections::BTreeSet::from([binding.repository.git_directory.clone()]);
    let error = super::super::super::audit::artifacts(&store.connection, &ledger, &roots, None)
        .unwrap_err();
    assert!(
        error.to_string().contains("evidence carrier differs"),
        "{error}"
    );
    assert_eq!(state(&store), before);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}
