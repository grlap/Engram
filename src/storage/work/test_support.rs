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

impl SqliteStore {
    /// Reproduces disposal by the older implementation, which did not guard
    /// pending offers. Only hide the projection during disposal; the restored
    /// row and its canonical offer/event bytes remain exactly as written.
    pub(crate) fn test_dispose_with_historical_offer(
        &mut self,
        request: &DisposeWorkRequest,
    ) -> WorkItem {
        self.connection
            .execute_batch(
                "CREATE TEMP TABLE historical_offers AS SELECT * FROM work_handoff_offers;
             DELETE FROM work_handoff_offers;",
            )
            .unwrap();
        let item = self
            .dispose_work(request, &DevelopmentNoopRedactor)
            .unwrap();
        self.connection
            .execute_batch(
                "INSERT INTO work_handoff_offers SELECT * FROM historical_offers;
             DROP TABLE historical_offers;",
            )
            .unwrap();
        assert!(self.verify_all().unwrap().is_healthy());
        item
    }
}

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
        landing: None,
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
    host_verification_from_basis(
        store,
        work,
        claim,
        holder,
        key,
        kind,
        result,
        second,
        crate::domain::ExecutionSourceBasis {
            workspace_id: format!("workspace-{key}"),
            source_revision: source_revision.into(),
            source_root_generation: None,
            source_root_state: None,
        },
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "test fixture mirrors the host verification surface"
)]
pub(super) fn host_verification_from_basis(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    holder: &str,
    key: &str,
    kind: crate::domain::VerificationKind,
    result: crate::domain::VerificationResult,
    second: i64,
    source_basis: crate::domain::ExecutionSourceBasis,
) -> ObjectId {
    host_verification_with_outcome(
        store,
        work,
        claim,
        holder,
        HostCheck {
            key,
            kind,
            outcome: crate::domain::ExecutionOutcome::Succeeded,
            result,
            summary: &format!("host observed {key}"),
        },
        second,
        source_basis,
    )
}

/// One host check as its verification records it: the producer outcome the
/// host observed and the typed result, which the fixture sets independently,
/// and the summary prose.
#[derive(Clone, Copy)]
pub(crate) struct HostCheck<'a> {
    pub key: &'a str,
    pub kind: crate::domain::VerificationKind,
    pub outcome: crate::domain::ExecutionOutcome,
    pub result: crate::domain::VerificationResult,
    pub summary: &'a str,
}

/// A file store at `database` holding one item claimed by `holder`, with one
/// host verification recorded as `check` on workspace-A at revision A3.
/// Returns the item's short ref, the verification's record id and the raw
/// claim id, which host views keep to themselves.
pub(crate) fn verification_note_fixture(
    database: &std::path::Path,
    project: &str,
    holder: &str,
    check: HostCheck<'_>,
) -> (String, ObjectId, String) {
    let mut store = SqliteStore::open(database).expect("store");
    let work = store
        .create_work(
            &root_request(project, "verification-note", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("work");
    let claim = claim(&mut store, &work, holder, "verification-claim", 2, 36_000);
    let verification = host_verification_with_outcome(
        &mut store,
        &work,
        &claim,
        holder,
        check,
        3,
        crate::domain::ExecutionSourceBasis {
            workspace_id: "workspace-A".into(),
            source_revision: "A3".into(),
            source_root_generation: None,
            source_root_state: None,
        },
    );
    (work.short_ref, verification, claim.claim_id.0.to_string())
}

/// A file store at `database` holding one item whose first criterion binds a
/// test, claimed by `holder`: a flagged source change in workspace-old at R1,
/// then a passed test of R2 in workspace-new, a move the host never reported
/// as a change. Returns the item's short ref and the check's record id.
pub(crate) fn unreported_move_fixture(
    database: &std::path::Path,
    project: &str,
    holder: &str,
) -> (String, ObjectId) {
    let mut store = SqliteStore::open(database).expect("store");
    let mut request = root_request(project, "unreported-move", 1);
    request.acceptance = vec!["tests pass".into(), "docs written".into()];
    request.acceptance_bindings = vec![crate::domain::AcceptanceBinding {
        criterion: 1,
        requirement: crate::domain::VerificationRequirement {
            check_kind: crate::domain::VerificationKind::Test,
            check_fingerprint: None,
        },
    }];
    let work = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("work");
    let claim = claim(
        &mut store,
        &work,
        holder,
        "unreported-move-claim",
        2,
        36_000,
    );
    let unbound = |workspace: &str, revision: &str| crate::domain::ExecutionSourceBasis {
        workspace_id: workspace.into(),
        source_revision: revision.into(),
        source_root_generation: None,
        source_root_state: None,
    };
    source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        holder,
        "old-change",
        3,
        Some(unbound("workspace-old", "R1")),
        None,
    );
    let check = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        holder,
        "new-test",
        crate::domain::VerificationKind::Test,
        crate::domain::VerificationResult::Passed,
        4,
        unbound("workspace-new", "R2"),
    );
    (work.short_ref, check)
}

