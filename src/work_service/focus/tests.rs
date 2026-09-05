use super::super::test_support::*;
use super::super::*;
use crate::domain::{GATE_EVIDENCE_SUMMARY, SCHEMA_VERSION};
use tempfile::tempdir;

#[test]
fn child_lifecycle_priority_keeps_every_unfinished_state_first() {
    assert_eq!(child_lifecycle_priority(WorkLifecycle::Open), 0);
    assert_eq!(child_lifecycle_priority(WorkLifecycle::Proposed), 0);
    assert_eq!(child_lifecycle_priority(WorkLifecycle::Completed), 1);
    assert_eq!(child_lifecycle_priority(WorkLifecycle::Cancelled), 1);
    assert_eq!(child_lifecycle_priority(WorkLifecycle::Superseded), 1);
}

#[test]
fn obligation_waiver_projection_names_asserted_attribution_not_authority() {
    for waived_by in ["shell-operator", "bound-host-session"] {
        let (kind, summary) = obligation_resolution_change_summary(
            "required-check",
            &WorkObligationResolution::Waived {
                waived_by: waived_by.into(),
                reason: "explicit exception".into(),
            },
        );
        assert_eq!(kind, "obligation_waived");
        assert_eq!(
            summary,
            format!("required-check waiver attributed to {waived_by}")
        );
        assert!(!summary.contains("authority"));
    }
}

#[test]
fn gate_evidence_projection_uses_bounded_words() {
    let gate = crate::GateEvidenceRecord {
        schema_version: crate::domain::SCHEMA_VERSION,
        name: "cargo-test".into(),
        passed: false,
        failed: ["suite::first", "suite::second", "suite::third"]
            .map(String::from)
            .to_vec(),
        previous: None,
    };
    let evidence = WorkEvidence {
        schema_version: crate::domain::SCHEMA_VERSION,
        work_id: WorkId(uuid::Uuid::from_u128(1)),
        run_id: WorkRunId(uuid::Uuid::from_u128(2)),
        claim_id: crate::WorkClaimId(uuid::Uuid::from_u128(3)),
        claim_fence: 1,
        summary: GATE_EVIDENCE_SUMMARY.into(),
        refs: Vec::new(),
        gate: Some(gate),
        actor: ActorContext {
            actor_id: "agent".into(),
            actor_kind: "agent".into(),
            assurance: AssuranceLevel::Asserted,
            run_id: None,
            session_id: Some(SessionId("session".into())),
            source_tool: Some("gate".into()),
            source_skill: None,
            provenance_chain: Vec::new(),
            reason: "test gate projection".into(),
        },
        created_at: DateTime::<Utc>::UNIX_EPOCH,
    };

    assert_eq!(
        compact_work_evidence(&evidence).expect("typed gate projection"),
        "gate cargo-test failed (3 failures): suite::first, suite::second (+1 more)"
    );
    let mut generic = evidence;
    generic.gate = None;
    assert_eq!(
        compact_work_evidence(&generic).expect("generic evidence projection"),
        generic.summary
    );
    let mut invalid = generic;
    invalid.gate = Some(crate::GateEvidenceRecord {
        schema_version: SCHEMA_VERSION,
        name: "cargo-test".into(),
        passed: true,
        failed: vec!["suite::failed".into()],
        previous: None,
    });
    assert!(matches!(
        compact_work_evidence(&invalid),
        Err(StoreError::InvalidWorkProjection(_))
    ));
}

#[test]
fn failing_gate_evidence_does_not_create_a_completion_barrier() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let service = LocalWorkService::new(
        database.clone(),
        ProjectId("failing-gate-completion".into()),
        "agent".into(),
        SessionId("failing-gate-session".into()),
        Some("protocol-test".into()),
    );
    let work = proposed_root(
        service
            .work_propose(
                root_input("Failed gate is evidence", "failed-gate-root"),
                at(0),
            )
            .expect("root proposal"),
    );
    service
        .work_focus(&work.short_ref, at(1))
        .expect("focus root");
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "failed-gate-claim".into(),
            },
            at(2),
        )
        .expect("claim root");
    let gate = service
        .work_gate(
            "cargo-test",
            &["suite::failure".into()],
            Some("test:failing-gate"),
            at(3),
        )
        .expect("record failing gate");
    let gate_evidence =
        serde_json::from_value::<ObjectHash>(gate.receipt.result).expect("gate evidence hash");

    let completed = service
        .work_complete(
            completion_input(
                "completion remains independent from gate result",
                "failed-gate-completion",
            ),
            at(4),
        )
        .expect("failing gate does not block completion");
    let WorkCompleteResult::Completed(completed) = completed else {
        panic!("failing gate evidence must not create a completion refusal");
    };
    let store = SqliteStore::open(&database).expect("store");
    let evidence = store
        .get::<WorkEvidence>(&gate_evidence)
        .expect("read failing gate evidence")
        .expect("canonical failing gate evidence");
    let gate = evidence.gate.expect("typed gate evidence");
    assert!(!gate.passed);
    assert_eq!(gate.failed, ["suite::failure"]);
    let seal = store
        .get::<CompletionSeal>(&completed.seal)
        .expect("read completion seal")
        .expect("canonical completion seal");
    assert!(seal.evidence.contains(&gate_evidence));
    assert!(seal.obligations.is_empty());
    assert!(seal.waivers.is_empty());
}

