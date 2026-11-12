use super::*;
use crate::storage::work::task_sync::reconciliation::holds::held_by_on;

#[tokio::test]
async fn stale_successor_retirement_preserves_the_original_hold_until_normal_resolution() {
    let (mut f, original, old_run) = ended_fixture_in(|home, tree, binding| {
        super::super::super::super::data_bootstrap::real_repository(home, tree, binding);
    });
    let (binding, id) = (f.input.binding.clone(), f.input.binding_id.clone());
    let (request, payload) = resolved(&f, &original);
    let retained = f
        .store
        .retain_task_sync_planning_successor(
            &id,
            &request,
            &payload,
            at(70),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let hold = held_by_on(&f.store.connection, f.selected).unwrap();
    assert_eq!(hold, Some(original.prepared.clone()));
    let item = f.store.get_work_item(f.selected).unwrap();
    let seal: crate::CompletionSeal = f
        .store
        .get(old_run.completion_seal.as_ref().unwrap())
        .unwrap()
        .unwrap();
    let mut late_actor = binding.actor.clone();
    late_actor
        .provenance_chain
        .push(crate::domain::ProvenanceLink {
            relation: crate::domain::ProvenanceRelation::DerivedFrom,
            source: crate::domain::POST_COMPLETION_EVIDENCE_PROVENANCE_SOURCE.into(),
            reference: Some(crate::domain::POST_COMPLETION_EVIDENCE_PROVENANCE_REFERENCE.into()),
        });
    // A late note is supported native movement on completed work; the hold
    // still prevents planning edits and the completed run stays immutable.
    let late = f
        .store
        .record_work_note(
            &crate::domain::RecordWorkNoteRequest {
                status: false,
                work_id: f.selected,
                run_id: old_run.run_id,
                expected_work_revision: item.revision,
                holder: binding.actor.session_id.clone().unwrap(),
                claim_id: seal.claim_id,
                claim_fence: seal.claim_fence,
                summary: "Late native finding after successor retention".into(),
                refs: vec![],
                actor: late_actor,
                idempotency_key: "native-after-successor-retention".into(),
                recorded_at: at(75),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let late_evidence = f.store.get::<crate::WorkEvidence>(&late.evidence).unwrap();
    let before = state(&f.store);
    let error = apply_planning_successor(
        &mut f.store,
        &id,
        &binding,
        &request,
        &payload,
        at(80),
        &DevelopmentNoopRedactor,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("native readset changed"),
        "{error}"
    );
    assert_eq!(state(&f.store), before);
    let mut audit = audit_request(
        &f.store,
        &binding,
        &id,
        CustodySeedSelection::AbandonRetainedConstruction {
            binding: id.clone(),
            family: RetainedConstructionKind::Successor,
            construction_fingerprint: retained.request_fingerprint().unwrap(),
        },
    );
    audit.operation = request.intent.operation;
    audit.key = request.intent.key.clone();
    f.store
        .audit_task_sync_custody(&audit, &binding.actor, at(90))
        .unwrap();
    assert_eq!(held_by_on(&f.store.connection, f.selected).unwrap(), hold);
    assert_eq!(
        f.store.get::<crate::WorkEvidence>(&late.evidence).unwrap(),
        late_evidence
    );
    assert_eq!(
        retention::successor::read(&id, &binding, &request).unwrap(),
        Some(retained)
    );
    assert_eq!(
        crate::storage::work::query::load_work_run(&f.store.connection, old_run.run_id).unwrap(),
        old_run
    );
    assert!(
        apply_planning_successor(
            &mut f.store,
            &id,
            &binding,
            &request,
            &payload,
            at(91),
            &DevelopmentNoopRedactor
        )
        .unwrap_err()
        .to_string()
        .contains("retired")
    );
    let blocked = newer_incoming(&f, "Still held planning", uuid::Uuid::new_v4(), None, false);
    let before = state(&f.store);
    let error = f
        .store
        .retain_task_sync_incoming_delta(&blocked, at(92))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("held by reconciliation candidate"),
        "{error}"
    );
    assert_eq!(state(&f.store), before);
    assert!(!blocked.intent.candidate.directory.exists());
    let (mut fresh, payload) = resolved(&f, &original);
    fresh.intent.key = fresh.intent.operation.to_string();
    apply_planning_successor(
        &mut f.store,
        &id,
        &binding,
        &fresh,
        &payload,
        at(100),
        &DevelopmentNoopRedactor,
    )
    .unwrap();
    assert!(
        held_by_on(&f.store.connection, f.selected)
            .unwrap()
            .is_none()
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
    drop(f.store);
    SqliteStore::repair_rebuildable_projections(&binding.store.database).unwrap();
    let store = SqliteStore::open(&binding.store.database).unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
}