/// A file store at `database` holding one item whose first criterion binds a
/// test, claimed by `holder`, with `changes` source changes and then
/// `records` passing tests of the newest revision. Returns the item's short
/// ref and the verifications' record ids, oldest first.
pub(crate) fn assessed_verification_fixture(
    database: &std::path::Path,
    project: &str,
    holder: &str,
    changes: usize,
    records: usize,
) -> (String, Vec<ObjectId>) {
    let (_, work, _, verifications) =
        assessed_verification_setup(database, project, holder, changes, records);
    (work.short_ref, verifications)
}

fn assessed_verification_setup(
    database: &std::path::Path,
    project: &str,
    holder: &str,
    changes: usize,
    records: usize,
) -> (SqliteStore, WorkItem, WorkClaim, Vec<ObjectId>) {
    let mut store = SqliteStore::open(database).expect("store");
    let mut request = root_request(project, "assessed-verification", 1);
    request.acceptance = vec!["run the tests".into()];
    request.acceptance_bindings = vec![crate::domain::AcceptanceBinding {
        criterion: 1,
        requirement: crate::domain::VerificationRequirement {
            check_kind: crate::domain::VerificationKind::Test,
            check_fingerprint: None,
        },
    }];
    let work = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("work");
    let claim = claim(&mut store, &work, holder, "assessed-claim", 2, 36_000);
    let second = |index: usize| 3 + i64::try_from(index).expect("small index");
    for index in 0..changes {
        source_mutation(
            &mut store,
            &work,
            &claim,
            holder,
            &format!("change-{index}"),
            second(index),
            Some(&format!("R{index}")),
        );
    }
    let verifications = (0..records)
        .map(|index| {
            host_verification_of(
                &mut store,
                &work,
                &claim,
                holder,
                &format!("suite-{index}"),
                crate::domain::VerificationKind::Test,
                crate::domain::VerificationResult::Passed,
                second(changes + index),
                &format!("R{}", changes.saturating_sub(1)),
            )
        })
        .collect();
    (store, work, claim, verifications)
}

/// A file store at `database` holding one item whose first criterion binds a
/// test, claimed by `holder`: `closed` source changes and a passing test of
/// the newest of them, which closes every obligation open so far, then `live`
/// further changes and a passing test of `last_revision`. Returns the item's
/// short ref and the two tests' record ids, oldest first.
pub(crate) fn closed_then_live_verification_fixture(
    database: &std::path::Path,
    project: &str,
    holder: &str,
    closed: usize,
    live: usize,
    last_revision: &str,
) -> (String, [ObjectId; 2]) {
    let (mut store, work, claim, first) =
        assessed_verification_setup(database, project, holder, closed, 1);
    let second = |index: usize| 3 + i64::try_from(closed + 1 + index).expect("small index");
    for index in 0..live {
        source_mutation(
            &mut store,
            &work,
            &claim,
            holder,
            &format!("live-change-{index}"),
            second(index),
            Some(&format!("L{index}")),
        );
    }
    let last = host_verification_of(
        &mut store,
        &work,
        &claim,
        holder,
        "suite-last",
        crate::domain::VerificationKind::Test,
        crate::domain::VerificationResult::Passed,
        second(live),
        last_revision,
    );
    (work.short_ref, [first[0].clone(), last])
}

