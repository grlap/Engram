//! Boundary-matrix coverage for host-evaluated, core-enforced acceptance.
//! Each test names the rows of the agreed matrix it exercises.

mod bound_criteria;
mod corrections;
mod review;

use super::super::test_support::*;
use super::*;
use crate::domain::{
    AcceptanceEvaluationMode as Mode, AcceptanceSourceBasis, CompletionSeal, ControlWorkBinding,
    IssuedTurnGrant, OBLIGATION_RULE_SET_SCHEMA_VERSION, ObligationRuleSet,
    RecordGateEvidenceRequest, ResourceCoverage, ResourceSubject, ReviseWorkRequest, WorkClaim,
};

fn policy(modes: &[Mode], mechanical: MechanicalBasis, fresh: bool) -> AcceptanceEvaluationPolicy {
    AcceptanceEvaluationPolicy {
        allowed_modes: modes.to_vec(),
        mechanical_basis: mechanical,
        require_source_freshness: fresh,
    }
}

fn enable(
    store: &mut SqliteStore,
    modes: &[Mode],
    mechanical: MechanicalBasis,
    fresh: bool,
    key: &str,
    second: i64,
) -> crate::storage::AcceptanceEvaluationPolicyUpdateReceipt {
    store
        .set_acceptance_evaluation_policy(
            &policy(modes, mechanical, fresh),
            &actor("policy-admin"),
            key,
            None,
            at(second),
            &DevelopmentNoopRedactor,
        )
        .expect("activate acceptance evaluation policy")
}

fn disable_obligation_rules(store: &mut SqliteStore, second: i64) {
    store
        .set_obligation_rule_set(
            &ObligationRuleSet {
                schema_version: OBLIGATION_RULE_SET_SCHEMA_VERSION,
                rules: Vec::new(),
            },
            &actor("obligation-rule-admin"),
            "disable-obligation-rules",
            None,
            at(second),
            &DevelopmentNoopRedactor,
        )
        .expect("activate empty obligation rule set");
}

fn gate(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    holder: &str,
    name: &str,
    failed: &[&str],
    second: i64,
) -> ObjectId {
    store
        .record_gate_evidence(
            &RecordGateEvidenceRequest {
                work_id: work.work_id,
                run_id: claim.run_id,
                expected_work_revision: work.revision,
                holder: SessionId(holder.into()),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                name: name.into(),
                failed: failed.iter().map(|failure| (*failure).into()).collect(),
                evidence_ref: None,
                actor: actor(holder),
                recorded_at: at(second),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("record gate evidence")
}

fn verdict(
    position: usize,
    verdict: AcceptanceVerdict,
    basis: AcceptanceBasis,
    evidence: &[ObjectId],
) -> CriterionVerdictInput {
    CriterionVerdictInput {
        criterion: position,
        verdict,
        basis,
        rationale: format!("criterion {position}: {}", verdict.word()),
        evidence: evidence.to_vec(),
    }
}

/// The run-feed position an evaluator reading the item right now would cite.
fn cut(store: &SqliteStore, work: &WorkItem) -> i64 {
    store
        .work_feed_head(&FeedId::RunExecution(
            work.active_run_id.expect("active run"),
        ))
        .expect("run feed head")
}

fn request(
    work: &WorkItem,
    evaluated_through: i64,
    session: &str,
    mode: Mode,
    verdicts: Vec<CriterionVerdictInput>,
    second: i64,
) -> RecordAcceptanceEvaluationRequest {
    RecordAcceptanceEvaluationRequest {
        project_id: work.project_id.clone(),
        work_id: work.work_id,
        expected_work_revision: work.revision,
        evaluated_through,
        mode,
        execution_identity: None,
        parent_session: None,
        evaluator_model: None,
        source_basis: None,
        verdicts,
        evaluator: actor(session),
        attempt_key: None,
        recorded_at: at(second),
    }
}

fn record(
    store: &mut SqliteStore,
    request: &RecordAcceptanceEvaluationRequest,
) -> Result<AcceptanceEvaluationReceipt, StoreError> {
    store.record_acceptance_evaluation(request, &DevelopmentNoopRedactor)
}

fn refusal(result: Result<AcceptanceEvaluationReceipt, StoreError>) -> String {
    match result {
        Err(StoreError::AcceptanceEvaluationRefused { reason, .. }) => reason,
        other => panic!("expected an acceptance evaluation refusal, got {other:?}"),
    }
}

/// A fresh final checkpoint followed by one completion attempt, as the `done`
/// word always does: the seal binds the dense run-feed cut, so anything that
/// landed on the feed after the previous checkpoint must be acknowledged.
#[allow(
    clippy::too_many_arguments,
    reason = "one test helper mirrors the full completion request surface"
)]
fn checkpoint_then_complete(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    holder: &str,
    evidence: &[ObjectId],
    explicit_acceptance: bool,
    source_fingerprint: Option<&str>,
    key: &str,
    second: i64,
) -> Result<CompletionSeal, StoreError> {
    checkpoint(
        store,
        work,
        claim,
        holder,
        &format!("{key}-checkpoint"),
        second,
        evidence,
    );
    let mut request = completion_request(work, claim, holder, &evidence[0], key, second);
    request.evidence = evidence.to_vec();
    if !explicit_acceptance {
        request.acceptance = Vec::new();
    }
    request.source_fingerprint = source_fingerprint.map(Into::into);
    store.complete_work(&request, &DevelopmentNoopRedactor)
}

/// Completion with no explicit acceptance: the evaluated route.
#[allow(
    clippy::too_many_arguments,
    reason = "one test helper mirrors the full completion request surface"
)]
fn complete_evaluated(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    holder: &str,
    evidence: &ObjectId,
    source_fingerprint: Option<&str>,
    key: &str,
    second: i64,
) -> Result<CompletionSeal, StoreError> {
    checkpoint_then_complete(
        store,
        work,
        claim,
        holder,
        std::slice::from_ref(evidence),
        false,
        source_fingerprint,
        key,
        second,
    )
}

fn recovery_cause(result: Result<CompletionSeal, StoreError>) -> WorkCompletionRecoveryCause {
    match result {
        Err(StoreError::WorkCompletionRecoveryRequired { cause, .. }) => cause,
        other => panic!("expected a completion recovery cause, got {other:?}"),
    }
}

fn empty_patch() -> WorkRevisionPatch {
    WorkRevisionPatch {
        acceptance_bindings: None,
        external_ref: None,
        clear_external: false,
        title: None,
        outcome: None,
        acceptance: None,
        kind: None,
        priority: None,
        labels: None,
        add_labels: Vec::new(),
        remove_labels: Vec::new(),
        assigned_to: None,
        clear_assignment: false,
        deferred_until: None,
        clear_deferral: false,
        evaluation_mode: None,
        clear_evaluation_mode: false,
    }
}

