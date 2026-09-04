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

pub(super) fn builtin_rule_set_hash() -> ObjectHash {
    CanonicalObject::freeze(&crate::control::builtin_obligation_rule_set())
        .expect("canonical built-in obligation rule set")
        .hash()
        .clone()
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
                expected_run_id: work.active_run_id.expect("active run"),
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
    evidence: &[ObjectHash],
) -> ObjectHash {
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
) -> ObjectHash {
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
    evidence: &ObjectHash,
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
    evidence: &ObjectHash,
    key: &str,
    second: i64,
) -> Result<CompletionSeal, StoreError> {
    store.complete_work(
        &completion_request(work, claim, holder, evidence, key, second),
        &DevelopmentNoopRedactor,
    )
}