#[test]
fn obligation_page_keeps_open_items_first_under_count_trimming() {
    let mut records = Vec::new();
    for identity in 1..=5_i64 {
        records.push(obligation_record(
            identity,
            WorkObligationState::Open,
            10 - identity,
            None,
            0,
        ));
    }
    for identity in 6..=12_i64 {
        records.push(obligation_record(
            identity,
            WorkObligationState::Satisfied,
            identity,
            Some(identity),
            0,
        ));
    }
    records.reverse();

    let page = work_obligation_page_from_records(records).expect("bounded obligation page");
    assert!(page.items.len() <= MAX_FOCUS_RELATIONS);
    assert_eq!(page.omitted_count, 12 - page.items.len());
    assert!(
        page.items[..5]
            .iter()
            .all(|item| item.state == WorkObligationState::Open)
    );
    assert!(
        page.items[5..]
            .iter()
            .all(|item| item.state == WorkObligationState::Satisfied)
    );
    let open_ids = page.items[..5]
        .iter()
        .map(|item| item.obligation_id.0.as_u128())
        .collect::<Vec<_>>();
    assert_eq!(open_ids, vec![5, 4, 3, 2, 1]);
    let terminal_ids = page.items[5..]
        .iter()
        .map(|item| item.obligation_id.0.as_u128())
        .collect::<Vec<_>>();
    let expected_terminal = (6_u128..=12)
        .rev()
        .take(page.items.len() - 5)
        .collect::<Vec<_>>();
    assert_eq!(terminal_ids, expected_terminal);
}

