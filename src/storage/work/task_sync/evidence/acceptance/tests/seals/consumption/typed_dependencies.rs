use super::*;
use crate::storage::test_database_shape_snapshot;
use crate::storage::work::task_sync::evidence::test_support::{
    capture_feeds_roundtrip, capture_roundtrip, dependency_refusals,
};
use crate::task_sync::evidence::{EvidenceError, inventory};

fn original<'a>(packs: &'a [EvidencePack], id: &ObjectId) -> &'a RecordEnvelope {
    packs
        .iter()
        .flat_map(EvidencePack::records)
        .find(|record| record.object_id() == id)
        .unwrap()
}

fn retain_original_context(data: &mut Selected) {
    // The selected-use fixture packs feed witnesses, but not every transitive
    // context object. Onward native capture requires that complete inventory.
    let origin = &data.verification.origin;
    let mut pending = data
        .target
        .store
        .connection
        .prepare(
            "SELECT DISTINCT member.object_id
             FROM work_task_sync_evidence_members member
             JOIN work_task_sync_evidence_packs pack ON pack.pack_id=member.pack_id
             WHERE pack.project_id=?1 AND member.origin_id=?2",
        )
        .unwrap()
        .query_map([project().0.as_str(), origin.as_str()], |row| {
            row.get::<_, String>(0)
        })
        .unwrap()
        .map(|row| ObjectId::from_stored(row.unwrap()).unwrap())
        .collect::<Vec<_>>();
    let mut records = std::collections::BTreeMap::new();
    while let Some(id) = pending.pop() {
        if records.contains_key(&id) {
            continue;
        }
        let (kind, bytes) = object_on(&data.target.store.connection, &id)
            .unwrap()
            .or_else(|| object_on(&data.origin.store.connection, &id).unwrap())
            .unwrap_or_else(|| panic!("original producer context is missing: {id}"));
        let record =
            RecordEnvelope::from_canonical(origin.clone(), id.clone(), &kind, bytes).unwrap();
        pending.extend(
            inventory::dependencies(&record)
                .unwrap()
                .into_iter()
                .map(|dependency| dependency.object_id),
        );
        records.insert(id, record);
    }
    let complete = pack(origin, records.into_values().collect());
    inventory::plan_batch(std::slice::from_ref(&complete), |_| Ok(None)).unwrap();
    retain(&mut data.target.store, &complete);
}

