use super::*;
use crate::task_sync::ledger::{EvidenceIntent, EvidencePackSelection, EvidenceReceipt};

#[test]
fn refused_evidence_replays_fixed_terminal_and_releases_global_pending() {
    use crate::task_sync::ledger::{EvidenceImportRecipe, EvidenceRefusal, EvidenceStop};
    let mut fixture = Fixture::new(Direction::Incoming);
    let intent = selection(&fixture);
    let (previous_id, previous) = fixture.chain.last().unwrap();
    let recipe = EvidenceImportRecipe::prepare(
        &fixture.id,
        &fixture.binding,
        intent.clone(),
        previous_id,
        previous.sequence,
        previous.recorded_at,
    )
    .unwrap();
    let stopped = recipe
        .stopped_event(EvidenceStop::Refused {
            reason: EvidenceRefusal::Dependency,
        })
        .unwrap();
    fixture
        .chain
        .push((recipe.prepared_id.clone(), recipe.prepared.clone()));
    fixture
        .chain
        .push((recipe.outcome_id.clone(), stopped.clone()));
    let reduced = fixture.reduced().unwrap();
    assert!(reduced.head.pending.is_none());
    assert!(reduced.head.last_incoming.is_none());
    assert!(reduced.head.last_outgoing.is_none());
    assert_eq!(
        reduced.operations[&intent.operation.to_string()]
            .completed
            .as_ref(),
        Some(&recipe.outcome_id)
    );
    assert!(recipe.matches_outcome(&stopped).unwrap());
    let mut changed = stopped.clone();
    changed.recorded_at += chrono::Duration::seconds(1);
    assert!(!recipe.matches_outcome(&changed).unwrap());
    let recovered = EvidenceImportRecipe::recover(
        recipe.prepared_id.clone(),
        recipe.prepared.clone(),
        recipe.outcome_id.clone(),
        &stopped,
    )
    .unwrap();
    assert_eq!(recovered, recipe);
    fixture.prepare();
    assert_eq!(fixture.reduced().unwrap().head.stage, Stage::Prepared);
}

#[test]
fn oversized_evidence_terminal_requires_actual_profile_excess_and_own_family() {
    use crate::task_sync::{evidence::limits, ledger::EvidenceStop};
    let stopped = EvidenceStop::Oversize {
        detail: limits::Oversize {
            kind: limits::LimitKind::ComponentTooLarge,
            subject: Some(ObjectId::mint()),
            dimension: limits::Dimension::Objects,
            actual: limits::MAX_COMPONENT_OBJECTS + 1,
            limit: limits::MAX_COMPONENT_OBJECTS,
        },
    };
    let mut fixture = Fixture::new(Direction::Incoming);
    let intent = selection(&fixture);
    let prepared = fixture.push(Transition::EvidencePrepared {
        intent: Box::new(intent),
    });
    fixture.push(Transition::EvidenceStopped {
        prepared,
        outcome: stopped.clone(),
    });
    assert_eq!(fixture.reduced().unwrap().head.stage, Stage::Settled);
    if let Transition::EvidenceStopped {
        outcome: EvidenceStop::Oversize { detail },
        ..
    } = &mut fixture.chain.last_mut().unwrap().1.transition
    {
        detail.actual = detail.limit;
    }
    assert!(fixture.reduced().is_err());
    let mut fixture = Fixture::new(Direction::Incoming);
    let prepared = fixture.prepare();
    fixture.push(Transition::EvidenceStopped {
        prepared,
        outcome: stopped,
    });
    assert!(fixture.reduced().is_err());
}

#[test]
fn evidence_recipe_retention_preserves_original_ids_and_partial_uncertainty() {
    use crate::task_sync::{ledger::EvidenceImportRecipe, retention};
    let directory = crate::test_support::temp_home().unwrap();
    let mut fixture = Fixture::new(Direction::Incoming);
    fixture.binding.repository.git_directory = directory.path().join("git-admin");
    std::fs::create_dir_all(&fixture.binding.repository.git_directory).unwrap();
    let intent = selection(&fixture);
    let (tail_id, tail) = fixture.chain.last().unwrap();
    let recipe = EvidenceImportRecipe::prepare(
        &fixture.id,
        &fixture.binding,
        intent.clone(),
        tail_id,
        tail.sequence,
        tail.recorded_at,
    )
    .unwrap();
    retention::evidence::retain_recipe(&fixture.id, &fixture.binding, &recipe).unwrap();
    retention::evidence::retain_recipe(&fixture.id, &fixture.binding, &recipe).unwrap();
    assert_eq!(
        retention::evidence::read_recipe(&fixture.id, &fixture.binding, &intent).unwrap(),
        Some(recipe.clone())
    );
    assert_eq!(
        retention::inventory(&fixture.binding.repository.git_directory)
            .unwrap()
            .evidence_imports[&intent.operation],
        recipe
    );
    let path = fixture
        .binding
        .repository
        .git_directory
        .join("engram-task-sync-evidence-imports")
        .join(format!("{}.json", intent.operation));
    std::fs::write(&path, b"{").unwrap();
    assert!(retention::evidence::read_recipe(&fixture.id, &fixture.binding, &intent).is_err());
    assert!(retention::evidence::retain_recipe(&fixture.id, &fixture.binding, &recipe).is_err());
    assert_eq!(std::fs::read(path).unwrap(), b"{");
}