#[test]
fn obligation_page_keeps_every_open_item_that_fits_under_byte_trimming() {
    let mut records = (1..=4_i64)
        .map(|identity| obligation_record(identity, WorkObligationState::Open, identity, None, 0))
        .chain((5..=8_i64).map(|identity| {
            obligation_record(
                identity,
                WorkObligationState::Waived,
                identity,
                Some(identity),
                3_000,
            )
        }))
        .collect::<Vec<_>>();
    let expected =
        work_obligation_page_from_records(records.clone()).expect("byte-bounded obligation page");
    records.reverse();
    let reversed = work_obligation_page_from_records(records)
        .expect("deterministic byte-bounded obligation page");

    assert_eq!(
        serde_json::to_vec(&expected).expect("serialize expected page"),
        serde_json::to_vec(&reversed).expect("serialize reversed page")
    );
    assert!(serde_json::to_vec(&expected).unwrap().len() <= MAX_OBLIGATION_PAGE_BYTES);
    assert!(expected.omitted_count > 0);
    assert_eq!(expected.omitted_count, 8 - expected.items.len());
    assert!(
        expected.items[..4]
            .iter()
            .all(|item| item.state == WorkObligationState::Open)
    );
    assert_eq!(
        expected.items[..4]
            .iter()
            .map(|item| item.obligation_id.0.as_u128())
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
}

#[test]
fn focus_evidence_keeps_required_environment_and_verification_closure() {
    for prefix in ["fixture-a", "fixture-b"] {
        let hash =
            |label: &str| ObjectHash::from_canonical_bytes(format!("{prefix}:{label}").as_bytes());
        let required = hash("required-environment");
        let environment_a = hash("environment-a");
        let environment_b = hash("environment-b");
        let mut candidates = vec![
            WorkEvidenceProjectionSummary {
                hash: required.clone(),
                kind: WorkEvidenceKind::Environment,
                environment: None,
            },
            WorkEvidenceProjectionSummary {
                hash: environment_a.clone(),
                kind: WorkEvidenceKind::Environment,
                environment: None,
            },
            WorkEvidenceProjectionSummary {
                hash: environment_b.clone(),
                kind: WorkEvidenceKind::Environment,
                environment: None,
            },
            WorkEvidenceProjectionSummary {
                hash: hash("verification-a"),
                kind: WorkEvidenceKind::Verification,
                environment: Some(environment_a.clone()),
            },
            WorkEvidenceProjectionSummary {
                hash: hash("verification-b"),
                kind: WorkEvidenceKind::Verification,
                environment: Some(environment_b.clone()),
            },
            WorkEvidenceProjectionSummary {
                hash: hash("verification-without-environment"),
                kind: WorkEvidenceKind::Verification,
                environment: None,
            },
        ];
        candidates.extend((0..6).map(|index| WorkEvidenceProjectionSummary {
            hash: hash(&format!("generic-{index}")),
            kind: WorkEvidenceKind::Generic,
            environment: None,
        }));
        let expected =
            prioritized_focus_evidence_hashes(candidates.clone(), vec![required.clone()]);
        candidates.reverse();
        let reversed =
            prioritized_focus_evidence_hashes(candidates.clone(), vec![required.clone()]);

        assert_eq!(expected, reversed);
        assert_eq!(expected.len(), MAX_FOCUS_RELATIONS);
        assert_eq!(expected.first(), Some(&required));
        for candidate in candidates
            .iter()
            .filter(|candidate| candidate.kind == WorkEvidenceKind::Verification)
        {
            let Some(verification_index) = expected.iter().position(|hash| hash == &candidate.hash)
            else {
                continue;
            };
            if let Some(environment) = candidate.environment.as_ref() {
                let environment_index = expected
                    .iter()
                    .position(|hash| hash == environment)
                    .expect("visible verification retains its environment");
                assert!(environment_index < verification_index);
            }
        }
    }
}

#[test]
fn focus_evidence_prioritizes_environments_from_the_visible_obligation_page() {
    let environment_hash = |identity: i64| {
        let value = if identity <= 8 {
            100 + identity
        } else {
            identity - 8
        };
        ObjectHash::from_stored(format!("{value:064x}")).expect("valid environment hash")
    };
    let count_records = (1..=10_i64)
        .rev()
        .map(|identity| obligation_record(identity, WorkObligationState::Open, identity, None, 0))
        .collect::<Vec<_>>();
    let mut count_page = count_bounded_work_obligation_page(count_records);
    assert_eq!(count_page.items.len(), MAX_FOCUS_RELATIONS);
    assert_eq!(count_page.omitted_count, 2);
    for item in &mut count_page.items {
        item.requirement.required_environment = Some(environment_hash(
            i64::try_from(item.obligation_id.0.as_u128()).expect("small fixture identity"),
        ));
    }

    let mut byte_records = (1..=10_i64)
        .map(|identity| {
            let mut record =
                obligation_record(identity, WorkObligationState::Open, identity, None, 0);
            record.obligation.requirement.required_environment = Some(environment_hash(identity));
            record
        })
        .collect::<Vec<_>>();
    byte_records.reverse();
    let byte_page = work_obligation_page_from_records(byte_records).expect("byte-bounded page");
    assert!(byte_page.items.len() < MAX_FOCUS_RELATIONS);
    assert_eq!(byte_page.omitted_count, 10 - byte_page.items.len());

    let candidates = (1..=10_i64)
        .rev()
        .map(|identity| WorkEvidenceProjectionSummary {
            hash: environment_hash(identity),
            kind: WorkEvidenceKind::Environment,
            environment: None,
        })
        .collect::<Vec<_>>();
    let selected = prioritized_focus_evidence(candidates.clone(), &count_page);
    for visible in &count_page.items {
        let required = visible
            .requirement
            .required_environment
            .as_ref()
            .expect("visible obligation requires an environment");
        assert!(selected.contains(required));
    }
    assert!(!selected.contains(&environment_hash(9)));
    assert!(!selected.contains(&environment_hash(10)));

    let selected_after_byte_trim = prioritized_focus_evidence(candidates, &byte_page);
    for visible in &byte_page.items {
        let required = visible
            .requirement
            .required_environment
            .as_ref()
            .expect("visible obligation requires an environment");
        assert!(selected_after_byte_trim.contains(required));
    }
}

#[test]
fn prerequisite_summary_preserves_states_and_public_omission_reasons() {
    let actor = ActorContext {
        actor_id: "agent".into(),
        actor_kind: "test_agent".into(),
        assurance: AssuranceLevel::Asserted,
        run_id: None,
        session_id: Some(SessionId("session".into())),
        source_tool: Some("test".into()),
        source_skill: None,
        provenance_chain: Vec::new(),
        reason: "test prerequisite summary translation".into(),
    };
    let item = |index: u128| {
        let work_id = WorkId(uuid::Uuid::from_u128(index));
        WorkItem {
            schema_version: SCHEMA_VERSION,
            project_id: ProjectId("prerequisite-summary".into()),
            work_id,
            short_ref: format!("w-{index:012x}"),
            root_id: work_id,
            parent_id: None,
            child_requirement: ChildRequirement::Optional,
            title: format!("Prerequisite {index}"),
            outcome: "Translated prerequisite".into(),
            acceptance: Vec::new(),
            kind: WorkItemKind::Task,
            priority: 2,
            labels: Vec::new(),
            assigned_to: None,
            deferred_until: None,
            origin: WorkOrigin::Local,
            source_snapshot_id: None,
            lifecycle: WorkLifecycle::Open,
            revision: 1,
            active_run_id: None,
            restored: false,
            superseded_by: None,
            created_by: actor.clone(),
            created_at: at(0),
            updated_at: at(0),
        }
    };
    let states = [
        WorkPrerequisiteState::Dead,
        WorkPrerequisiteState::Pending,
        WorkPrerequisiteState::Satisfied,
    ];
    let prerequisites = states
        .into_iter()
        .enumerate()
        .map(|(index, state)| (item(index as u128), state))
        .collect();
    let (summaries, omissions) = bounded_prerequisite_summaries(prerequisites, [3, 2, 1]);

    assert_eq!(
        summaries
            .iter()
            .map(|summary| summary.prerequisite_state)
            .collect::<Vec<_>>(),
        states.map(Some)
    );
    assert_eq!(
        omissions
            .iter()
            .map(|omission| (omission.reason, omission.omitted_count))
            .collect::<Vec<_>>(),
        vec![
            (WorkSectionOmissionReason::DeadPrerequisiteCountLimit, 3),
            (WorkSectionOmissionReason::PendingPrerequisiteCountLimit, 2,),
            (
                WorkSectionOmissionReason::SatisfiedPrerequisiteCountLimit,
                1,
            ),
        ]
    );
}

#[test]
fn execution_observation_has_a_compact_agent_work_projection() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("execution-observation-projection".into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("session".into()),
        Some("protocol-test".into()),
    );
    let work = match service
        .work_propose(root_input("Observe execution", "root"), at(0))
        .expect("root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    let run_id = work.active_run_id.expect("active run");
    let observation = ExecutionObservation {
        schema_version: SCHEMA_VERSION,
        project_id: project.clone(),
        binding: crate::ControlWorkBinding {
            root_execution_id: crate::RootExecutionId::new(),
            work_id: work.work_id,
            run_id,
            work_revision: work.revision,
            claim_id: crate::WorkClaimId::new(),
            claim_fence: 1,
        },
        session_id: SessionId("session".into()),
        grant_id: "grant".into(),
        observation_id: "observation".into(),
        action_fingerprint: ObjectHash::from_canonical_bytes(b"write source"),
        effect: crate::EffectClass::MutateLocal,
        outcome: crate::ExecutionOutcome::Succeeded,
        source_changed: true,
        obligation_rule_set: ObjectHash::from_canonical_bytes(b"obligation-rule-set"),
        source_basis: Some(crate::ExecutionSourceBasis {
            workspace_id: "workspace-a".into(),
            source_revision: "revision-a".into(),
        }),
        observed_at: Some(at(1)),
        actor: ActorContext {
            actor_id: "host".into(),
            actor_kind: "host".into(),
            assurance: AssuranceLevel::Asserted,
            run_id: Some(run_id.0.to_string()),
            session_id: Some(SessionId("session".into())),
            source_tool: Some("host-control:turn_checkpoint".into()),
            source_skill: None,
            provenance_chain: Vec::new(),
            reason: "record execution fact".into(),
        },
        recorded_at: at(2),
    };
    let store = SqliteStore::open(database).expect("store");
    let projection = agent_change_object(
        &store,
        &project,
        Some(work.root_id),
        None,
        "execution_observation",
        serde_json::to_value(observation).expect("observation json"),
    )
    .expect("agent projection");
    let WorkChangeProjection::Visible(summary) = projection else {
        panic!("execution observation must remain visible");
    };
    assert_eq!(summary.work_id, Some(work.work_id));
    assert_eq!(summary.change_kind, "execution_observation");
    assert!(summary.summary.contains("MutateLocal Succeeded"));
    assert!(!summary.summary.contains("write source"));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one canonical event fixture exercises the complete agent projection boundary"
)]
fn work_event_projection_does_not_expose_transition_fences_or_hashes() {
    let work_id = WorkId(uuid::Uuid::from_u128(11));
    let run_id = WorkRunId(uuid::Uuid::from_u128(12));
    let claim_id = crate::WorkClaimId(uuid::Uuid::from_u128(13));
    let actor = ActorContext {
        actor_id: "agent".into(),
        actor_kind: "agent".into(),
        assurance: AssuranceLevel::Asserted,
        run_id: Some(run_id.0.to_string()),
        session_id: Some(SessionId("projection-session".into())),
        source_tool: Some("test".into()),
        source_skill: None,
        provenance_chain: Vec::new(),
        reason: "pin the agent projection boundary".into(),
    };
    let work = WorkItem {
        schema_version: SCHEMA_VERSION,
        project_id: ProjectId("projection-project".into()),
        work_id,
        short_ref: "w-projection".into(),
        root_id: work_id,
        parent_id: None,
        child_requirement: ChildRequirement::Required,
        title: "Projection boundary".into(),
        outcome: "No transition secrets".into(),
        acceptance: vec!["Only compact fields are visible".into()],
        kind: WorkItemKind::Task,
        priority: 1,
        labels: Vec::new(),
        assigned_to: None,
        deferred_until: None,
        origin: WorkOrigin::Local,
        source_snapshot_id: None,
        lifecycle: WorkLifecycle::Open,
        revision: 1,
        active_run_id: Some(run_id),
        restored: false,
        superseded_by: None,
        created_by: actor.clone(),
        created_at: at(0),
        updated_at: at(0),
    };
    let claim = WorkClaim {
        claim_id,
        work_id,
        run_id,
        accepted_work_revision: 1,
        holder: SessionId("local-process-42-123e4567-e89b-42d3-a456-426614174000".into()),
        expires_at: at(60),
        revision: 1,
        fence: 77,
        state: WorkClaimState::Active,
    };
    let mut event = WorkEvent {
        schema_version: SCHEMA_VERSION,
        project_id: work.project_id.clone(),
        root_id: work_id,
        work_id,
        run_id: Some(run_id),
        revision: 1,
        work,
        run: None,
        root_execution: None,
        claim: Some(claim.clone()),
        handoff_offer: None,
        blocker: None,
        relation_fingerprint: ObjectHash::from_canonical_bytes(b"relations"),
        transition: WorkTransition::Claimed {
            claim: claim.clone(),
            recovered: false,
        },
        actor,
        created_at: at(1),
    };
    let claimed_summary = agent_work_event_summary(&event);
    assert_eq!(
        claimed_summary.summary,
        "claimed: by a session: \"Projection boundary\""
    );
    let claimed = serde_json::to_string(&claimed_summary).expect("serialize claimed summary");
    assert!(!claimed.contains(&claim_id.0.to_string()));
    assert!(!claimed.contains("123e4567-e89b-42d3-a456-426614174000"));
    assert!(!claimed.contains("\"fence\""));

    let checkpoint = ObjectHash::from_canonical_bytes(b"private-checkpoint-marker");
    let offer = ObjectHash::from_canonical_bytes(b"private-offer-marker");
    event.transition = WorkTransition::HandoffOffered {
        offer_id: crate::WorkHandoffOfferId(uuid::Uuid::from_u128(14)),
        to: SessionId("next-session".into()),
        checkpoint: checkpoint.clone(),
        offer: offer.clone(),
    };
    let offered_summary = agent_work_event_summary(&event);
    assert_eq!(
        offered_summary.summary,
        "handoff_offered: to another session: \"Projection boundary\""
    );
    let offered = serde_json::to_string(&offered_summary).expect("serialize handoff summary");
    assert!(!offered.contains(&checkpoint.to_string()));
    assert!(!offered.contains(&offer.to_string()));
    assert!(!offered.contains("offer_id"));

    event.work.title = "long title ".repeat(80);
    event.transition = WorkTransition::Claimed {
        claim,
        recovered: true,
    };
    let long_claim = agent_work_event_summary(&event).summary;
    assert!(long_claim.starts_with("claimed: after recovery by a session: \""));
    assert!(long_claim.len() <= MAX_SUMMARY_BYTES);

    event.transition = WorkTransition::TypedEvidenceAdded {
        evidence: ObjectHash::from_canonical_bytes(b"verification"),
        evidence_kind: WorkEvidenceKind::Verification,
    };
    assert!(
        agent_work_event_summary(&event)
            .summary
            .starts_with("typed_evidence_added: verification evidence: \"")
    );

    event.transition = WorkTransition::Disposed {
        lifecycle: WorkLifecycle::Cancelled,
        replacement_id: None,
        reason: "bounded reason ".repeat(40),
    };
    let disposed = agent_work_event_summary(&event).summary;
    assert!(disposed.starts_with("disposed: to cancelled because bounded reason"));
    assert!(disposed.len() <= MAX_SUMMARY_BYTES);
}

