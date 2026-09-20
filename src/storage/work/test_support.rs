use super::*;

pub(super) use crate::domain::{
    ActorContext, AssuranceLevel, CheckpointWorkRequest, ChildWorkDraft, ChildWorkPrerequisite,
    CompleteWorkRequest, ControlAssurance, ControlRefusalCode, ControlTurnBeginDecision,
    ControlTurnCheckpointDecision, ControlTurnDecision, CreateWorkRequest, EffectClass,
    EnvironmentComponents, EnvironmentEvidenceInput, EnvironmentEvidenceReference,
    ExecutionObservationInput, ExecutionObservationReference, ExecutionOutcome,
    ExecutionSourceBasis, NoteRequest, NoteVisibility, ProvenanceLink, ProvenanceRelation,
    RecordWorkEvidenceRequest, Scope, Sensitivity, TurnIntent, TurnNextIntent, TurnPurpose,
    VerificationEvidenceInput, VerificationEvidenceMismatch, VerificationKind, VerificationResult,
    WorkItemKind, WorkPlanningAuthority, WorkRevisionPatch,
};
pub(super) use crate::memory::DevelopmentNoopRedactor;
pub(super) use crate::storage::test_database_shape_snapshot;
pub(super) use crate::work_service::{
    LocalWorkService, WorkAcceptanceInput, WorkCompleteInput, WorkCompleteResult,
    WorkCompletionCaptureInput,
};
pub(super) use crate::{ProjectId, VerificationEvidenceMatchInput, match_verification_evidence};
pub(super) use chrono::{Duration, TimeZone};

pub(super) fn at(second: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 27, 1, 0, 0)
        .single()
        .expect("fixed test timestamp")
        + Duration::seconds(second)
}

pub(super) fn process_default_session_at(pid: u32, created_at: DateTime<Utc>) -> SessionId {
    let seconds = u64::try_from(created_at.timestamp()).expect("positive test timestamp");
    let timestamp = uuid::Timestamp::from_unix(
        uuid::NoContext,
        seconds,
        created_at.timestamp_subsec_nanos(),
    );
    SessionId(format!(
        "local-process-v1-{pid}-{}",
        uuid::Uuid::new_v7(timestamp)
    ))
}

/// The id of the rule set that the store's active control policy names.
pub(super) fn active_rule_set_id(connection: &Connection) -> ObjectId {
    let policy: String = connection
        .query_row(
            "SELECT policy_id FROM control_policy_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("active control policy");
    let policy = ObjectId::from_stored(policy).expect("stored policy id");
    SqliteStore::obligation_rule_set_for_policy_on(connection, &policy)
        .expect("active obligation rule set")
        .0
}

pub(super) fn restore_savepoint(store: &SqliteStore) {
    store
        .connection
        .execute_batch("ROLLBACK TO corrupt; RELEASE corrupt")
        .expect("restore corruption savepoint");
}

pub(super) fn actor(session: &str) -> ActorContext {
    ActorContext {
        actor_id: session.into(),
        actor_kind: "test_agent".into(),
        assurance: AssuranceLevel::Asserted,
        run_id: None,
        session_id: Some(SessionId(session.into())),
        source_tool: Some("work_test".into()),
        source_skill: None,
        provenance_chain: Vec::<ProvenanceLink>::new(),
        reason: "exercise local work lifecycle".into(),
    }
}

pub(super) fn delegated(_project: &str, _actor_id: &str) -> WorkPlanningAuthority {
    WorkPlanningAuthority::Project
}

pub(super) struct RejectingRedactor;

impl Redactor for RejectingRedactor {
    fn inspect(&self, _prose: &str) -> Result<(), String> {
        Err("test policy refused candidate work content".into())
    }

    fn description(&self) -> &'static str {
        "test rejecting redactor"
    }
}

pub(super) fn root_request(project: &str, key: &str, second: i64) -> CreateWorkRequest {
    CreateWorkRequest {
        acceptance_bindings: Vec::new(),
        evaluation_mode: None,
        external_ref: None,
        notes: Vec::new(),
        project_id: crate::domain::ProjectId(project.into()),
        parent_id: None,
        child_requirement: ChildRequirement::Required,
        title: "Ship local work".into(),
        outcome: "The local work lifecycle operates end to end".into(),
        acceptance: vec!["root accepted".into()],
        kind: WorkItemKind::Feature,
        priority: 1,
        labels: vec!["local-work".into()],
        assigned_to: None,
        deferred_until: None,
        origin: WorkOrigin::Local,
        source_snapshot_id: None,
        actor: actor("planner"),
        idempotency_key: key.into(),
        created_at: at(second),
    }
}

pub(super) fn child(key: &str, requirement: ChildRequirement, title: &str) -> ChildWorkDraft {
    ChildWorkDraft {
        acceptance_bindings: Vec::new(),
        evaluation_mode: None,
        external_ref: None,
        notes: Vec::new(),
        local_key: key.into(),
        child_requirement: requirement,
        title: title.into(),
        outcome: format!("{title} outcome"),
        acceptance: vec![format!("{key} accepted")],
        kind: WorkItemKind::Task,
        priority: 1,
        labels: vec![key.into()],
        assigned_to: None,
        deferred_until: None,
    }
}