/// A file store at `database` holding one item whose first criterion binds a
/// test, claimed by `holder`: `closed` source changes and a first passing
/// test of the newest, then `later` changes that a second passing test
/// satisfies, then one more change that nothing satisfies. Seen from the
/// first test, the later changes are left out as not yet defined with their
/// obligations ended by another record, and the last is left out but still
/// open. Returns the item's short ref and the first test's record id.
pub(crate) fn later_ended_and_open_verification_fixture(
    database: &std::path::Path,
    project: &str,
    holder: &str,
    closed: usize,
    later: usize,
) -> (String, ObjectId) {
    let (mut store, work, claim, first) =
        assessed_verification_setup(database, project, holder, closed, 1);
    let second = |index: usize| 3 + i64::try_from(closed + 1 + index).expect("small index");
    for index in 0..later {
        source_mutation(
            &mut store,
            &work,
            &claim,
            holder,
            &format!("later-change-{index}"),
            second(index),
            Some(&format!("L{index}")),
        );
    }
    host_verification_of(
        &mut store,
        &work,
        &claim,
        holder,
        "suite-later",
        crate::domain::VerificationKind::Test,
        crate::domain::VerificationResult::Passed,
        second(later),
        &format!("L{}", later.saturating_sub(1)),
    );
    source_mutation(
        &mut store,
        &work,
        &claim,
        holder,
        "still-open-change",
        second(later + 1),
        Some("OPEN"),
    );
    (work.short_ref, first[0].clone())
}

/// The records needed to exercise a bound-check completion refusal.
pub(crate) struct BoundVerificationFixture {
    pub work: WorkItem,
    pub claim: WorkClaim,
    pub generic: ObjectId,
    pub satisfied_by: ObjectId,
    pub verification: ObjectId,
}

/// A satisfied build binding, an outstanding source-change test obligation,
/// and a newer build that either did not pass or passed on the older source.
pub(crate) fn bound_verification_refusal_fixture(
    database: &std::path::Path,
    project: &str,
    holder: &str,
    result: VerificationResult,
    second: i64,
) -> BoundVerificationFixture {
    let mut store = SqliteStore::open(database).expect("store");
    let mut request = root_request(project, "bound-refusal", 1);
    request.acceptance = vec!["build is clean".into()];
    request.acceptance_bindings = vec![crate::domain::AcceptanceBinding {
        criterion: 1,
        requirement: crate::domain::VerificationRequirement {
            check_kind: VerificationKind::Build,
            check_fingerprint: None,
        },
    }];
    let work = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("work");
    let claim = claim(
        &mut store,
        &work,
        holder,
        "bound-refusal-claim",
        second,
        36_000,
    );
    let satisfied_by = host_verification(
        &mut store,
        &work,
        &claim,
        holder,
        "first-build",
        VerificationKind::Build,
        VerificationResult::Passed,
        second + 1,
    );
    let generic = evidence(&mut store, &work, &claim, holder, "generic", second + 2);
    source_mutation(
        &mut store,
        &work,
        &claim,
        holder,
        "change",
        second + 3,
        Some("changed-source"),
    );
    let verification = host_verification_with_outcome(
        &mut store,
        &work,
        &claim,
        holder,
        HostCheck {
            key: "newest-build",
            kind: VerificationKind::Build,
            outcome: match result {
                VerificationResult::Passed => ExecutionOutcome::Succeeded,
                VerificationResult::Failed => ExecutionOutcome::Failed,
                VerificationResult::Indeterminate => ExecutionOutcome::Unknown,
            },
            result,
            summary: "host observed the newest build",
        },
        second + 4,
        ExecutionSourceBasis {
            workspace_id: "workspace-newest-build".into(),
            source_revision: "revision-as-it-stands".into(),
            source_root_generation: None,
            source_root_state: None,
        },
    );
    let all = store.work_run_evidence(claim.run_id).expect("evidence");
    checkpoint(
        &mut store,
        &work,
        &claim,
        holder,
        "checkpoint",
        second + 5,
        &all,
    );
    BoundVerificationFixture {
        work,
        claim,
        generic,
        satisfied_by,
        verification,
    }
}