#[test]
fn oversized_ready_item_degrades_to_one_progress_making_summary() {
    let work_id = WorkId::new();
    let actor = ActorContext {
        actor_id: "agent".into(),
        actor_kind: "coding_agent".into(),
        assurance: AssuranceLevel::Asserted,
        run_id: None,
        session_id: Some(SessionId("session".into())),
        source_tool: Some("test".into()),
        source_skill: None,
        provenance_chain: Vec::new(),
        reason: "exercise bounded ready delivery".into(),
    };
    let work = WorkItem {
        schema_version: SCHEMA_VERSION,
        project_id: ProjectId("bounded-prefix".into()),
        work_id,
        short_ref: "bounded-ref".into(),
        root_id: work_id,
        parent_id: None,
        child_requirement: ChildRequirement::Required,
        title: "x".repeat(1_000),
        outcome: "x".repeat(1_000),
        acceptance: (0..MAX_ACCEPTANCE_ITEMS)
            .map(|_| "x".repeat(1_000))
            .collect(),
        kind: WorkItemKind::Task,
        priority: 1,
        labels: (0..MAX_LABEL_ITEMS).map(|_| "x".repeat(1_000)).collect(),
        assigned_to: Some("x".repeat(1_000)),
        deferred_until: None,
        origin: WorkOrigin::Local,
        source_snapshot_id: None,
        lifecycle: WorkLifecycle::Open,
        revision: 1,
        active_run_id: None,
        restored: false,
        superseded_by: None,
        created_by: actor,
        created_at: at(0),
        updated_at: at(0),
    };
    let source = vec![ReadyWorkSummary {
        work: work_item_summary(&work),
        availability: WorkAvailability::Ready,
        reason_codes: Vec::new(),
        why: vec!["x".repeat(1_000); MAX_FOCUS_RELATIONS],
        blocked_by: vec![WorkId::new(); MAX_FOCUS_RELATIONS],
        blocker_count: MAX_FOCUS_RELATIONS,
    }];
    assert!(serde_json::to_vec(&source).expect("serialize source").len() > MAX_READY_SECTION_BYTES);

    let bounded = bounded_ready_prefix(source, MAX_READY_SECTION_BYTES)
        .expect("degrade oversized ready summary");
    assert_eq!(bounded.len(), 1);
    assert!(
        serde_json::to_vec(&bounded)
            .expect("serialize bounded")
            .len()
            <= MAX_READY_SECTION_BYTES
    );
}