#[test]
fn evidence_recipe_keeps_exact_event_identities_and_refuses_spliced_retries() {
    use crate::task_sync::ledger::EvidenceImportRecipe;
    let fixture = Fixture::new(Direction::Incoming);
    let intent = selection(&fixture);
    let (tail_id, tail) = fixture.chain.last().unwrap();
    let recipe = EvidenceImportRecipe::prepare(
        &fixture.id,
        &fixture.binding,
        intent.clone(),
        tail_id,
        tail.sequence,
        tail.recorded_at,
    )
    .unwrap();
    let bytes = crate::canonical::canonical_bytes(&recipe).unwrap();
    let replay: EvidenceImportRecipe = serde_json::from_slice(&bytes).unwrap();
    replay.validate(&fixture.id, &fixture.binding).unwrap();
    assert_eq!(recipe, replay);
    assert_eq!(replay.intent().unwrap(), &intent);
    let mut changed = replay.clone();
    changed.outcome.predecessor = Some(ObjectId::mint());
    assert!(changed.validate(&fixture.id, &fixture.binding).is_err());
    let mut changed = replay.clone();
    changed.outcome.actor.reason = "different attribution".into();
    assert!(changed.validate(&fixture.id, &fixture.binding).is_err());
    let mut changed = replay;
    changed.outcome_id = changed.prepared_id.clone();
    assert!(changed.validate(&fixture.id, &fixture.binding).is_err());
}

#[test]
fn evidence_recipe_refuses_exhausted_chain_before_returning_construction_bytes() {
    use crate::task_sync::ledger::EvidenceImportRecipe;
    let fixture = Fixture::new(Direction::Incoming);
    let (tail_id, tail) = fixture.chain.last().unwrap();
    assert!(
        EvidenceImportRecipe::prepare(
            &fixture.id,
            &fixture.binding,
            selection(&fixture),
            tail_id,
            u64::MAX - 1,
            tail.recorded_at
        )
        .is_err()
    );
}

pub(super) fn selection(fixture: &Fixture) -> EvidenceIntent {
    let operation = uuid::Uuid::new_v4();
    EvidenceIntent {
        operation,
        key: operation.to_string(),
        direction: Direction::Incoming,
        actor: fixture.binding.actor.clone(),
        producer: comparison("evidence encoder"),
        expected_head: None,
        source: commit(operation, "evidence Git carrier"),
        local_cut: fixture.intent.local_cut.clone(),
        packs: BTreeMap::from([(
            ObjectId::mint(),
            EvidencePackSelection {
                publishing_origin: ObjectId::mint(),
                admission: ObjectId::mint(),
                content_fingerprint: comparison("exact encoded pack"),
                encoded_bytes: 120,
                records: 2,
            },
        )]),
    }
}

fn applied(fixture: &mut Fixture, prepared: ObjectId, intent: &EvidenceIntent) -> ObjectId {
    fixture.push(Transition::EvidenceImportApplied {
        prepared,
        receipt: EvidenceReceipt {
            admissions: intent
                .packs
                .iter()
                .map(|(id, pack)| (id.clone(), pack.admission.clone()))
                .collect(),
        },
    })
}

#[test]
fn evidence_family_uses_global_pending_slot_without_advancing_planning_heads() {
    let mut fixture = Fixture::new(Direction::Incoming);
    let intent = selection(&fixture);
    let prepared = fixture.push(Transition::EvidencePrepared {
        intent: Box::new(intent.clone()),
    });
    let reduced = fixture.reduced().unwrap();
    assert_eq!(reduced.head.pending.as_ref(), Some(&prepared));
    assert_eq!(reduced.head.stage, Stage::Prepared);
    let unfinished = fixture.chain.clone();
    fixture.prepare();
    assert!(fixture.reduced().is_err());
    fixture.chain = unfinished;
    let outcome = applied(&mut fixture, prepared.clone(), &intent);
    let reduced = fixture.reduced().unwrap();
    assert!(reduced.head.pending.is_none());
    assert!(reduced.head.last_incoming.is_none());
    assert!(reduced.head.last_outgoing.is_none());
    let row = &reduced.operations[&intent.operation.to_string()];
    assert_eq!(row.prepared, prepared);
    assert_eq!(row.completed.as_ref(), Some(&outcome));
    fixture.prepare();
    assert_eq!(fixture.reduced().unwrap().head.stage, Stage::Prepared);
}