/// A done refused for a stale evaluation, with the source observation that
/// decided it.
pub(crate) struct StaleDecidingFixture {
    pub work: WorkItem,
    pub claim: WorkClaim,
    pub generic: ObjectId,
    /// The run-feed position of the flagged change that voided the evaluation.
    pub position: i64,
}

/// A same-session pass judged at `judged`, then a flagged change to
/// `revision` in `workspace` after its cut: the evaluation reads stale, and
/// that change is the observation that decided it. With `decided` false the
/// evaluation reads stale for another cause instead: the project policy no
/// longer admits the mode it was recorded in.
#[allow(
    clippy::too_many_arguments,
    reason = "test fixture mirrors the host observation surface"
)]
pub(crate) fn stale_deciding_refusal_fixture(
    database: &std::path::Path,
    project: &str,
    holder: &str,
    judged: &str,
    workspace: &str,
    revision: &str,
    decided: bool,
    second: i64,
) -> StaleDecidingFixture {
    use crate::domain::{
        AcceptanceBasis, AcceptanceEvaluationMode, AcceptanceEvaluationPolicy, AcceptanceVerdict,
        CriterionVerdictInput, MechanicalBasis, OBLIGATION_RULE_SET_SCHEMA_VERSION,
        ObligationRuleSet, RecordAcceptanceEvaluationRequest,
    };
    let mut store = SqliteStore::open(database).expect("store");
    let work = store
        .create_work(
            &root_request(project, "stale-deciding", second),
            &DevelopmentNoopRedactor,
        )
        .expect("work");
    let claim = claim(
        &mut store,
        &work,
        holder,
        "stale-deciding-claim",
        second + 1,
        36_000,
    );
    let generic = evidence(&mut store, &work, &claim, holder, "generic", second + 2);
    checkpoint(
        &mut store,
        &work,
        &claim,
        holder,
        "checkpoint",
        second + 3,
        std::slice::from_ref(&generic),
    );
    let policy = |modes: Vec<AcceptanceEvaluationMode>| AcceptanceEvaluationPolicy {
        allowed_modes: modes,
        mechanical_basis: MechanicalBasis::Asserted,
        require_source_freshness: false,
    };
    store
        .set_acceptance_evaluation_policy(
            &policy(vec![AcceptanceEvaluationMode::SameSession]),
            &actor("policy-admin"),
            "enable",
            None,
            at(second + 4),
            &DevelopmentNoopRedactor,
        )
        .expect("evaluated policy");
    store
        .set_obligation_rule_set(
            &ObligationRuleSet {
                schema_version: OBLIGATION_RULE_SET_SCHEMA_VERSION,
                rules: Vec::new(),
            },
            &actor("obligation-rule-admin"),
            "no-obligation-rules",
            None,
            at(second + 5),
            &DevelopmentNoopRedactor,
        )
        .expect("empty obligation rules");
    let basis = |workspace: &str, revision: &str| crate::domain::ExecutionSourceBasis {
        workspace_id: workspace.into(),
        source_revision: revision.into(),
        source_root_generation: None,
        source_root_state: None,
    };
    source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        holder,
        "judged",
        second + 6,
        Some(basis("workspace-judged", judged)),
        None,
    );
    let run_feed = crate::domain::FeedId::RunExecution(claim.run_id);
    store
        .record_acceptance_evaluation(
            &RecordAcceptanceEvaluationRequest {
                supersedes: None,
                project_id: work.project_id.clone(),
                work_id: work.work_id,
                expected_work_revision: work.revision,
                evaluated_through: store.work_feed_head(&run_feed).expect("run feed head"),
                mode: AcceptanceEvaluationMode::SameSession,
                execution_identity: None,
                parent_session: None,
                evaluator_model: None,
                source_basis: None,
                verdicts: vec![CriterionVerdictInput {
                    criterion: 1,
                    verdict: AcceptanceVerdict::Pass,
                    basis: AcceptanceBasis::Judgment,
                    rationale: "criterion 1: pass".into(),
                    evidence: vec![generic.clone()],
                }],
                evaluator: actor(holder),
                attempt_key: None,
                recorded_at: at(second + 7),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("the evaluation records");
    if decided {
        source_mutation_from_basis(
            &mut store,
            &work,
            &claim,
            holder,
            "moved",
            second + 8,
            Some(basis(workspace, revision)),
            None,
        );
    } else {
        store
            .set_acceptance_evaluation_policy(
                &policy(vec![AcceptanceEvaluationMode::IndependentSession]),
                &actor("policy-admin"),
                "independent-only",
                None,
                at(second + 8),
                &DevelopmentNoopRedactor,
            )
            .expect("stricter policy");
    }
    let position = store.work_feed_head(&run_feed).expect("run feed head");
    StaleDecidingFixture {
        work,
        claim,
        generic,
        position,
    }
}

/// Records a host check, its environment and its verification at `second`.
pub(super) fn host_verification_with_outcome(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    holder: &str,
    check: HostCheck<'_>,
    second: i64,
    source_basis: crate::domain::ExecutionSourceBasis,
) -> ObjectId {
    use crate::domain::{
        ControlWorkBinding, EffectClass, EnvironmentComponents, EnvironmentEvidence,
        ExecutionObservation, VerificationEvidence,
    };
    let HostCheck {
        key,
        kind,
        outcome,
        result,
        summary,
    } = check;
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
    let observation = ExecutionObservation {
        schema_version: SCHEMA_VERSION,
        project_id: work.project_id.clone(),
        binding: binding.clone(),
        session_id: SessionId(holder.into()),
        grant_id: format!("grant-{key}"),
        observation_id: format!("check-{key}"),
        action_fingerprint: check_fingerprint(key),
        effect: EffectClass::Observe,
        outcome,
        source_changed: false,
        reported_source_change: None,
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
            summary: summary.to_owned(),
            refs: vec![format!("command:{key}")],
            actor: run_actor,
            recorded_at: at(second),
            bound_from: None,
        },
    )
    .expect("append verification evidence");
    transaction.commit().expect("commit verification");
    verification
}