#[test]
fn focus_exposes_blocker_ids_and_single_blocker_unblock_is_ambient() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("ambient-blockers".into());
    let service = LocalWorkService::new(
        database,
        project,
        "agent".into(),
        SessionId("session".into()),
        Some("protocol-test".into()),
    );
    let root = match service
        .work_propose(root_input("Resolve blockers", "root"), at(0))
        .expect("root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    for (key, detail) in [
        ("block-a", "first ".repeat(200)),
        ("block-b", "second ".repeat(200)),
    ] {
        service
            .work_update(
                WorkUpdateInput::Block {
                    blocker_kind: WorkBlockerKind::ExternalInput,
                    detail,
                    idempotency_key: key.into(),
                },
                at(1),
            )
            .expect("block work");
    }
    let focus = service
        .work_focus(&root.short_ref, at(2))
        .expect("blocked focus");
    assert_eq!(focus.blockers.len(), 2);
    assert!(
        focus
            .blockers
            .iter()
            .all(|blocker| !blocker.blocker_id.is_empty())
    );
    assert!(
        serde_json::to_vec(&focus).expect("serialize focus").len() <= MAX_AGENT_WORK_RESPONSE_BYTES
    );

    let ambiguous = service.work_update(
        WorkUpdateInput::Unblock {
            blocker_id: None,
            idempotency_key: "ambiguous-unblock".into(),
        },
        at(3),
    );
    assert!(matches!(ambiguous, Err(StoreError::InvalidWork(_))));
    service
        .work_update(
            WorkUpdateInput::Unblock {
                blocker_id: Some(focus.blockers[0].blocker_id.clone()),
                idempotency_key: "explicit-unblock".into(),
            },
            at(4),
        )
        .expect("explicit unblock");
    service
        .work_update(
            WorkUpdateInput::Unblock {
                blocker_id: None,
                idempotency_key: "ambient-unblock".into(),
            },
            at(5),
        )
        .expect("infer sole blocker");
    assert!(
        service
            .work_focus(&root.short_ref, at(6))
            .expect("unblocked focus")
            .blockers
            .is_empty()
    );
}