#[test]
fn evidence_family_cannot_reuse_a_planning_key_or_settle_another_family() {
    let mut fixture = Fixture::new(Direction::Incoming);
    let mut intent = selection(&fixture);
    intent.key.clone_from(&fixture.intent.key);
    let prepared = fixture.push(Transition::EvidencePrepared {
        intent: Box::new(intent.clone()),
    });
    applied(&mut fixture, prepared, &intent);
    fixture.prepare();
    assert!(fixture.reduced().is_err());

    let mut fixture = Fixture::new(Direction::Incoming);
    let prepared = fixture.prepare();
    let intent = selection(&fixture);
    applied(&mut fixture, prepared, &intent);
    assert!(fixture.reduced().is_err());
}

#[test]
fn evidence_receipt_requires_exact_prepared_admission_ids_and_actor() {
    let mut fixture = Fixture::new(Direction::Incoming);
    let intent = selection(&fixture);
    let prepared = fixture.push(Transition::EvidencePrepared {
        intent: Box::new(intent.clone()),
    });
    let mut changed = intent.clone();
    changed.packs.values_mut().next().unwrap().admission = ObjectId::mint();
    applied(&mut fixture, prepared.clone(), &changed);
    assert!(fixture.reduced().is_err());
    fixture.chain.pop();
    applied(&mut fixture, prepared, &intent);
    fixture
        .chain
        .last_mut()
        .unwrap()
        .1
        .actor
        .actor_id
        .push_str("-other");
    assert!(fixture.reduced().is_err());
}

#[test]
fn evidence_preparation_refuses_shared_ids_foreign_cuts_and_oversized_metadata() {
    let fixture = Fixture::new(Direction::Incoming);
    let intent = selection(&fixture);
    intent.validate(&fixture.id, &fixture.binding).unwrap();
    let mut changed = intent.clone();
    let id = changed.packs.keys().next().unwrap().clone();
    changed.packs.get_mut(&id).unwrap().admission = id.clone();
    assert!(changed.validate(&fixture.id, &fixture.binding).is_err());
    changed = intent.clone();
    changed.local_cut.origin = ObjectId::mint();
    assert!(changed.validate(&fixture.id, &fixture.binding).is_err());
    changed = intent;
    changed.packs.get_mut(&id).unwrap().encoded_bytes =
        crate::task_sync::evidence::limits::MAX_PACK_BYTES + 1;
    assert!(changed.validate(&fixture.id, &fixture.binding).is_err());
}

#[test]
fn evidence_completed_lookup_replays_original_metadata_without_pack_files() {
    let store = crate::storage::SqliteStore::open_in_memory().unwrap();
    let mut fixture = Fixture::new(Direction::Incoming);
    let intent = selection(&fixture);
    let prepared = fixture.push(Transition::EvidencePrepared {
        intent: Box::new(intent.clone()),
    });
    let outcome = applied(&mut fixture, prepared.clone(), &intent);
    let object = CanonicalObject::identified(&fixture.id, &fixture.binding).unwrap();
    crate::storage::SqliteStore::insert_object(
        &store.connection,
        super::super::super::BINDING_KIND,
        &object,
    )
    .unwrap();
    for (id, event) in &fixture.chain {
        let object = CanonicalObject::identified(id, event).unwrap();
        crate::storage::SqliteStore::insert_object(
            &store.connection,
            super::super::super::EVENT_KIND,
            &object,
        )
        .unwrap();
    }
    let ledger = Ledger {
        bindings: BTreeMap::from([(fixture.id.clone(), fixture.binding.clone())]),
        events: fixture.chain.iter().cloned().collect(),
        bytes: 0,
    };
    view::record(&store.connection, &ledger, &fixture.chain.last().unwrap().1).unwrap();
    // Deliberately no pack payloads: historical lookup checks its selected
    // canonical operation, while cold canonical validation checks provenance.
    let replay = store
        .completed_task_sync_evidence(&fixture.id, &intent)
        .unwrap()
        .unwrap();
    assert_eq!(replay.prepared, prepared);
    assert_eq!(replay.outcome, outcome);
    assert!(Ledger::load(&store.connection).is_err());
    let mut changed = intent.clone();
    changed.actor.actor_id.push_str("-other");
    assert!(
        store
            .completed_task_sync_evidence(&fixture.id, &changed)
            .is_err()
    );
    store
        .connection
        .execute(
            "DELETE FROM work_task_sync_exchange_operations WHERE binding_id=?1",
            [fixture.id.as_str()],
        )
        .unwrap();
    assert!(
        store
            .completed_task_sync_evidence(&fixture.id, &intent)
            .is_err()
    );
}
