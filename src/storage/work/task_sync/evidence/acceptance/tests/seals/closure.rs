//! Real native completions, followed by a first foreign delivery of altered proof.
use super::*;
use crate::CanonicalObject;

pub(super) fn complete_origin(
    origin: &mut Fixture,
    root: &std::path::Path,
) -> (ObjectId, CompletionSeal) {
    origin.report_source(root, 5, true);
    let receipt = origin.record(10);
    let work = origin.store.get_work_item(origin.binding.work_id).unwrap();
    let held = origin.store.claim_fixture(work.work_id);
    let check = &receipt.verifications[0].record_id;
    checkpoint(
        &mut origin.store,
        &work,
        &held,
        "runner",
        "closure-checkpoint",
        12,
        std::slice::from_ref(check),
    );
    let mut request = completion_request(&work, &held, "runner", check, "closure-complete", 15);
    request.source_id = Some(source(root));
    let seal = origin
        .store
        .complete_work(&request, &DevelopmentNoopRedactor)
        .unwrap();
    let id = origin
        .store
        .get_work_run(seal.run_id)
        .unwrap()
        .completion_seal
        .unwrap();
    (id, seal)
}

fn request(
    target: &Fixture,
    origin: ObjectId,
    seal: ObjectId,
    cuts: Vec<ObjectId>,
) -> AcceptanceUseRequest {
    AcceptanceUseRequest {
        decision: ObjectId::mint(),
        origin,
        proof: HistoricalProof::NativeCompletion {
            seal,
            governing_witness: None,
        },
        origin_cuts: cuts,
        binding: target.binding.clone(),
        verification_decisions: vec![],
    }
}

fn altered<T: serde::Serialize>(record: &RecordEnvelope, value: &T) -> RecordEnvelope {
    RecordEnvelope::from_canonical(
        record.origin().clone(),
        record.object_id().clone(),
        record.kind().as_str(),
        serde_json_canonicalizer::to_vec(value).unwrap(),
    )
    .unwrap()
}

#[test]
fn native_seal_rechecks_rule_trigger_matcher_and_exact_terminal_accounting() {
    for fault in [
        "none",
        "fingerprint",
        "rule",
        "trigger",
        "missing-binding",
        "foreign-list",
    ] {
        let root = repository();
        let mut origin = Fixture::with_bindings(root.path(), false);
        let mut target = destination(&mut origin, root.path());
        target.observe(root.path(), 16);
        let (seal_id, seal) = complete_origin(&mut origin, root.path());
        assert_ne!(seal.obligations.len(), 0);
        let (original, origin_id, cuts) = seal_pack(&origin, &seal_id, &seal, false);
        let records = original
            .records()
            .iter()
            .map(|record| {
                if fault == "fingerprint" && record.kind() == RecordKind::VerificationEvidence {
                    let mut check: VerificationEvidence =
                        serde_json::from_slice(record.canonical_bytes()).unwrap();
                    check.check_fingerprint = CanonicalObject::freeze(&check.check_fingerprint)
                        .unwrap()
                        .key()
                        .clone();
                    return altered(record, &check);
                }
                if matches!(fault, "rule" | "trigger")
                    && record.kind() == RecordKind::WorkObligation
                {
                    let mut definition: WorkObligation =
                        serde_json::from_slice(record.canonical_bytes()).unwrap();
                    if fault == "rule" {
                        definition.rule.rule_version += 1;
                    } else {
                        definition.opened_at += chrono::Duration::seconds(1);
                    }
                    return altered(record, &definition);
                }
                if record.object_id() == &seal_id
                    && matches!(fault, "missing-binding" | "foreign-list")
                {
                    let mut seal = seal.clone();
                    if fault == "missing-binding" {
                        seal.obligations.pop();
                    } else {
                        seal.foreign_workspace_changes
                            .push(seal.obligations[0].definition.clone());
                    }
                    return altered(record, &seal);
                }
                record.clone()
            })
            .collect();
        let incoming = pack(&origin_id, records);
        retain(&mut target.store, &incoming);
        let request = request(&target, origin_id, seal_id.clone(), cuts);
        let before =
            crate::storage::test_database_shape_snapshot(&target.store.connection).unwrap();
        let actor = target.local_actor();
        match target.store.associate_imported_acceptance(
            &project(),
            &session(),
            &actor,
            &request,
            at(22),
        ) {
            Ok(id) => {
                let summary = target
                    .store
                    .imported_acceptance_summary(&id, at(23))
                    .unwrap();
                assert_eq!(
                    summary.currently_eligible,
                    fault == "none",
                    "fault={fault}: {summary:?}"
                );
                let again = target
                    .store
                    .associate_imported_acceptance(&project(), &session(), &actor, &request, at(24))
                    .unwrap();
                assert_eq!(id, again);
            }
            Err(error) => {
                assert_ne!(fault, "none", "{error:?}");
                assert_eq!(
                    before,
                    crate::storage::test_database_shape_snapshot(&target.store.connection).unwrap()
                );
            }
        }
        assert!(
            !target
                .store
                .get_work_run(target.binding.run_id)
                .unwrap()
                .is_finished()
        );
        let (_, retained) = object_on(&target.store.connection, &seal_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            retained,
            incoming
                .records()
                .iter()
                .find(|record| record.object_id() == &seal_id)
                .unwrap()
                .canonical_bytes()
        );
        healthy(&origin.store);
    }
}