#[test]
fn select_work_sets_focus_for_the_next_mutation() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("select-work".into());
    let service = LocalWorkService::new(
        database,
        project,
        "agent".into(),
        SessionId("select-work-session".into()),
        Some("protocol-test".into()),
    );
    let first = match service
        .work_propose(root_input("Select first", "select-first"), at(0))
        .expect("first root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    service
        .work_propose(root_input("Select second", "select-second"), at(1))
        .expect("second root becomes focus");
    service
        .select_work(&first.short_ref, at(2))
        .expect("select the first root by short ref");
    let claimed = service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "select-claim".into(),
            },
            at(3),
        )
        .expect("claim the selected root");
    assert_eq!(claimed.receipt.work_id, first.work_id);
    assert!(matches!(
        service.select_work("no-such-ref", at(4)),
        Err(StoreError::WorkNotFound(_) | StoreError::InvalidWork(_))
    ));
}

#[test]
fn allowed_next_distinguishes_ordinary_claim_from_attributed_recovery() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("claim-recovery-guidance".into());
    let first = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("first-holder".into()),
        Some("protocol-test".into()),
    );
    let successor = LocalWorkService::new(
        database,
        project,
        "agent".into(),
        SessionId("successor".into()),
        Some("protocol-test".into()),
    );
    let root = match first
        .work_propose(
            root_input("Recovery guidance", "recovery-guidance-root"),
            at(0),
        )
        .expect("root proposal")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    first
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(2),
                recovery_reason: None,
                idempotency_key: "first-claim".into(),
            },
            at(1),
        )
        .expect("initial claim");

    let live_foreign_guidance = successor
        .work_focus(&root.short_ref, at(2))
        .expect("focus while another session holds the live claim");
    assert_eq!(
        live_foreign_guidance.allowed_next,
        vec!["work_focus", "work_update:note"]
    );

    let guidance = successor
        .work_focus(&root.short_ref, at(4))
        .expect("focus after prior claim expiry");
    assert!(
        guidance
            .allowed_next
            .contains(&"work_update:claim(recovery_reason_required)".into())
    );
    assert!(!guidance.allowed_next.contains(&"work_update:claim".into()));
    successor
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(60),
                recovery_reason: Some("prior executor stopped before checkpointing".into()),
                idempotency_key: "successor-recovery".into(),
            },
            at(4),
        )
        .expect("typed recovery guidance maps to an executable claim");
}