/// The holder revises its own claimed item through claim-bound planning.
fn revise(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    patch: WorkRevisionPatch,
    key: &str,
    second: i64,
) -> Result<WorkItem, StoreError> {
    store.revise_work(
        &ReviseWorkRequest {
            work_id: work.work_id,
            expected_revision: work.revision,
            patch,
            authority: WorkPlanningAuthority::Claim {
                run_id: claim.run_id,
                holder: claim.holder.clone(),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
            },
            actor: actor(&claim.holder.0),
            idempotency_key: key.into(),
            updated_at: at(second),
        },
        &DevelopmentNoopRedactor,
    )
}

/// Bound host control session that mints observed execution evidence on the
/// holder's run, mirroring the host checkpoint protocol.
struct HostSession {
    project_id: ProjectId,
    session_id: SessionId,
    connection_token: String,
    routing_token: String,
    subject: ResourceSubject,
    basis: ExecutionSourceBasis,
    turns: usize,
}

impl HostSession {
    fn bind(store: &mut SqliteStore, work: &WorkItem, claim: &WorkClaim, second: i64) -> Self {
        let run = load_work_run(&store.connection, claim.run_id).expect("claimed run");
        let binding = ControlWorkBinding {
            root_execution_id: run.root_execution_id,
            work_id: work.work_id,
            run_id: run.run_id,
            work_revision: claim.accepted_work_revision,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
        };
        let session_id = claim.holder.clone();
        let connection_token = store
            .resume_control_connection(&session_id, at(second))
            .expect("resume host control connection");
        let mut host_actor = actor(&session_id.0);
        host_actor.run_id = Some(run.run_id.0.to_string());
        let control = store
            .bind_control_session_with_work(
                &work.project_id,
                "local-work:acceptance-evaluation",
                "Mint observed evidence for acceptance evaluation",
                &session_id,
                &connection_token,
                &host_actor,
                Some(&binding),
                ControlAssurance::TurnGated,
                &[EffectClass::Observe, EffectClass::MutateLocal],
                1,
                "bind-host-session",
                at(second),
            )
            .expect("bind control session to live claim");
        let mut host = Self {
            project_id: work.project_id.clone(),
            session_id,
            connection_token,
            routing_token: control.routing_token,
            subject: ResourceSubject::Path {
                project_id: work.project_id.clone(),
                segments: vec!["src".into()],
                coverage: ResourceCoverage::Tree,
            },
            basis: ExecutionSourceBasis {
                workspace_id: "workspace-evaluated".into(),
                source_revision: "content-revision-1".into(),
            },
            turns: 0,
        };
        let synchronize = host.grant(store, &[EffectClass::Observe], false, second + 1);
        host.begin(store, &synchronize, second + 2);
        assert!(matches!(
            store
                .checkpoint_control_turn(
                    &host.project_id,
                    &host.session_id,
                    &host.connection_token,
                    &host.routing_token,
                    &synchronize.grant_id,
                    TurnNextIntent::Continue,
                    &host.key("checkpoint-sync"),
                    at(second + 3),
                )
                .expect("checkpoint synchronization turn"),
            ControlTurnCheckpointDecision::Checkpointed { .. }
        ));
        host
    }

    fn key(&self, stem: &str) -> String {
        format!("{stem}-{}", self.turns)
    }

    fn grant(
        &mut self,
        store: &mut SqliteStore,
        effects: &[EffectClass],
        with_subject: bool,
        second: i64,
    ) -> IssuedTurnGrant {
        self.turns += 1;
        let decision = store
            .evaluate_control_turn(
                &self.project_id,
                &self.session_id,
                &self.connection_token,
                &self.routing_token,
                &TurnIntent {
                    idempotency_key: self.key("evaluate"),
                    intent_fingerprint: ObjectId::from_canonical_bytes(
                        self.key("intent").as_bytes(),
                    ),
                    purpose: TurnPurpose::Ordinary,
                    requested_effects: effects.to_vec(),
                    resource_intents: if with_subject {
                        vec![self.subject.clone()]
                    } else {
                        Vec::new()
                    },
                },
                at(second),
            )
            .expect("evaluate host turn");
        let ControlTurnDecision::Grant { grant } = decision else {
            panic!("host turn must grant: {decision:?}");
        };
        *grant
    }

    fn begin(&self, store: &mut SqliteStore, grant: &IssuedTurnGrant, second: i64) {
        let tokens = grant
            .delivery
            .as_ref()
            .map(|delivery| vec![delivery.page.delivery_token.clone()])
            .unwrap_or_default();
        assert!(matches!(
            store
                .begin_control_turn(
                    &self.project_id,
                    &self.session_id,
                    &self.connection_token,
                    &self.routing_token,
                    &grant.grant_id,
                    &tokens,
                    &self.key("begin"),
                    at(second),
                )
                .expect("begin host turn"),
            ControlTurnBeginDecision::Begin { .. }
        ));
    }

    /// One mutation turn: an optional source change plus an optional
    /// host-observed check. Returns the minted verification evidence ides.
    fn checkpoint(
        &mut self,
        store: &mut SqliteStore,
        source_changed: bool,
        check: Option<(VerificationKind, ExecutionOutcome)>,
        second: i64,
    ) -> Vec<ObjectId> {
        let grant = self.grant(store, &[EffectClass::MutateLocal], true, second);
        self.begin(store, &grant, second + 1);
        let mut observations = Vec::new();
        if source_changed {
            observations.push(ExecutionObservationInput {
                observation_id: self.key("source-mutation"),
                action_fingerprint: ObjectId::from_canonical_bytes(
                    self.key("write src").as_bytes(),
                ),
                effect: EffectClass::MutateLocal,
                outcome: ExecutionOutcome::Succeeded,
                source_changed: true,
                source_basis: Some(self.basis.clone()),
                observed_at: Some(at(second + 1)),
            });
        }
        let mut verifications = Vec::new();
        let mut environments = Vec::new();
        if let Some((kind, outcome)) = check {
            observations.push(ExecutionObservationInput {
                observation_id: self.key("check"),
                action_fingerprint: ObjectId::from_canonical_bytes(
                    self.key("run check").as_bytes(),
                ),
                effect: EffectClass::MutateLocal,
                outcome,
                source_changed: false,
                source_basis: Some(self.basis.clone()),
                observed_at: Some(at(second + 1)),
            });
            let components = EnvironmentComponents {
                toolchain: "rustc-test".into(),
                sandbox: Some("test-host-sandbox".into()),
                workspace_id: self.basis.workspace_id.clone(),
                capability_map_revision: 1,
            };
            environments.push(EnvironmentEvidenceInput {
                source_basis: self.basis.clone(),
                environment_fingerprint: CanonicalObject::freeze(&components)
                    .expect("freeze environment components")
                    .key()
                    .clone(),
                components: Some(components),
                observed_at: at(second + 1),
            });
            verifications.push(VerificationEvidenceInput {
                producer_observation: ExecutionObservationReference::ObservationId {
                    observation_id: self.key("check"),
                },
                check_kind: kind,
                environment: Some(EnvironmentEvidenceReference::Index { index: 0 }),
                summary: Some("host observed the check".into()),
                refs: vec!["command:check".into()],
            });
        }
        let checkpointed = store
            .checkpoint_control_turn_with_evidence(
                &self.project_id,
                &self.session_id,
                &self.connection_token,
                &self.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                &observations,
                &verifications,
                &environments,
                &self.key("checkpoint"),
                at(second + 2),
            )
            .expect("checkpoint host turn with evidence");
        let ControlTurnCheckpointDecision::Checkpointed { receipt } = checkpointed else {
            panic!("host turn must checkpoint: {checkpointed:?}");
        };
        receipt.verification_evidence.clone()
    }
}