/// Appends only the producer observation of a check that ran at `second`
/// in `source_basis`, so a test can record its verification later, the way
/// a checkpoint may cite a stored producer.
#[allow(
    clippy::too_many_arguments,
    reason = "test fixture mirrors the host observation surface"
)]
pub(super) fn host_check_producer(
    transaction: &rusqlite::Transaction<'_>,
    work: &WorkItem,
    claim: &WorkClaim,
    holder: &str,
    key: &str,
    second: i64,
    source_basis: crate::domain::ExecutionSourceBasis,
) -> ObjectId {
    use crate::domain::{ControlWorkBinding, EffectClass, ExecutionObservation, ExecutionOutcome};
    let run = super::query::load_work_run(transaction, claim.run_id).expect("claimed run");
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
        grant_id: format!("grant-{key}"),
        observation_id: format!("check-{key}"),
        action_fingerprint: check_fingerprint(key),
        effect: EffectClass::Observe,
        outcome: ExecutionOutcome::Succeeded,
        source_changed: false,
        reported_source_change: None,
        obligation_rule_set: active_rule_set_id(transaction),
        source_basis: Some(source_basis),
        observed_at: Some(at(second)),
        actor: run_actor.clone(),
        recorded_at: at(second),
    };
    super::completion::append_control_execution_observation_on(transaction, &observation)
        .expect("append the check's producer")
}

/// Records a passing test verification of the stored `producer`, a check
/// [`host_check_producer`] appended at `second` under `key`.
#[allow(
    clippy::too_many_arguments,
    reason = "test fixture mirrors the host verification surface"
)]
pub(super) fn host_verification_of_producer(
    transaction: &rusqlite::Transaction<'_>,
    work: &WorkItem,
    claim: &WorkClaim,
    holder: &str,
    key: &str,
    second: i64,
    recorded_second: i64,
    source_basis: crate::domain::ExecutionSourceBasis,
    producer: ObjectId,
) -> ObjectId {
    use crate::domain::{ControlWorkBinding, VerificationEvidence};
    let run = super::query::load_work_run(transaction, claim.run_id).expect("claimed run");
    let mut run_actor = actor(holder);
    run_actor.run_id = Some(run.run_id.0.to_string());
    super::completion::append_control_verification_evidence_on(
        transaction,
        &VerificationEvidence {
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
            producer_observation: producer,
            source_basis,
            environment: None,
            check_kind: crate::domain::VerificationKind::Test,
            check_fingerprint: check_fingerprint(key),
            result: crate::domain::VerificationResult::Passed,
            completed_at: at(second),
            summary: format!("host observed {key}"),
            refs: vec![format!("command:{key}")],
            actor: run_actor,
            recorded_at: at(recorded_second),
            bound_from: None,
        },
    )
    .expect("append verification of the stored producer")
}