#[test]
fn required_child_waiver_is_checked_against_its_original_event_and_child_set() {
    use crate::storage::work::test_support::child;
    for missing in [false, true] {
        let root = repository();
        let mut origin = Fixture::with_bindings(root.path(), false);
        let work = origin.store.get_work_item(origin.binding.work_id).unwrap();
        let children = origin
            .store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: work.work_id,
                    expected_parent_revision: work.revision,
                    children: vec![child(
                        "omitted",
                        ChildRequirement::Required,
                        "Deliberately omitted child",
                    )],
                    prerequisites: vec![],
                    authority: WorkPlanningAuthority::Claim {
                        run_id: origin.binding.run_id,
                        holder: session(),
                        claim_id: origin.binding.claim_id,
                        claim_fence: origin.binding.claim_fence,
                    },
                    actor: actor("runner"),
                    idempotency_key: "closure-child".into(),
                    created_at: at(4),
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        origin.binding.work_revision = children.parent.revision;
        origin.request.association.binding = origin.binding.clone();
        let child = &children.children[0];
        origin
            .store
            .dispose_work(
                &DisposeWorkRequest {
                    work_id: child.work_id,
                    expected_work_revision: child.revision,
                    disposition: WorkDisposition::Cancelled,
                    replacement_id: None,
                    reason: "deliberately omit this part".into(),
                    actor: actor("planner"),
                    idempotency_key: "closure-cancel".into(),
                    disposed_at: at(4),
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        origin
            .store
            .waive_required_child(
                &WaiveRequiredChildRequest {
                    parent_id: children.parent.work_id,
                    child_id: child.work_id,
                    expected_parent_revision: children.parent.revision,
                    reason: "explicitly accepted omission".into(),
                    actor: actor("planner"),
                    idempotency_key: "closure-waiver".into(),
                    waived_at: at(4),
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        let mut target = destination(&mut origin, root.path());
        target.observe(root.path(), 16);
        let (seal_id, seal) = complete_origin(&mut origin, root.path());
        assert_eq!(seal.required_child_waivers.len(), 1);
        let (original, origin_id, cuts) = seal_pack(&origin, &seal_id, &seal, false);
        let records = original
            .records()
            .iter()
            .map(|record| {
                if missing && record.object_id() == &seal_id {
                    let mut omitted = seal.clone();
                    omitted.required_child_waivers.clear();
                    altered(record, &omitted)
                } else {
                    record.clone()
                }
            })
            .collect();
        retain(&mut target.store, &pack(&origin_id, records));
        let request = request(&target, origin_id, seal_id, cuts);
        let actor = target.local_actor();
        let id = target
            .store
            .associate_imported_acceptance(&project(), &session(), &actor, &request, at(22))
            .unwrap();
        let summary = target
            .store
            .imported_acceptance_summary(&id, at(23))
            .unwrap();
        assert_eq!(summary.currently_eligible, !missing, "{summary:?}");
        assert!(
            !target
                .store
                .get_work_run(target.binding.run_id)
                .unwrap()
                .is_finished()
        );
    }
}

#[test]
fn completed_required_child_is_checked_recursively_and_cannot_be_omitted() {
    use crate::storage::work::test_support::{child, complete, evidence};
    for missing in [false, true] {
        let root = repository();
        let mut origin = Fixture::with_bindings(root.path(), false);
        let work = origin.store.get_work_item(origin.binding.work_id).unwrap();
        let children = origin
            .store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: work.work_id,
                    expected_parent_revision: work.revision,
                    children: vec![child(
                        "finished",
                        ChildRequirement::Required,
                        "Required finished child",
                    )],
                    prerequisites: vec![],
                    authority: WorkPlanningAuthority::Claim {
                        run_id: origin.binding.run_id,
                        holder: session(),
                        claim_id: origin.binding.claim_id,
                        claim_fence: origin.binding.claim_fence,
                    },
                    actor: actor("runner"),
                    idempotency_key: "completed-child".into(),
                    created_at: at(4),
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        origin.binding.work_revision = children.parent.revision;
        origin.request.association.binding = origin.binding.clone();
        let child = &children.children[0];
        let held = claim(
            &mut origin.store,
            child,
            "child-runner",
            "child-claim",
            4,
            3600,
        );
        let proof = evidence(
            &mut origin.store,
            child,
            &held,
            "child-runner",
            "child-note",
            4,
        );
        checkpoint(
            &mut origin.store,
            child,
            &held,
            "child-runner",
            "child-checkpoint",
            4,
            std::slice::from_ref(&proof),
        );
        complete(
            &mut origin.store,
            child,
            &held,
            "child-runner",
            &proof,
            "child-complete",
            4,
        )
        .unwrap();
        let mut target = destination(&mut origin, root.path());
        target.observe(root.path(), 16);
        let (seal_id, seal) = complete_origin(&mut origin, root.path());
        assert_eq!(seal.required_child_seals.len(), 1);
        let (original, origin_id, cuts) = seal_pack(&origin, &seal_id, &seal, false);
        let records = original
            .records()
            .iter()
            .map(|record| {
                if missing && record.object_id() == &seal_id {
                    let mut omitted = seal.clone();
                    omitted.required_child_seals.clear();
                    altered(record, &omitted)
                } else {
                    record.clone()
                }
            })
            .collect();
        retain(&mut target.store, &pack(&origin_id, records));
        let request = request(&target, origin_id, seal_id, cuts);
        let actor = target.local_actor();
        let id = target
            .store
            .associate_imported_acceptance(&project(), &session(), &actor, &request, at(22))
            .unwrap();
        let summary = target
            .store
            .imported_acceptance_summary(&id, at(23))
            .unwrap();
        assert_eq!(summary.currently_eligible, !missing, "{summary:?}");
        assert!(
            !target
                .store
                .get_work_run(target.binding.run_id)
                .unwrap()
                .is_finished()
        );
    }
}