// Field order matters: the store closes before its temporary home is removed.
struct Fixture {
    store: SqliteStore,
    /// Keeps the database alive for the fixture's lifetime; tests that drive
    /// a second connection (the word, a reopen) take its path.
    directory: crate::test_support::TempHome,
    work: WorkItem,
    claim: WorkClaim,
    evidence: ObjectId,
}

/// A claimed root with one checkpointed generic evidence object, ready to
/// complete once acceptance is settled.
fn fixture(project: &str) -> Fixture {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let mut store = SqliteStore::open(&database).expect("store");
    let work = store
        .create_work(
            &root_request(project, "create-evaluated-work", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("create local work");
    let claim = claim(
        &mut store,
        &work,
        "runner",
        "claim-evaluated-work",
        2,
        3_600,
    );
    let evidence = evidence(&mut store, &work, &claim, "runner", "evidence-evaluated", 3);
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-evaluated",
        4,
        std::slice::from_ref(&evidence),
    );
    Fixture {
        store,
        directory,
        work,
        claim,
        evidence,
    }
}

// B01, B04, B27: the policy defaults to the self-asserted path, activates under one
// audited epoch, replays identical operations, and gates modes by explicit
// membership rather than rank.
#[test]
fn policy_defaults_self_asserted_and_admits_only_listed_modes() {
    let mut fixture = fixture("project-evaluation-policy");
    let store = &mut fixture.store;
    assert!(
        store
            .acceptance_evaluation_policy()
            .expect("read bootstrap policy")
            .is_self_asserted()
    );
    let note = fixture.evidence.clone();
    let self_asserted = refusal(record(
        store,
        &request(
            &fixture.work,
            cut(store, &fixture.work),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Judgment,
                std::slice::from_ref(&note),
            )],
            5,
        ),
    ));
    assert!(
        self_asserted.contains("does not enable acceptance evaluation"),
        "{self_asserted}"
    );

    let first = enable(
        store,
        &[Mode::IndependentSession],
        MechanicalBasis::Asserted,
        false,
        "enable-independent",
        6,
    );
    assert!(first.changed);
    assert!(first.previous_acceptance_evaluation.is_self_asserted());
    let replay = enable(
        store,
        &[Mode::IndependentSession],
        MechanicalBasis::Asserted,
        false,
        "enable-independent",
        7,
    );
    assert_eq!(replay, first);
    let unchanged = enable(
        store,
        &[Mode::IndependentSession],
        MechanicalBasis::Asserted,
        false,
        "enable-independent-again",
        8,
    );
    assert!(!unchanged.changed);
    assert_eq!(unchanged.active_policy, first.active_policy);
    let diagnostics = store.control_diagnostics().expect("control diagnostics");
    assert_eq!(diagnostics.policy_epoch, first.policy_epoch);

    let same_session = refusal(record(
        store,
        &request(
            &fixture.work,
            cut(store, &fixture.work),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Judgment,
                std::slice::from_ref(&note),
            )],
            9,
        ),
    ));
    assert!(
        same_session.contains("mode same_session is not allowed"),
        "{same_session}"
    );
    let stricter_is_not_implied = refusal(record(
        store,
        &request(
            &fixture.work,
            cut(store, &fixture.work),
            "sub-agent",
            Mode::SubAgent,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Judgment,
                std::slice::from_ref(&note),
            )],
            10,
        ),
    ));
    assert!(
        stricter_is_not_implied.contains("sub_agent mode needs"),
        "{stricter_is_not_implied}"
    );
    let sub_agent = refusal(record(
        store,
        &RecordAcceptanceEvaluationRequest {
            execution_identity: Some("sub-agent-exec".into()),
            parent_session: Some(SessionId("runner".into())),
            ..request(
                &fixture.work,
                cut(store, &fixture.work),
                "sub-agent",
                Mode::SubAgent,
                vec![verdict(
                    1,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Judgment,
                    std::slice::from_ref(&note),
                )],
                10,
            )
        },
    ));
    assert!(
        sub_agent.contains("mode sub_agent is not allowed"),
        "{sub_agent}"
    );
}

// B01, B27: a self-asserted project seals exactly as before and binds no evaluation.
#[test]
fn self_asserted_completion_seals_without_an_evaluation_binding() {
    let mut fixture = fixture("project-self-asserted-completion");
    let seal = complete(
        &mut fixture.store,
        &fixture.work,
        &fixture.claim,
        "runner",
        &fixture.evidence,
        "complete-self-asserted",
        5,
    )
    .expect("self-asserted completion");
    assert_eq!(seal.acceptance_evaluation, None);
    assert_eq!(
        fixture
            .store
            .acceptance_evaluation_status(fixture.work.work_id, None)
            .expect("status read"),
        None
    );
    // A self-asserted seal binds nothing and scans healthy under the binding check.
    let report = fixture.store.verify_all().expect("scan");
    assert!(report.is_healthy(), "{report:?}");
}