/// The smallest update result a gate stores for `work_id`, for a fixture
/// that completes a protocol attempt without running the gate.
pub(super) fn gate_result(work_id: WorkId) -> crate::work_service::WorkUpdateResult {
    crate::work_service::WorkUpdateResult {
        operation: "evidence".into(),
        receipt: crate::work_service::WorkMutationReceipt {
            work_id,
            work_ref: "w-fixture".into(),
            revision: 1,
            control_binding: None,
            result: serde_json::json!({}),
        },
        obligations: Vec::new(),
        obligation_page: crate::work_service::WorkObligationPage::default(),
        allowed_next: Vec::new(),
    }
}

/// The command fingerprint `host_verification` records for the check `key`.
pub(super) fn check_fingerprint(key: &str) -> ObjectId {
    ObjectId::from_canonical_bytes(format!("check {key}").as_bytes())
}

impl SqliteStore {
    /// Appends, for tests outside this module, a host-observed source change
    /// on `work_id`'s claimed run that leaves the source at
    /// `source_revision`, as a control turn checkpoint records one.
    pub(crate) fn append_source_change_fixture(
        &mut self,
        work_id: WorkId,
        key: &str,
        observed_at: DateTime<Utc>,
        source_revision: &str,
    ) -> ObjectId {
        let second = (observed_at - at(0)).num_seconds();
        let work = super::query::load_work_item(&self.connection, work_id).expect("fixture work");
        let run_id = work.active_run_id.expect("fixture work has an active run");
        let claim = super::query::load_work_claim_optional(&self.connection, run_id)
            .expect("fixture claim read")
            .expect("fixture run is claimed");
        let holder = claim.holder.0.clone();
        source_mutation(
            self,
            &work,
            &claim,
            &holder,
            key,
            second,
            Some(source_revision),
        )
    }

    /// The live claim on `work_id`'s active run, for a fixture that keeps
    /// acting with it after the run finishes.
    pub(crate) fn claim_fixture(&self, work_id: WorkId) -> crate::domain::WorkClaim {
        let work = super::query::load_work_item(&self.connection, work_id).expect("fixture work");
        let run_id = work.active_run_id.expect("fixture work has an active run");
        super::query::load_work_claim_optional(&self.connection, run_id)
            .expect("fixture claim read")
            .expect("fixture run is claimed")
    }

    /// A source change a host recorded late, with the claim it held, against
    /// a run that had since completed, together with the obligation it
    /// opened there. The write path no longer opens one on a finished run,
    /// but stores written before it stopped hold such rows, and readers must
    /// show them as history. So this persists the obligation directly, the
    /// way the old path did.
    pub(crate) fn append_late_source_change_fixture(
        &mut self,
        work_id: WorkId,
        claim: &crate::domain::WorkClaim,
        key: &str,
        observed_at: DateTime<Utc>,
        source_revision: &str,
    ) -> ObjectId {
        let second = (observed_at - at(0)).num_seconds();
        let work = super::query::load_work_item(&self.connection, work_id).expect("fixture work");
        let holder = claim.holder.0.clone();
        let observation = source_mutation_observation(
            &self.connection,
            &work,
            claim,
            &holder,
            key,
            second,
            Some(crate::domain::ExecutionSourceBasis {
                workspace_id: format!("workspace-{key}"),
                source_revision: source_revision.into(),
                source_root_generation: None,
                source_root_state: None,
            }),
            None,
        );
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("late change transaction");
        let late = append_source_mutation_on(&transaction, &observation);
        let position =
            super::feeds::run_feed_position_for_object_on(&transaction, claim.run_id, &late)
                .expect("the late change's run-feed position");
        super::completion::append_builtin_obligations_on(
            &transaction,
            &crate::domain::SourceObservation::admitted(late.clone(), &observation),
            &position,
        )
        .expect("persist the obligation the late change opened");
        transaction.commit().expect("commit the late change");
        late
    }