fn kind_refusal(
    packs: &[EvidencePack],
    changed: &RecordEnvelope,
    body: &impl serde::Serialize,
    dependency: &ObjectId,
) {
    let replacement = RecordEnvelope::from_canonical(
        changed.origin().clone(),
        changed.object_id().clone(),
        changed.kind().as_str(),
        serde_json_canonicalizer::to_vec(body).unwrap(),
    )
    .unwrap();
    let altered = packs
        .iter()
        .map(|pack| {
            EvidencePack::prepare(
                ObjectId::mint(),
                pack.project_id().clone(),
                pack.publishing_origin().clone(),
                pack.records()
                    .iter()
                    .map(|record| {
                        if record.object_id() == changed.object_id() {
                            replacement.clone()
                        } else {
                            record.clone()
                        }
                    })
                    .collect(),
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    let mut destination = SqliteStore::open_in_memory().unwrap();
    let before = test_database_shape_snapshot(&destination.connection).unwrap();
    // Incoming publication plans the complete typed inventory before retention.
    let result = inventory::plan_batch(&altered, |_| Ok(None)).and_then(|_| {
        let admissions = altered.iter().map(|_| ObjectId::mint()).collect::<Vec<_>>();
        destination
            .retain_evidence_batch(
                &project(),
                &altered.iter().zip(&admissions).collect::<Vec<_>>(),
            )
            .map_err(|_| EvidenceError::InvalidPack("unexpected retention failure"))
    });
    assert!(
        matches!(result, Err(EvidenceError::Dependency { ref object, problem: "required original kind differs" }) if object == dependency),
        "{result:?}"
    );
    assert_eq!(
        test_database_shape_snapshot(&destination.connection).unwrap(),
        before
    );
}

fn rule_context_refusals(
    packs: &[EvidencePack],
    binding: &crate::storage::WorkObligationRecord,
    resolution: &WorkObligationResolutionEvent,
    consumed: &ConsumptionRequest,
    source: &crate::source::SourceId,
) {
    let stock = packs
        .iter()
        .flat_map(EvidencePack::records)
        .filter(|record| record.kind() == RecordKind::WorkObligation)
        .find_map(|record| {
            let obligation: WorkObligation =
                serde_json::from_slice(record.canonical_bytes()).unwrap();
            crate::control::acceptance_binding_criterion(&obligation.rule)
                .is_none()
                .then_some((record, obligation))
        })
        .unwrap();
    let record = original(packs, &binding.definition_id);
    let mut wrong = binding.obligation.clone();
    wrong.rule = stock.1.rule.clone();
    kind_refusal(packs, record, &wrong, &wrong.triggering_observation);
    let mut wrong = stock.1;
    wrong.rule = binding.obligation.rule.clone();
    kind_refusal(packs, stock.0, &wrong, &wrong.triggering_observation);

    let consumption: crate::task_sync::evidence::consumption::ImportedEvidenceConsumption =
        serde_json::from_slice(original(packs, &consumed.consumption).canonical_bytes()).unwrap();
    let verification = packs
        .iter()
        .flat_map(EvidencePack::records)
        .find(|record| record.kind() == RecordKind::VerificationEvidence)
        .unwrap();
    let mut wrong = resolution.clone();
    wrong.registered_input_decision = Some(RegisteredInputDecision {
        verification: verification.object_id().clone(),
        named_root_binding: consumption.named_root,
        source: RegisteredDecisionSource::SourceId {
            source_id: source.clone(),
        },
        measured_at: at(23),
    });
    kind_refusal(
        packs,
        original(packs, binding.resolution_id.as_ref().unwrap()),
        &wrong,
        &consumed.consumption,
    );
}

#[test]
fn native_obligation_dependencies_preserve_binding_events_and_consumed_proofs() {
    for acceptance in [false, true] {
        let mut data = selected(false);
        retain_original_context(&mut data);
        let decision = if acceptance {
            data.acceptance.decision.clone()
        } else {
            data.verification.decision.clone()
        };
        let consumed = consume(&mut data.target, &decision, acceptance);
        let obligations = data
            .target
            .store
            .work_run_obligations(data.target.binding.run_id)
            .unwrap();
        let binding = obligations
            .iter()
            .find(|record| {
                crate::control::acceptance_binding_criterion(&record.obligation.rule).is_some()
            })
            .unwrap();
        assert_eq!(binding.state, WorkObligationState::Satisfied);
        let resolution = binding.resolution.as_ref().unwrap();
        assert!(resolution.registered_input_decision.is_none());
        assert!(
            matches!(&resolution.resolution, WorkObligationResolution::Satisfied { evidence, .. } if evidence == &consumed.consumption)
        );

        let home = crate::test_support::temp_home().unwrap();
        let database = crate::project_database_path(home.path(), &project());
        std::fs::create_dir_all(database.parent().unwrap()).unwrap();
        data.target
            .store
            .connection
            .execute("VACUUM INTO ?1", [database.to_str().unwrap()])
            .unwrap();
        let mut store = SqliteStore::open(&database).unwrap();
        let packs = capture_roundtrip(
            &mut store,
            &project(),
            data.target.binding.run_id,
            vec![binding.resolution_id.clone().unwrap()],
        );
        assert_eq!(
            original(&packs, &binding.obligation.triggering_observation).kind(),
            RecordKind::WorkEvent
        );
        assert_eq!(
            original(&packs, &consumed.consumption).kind(),
            RecordKind::ImportedConsumption
        );
        assert_eq!(
            original(&packs, &data.verification.original).origin(),
            &data.verification.origin
        );
        assert_ne!(
            original(&packs, &consumed.consumption).origin(),
            &data.verification.origin
        );
        dependency_refusals(&packs, &binding.obligation.triggering_observation);
        dependency_refusals(&packs, &consumed.consumption);
        rule_context_refusals(
            &packs,
            binding,
            resolution,
            &consumed,
            &source(data.root.path()),
        );
        healthy(&data.origin.store);
        healthy(&data.target.store);
    }
}

fn capture_completed_source_change(data: &mut Selected, seal: &ObjectId) -> Vec<EvidencePack> {
    // Native capture preserves the original verification's separate origin and
    // the complete dependency closure of its immutable consumption.
    let home = crate::test_support::temp_home().unwrap();
    let database = crate::project_database_path(home.path(), &project());
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    data.target
        .store
        .connection
        .execute("VACUUM INTO ?1", [database.to_str().unwrap()])
        .unwrap();
    let mut store = SqliteStore::open(&database).unwrap();
    let run = data.target.binding.run_id;
    let root = store
        .get_work_item(data.target.binding.work_id)
        .unwrap()
        .root_id;
    // A seal is a linked object. Its Completed event is the native feed member
    // that seeds capture; both run and root history qualify the onward proof.
    let completed: String = store.connection.query_row(
        "SELECT object_id FROM work_feed_entries WHERE feed_kind='run_execution' AND feed_id=?1 ORDER BY position DESC LIMIT 1",
        [run.0.to_string()], |row| row.get(0),
    ).unwrap();
    let packs = capture_feeds_roundtrip(
        &mut store,
        &project(),
        vec![FeedId::RunExecution(run), FeedId::RootWork(root)],
        vec![ObjectId::from_stored(completed).unwrap()],
    );
    assert_eq!(original(&packs, seal).kind(), RecordKind::CompletionSeal);
    packs
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one real three-host proof is checked before and after a later terminal without rewriting history"
)]
fn onward_source_change_consumption_rejects_a_later_original_terminal_conflict() {
    let mut data = selected_bindings(false, false);
    retain_original_context(&mut data);
    data.target.report_source(data.root.path(), 22, true);
    let consumed = consume(&mut data.target, &data.verification.decision, false);
    let obligations = data
        .target
        .store
        .work_run_obligations(data.target.binding.run_id)
        .unwrap();
    assert_ne!(obligations, []);
    assert!(obligations.iter().all(|binding| {
        crate::control::acceptance_binding_criterion(&binding.obligation.rule).is_none()
            && binding.state == WorkObligationState::Satisfied
            && matches!(&binding.resolution.as_ref().unwrap().resolution,
                WorkObligationResolution::Satisfied { evidence, .. } if evidence == &consumed.consumption)
    }), "the imported check must be needed only by the source-change obligation");
    // Restore open planning before the middle host seals its native run.
    let mut onward = destination_at(&mut data.target, data.root.path(), 31);
    onward.observe(data.root.path(), 36);
    assert_eq!(
        onward
            .store
            .get_work_item(onward.binding.work_id)
            .unwrap()
            .acceptance_bindings,
        []
    );
    let input = completion(
        &mut data.target,
        data.root.path(),
        &consumed.consumption,
        false,
    );
    let seal = data
        .target
        .store
        .complete_work(&input, &DevelopmentNoopRedactor)
        .unwrap();
    let seal_id = data
        .target
        .store
        .get_work_run(seal.run_id)
        .unwrap()
        .completion_seal
        .unwrap();
    let packs = capture_completed_source_change(&mut data, &seal_id);
    let origin = original(&packs, &seal_id).origin().clone();
    let cuts = packs
        .iter()
        .flat_map(EvidencePack::records)
        .filter_map(|record| {
            if record.kind() != RecordKind::OriginFeedCut {
                return None;
            }
            let cut: crate::task_sync::evidence::witnesses::OriginFeedCut =
                serde_json::from_slice(record.canonical_bytes()).unwrap();
            (cut.source.origin == origin).then(|| record.object_id().clone())
        })
        .collect::<Vec<_>>();
    let admissions = packs.iter().map(|_| ObjectId::mint()).collect::<Vec<_>>();
    onward
        .store
        .retain_evidence_batch(
            &project(),
            &packs.iter().zip(&admissions).collect::<Vec<_>>(),
        )
        .unwrap();
    let consumed_bytes = object_on(&onward.store.connection, &consumed.consumption)
        .unwrap()
        .unwrap();
    let seal_bytes = object_on(&onward.store.connection, &seal_id)
        .unwrap()
        .unwrap();
    let mut request = AcceptanceUseRequest {
        decision: ObjectId::mint(),
        origin,
        proof: HistoricalProof::NativeCompletion {
            seal: seal_id.clone(),
            governing_witness: None,
        },
        origin_cuts: cuts,
        binding: onward.binding.clone(),
        verification_decisions: vec![],
    };
    let actor = onward.local_actor();
    let valid = onward
        .store
        .associate_imported_acceptance(&project(), &session(), &actor, &request, at(40))
        .unwrap();
    let initial = onward
        .store
        .imported_acceptance_summary(&valid, at(41))
        .unwrap();
    assert!(
        initial.currently_eligible,
        "initial complete onward proof: {initial:?}"
    );
    healthy(&onward.store);

    // The original execution is unchanged; a new carrier reports an opposing
    // terminal. It belongs to the original verification's origin, not the seal's.
    data.origin.request.operation_key = "late-onward-source-terminal".into();
    data.origin.request.completion.result.verdict = RemoteRunVerdict::Fail;
    let conflicting = data.origin.record(42);
    retain(
        &mut onward.store,
        &pack(
            &data.verification.origin,
            vec![envelope(
                &data.origin.store,
                &data.verification.origin,
                &conflicting.operation,
            )],
        ),
    );
    let (_, _, execution) =
        crate::storage::work::task_sync::evidence::verification::applicability::candidate_facts_on(
            &onward.store.connection,
            &project(),
            &data.verification.origin,
            &data.verification.original,
        )
        .unwrap();
    assert!(
        crate::storage::work::task_sync::evidence::verification::union_on(
            &onward.store.connection,
            &execution.unwrap()
        )
        .unwrap()
        .conflicted
    );
    // Doctor validates what was consumed then, without changing the sealed past.
    healthy(&onward.store);
    crate::storage::work::task_sync::evidence::consumption::load_on(
        &onward.store.connection,
        &consumed.consumption,
    )
    .unwrap();
    assert_eq!(
        object_on(&onward.store.connection, &consumed.consumption)
            .unwrap()
            .unwrap(),
        consumed_bytes
    );
    assert_eq!(
        object_on(&onward.store.connection, &seal_id)
            .unwrap()
            .unwrap(),
        seal_bytes
    );

    request.decision = ObjectId::mint();
    let rejected = onward
        .store
        .associate_imported_acceptance(&project(), &session(), &actor, &request, at(43))
        .unwrap();
    let current = onward
        .store
        .imported_acceptance_summary(&rejected, at(44))
        .unwrap();
    assert!(
        !current.currently_eligible,
        "new onward use must recheck the consumed check's current terminal union: {current:?}"
    );
    assert_eq!(
        current.refusal,
        Some(AcceptanceUseRefusal::IncompleteClosure)
    );
    healthy(&onward.store);
    assert_eq!(
        object_on(&onward.store.connection, &consumed.consumption)
            .unwrap()
            .unwrap(),
        consumed_bytes
    );
    assert_eq!(
        object_on(&onward.store.connection, &seal_id)
            .unwrap()
            .unwrap(),
        seal_bytes
    );
    assert!(
        !onward
            .store
            .get_work_run(onward.binding.run_id)
            .unwrap()
            .is_finished()
    );
}