// B03, B18, B24, B34: a record binds the exact revision, run, and evaluated
// cut; identical attempts replay; explicit keys separate retries from
// deliberate re-evaluations; the newest valid record decides.
#[test]
fn record_binds_the_cut_and_distinguishes_retries_from_re_evaluations() {
    let mut fixture = fixture("project-evaluation-record");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-same-session",
        5,
    );
    let gate_pass = gate(
        store,
        &fixture.work,
        &fixture.claim,
        "runner",
        "cargo-test",
        &[],
        6,
    );
    let pass = request(
        &fixture.work,
        cut(store, &fixture.work),
        "runner",
        Mode::SameSession,
        vec![verdict(
            1,
            AcceptanceVerdict::Pass,
            AcceptanceBasis::Asserted,
            std::slice::from_ref(&gate_pass),
        )],
        7,
    );
    let first = record(store, &pass).expect("record first evaluation");
    assert!(!first.replayed);
    let head = store
        .work_feed_head(&FeedId::RunExecution(fixture.claim.run_id))
        .expect("run feed head");
    assert_eq!(first.record.evaluated_cut.position, head - 1);
    assert_eq!(first.record.work_revision, fixture.work.revision);
    assert_eq!(first.record.run_id, fixture.claim.run_id);
    assert_eq!(first.record.criteria, fixture.work.acceptance);
    assert!(first.record.evidence_basis.contains(&gate_pass));
    assert!(first.record.attempt_key.starts_with("content:"));
    assert_eq!(first.record.verdicts[0].evidence, vec![gate_pass.clone()]);

    let replayed = record(
        store,
        &RecordAcceptanceEvaluationRequest {
            recorded_at: at(8),
            ..pass.clone()
        },
    )
    .expect("identical resend replays");
    assert!(replayed.replayed);
    assert_eq!(replayed.evaluation, first.evaluation);
    assert_eq!(replayed.record, first.record);

    let keyed = record(
        store,
        &RecordAcceptanceEvaluationRequest {
            attempt_key: Some("attempt-1".into()),
            ..pass.clone()
        },
    )
    .expect("explicit attempt records a distinct evaluation");
    assert!(!keyed.replayed);
    assert_ne!(keyed.evaluation, first.evaluation);
    assert_eq!(
        keyed.record.attempt_key,
        format!(
            "explicit:{}:{}:attempt-1",
            fixture.work.work_id.0, fixture.claim.run_id.0
        )
    );
    let mut contradicting = pass.clone();
    contradicting.attempt_key = Some("attempt-1".into());
    contradicting.verdicts[0].rationale = "criterion 1: changed my mind".into();
    assert!(matches!(
        record(store, &contradicting),
        Err(StoreError::WorkOperationIdempotencyConflict { .. })
    ));
    let blank_key = refusal(record(
        store,
        &RecordAcceptanceEvaluationRequest {
            attempt_key: Some("  ".into()),
            ..pass.clone()
        },
    ));
    assert!(blank_key.contains("attempt key"), "{blank_key}");

    let mut re_evaluated = pass.clone();
    re_evaluated.verdicts[0].rationale = "criterion 1: re-evaluated after review".into();
    let newest = record(store, &re_evaluated).expect("changed content is a new evaluation");
    assert!(!newest.replayed);
    assert_ne!(newest.evaluation, keyed.evaluation);
    let status = store
        .acceptance_evaluation_status(fixture.work.work_id, None)
        .expect("status read")
        .expect("newest evaluation");
    assert_eq!(status.evaluation, newest.evaluation);
    assert_eq!(status.stale, None);

    let report = store.verify_all().expect("integrity scan");
    assert!(
        report.invalid_work_records.is_empty() && report.invalid_objects.is_empty(),
        "evaluations must not corrupt feeds: {report:?}"
    );
}

// B10, B11, B12, B14, B17: structure and provenance are validated at record time;
// relevance is not judged. A pass needs a citation and a rationale; an
// asserted pass needs a clean gate; failing checks may back non-pass verdicts.
#[test]
fn verdict_structure_and_provenance_are_validated_at_record_time() {
    let mut fixture = fixture("project-evaluation-verdicts");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession, Mode::SubAgent],
        MechanicalBasis::Asserted,
        false,
        "enable-same-and-sub",
        5,
    );
    let work = fixture.work.clone();
    let claim = fixture.claim.clone();
    let gate_pass = gate(store, &work, &claim, "runner", "cargo-test", &[], 6);
    let gate_fail = gate(
        store,
        &work,
        &claim,
        "runner",
        "cargo-clippy",
        &["clippy::needless_return"],
        7,
    );
    let note = fixture.evidence.clone();
    let stranger = ObjectId::from_canonical_bytes(b"not on this run");
    let cases: Vec<(CriterionVerdictInput, &str)> = vec![
        (
            verdict(1, AcceptanceVerdict::Pass, AcceptanceBasis::Judgment, &[]),
            "passes without a citation",
        ),
        (
            verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Judgment,
                std::slice::from_ref(&stranger),
            ),
            "not evidence on this run",
        ),
        (
            verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Asserted,
                std::slice::from_ref(&gate_fail),
            ),
            "gate record with no failure labels",
        ),
        (
            verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Asserted,
                std::slice::from_ref(&note),
            ),
            "gate record with no failure labels",
        ),
        (
            verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Observed,
                std::slice::from_ref(&gate_pass),
            ),
            "host-minted verification evidence",
        ),
        (
            verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::HumanRequired,
                std::slice::from_ref(&gate_pass),
            ),
            "human_required",
        ),
        (
            CriterionVerdictInput {
                rationale: "   ".into(),
                ..verdict(
                    1,
                    AcceptanceVerdict::Fail,
                    AcceptanceBasis::Judgment,
                    std::slice::from_ref(&gate_fail),
                )
            },
            "needs a rationale",
        ),
        (
            verdict(2, AcceptanceVerdict::Fail, AcceptanceBasis::Judgment, &[]),
            "exactly the 1 current criteria",
        ),
        (
            verdict(0, AcceptanceVerdict::Fail, AcceptanceBasis::Judgment, &[]),
            "one-based",
        ),
    ];
    for (input, expected) in cases {
        let reason = refusal(record(
            store,
            &request(
                &work,
                cut(store, &work),
                "runner",
                Mode::SameSession,
                vec![input.clone()],
                8,
            ),
        ));
        assert!(reason.contains(expected), "{input:?}: {reason}");
    }
    let duplicate = refusal(record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            vec![
                verdict(1, AcceptanceVerdict::Fail, AcceptanceBasis::Judgment, &[]),
                verdict(1, AcceptanceVerdict::Fail, AcceptanceBasis::Judgment, &[]),
            ],
            8,
        ),
    ));
    assert!(duplicate.contains("exactly once"), "{duplicate}");
    let empty = refusal(record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            Vec::new(),
            8,
        ),
    ));
    assert!(empty.contains("one verdict per criterion"), "{empty}");
    let stray_identity = refusal(record(
        store,
        &RecordAcceptanceEvaluationRequest {
            execution_identity: Some("exec".into()),
            ..request(
                &work,
                cut(store, &work),
                "runner",
                Mode::SameSession,
                vec![verdict(
                    1,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Judgment,
                    std::slice::from_ref(&note),
                )],
                8,
            )
        },
    ));
    assert!(
        stray_identity.contains("belong to sub_agent mode only"),
        "{stray_identity}"
    );

    let accepted = [
        verdict(
            1,
            AcceptanceVerdict::Pass,
            AcceptanceBasis::Asserted,
            std::slice::from_ref(&gate_pass),
        ),
        verdict(
            1,
            AcceptanceVerdict::Pass,
            AcceptanceBasis::Judgment,
            std::slice::from_ref(&note),
        ),
        verdict(
            1,
            AcceptanceVerdict::Fail,
            AcceptanceBasis::Asserted,
            std::slice::from_ref(&gate_fail),
        ),
        verdict(
            1,
            AcceptanceVerdict::InsufficientEvidence,
            AcceptanceBasis::Judgment,
            &[],
        ),
        verdict(
            1,
            AcceptanceVerdict::NeedsHuman,
            AcceptanceBasis::HumanRequired,
            &[],
        ),
    ];
    for (index, input) in accepted.into_iter().enumerate() {
        let receipt = record(
            store,
            &request(
                &work,
                cut(store, &work),
                "runner",
                Mode::SameSession,
                vec![input.clone()],
                9,
            ),
        )
        .unwrap_or_else(|error| panic!("case {index} {input:?} must record: {error:?}"));
        assert_eq!(receipt.record.verdicts[0].verdict, input.verdict);
    }
}