#[test]
fn allowed_next_advertises_plain_claim_without_recovery_for_a_ready_lapsed_holder() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("retake-readiness-guidance".into());
    let service = LocalWorkService::new(
        database,
        project,
        "agent".into(),
        SessionId("retake-readiness-session".into()),
        Some("protocol-test".into()),
    );
    let work = match service
        .work_propose(
            root_input(
                "Retake readiness guidance",
                "retake-readiness-guidance-root",
            ),
            at(0),
        )
        .expect("root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(1),
                recovery_reason: None,
                idempotency_key: "retake-readiness-guidance-claim".into(),
            },
            at(1),
        )
        .expect("claim");
    let store = service.store().expect("store");
    let guidance = service
        .work_guidance(&store, work.work_id, at(3))
        .expect("lapsed holder guidance basis");
    let claim = guidance.claim.as_ref().expect("lapsed holder claim");
    let ready = allowed_next(
        &guidance.status,
        AllowedNextContext {
            claim: Some(claim),
            handoffs: &[],
            session: &service.session_id,
            now: at(3),
            can_waive_required_child: false,
            claim_recovery_required: false,
            completion_capture_ready: true,
            completion_preflight_ready: true,
        },
    );
    let without_claim = vec![
        "work_focus",
        "work_propose:decompose",
        "work_update:add_prerequisite",
        "work_update:block",
        "work_update:cancel",
        "work_update:note",
        "work_update:remove_prerequisite",
        "work_update:revise",
        "work_update:supersede",
        "work_update:unblock",
    ];
    let mut with_claim = without_claim.clone();
    with_claim.push("work_update:claim");
    with_claim.sort_unstable();
    assert_eq!(ready, with_claim);
    for availability in [
        WorkAvailability::Blocked,
        WorkAvailability::Deferred,
        WorkAvailability::Waiting,
    ] {
        let mut status = guidance.status.clone();
        status.availability = availability;
        for claim in [Some(claim), None] {
            let next = allowed_next(
                &status,
                AllowedNextContext {
                    claim,
                    handoffs: &[],
                    session: &service.session_id,
                    now: at(3),
                    can_waive_required_child: false,
                    claim_recovery_required: false,
                    completion_capture_ready: true,
                    completion_preflight_ready: true,
                },
            );
            assert_eq!(next, without_claim);
        }
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the end-to-end regression keeps parent, child-scope, waiver, and refresh assertions in one lifecycle"
)]
fn required_child_waiver_guidance_is_exact_and_carries_an_actionable_child() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("waiver-guidance".into());
    let service = LocalWorkService::new(
        database,
        project,
        "agent".into(),
        SessionId("waiver-session".into()),
        Some("protocol-test".into()),
    );
    let (root, fresh_focus) = match service
        .work_propose(root_input("Waiver guidance", "waiver-root"), at(0))
        .expect("root proposal")
    {
        WorkProposeResult::Root { work, focus } => (work, focus),
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    assert!(
        !fresh_focus
            .allowed_next
            .contains(&"work_update:waive_required_child".into())
    );
    assert!(fresh_focus.waivable_required_children.is_empty());

    let decomposition = service
        .work_propose(
            WorkProposeInput::Decompose {
                children: ["disposed", "open"]
                    .into_iter()
                    .map(|key| WorkChildInput {
                        key: key.into(),
                        title: format!("{key} child"),
                        outcome: format!("{key} outcome"),
                        acceptance: vec![format!("{key} accepted")],
                        requirement: Some(ChildRequirement::Required),
                        kind: Some(WorkItemKind::Task),
                        priority: Some(1),
                        labels: Vec::new(),
                        assigned_to: None,
                        deferred_until: None,
                    })
                    .collect(),
                prerequisites: Vec::new(),
                idempotency_key: "waiver-decompose".into(),
            },
            at(1),
        )
        .expect("decompose root");
    let WorkProposeResult::Decomposition(decomposition) = decomposition else {
        panic!("expected decomposition");
    };
    let disposed = decomposition.children[0].clone();
    service
        .work_focus(&disposed.short_ref, at(2))
        .expect("focus required child");
    service
        .work_update(
            WorkUpdateInput::Cancel {
                reason: "child outcome is deliberately omitted".into(),
                idempotency_key: "cancel-required-child".into(),
            },
            at(3),
        )
        .expect("cancel required child");

    let parent = service
        .work_focus(&root.short_ref, at(4))
        .expect("focus parent with one waivable child");
    assert!(
        parent
            .allowed_next
            .contains(&"work_update:waive_required_child".into())
    );
    assert_eq!(parent.waivable_required_children.len(), 1);
    assert_eq!(
        parent.waivable_required_children[0].work_id,
        disposed.work_id
    );
    assert_eq!(
        parent.waivable_required_children[0].lifecycle,
        WorkLifecycle::Cancelled
    );
    service
        .work_update(
            WorkUpdateInput::WaiveRequiredChild {
                child: disposed.short_ref,
                reason: "the omission is explicit and accepted".into(),
                idempotency_key: "waive-required-child".into(),
            },
            at(5),
        )
        .expect("execute advertised waiver");
    let refreshed = service
        .work_focus(&root.short_ref, at(6))
        .expect("refresh parent after waiver");
    assert!(
        !refreshed
            .allowed_next
            .contains(&"work_update:waive_required_child".into())
    );
    assert!(refreshed.waivable_required_children.is_empty());
}