// Preserve the caller's revision and run basis: silently refreshing either
// would hide stale fixture state instead of exercising admission's refusal.
pub(super) fn claim(
    store: &mut SqliteStore,
    work: &WorkItem,
    holder: &str,
    key: &str,
    second: i64,
    ttl_seconds: i64,
) -> WorkClaim {
    store
        .claim_work(
            &ClaimWorkRequest {
                work_id: work.work_id,
                expected_work_revision: work.revision,
                expected_run_id: Some(work.active_run_id.expect("active run")),
                holder: SessionId(holder.into()),
                ttl_seconds,
                recovery_reason: Some("recover abandoned test claim".into()),
                actor: actor(holder),
                idempotency_key: key.into(),
                claimed_at: at(second),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("claim work")
}

pub(super) fn checkpoint(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    holder: &str,
    key: &str,
    second: i64,
    evidence: &[ObjectId],
) -> ObjectId {
    store
        .checkpoint_work(
            &CheckpointWorkRequest {
                work_id: work.work_id,
                run_id: claim.run_id,
                expected_work_revision: work.revision,
                holder: SessionId(holder.into()),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                summary: "checkpointed implementation progress".into(),
                evidence: Some(evidence.to_vec()),
                actor: actor(holder),
                idempotency_key: key.into(),
                checkpointed_at: at(second),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("checkpoint work")
}

pub(super) fn evidence(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    holder: &str,
    key: &str,
    second: i64,
) -> ObjectId {
    store
        .record_work_evidence(
            &RecordWorkEvidenceRequest {
                work_id: work.work_id,
                run_id: claim.run_id,
                expected_work_revision: work.revision,
                holder: SessionId(holder.into()),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                summary: "focused validation passed".into(),
                refs: vec!["cargo:test".into()],
                actor: actor(holder),
                idempotency_key: key.into(),
                recorded_at: at(second),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("record evidence")
}

pub(super) fn completion_request(
    work: &WorkItem,
    claim: &WorkClaim,
    holder: &str,
    evidence: &ObjectId,
    key: &str,
    second: i64,
) -> CompleteWorkRequest {
    CompleteWorkRequest {
        work_id: work.work_id,
        run_id: claim.run_id,
        holder: SessionId(holder.into()),
        expected_work_revision: work.revision,
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
        evidence: vec![evidence.clone()],
        acceptance: work
            .acceptance
            .iter()
            .map(|criterion| AcceptanceResult {
                criterion: criterion.clone(),
                satisfied: true,
                evidence: vec![evidence.clone()],
                assurance: AssuranceLevel::Asserted,
                note: "verified".into(),
            })
            .collect(),
        drain: crate::domain::CompletionDrainAttestation {
            reconciled_action_outcomes: Vec::new(),
            released_resource_leases: Vec::new(),
        },
        source_fingerprint: None,
        actor: actor(holder),
        idempotency_key: key.into(),
        completed_at: at(second),
    }
}

pub(super) fn complete(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    holder: &str,
    evidence: &ObjectId,
    key: &str,
    second: i64,
) -> Result<CompletionSeal, StoreError> {
    store.complete_work(
        &completion_request(work, claim, holder, evidence, key, second),
        &DevelopmentNoopRedactor,
    )
}

/// A host-minted verification of `kind` with `result` on the claimed run,
/// with its producer observation and environment record, as the host
/// checkpoint path mints them; no source mutation is observed.
#[allow(
    clippy::too_many_arguments,
    reason = "one test helper mirrors the host verification surface"
)]
pub(super) fn host_verification(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    holder: &str,
    key: &str,
    kind: crate::domain::VerificationKind,
    result: crate::domain::VerificationResult,
    second: i64,
) -> ObjectId {
    host_verification_of(
        store,
        work,
        claim,
        holder,
        key,
        kind,
        result,
        second,
        "revision-as-it-stands",
    )
}

/// `host_verification` of the source at `source_revision`: the content the
/// check ran against, which the anti-stale rule compares with a mutation's.
#[allow(
    clippy::too_many_arguments,
    reason = "one test helper mirrors the host verification surface"
)]
pub(super) fn host_verification_of(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    holder: &str,
    key: &str,
    kind: crate::domain::VerificationKind,
    result: crate::domain::VerificationResult,
    second: i64,
    source_revision: &str,
) -> ObjectId {
    use crate::domain::{
        ControlWorkBinding, EffectClass, EnvironmentComponents, EnvironmentEvidence,
        ExecutionObservation, ExecutionOutcome, ExecutionSourceBasis, VerificationEvidence,
    };
    let run = super::query::load_work_run(&store.connection, claim.run_id).expect("claimed run");
    let binding = ControlWorkBinding {
        root_execution_id: run.root_execution_id,
        work_id: work.work_id,
        run_id: run.run_id,
        work_revision: claim.accepted_work_revision,
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
    };
    let mut run_actor = actor(holder);
    run_actor.run_id = Some(run.run_id.0.to_string());
    let source_basis = ExecutionSourceBasis {
        workspace_id: format!("workspace-{key}"),
        source_revision: source_revision.into(),
    };
    let observation = ExecutionObservation {
        schema_version: SCHEMA_VERSION,
        project_id: work.project_id.clone(),
        binding: binding.clone(),
        session_id: SessionId(holder.into()),
        grant_id: format!("grant-{key}"),
        observation_id: format!("check-{key}"),
        action_fingerprint: check_fingerprint(key),
        effect: EffectClass::Observe,
        outcome: ExecutionOutcome::Succeeded,
        source_changed: false,
        obligation_rule_set: active_rule_set_id(&store.connection),
        source_basis: Some(source_basis.clone()),
        observed_at: Some(at(second)),
        actor: run_actor.clone(),
        recorded_at: at(second),
    };
    let components = EnvironmentComponents {
        toolchain: "rustc-test".into(),
        sandbox: Some("test-sandbox".into()),
        workspace_id: source_basis.workspace_id.clone(),
        capability_map_revision: 1,
    };
    let environment_fingerprint = CanonicalObject::freeze(&components)
        .expect("freeze environment components")
        .key()
        .clone();
    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("verification transaction");
    let producer =
        super::completion::append_control_execution_observation_on(&transaction, &observation)
            .expect("append the check's observation");
    let environment = super::completion::append_control_environment_evidence_on(
        &transaction,
        &EnvironmentEvidence {
            schema_version: SCHEMA_VERSION,
            project_id: work.project_id.clone(),
            binding: binding.clone(),
            session_id: SessionId(holder.into()),
            source_basis: source_basis.clone(),
            environment_fingerprint,
            components: Some(components),
            observed_at: at(second),
            actor: run_actor.clone(),
            recorded_at: at(second),
        },
    )
    .expect("append environment evidence");
    let verification = super::completion::append_control_verification_evidence_on(
        &transaction,
        &VerificationEvidence {
            schema_version: SCHEMA_VERSION,
            project_id: work.project_id.clone(),
            binding,
            session_id: SessionId(holder.into()),
            producer_observation: producer,
            source_basis,
            environment: Some(environment),
            check_kind: kind,
            check_fingerprint: observation.action_fingerprint.clone(),
            result,
            completed_at: at(second),
            summary: format!("host observed {key}"),
            refs: vec![format!("command:{key}")],
            actor: run_actor,
            recorded_at: at(second),
        },
    )
    .expect("append verification evidence");
    transaction.commit().expect("commit verification");
    verification
}

/// The command fingerprint `host_verification` records for the check `key`.
pub(super) fn check_fingerprint(key: &str) -> ObjectId {
    ObjectId::from_canonical_bytes(format!("check {key}").as_bytes())
}

/// Appends a host-observed source mutation on the claimed run that leaves
/// the source at `source_revision`; a verification of that revision recorded
/// afterwards answers it. `None` records the change the way a host that
/// observed no source basis does: with neither a revision nor a time.
pub(super) fn source_mutation(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    holder: &str,
    key: &str,
    second: i64,
    source_revision: Option<&str>,
) -> ObjectId {
    use crate::domain::{
        ControlWorkBinding, EffectClass, ExecutionObservation, ExecutionOutcome,
        ExecutionSourceBasis,
    };
    let run = super::query::load_work_run(&store.connection, claim.run_id).expect("claimed run");
    let mut run_actor = actor(holder);
    run_actor.run_id = Some(run.run_id.0.to_string());
    let observation = ExecutionObservation {
        schema_version: SCHEMA_VERSION,
        project_id: work.project_id.clone(),
        binding: ControlWorkBinding {
            root_execution_id: run.root_execution_id,
            work_id: work.work_id,
            run_id: run.run_id,
            work_revision: claim.accepted_work_revision,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
        },
        session_id: SessionId(holder.into()),
        grant_id: format!("grant-write-{key}"),
        observation_id: format!("write-{key}"),
        action_fingerprint: ObjectId::from_canonical_bytes(format!("write {key}").as_bytes()),
        effect: EffectClass::MutateLocal,
        outcome: ExecutionOutcome::Succeeded,
        source_changed: true,
        obligation_rule_set: active_rule_set_id(&store.connection),
        source_basis: source_revision.map(|revision| ExecutionSourceBasis {
            workspace_id: format!("workspace-{key}"),
            source_revision: revision.into(),
        }),
        observed_at: source_revision.map(|_| at(second)),
        actor: run_actor.clone(),
        recorded_at: at(second),
    };
    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("mutation transaction");
    let mutation =
        super::completion::append_control_execution_observation_on(&transaction, &observation)
            .expect("append the source mutation");
    transaction.commit().expect("commit mutation");
    mutation
}