// B05, B06, B07: identity is checked per mode against the live holder and
// executor; sub-agent evaluations carry a distinct execution identity and an
// attested parent that must execute the run. Identity stays asserted.
#[test]
fn identity_admission_follows_the_selected_mode() {
    let mut fixture = fixture("project-evaluation-identity");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession, Mode::SubAgent, Mode::IndependentSession],
        MechanicalBasis::Asserted,
        false,
        "enable-all-modes",
        5,
    );
    let work = fixture.work.clone();
    let note = fixture.evidence.clone();
    let pass = |position: usize| {
        verdict(
            position,
            AcceptanceVerdict::Pass,
            AcceptanceBasis::Judgment,
            std::slice::from_ref(&note),
        )
    };
    let by_stranger = refusal(record(
        store,
        &request(
            &work,
            cut(store, &work),
            "reviewer",
            Mode::SameSession,
            vec![pass(1)],
            6,
        ),
    ));
    assert!(
        by_stranger.contains("same_session evaluation must come from the session"),
        "{by_stranger}"
    );
    let by_holder = refusal(record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::IndependentSession,
            vec![pass(1)],
            6,
        ),
    ));
    assert!(
        by_holder.contains("neither holds nor executes"),
        "{by_holder}"
    );
    let independent = record(
        store,
        &request(
            &work,
            cut(store, &work),
            "reviewer",
            Mode::IndependentSession,
            vec![pass(1)],
            6,
        ),
    )
    .expect("independent session evaluates");
    assert_eq!(
        independent.record.evaluator.assurance,
        AssuranceLevel::Asserted
    );
    assert_eq!(independent.record.mode, Mode::IndependentSession);

    let orphaned = refusal(record(
        store,
        &RecordAcceptanceEvaluationRequest {
            execution_identity: Some("sub-agent-exec".into()),
            parent_session: Some(SessionId("reviewer".into())),
            ..request(
                &work,
                cut(store, &work),
                "sub-agent",
                Mode::SubAgent,
                vec![pass(1)],
                7,
            )
        },
    ));
    assert!(
        orphaned.contains("parent session must hold or execute"),
        "{orphaned}"
    );
    let sub_agent = record(
        store,
        &RecordAcceptanceEvaluationRequest {
            execution_identity: Some("sub-agent-exec".into()),
            parent_session: Some(SessionId("runner".into())),
            evaluator_model: Some(crate::domain::EvaluatorModel {
                provider: "provider-a".into(),
                model: "model-b".into(),
                version: None,
            }),
            ..request(
                &work,
                cut(store, &work),
                "sub-agent",
                Mode::SubAgent,
                vec![pass(1)],
                7,
            )
        },
    )
    .expect("attested sub-agent evaluates");
    assert_eq!(
        sub_agent.record.execution_identity.as_deref(),
        Some("sub-agent-exec")
    );
    assert_eq!(
        sub_agent.record.parent_session,
        Some(SessionId("runner".into()))
    );
    assert_eq!(
        sub_agent
            .record
            .evaluator_model
            .as_ref()
            .map(|model| model.model.as_str()),
        Some("model-b")
    );
}

// B08: a task may pin one mode by revision; evaluations in another mode
// are refused with guidance, and a set/clear conflict is refused.
#[test]
fn task_mode_selection_constrains_evaluations() {
    let mut fixture = fixture("project-evaluation-task-mode");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession, Mode::IndependentSession],
        MechanicalBasis::Asserted,
        false,
        "enable-two-modes",
        5,
    );
    let note = fixture.evidence.clone();
    assert!(matches!(
        revise(
            store,
            &fixture.work,
            &fixture.claim,
            WorkRevisionPatch {
                evaluation_mode: Some(Mode::IndependentSession),
                clear_evaluation_mode: true,
                ..empty_patch()
            },
            "revise-conflict",
            6,
        ),
        Err(StoreError::InvalidWork(reason)) if reason.contains("evaluation mode")
    ));
    let pinned = revise(
        store,
        &fixture.work,
        &fixture.claim,
        WorkRevisionPatch {
            evaluation_mode: Some(Mode::IndependentSession),
            ..empty_patch()
        },
        "revise-pin-independent",
        6,
    )
    .expect("pin the independent mode");
    assert_eq!(pinned.evaluation_mode, Some(Mode::IndependentSession));
    assert_eq!(pinned.revision, fixture.work.revision + 1);
    let same_session = refusal(record(
        store,
        &request(
            &pinned,
            cut(store, &pinned),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Judgment,
                std::slice::from_ref(&note),
            )],
            7,
        ),
    ));
    assert!(
        same_session.contains("selects mode independent_session"),
        "{same_session}"
    );
    let cleared = revise(
        store,
        &pinned,
        &fixture.claim,
        WorkRevisionPatch {
            clear_evaluation_mode: true,
            ..empty_patch()
        },
        "revise-clear-mode",
        8,
    )
    .expect("clear the pinned mode");
    assert_eq!(cleared.evaluation_mode, None);
    record(
        store,
        &request(
            &cleared,
            cut(store, &cleared),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Judgment,
                std::slice::from_ref(&note),
            )],
            9,
        ),
    )
    .expect("any allowed mode after clearing");
}