    /// `append_source_change_fixture` for a host that also says how it
    /// established the change.
    pub(crate) fn append_detected_source_change_fixture(
        &mut self,
        work_id: WorkId,
        key: &str,
        observed_at: DateTime<Utc>,
        source_revision: &str,
        detection: crate::domain::SourceChangeDetection,
    ) -> ObjectId {
        let second = (observed_at - at(0)).num_seconds();
        let work = super::query::load_work_item(&self.connection, work_id).expect("fixture work");
        let run_id = work.active_run_id.expect("fixture work has an active run");
        let claim = super::query::load_work_claim_optional(&self.connection, run_id)
            .expect("fixture claim read")
            .expect("fixture run is claimed");
        let holder = claim.holder.0.clone();
        source_mutation_detected(
            self,
            &work,
            &claim,
            &holder,
            key,
            second,
            Some(source_revision),
            Some(detection),
        )
    }
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
    source_mutation_detected(
        store,
        work,
        claim,
        holder,
        key,
        second,
        source_revision,
        None,
    )
}

/// `source_mutation` with the host's word on how it established the change.
#[allow(
    clippy::too_many_arguments,
    reason = "one test helper mirrors the host observation surface"
)]
pub(super) fn source_mutation_detected(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    holder: &str,
    key: &str,
    second: i64,
    source_revision: Option<&str>,
    reported_source_change: Option<crate::domain::SourceChangeDetection>,
) -> ObjectId {
    source_mutation_from_basis(
        store,
        work,
        claim,
        holder,
        key,
        second,
        source_revision.map(|revision| crate::domain::ExecutionSourceBasis {
            workspace_id: format!("workspace-{key}"),
            source_revision: revision.into(),
            source_root_generation: None,
            source_root_state: None,
        }),
        reported_source_change,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "test fixture mirrors the host observation surface"
)]
pub(crate) fn source_mutation_from_basis(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    holder: &str,
    key: &str,
    second: i64,
    source_basis: Option<crate::domain::ExecutionSourceBasis>,
    reported_source_change: Option<crate::domain::SourceChangeDetection>,
) -> ObjectId {
    let observation = source_mutation_observation(
        &store.connection,
        work,
        claim,
        holder,
        key,
        second,
        source_basis,
        reported_source_change,
    );
    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("mutation transaction");
    let mutation = append_source_mutation_on(&transaction, &observation);
    transaction.commit().expect("commit mutation");
    mutation
}

/// The source-change observation [`source_mutation_from_basis`] records,
/// prepared without appending it.
#[allow(
    clippy::too_many_arguments,
    reason = "test fixture mirrors the host observation surface"
)]
pub(super) fn source_mutation_observation(
    connection: &Connection,
    work: &WorkItem,
    claim: &WorkClaim,
    holder: &str,
    key: &str,
    second: i64,
    source_basis: Option<crate::domain::ExecutionSourceBasis>,
    reported_source_change: Option<crate::domain::SourceChangeDetection>,
) -> crate::domain::ExecutionObservation {
    use crate::domain::{ControlWorkBinding, EffectClass, ExecutionObservation, ExecutionOutcome};
    let run = super::query::load_work_run(connection, claim.run_id).expect("claimed run");
    let mut run_actor = actor(holder);
    run_actor.run_id = Some(run.run_id.0.to_string());
    ExecutionObservation {
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
        reported_source_change,
        obligation_rule_set: active_rule_set_id(connection),
        observed_at: source_basis.as_ref().map(|_| at(second)),
        source_basis,
        actor: run_actor,
        recorded_at: at(second),
    }
}

/// Appends a prepared source-change observation inside `transaction`.
pub(super) fn append_source_mutation_on(
    transaction: &rusqlite::Transaction<'_>,
    observation: &crate::domain::ExecutionObservation,
) -> ObjectId {
    super::completion::append_control_execution_observation_on(transaction, observation)
        .expect("append the source mutation")
}