#[test]
fn focus_bounds_repeated_direct_decomposition_at_the_root_open_work_limit() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let service = LocalWorkService::new(
        database,
        ProjectId("repeated-direct-fanout".into()),
        "agent".into(),
        SessionId("repeated-direct-fanout-session".into()),
        Some("protocol-test".into()),
    );
    let root = proposed_root(
        service
            .work_propose(root_input("Repeated direct fanout", "fanout-root"), at(0))
            .expect("root proposal"),
    );

    for batch in 0..8 {
        let second = 1 + i64::from(batch) * 2;
        service
            .work_focus(&root.short_ref, at(second))
            .expect("refocus current parent revision");
        service
            .work_propose(
                WorkProposeInput::Decompose {
                    children: (0..16)
                        .map(|index| WorkChildInput {
                            key: format!("batch-{batch}-child-{index}"),
                            title: format!("Open child {batch}-{index} {}", "x".repeat(256)),
                            outcome: format!("Open child {batch}-{index} outcome"),
                            acceptance: vec![format!("Open child {batch}-{index} accepted")],
                            requirement: Some(ChildRequirement::Required),
                            kind: Some(WorkItemKind::Task),
                            priority: Some(1),
                            labels: Vec::new(),
                            assigned_to: None,
                            deferred_until: None,
                        })
                        .collect(),
                    prerequisites: Vec::new(),
                    idempotency_key: format!("fanout-batch-{batch}"),
                },
                at(second + 1),
            )
            .expect("add one direct-child batch");
    }

    let focus = service
        .work_focus(&root.short_ref, at(20))
        .expect("bounded focus at the root open-work limit");
    assert_eq!(focus.child_count, 128);
    assert_eq!(focus.children.len(), MAX_FOCUS_RELATIONS);
    assert!(
        focus
            .children
            .iter()
            .all(|child| child.lifecycle == WorkLifecycle::Open)
    );
    assert!(focus.omissions.iter().any(|omission| {
        omission.reason == WorkSectionOmissionReason::UnfinishedChildCountLimit
            && omission.omitted_count == 128 - MAX_FOCUS_RELATIONS
    }));
    assert!(
        focus.omissions.iter().all(|omission| {
            omission.reason != WorkSectionOmissionReason::TerminalChildCountLimit
        })
    );
    assert!(
        serde_json::to_vec(&focus)
            .expect("serialize bounded focus")
            .len()
            <= MAX_AGENT_WORK_RESPONSE_BYTES
    );
}