// B02, B13, B15, B16, B19, B22, B25, B26, B28: completion under an evaluated
// policy refuses self-assertion, requires a fresh all-pass record, treats a
// revision change as staleness, ignores non-mutating notes, and seals the
// derived vector bound to the consumed evaluation.
#[test]
fn completion_consumes_only_a_fresh_passing_evaluation() {
    let mut fixture = fixture("project-evaluated-completion");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-same-session",
        5,
    );
    let work = fixture.work.clone();
    let claim = fixture.claim.clone();
    let evidence = fixture.evidence.clone();
    let explicit = checkpoint_then_complete(
        store,
        &work,
        &claim,
        "runner",
        std::slice::from_ref(&evidence),
        true,
        None,
        "complete-explicit",
        6,
    );
    assert!(
        matches!(
            &explicit,
            Err(StoreError::WorkCompletionRefused { reason, .. })
                if reason.contains("record an acceptance evaluation with evaluate")
        ),
        "{explicit:?}"
    );
    assert!(matches!(
        recovery_cause(complete_evaluated(
            store,
            &work,
            &claim,
            "runner",
            &evidence,
            None,
            "complete-missing",
            6,
        )),
        WorkCompletionRecoveryCause::MissingAcceptanceEvaluation { criterion }
            if criterion == "root accepted"
    ));

    let gate_pass = gate(store, &work, &claim, "runner", "cargo-test", &[], 7);
    let non_passing = [
        (AcceptanceVerdict::Fail, AcceptanceBasis::Judgment),
        (
            AcceptanceVerdict::InsufficientEvidence,
            AcceptanceBasis::Judgment,
        ),
        (
            AcceptanceVerdict::NeedsHuman,
            AcceptanceBasis::HumanRequired,
        ),
    ];
    for (index, (outcome, basis)) in non_passing.into_iter().enumerate() {
        record(
            store,
            &request(
                &work,
                cut(store, &work),
                "runner",
                Mode::SameSession,
                vec![verdict(1, outcome, basis, &[])],
                8,
            ),
        )
        .expect("record a non-passing evaluation");
        let cause = recovery_cause(complete_evaluated(
            store,
            &work,
            &claim,
            "runner",
            &evidence,
            None,
            &format!("complete-non-passing-{index}"),
            9,
        ));
        match outcome {
            AcceptanceVerdict::Fail => assert!(matches!(
                cause,
                WorkCompletionRecoveryCause::AcceptanceFailed { .. }
            )),
            AcceptanceVerdict::InsufficientEvidence => assert!(matches!(
                cause,
                WorkCompletionRecoveryCause::AcceptanceInsufficientEvidence { .. }
            )),
            AcceptanceVerdict::NeedsHuman => assert!(matches!(
                cause,
                WorkCompletionRecoveryCause::AcceptanceNeedsHuman { .. }
            )),
            AcceptanceVerdict::Pass => unreachable!(),
        }
    }

    let passing = record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Asserted,
                std::slice::from_ref(&gate_pass),
            )],
            10,
        ),
    )
    .expect("record the passing evaluation");
    let revised = revise(
        store,
        &work,
        &claim,
        WorkRevisionPatch {
            title: Some("Ship local work, revised".into()),
            ..empty_patch()
        },
        "revise-title",
        11,
    )
    .expect("revise the item");
    assert!(matches!(
        recovery_cause(complete_evaluated(
            store,
            &revised,
            &claim,
            "runner",
            &evidence,
            None,
            "complete-after-revision",
            12,
        )),
        WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
            reason: AcceptanceStaleReason::Revision
        }
    ));
    let status = store
        .acceptance_evaluation_status(work.work_id, None)
        .expect("status read")
        .expect("stale evaluation is still visible");
    assert_eq!(status.evaluation, passing.evaluation);
    assert_eq!(status.stale, Some(AcceptanceStaleReason::Revision));

    let renewed = record(
        store,
        &request(
            &revised,
            cut(store, &revised),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Asserted,
                std::slice::from_ref(&gate_pass),
            )],
            13,
        ),
    )
    .expect("re-evaluate the revised item");
    let late_note = self::evidence(store, &revised, &claim, "runner", "late-note", 14);
    // The direct core completion names the cited gate in its evidence set;
    // the closure invariant refuses a set that omits a citation.
    let final_evidence = [evidence.clone(), late_note, gate_pass.clone()];
    let seal = checkpoint_then_complete(
        store,
        &revised,
        &claim,
        "runner",
        &final_evidence,
        false,
        None,
        "complete-evaluated",
        16,
    )
    .expect("notes and checkpoints after the evaluation do not invalidate it");
    assert_eq!(seal.acceptance_evaluation, Some(renewed.evaluation.clone()));
    assert_eq!(seal.acceptance.len(), 1);
    assert!(seal.acceptance[0].satisfied);
    assert_eq!(seal.acceptance[0].criterion, "root accepted");
    assert_eq!(seal.acceptance[0].evidence, vec![gate_pass]);
    assert!(seal.acceptance[0].note.starts_with("pass (asserted)"));
    let replay = checkpoint_then_complete(
        store,
        &revised,
        &claim,
        "runner",
        &final_evidence,
        false,
        None,
        "complete-evaluated",
        16,
    )
    .expect("completion replay");
    assert_eq!(replay, seal);
    let report = store.verify_all().expect("integrity scan");
    assert!(
        report.invalid_work_records.is_empty() && report.invalid_objects.is_empty(),
        "{report:?}"
    );
}

// B09, B10, B12, B20: observed passes cite host-minted passed verification only;
// an observed policy refuses gate records as mechanical proof; a host-observed
// source mutation after the evaluated cut makes the evaluation stale, while a
// failed host check may back a non-pass verdict.
#[test]
fn host_observed_evidence_governs_mechanical_passes_and_freshness() {
    let mut fixture = fixture("project-evaluation-observed");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Observed,
        false,
        "enable-observed",
        5,
    );
    disable_obligation_rules(store, 6);
    let work = fixture.work.clone();
    let claim = fixture.claim.clone();
    let evidence = fixture.evidence.clone();
    let mut host = HostSession::bind(store, &work, &claim, 10);
    let passed = host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    );
    assert_eq!(passed.len(), 1);
    assert_eq!(
        store
            .load_verification_evidence(&passed[0])
            .expect("load verification")
            .result,
        VerificationResult::Passed
    );
    let gate_pass = gate(store, &work, &claim, "runner", "cargo-test", &[], 25);
    let asserted = refusal(record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Asserted,
                std::slice::from_ref(&gate_pass),
            )],
            26,
        ),
    ));
    assert!(
        asserted.contains("requires observed check evidence"),
        "{asserted}"
    );
    let gate_as_observed = refusal(record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Observed,
                std::slice::from_ref(&gate_pass),
            )],
            26,
        ),
    ));
    assert!(
        gate_as_observed.contains("host-minted verification evidence"),
        "{gate_as_observed}"
    );
    let observed = record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Observed,
                &passed,
            )],
            27,
        ),
    )
    .expect("observed pass cites host verification");
    assert_eq!(
        store
            .acceptance_evaluation_status(work.work_id, None)
            .expect("status read")
            .map(|status| status.stale),
        Some(None)
    );

    let none = host.checkpoint(store, true, None, 30);
    assert!(none.is_empty());
    assert!(matches!(
        recovery_cause(complete_evaluated(
            store,
            &work,
            &claim,
            "runner",
            &evidence,
            None,
            "complete-after-mutation",
            35,
        )),
        WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
            reason: AcceptanceStaleReason::Mutation
        }
    ));
    assert_eq!(
        store
            .acceptance_evaluation_status(work.work_id, None)
            .expect("status read")
            .map(|status| (status.evaluation, status.stale)),
        Some((observed.evaluation, Some(AcceptanceStaleReason::Mutation)))
    );

    let failed = host.checkpoint(
        store,
        false,
        Some((VerificationKind::Build, ExecutionOutcome::Failed)),
        40,
    );
    assert_eq!(
        store
            .load_verification_evidence(&failed[0])
            .expect("load failed verification")
            .result,
        VerificationResult::Failed
    );
    let failed_as_pass = refusal(record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Observed,
                &failed,
            )],
            45,
        ),
    ));
    assert!(failed_as_pass.contains("passed result"), "{failed_as_pass}");
    record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Fail,
                AcceptanceBasis::Observed,
                &failed,
            )],
            46,
        ),
    )
    .expect("a failed build backs a failing verdict");
    assert!(matches!(
        recovery_cause(complete_evaluated(
            store,
            &work,
            &claim,
            "runner",
            &evidence,
            None,
            "complete-after-failure",
            47,
        )),
        WorkCompletionRecoveryCause::AcceptanceFailed { .. }
    ));

    let repaired = host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        50,
    );
    let final_evaluation = record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Observed,
                &repaired,
            )],
            55,
        ),
    )
    .expect("observed pass after the repair");
    // The direct core completion names the cited verification evidence in
    // its evidence set; the closure invariant refuses a set that omits it.
    let mut final_evidence = vec![evidence.clone()];
    final_evidence.extend(repaired.iter().cloned());
    let seal = checkpoint_then_complete(
        store,
        &work,
        &claim,
        "runner",
        &final_evidence,
        false,
        None,
        "complete-observed",
        56,
    )
    .expect("fresh observed pass completes");
    assert_eq!(
        seal.acceptance_evaluation,
        Some(final_evaluation.evaluation)
    );
    assert_eq!(seal.acceptance[0].evidence, repaired);
}

// B21, B22: with source freshness required, the host-measured fingerprint at
// completion must equal the one the evaluation recorded.
#[test]
fn source_freshness_binds_the_host_fingerprint() {
    let mut fixture = fixture("project-evaluation-source");
    enable(
        &mut fixture.store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        true,
        "enable-source-freshness",
        5,
    );
    let store = &mut fixture.store;
    let work = fixture.work.clone();
    let claim = fixture.claim.clone();
    let evidence = fixture.evidence.clone();
    let mut pass = request(
        &work,
        cut(store, &work),
        "runner",
        Mode::SameSession,
        vec![verdict(
            1,
            AcceptanceVerdict::Pass,
            AcceptanceBasis::Judgment,
            std::slice::from_ref(&evidence),
        )],
        6,
    );
    record(store, &pass).expect("record without a source basis");
    assert!(matches!(
        recovery_cause(complete_evaluated(
            store,
            &work,
            &claim,
            "runner",
            &evidence,
            Some("fingerprint-1"),
            "complete-unfingerprinted",
            7,
        )),
        WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
            reason: AcceptanceStaleReason::Source
        }
    ));
    pass.source_basis = Some(AcceptanceSourceBasis {
        workspace_id: Some("workspace-source".into()),
        fingerprint: "fingerprint-1".into(),
    });
    let blank = refusal(record(
        store,
        &RecordAcceptanceEvaluationRequest {
            source_basis: Some(AcceptanceSourceBasis {
                workspace_id: None,
                fingerprint: " ".into(),
            }),
            ..pass.clone()
        },
    ));
    assert!(blank.contains("must not be blank"), "{blank}");
    record(store, &pass).expect("record with a source basis");
    for (presented, key) in [
        (None, "complete-no-fingerprint"),
        (Some("fingerprint-2"), "complete-other"),
    ] {
        assert!(matches!(
            recovery_cause(complete_evaluated(
                store, &work, &claim, "runner", &evidence, presented, key, 8,
            )),
            WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
                reason: AcceptanceStaleReason::Source
            }
        ));
    }
    assert_eq!(
        store
            .acceptance_evaluation_status(work.work_id, Some("fingerprint-1"))
            .expect("status read")
            .map(|status| status.stale),
        Some(None)
    );
    complete_evaluated(
        store,
        &work,
        &claim,
        "runner",
        &evidence,
        Some("fingerprint-1"),
        "complete-fresh-source",
        9,
    )
    .expect("matching fingerprint completes");
}

// B32, B01: a policy change after the record makes it stale; returning to the
// self-asserted policy restores self-asserted completion and ignores evaluations.
#[test]
fn policy_changes_invalidate_and_self_asserted_restores_self_assertion() {
    let mut fixture = fixture("project-evaluation-policy-change");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-same-session",
        5,
    );
    let work = fixture.work.clone();
    let claim = fixture.claim.clone();
    let evidence = fixture.evidence.clone();
    record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Judgment,
                std::slice::from_ref(&evidence),
            )],
            6,
        ),
    )
    .expect("record under the same-session policy");
    enable(
        store,
        &[Mode::IndependentSession],
        MechanicalBasis::Asserted,
        false,
        "switch-to-independent",
        7,
    );
    assert!(matches!(
        recovery_cause(complete_evaluated(
            store,
            &work,
            &claim,
            "runner",
            &evidence,
            None,
            "complete-after-policy-change",
            8,
        )),
        WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
            reason: AcceptanceStaleReason::Policy
        }
    ));
    let self_asserted = enable(
        store,
        &[],
        MechanicalBasis::Asserted,
        false,
        "return-to-self-asserted",
        9,
    );
    assert!(self_asserted.acceptance_evaluation.is_self_asserted());
    let seal = checkpoint_then_complete(
        store,
        &work,
        &claim,
        "runner",
        std::slice::from_ref(&evidence),
        true,
        None,
        "complete-self-asserted-again",
        10,
    )
    .expect("self-asserted completion after the policy returns");
    assert_eq!(seal.acceptance_evaluation, None);
}

// B31: the submission names the cut the evaluator read; a host-observed
// change after it, a cut ahead of the feed, or a citation beyond it refuses,
// and the record binds the supplied cut rather than the head at submission.
#[test]
fn submissions_behind_a_host_observed_change_are_refused() {
    let mut fixture = fixture("project-evaluation-race");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-race",
        5,
    );
    disable_obligation_rules(store, 6);
    let work = fixture.work.clone();
    let claim = fixture.claim.clone();
    let note = fixture.evidence.clone();
    let mut host = HostSession::bind(store, &work, &claim, 10);
    let read_before_mutation = cut(store, &work);
    let passed = host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    );
    let raced = refusal(record(
        store,
        &request(
            &work,
            read_before_mutation,
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Judgment,
                std::slice::from_ref(&note),
            )],
            25,
        ),
    ));
    assert!(raced.contains("changed after evidence basis"), "{raced}");
    let ahead = refusal(record(
        store,
        &request(
            &work,
            cut(store, &work) + 1,
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Judgment,
                std::slice::from_ref(&note),
            )],
            26,
        ),
    ));
    assert!(ahead.contains("not a position"), "{ahead}");
    let read_before_gate = cut(store, &work);
    let late_gate = gate(store, &work, &claim, "runner", "late-gate", &[], 30);
    let beyond = refusal(record(
        store,
        &request(
            &work,
            read_before_gate,
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Asserted,
                std::slice::from_ref(&late_gate),
            )],
            31,
        ),
    ));
    assert!(beyond.contains("beyond evidence basis"), "{beyond}");
    let current = cut(store, &work);
    let recorded = record(
        store,
        &request(
            &work,
            current,
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Observed,
                &passed,
            )],
            32,
        ),
    )
    .expect("the current basis records");
    assert_eq!(recorded.record.evaluated_cut.position, current);
    assert!(recorded.record.evidence_basis.contains(&passed[0]));
    assert!(recorded.record.evidence_basis.contains(&late_gate));
    assert!(recorded.record.evidence_basis.contains(&note));
    assert_eq!(
        store
            .acceptance_evaluation_status(work.work_id, None)
            .expect("status read")
            .map(|status| status.stale),
        Some(None)
    );
}

// B32: a strengthened policy retires the weaker record it would no longer
// admit, whether the mechanical basis or the source-freshness requirement grew.
#[test]
fn policy_strengthening_retires_weaker_passes() {
    let mut fixture = fixture("project-evaluation-strengthen");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-asserted",
        5,
    );
    let work = fixture.work.clone();
    let claim = fixture.claim.clone();
    let evidence = fixture.evidence.clone();
    let gate_pass = gate(store, &work, &claim, "runner", "cargo-test", &[], 6);
    record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Asserted,
                std::slice::from_ref(&gate_pass),
            )],
            7,
        ),
    )
    .expect("asserted pass under the asserted policy");
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Observed,
        false,
        "strengthen-mechanical",
        8,
    );
    assert!(matches!(
        recovery_cause(complete_evaluated(
            store,
            &work,
            &claim,
            "runner",
            &evidence,
            None,
            "complete-under-observed",
            9,
        )),
        WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
            reason: AcceptanceStaleReason::Policy
        }
    ));
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        true,
        "strengthen-freshness",
        10,
    );
    assert!(matches!(
        recovery_cause(complete_evaluated(
            store,
            &work,
            &claim,
            "runner",
            &evidence,
            Some("fingerprint-now"),
            "complete-under-freshness",
            11,
        )),
        WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
            reason: AcceptanceStaleReason::Source
        }
    ));
}

// B33, B22: a newer record of a gate the pass relied on retires the pass;
// a gate the pass did not cite leaves it fresh.
#[test]
fn a_newer_record_of_a_cited_gate_retires_the_pass() {
    let mut fixture = fixture("project-evaluation-superseded");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-superseded",
        5,
    );
    let work = fixture.work.clone();
    let claim = fixture.claim.clone();
    let evidence = fixture.evidence.clone();
    let gate_pass = gate(store, &work, &claim, "runner", "cargo-test", &[], 6);
    record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Asserted,
                std::slice::from_ref(&gate_pass),
            )],
            7,
        ),
    )
    .expect("asserted pass citing cargo-test");
    gate(store, &work, &claim, "runner", "cargo-clippy", &[], 8);
    let stale_after_unrelated = store
        .acceptance_evaluation_status(work.work_id, None)
        .expect("status read")
        .and_then(|status| status.stale);
    assert_eq!(stale_after_unrelated, None);
    gate(
        store,
        &work,
        &claim,
        "runner",
        "cargo-test",
        &["suite::regression"],
        9,
    );
    assert_eq!(
        store
            .acceptance_evaluation_status(work.work_id, None)
            .expect("status read")
            .and_then(|status| status.stale),
        Some(AcceptanceStaleReason::Evidence)
    );
    assert!(matches!(
        recovery_cause(complete_evaluated(
            store,
            &work,
            &claim,
            "runner",
            &evidence,
            None,
            "complete-after-superseded-gate",
            10,
        )),
        WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
            reason: AcceptanceStaleReason::Evidence
        }
    ));
}

// B34: the newest record decides; an older pass is never selected around a
// newer non-passing verdict.
#[test]
fn a_newer_non_passing_record_blocks_an_older_pass() {
    let mut fixture = fixture("project-evaluation-newest");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-newest",
        5,
    );
    let work = fixture.work.clone();
    let claim = fixture.claim.clone();
    let evidence = fixture.evidence.clone();
    record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Judgment,
                std::slice::from_ref(&evidence),
            )],
            6,
        ),
    )
    .expect("older pass");
    for (index, (later, basis)) in [
        (
            AcceptanceVerdict::NeedsHuman,
            AcceptanceBasis::HumanRequired,
        ),
        (
            AcceptanceVerdict::InsufficientEvidence,
            AcceptanceBasis::Judgment,
        ),
        (AcceptanceVerdict::Fail, AcceptanceBasis::Judgment),
    ]
    .into_iter()
    .enumerate()
    {
        record(
            store,
            &request(
                &work,
                cut(store, &work),
                "runner",
                Mode::SameSession,
                vec![verdict(1, later, basis, &[])],
                7,
            ),
        )
        .expect("newer non-passing record");
        let cause = recovery_cause(complete_evaluated(
            store,
            &work,
            &claim,
            "runner",
            &evidence,
            None,
            &format!("complete-newest-{index}"),
            8,
        ));
        match later {
            AcceptanceVerdict::NeedsHuman => assert!(matches!(
                cause,
                WorkCompletionRecoveryCause::AcceptanceNeedsHuman { .. }
            )),
            AcceptanceVerdict::InsufficientEvidence => assert!(matches!(
                cause,
                WorkCompletionRecoveryCause::AcceptanceInsufficientEvidence { .. }
            )),
            AcceptanceVerdict::Fail => assert!(matches!(
                cause,
                WorkCompletionRecoveryCause::AcceptanceFailed { .. }
            )),
            AcceptanceVerdict::Pass => unreachable!(),
        }
    }
}
