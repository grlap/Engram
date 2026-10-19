//! Real host opening/evaluate/done/closing sequences. Recording a quiet
//! closing endpoint is evidence even though it opens no source obligation.

use super::*;
use crate::domain::{
    ExecutionObserveInput, MeasuredBaseline, MeasuredSighting, NamedRootState,
    ObservationAccounting, ObservationAuditReason, ObservationCausality, ObservationPolicyBasis,
    ObservationRootBasis, ObservedInterval, ObservedOccurrence, ObservedSourceChange,
};

pub(crate) struct SealedSourceTransportFixture {
    pub database: std::path::PathBuf,
    pub work: WorkItem,
    pub seal: ObjectId,
    owner: Fixture,
}

impl SealedSourceTransportFixture {
    pub(crate) fn append_later_sighting(&mut self) {
        let fixture = &mut self.owner;
        let diagnostic = fixture
            .store
            .sealed_source_page(&self.seal, None, None, 0)
            .unwrap()
            .diagnostic;
        let observer = bind_observer(fixture, "appending-observer", 80);
        let input = ExecutionObserveInput {
            idempotency_key: "appended-after-window".into(),
            binding: ControlWorkBinding {
                root_execution_id: diagnostic.root_execution,
                work_id: fixture.work.work_id,
                run_id: diagnostic.run,
                work_revision: fixture.claim.accepted_work_revision,
                claim_id: diagnostic.claim,
                claim_fence: diagnostic.claim_fence,
            },
            root_basis: ObservationRootBasis {
                capture_run_cut: diagnostic.evaluated_cut.unwrap(),
                latest_event: diagnostic.named_root_binding,
                state: NamedRootState::Bound {
                    workspace_id: "workspace-B".into(),
                    generation: 9,
                    named_at: at(20),
                },
            },
            observed_interval: ObservedInterval {
                from: at(47),
                through: at(49),
            },
            occurrence: ObservedOccurrence::InterTurnChange {
                source_change: ObservedSourceChange::AssumedMissingBaseline {
                    workspace_id: "workspace-B".into(),
                    sighting: MeasuredSighting {
                        source_basis: workspace("workspace-B", "C", Some(9)),
                        observed_at: at(49),
                    },
                },
            },
            causality: ObservationCausality::Unknown {},
            policy_basis: ObservationPolicyBasis::AuditOnly {},
        };
        fixture
            .store
            .record_unadmitted_execution_observation(
                &fixture.work.project_id,
                &observer.status.session_id,
                &observer.connection_token,
                &observer.routing_token,
                &actor("appending-observer"),
                input,
                at(90),
            )
            .unwrap();
    }
}

pub(crate) fn sealed_source_transport_fixture(case: &str) -> SealedSourceTransportFixture {
    let (mut fixture, host, grant, seal) = prepared("sealed-source-transports");
    let reports = match case {
        "difference" => {
            let mut reports = vec![quiet(
                &host,
                "different",
                Some(workspace("workspace-B", "B", Some(9))),
            )];
            reports.extend(
                (0..39)
                    .map(|index| quiet(&host, &format!("same-{index}"), Some(host.basis.clone()))),
            );
            reports
        }
        "agreement" => vec![quiet(&host, "same", Some(host.basis.clone()))],
        "missing" => Vec::new(),
        _ => panic!("unknown sealed source fixture"),
    };
    if !reports.is_empty() {
        close(&mut fixture, &host, &grant, &reports);
    }
    SealedSourceTransportFixture {
        database: fixture.directory.path().join("engram.sqlite3"),
        work: fixture.work.clone(),
        seal,
        owner: fixture,
    }
}

fn prepared(project: &str) -> (Fixture, HostSession, IssuedTurnGrant, ObjectId) {
    prepared_on(fixture(project))
}

fn bind_observer(
    fixture: &mut Fixture,
    session: &str,
    second: i64,
) -> crate::storage::test_support::TestControlBinding {
    let session_id = SessionId(session.into());
    let connection_token = fixture
        .store
        .resume_control_connection(&session_id, at(second))
        .unwrap();
    let binding = fixture
        .store
        .bind_control_session(
            &fixture.work.project_id,
            &fixture.work.short_ref,
            "Observe the completed fixture's source",
            &session_id,
            &connection_token,
            &actor(session),
            crate::ControlAssurance::TurnGated,
            &[EffectClass::Observe],
            1,
            "bind-late-observer",
            at(second),
        )
        .unwrap();
    crate::storage::test_support::TestControlBinding {
        binding,
        connection_token,
    }
}

fn prepared_on(fixture: Fixture) -> (Fixture, HostSession, IssuedTurnGrant, ObjectId) {
    prepared_with_prior_root(fixture, None)
}

fn prepared_with_prior_root(
    mut fixture: Fixture,
    prior_generation: Option<i64>,
) -> (Fixture, HostSession, IssuedTurnGrant, ObjectId) {
    let work = fixture.work.clone();
    let claim = fixture.claim.clone();
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "evaluated",
        5,
    );
    let mut host = HostSession::bind(store, &work, &claim, 6);
    if let Some(generation) = prior_generation {
        host_binds(
            store,
            &host,
            &claim,
            "workspace-B",
            generation,
            NamedRootBindingKind::Bound,
            15,
            "prior-named",
            15,
        )
        .unwrap();
    }
    host_binds(
        store,
        &host,
        &claim,
        "workspace-B",
        9,
        NamedRootBindingKind::Bound,
        20,
        "named",
        20,
    )
    .unwrap();
    host.basis = workspace("workspace-B", "A", Some(9));
    host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        30,
    );
    let input = declared_pass(
        &work,
        &fixture.evidence,
        cut(store, &work),
        "A",
        "judged-A",
        40,
    );
    record(store, &input).unwrap();
    let closing = host.grant(store, &[EffectClass::MutateLocal], true, 45);
    host.begin(store, &closing, 46);
    complete_evaluated(
        store,
        &work,
        &claim,
        "runner",
        &fixture.evidence,
        Some("A"),
        "done",
        50,
    )
    .unwrap();
    let seal = load_work_run(&store.connection, claim.run_id)
        .unwrap()
        .completion_seal
        .unwrap();
    (fixture, host, closing, seal)
}

fn quiet(
    host: &HostSession,
    label: &str,
    basis: Option<ExecutionSourceBasis>,
) -> ExecutionObservationInput {
    let observed_at = basis.as_ref().map(|_| at(47));
    ExecutionObservationInput {
        observation_id: host.key(label),
        action_fingerprint: ObjectId::from_canonical_bytes(label.as_bytes()),
        effect: EffectClass::MutateLocal,
        outcome: ExecutionOutcome::Succeeded,
        source_changed: false,
        reported_source_change: None,
        source_basis: basis,
        // Earlier than completion deliberately: classification is feed order,
        // never a timestamp inference about when a write happened.
        observed_at,
    }
}

fn close(
    fixture: &mut Fixture,
    host: &HostSession,
    grant: &IssuedTurnGrant,
    inputs: &[ExecutionObservationInput],
) -> ControlTurnCheckpointDecision {
    let result = fixture
        .store
        .checkpoint_control_turn_with_evidence(
            &host.project_id,
            &host.session_id,
            &host.connection_token,
            &host.routing_token,
            &grant.grant_id,
            TurnNextIntent::Continue,
            inputs,
            &[],
            &[],
            "closing",
            at(60),
        )
        .unwrap();
    assert!(
        matches!(result, ControlTurnCheckpointDecision::Checkpointed { .. }),
        "{result:?}"
    );
    result
}

#[test]
fn sealed_source_quiet_difference_survives_many_later_agreements_and_duplicate_delivery() {
    let (mut fixture, host, grant, seal) = prepared("sealed-source-difference");
    let before = fixture.store.get::<CompletionSeal>(&seal).unwrap().unwrap();
    let obligations = fixture
        .store
        .work_run_obligations(fixture.claim.run_id)
        .unwrap();
    let mut reports = vec![quiet(
        &host,
        "different",
        Some(workspace("workspace-B", "B", Some(9))),
    )];
    reports.extend(
        (0..39).map(|index| quiet(&host, &format!("same-{index}"), Some(host.basis.clone()))),
    );
    close(&mut fixture, &host, &grant, &reports);
    let page = fixture
        .store
        .sealed_source_page(&seal, None, None, 32)
        .unwrap();
    assert_eq!(page.diagnostic.differences, 1);
    assert_eq!(page.diagnostic.agreements, 39);
    let difference = page.diagnostic.first_difference.as_ref().unwrap();
    assert!(
        page.rows
            .iter()
            .all(|row| row.observation != difference.observation),
        "difference deliberately outside the first page"
    );
    assert_eq!(
        page.diagnostic.latest_endpoint.as_ref().unwrap().comparison,
        "agreement"
    );
    assert!(difference.position > before.completion_cut.position);
    let horizon = page.diagnostic.horizon;
    let last = page.rows.last().unwrap();
    let older = fixture
        .store
        .sealed_source_page(&seal, Some(horizon), Some(last.position), 32)
        .unwrap();
    assert_eq!(older.rows.len(), 8);
    assert!(
        older
            .rows
            .iter()
            .any(|row| row.observation == difference.observation)
    );
    close(&mut fixture, &host, &grant, &reports);
    let replay = fixture
        .store
        .sealed_source_page(&seal, Some(horizon), None, 32)
        .unwrap();
    assert_eq!(replay.diagnostic.total, 40);
    assert_eq!(
        replay
            .diagnostic
            .first_difference
            .as_ref()
            .unwrap()
            .observation,
        difference.observation
    );
    assert_eq!(
        fixture.store.get::<CompletionSeal>(&seal).unwrap().unwrap(),
        before
    );
    assert_eq!(
        fixture
            .store
            .work_run_obligations(fixture.claim.run_id)
            .unwrap()
            .len(),
        obligations.len()
    );
    assert_eq!(
        fixture
            .store
            .get_work_item(fixture.work.work_id)
            .unwrap()
            .lifecycle,
        crate::WorkLifecycle::Completed
    );
}

#[test]
fn sealed_source_agreement_is_an_endpoint_and_missing_or_foreign_context_is_not_agreement() {
    // A retained older generation is admissible evidence, but cannot confirm
    // the newer generation bound to the sealed evaluation.
    let (mut fixture, host, grant, seal) =
        prepared_with_prior_root(fixture("sealed-source-incomparable"), Some(8));
    let reports = vec![
        quiet(&host, "same", Some(host.basis.clone())),
        quiet(
            &host,
            "foreign-workspace",
            Some(workspace("other", "A", Some(9))),
        ),
        quiet(
            &host,
            "foreign-generation",
            Some(workspace("workspace-B", "A", Some(8))),
        ),
        quiet(
            &host,
            "missing-generation",
            Some(workspace("workspace-B", "A", None)),
        ),
        quiet(&host, "missing-basis", None),
    ];
    close(&mut fixture, &host, &grant, &reports);
    let page = fixture
        .store
        .sealed_source_page(&seal, None, None, 32)
        .unwrap();
    assert_eq!(page.diagnostic.differences, 0);
    assert_eq!(page.diagnostic.agreements, 1);
    assert_eq!(page.diagnostic.incomparable, 4);
    assert!(page.diagnostic.first_difference.is_none());
    assert_eq!(
        page.diagnostic.latest_endpoint.unwrap().comparison,
        "agreement"
    );
}

#[test]
fn sealed_source_no_closing_sighting_is_explicit_and_reads_append_nothing() {
    let (fixture, _, _, seal) = prepared("sealed-source-missing");
    let before: i64 = fixture
        .store
        .connection
        .query_row("SELECT COUNT(*) FROM objects", [], |r| r.get(0))
        .unwrap();
    let snapshot = test_database_shape_snapshot(&fixture.store.connection).unwrap();
    let page = fixture
        .store
        .sealed_source_page(&seal, None, None, 32)
        .unwrap();
    assert_eq!(page.diagnostic.total, 0);
    assert!(page.diagnostic.judged.is_some());
    assert!(page.diagnostic.latest_endpoint.is_none());
    let after: i64 = fixture
        .store
        .connection
        .query_row("SELECT COUNT(*) FROM objects", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(
        snapshot,
        test_database_shape_snapshot(&fixture.store.connection).unwrap()
    );
}

#[test]
fn sealed_source_missing_evaluation_never_confirms_an_endpoint() {
    let mut fixture = fixture("sealed-source-self-asserted");
    checkpoint_then_complete(
        &mut fixture.store,
        &fixture.work,
        &fixture.claim,
        "runner",
        std::slice::from_ref(&fixture.evidence),
        true,
        None,
        "self-asserted-done",
        6,
    )
    .unwrap();
    let seal = load_work_run(&fixture.store.connection, fixture.claim.run_id)
        .unwrap()
        .completion_seal
        .unwrap();
    let diagnostic = fixture
        .store
        .sealed_source_page(&seal, None, None, 32)
        .unwrap()
        .diagnostic;
    assert!(diagnostic.evaluation.is_none());
    assert!(diagnostic.judged.is_none());
    assert!(
        diagnostic
            .unavailable
            .unwrap()
            .contains("no bound evaluation")
    );
    assert!(diagnostic.latest_endpoint.is_none());
}

#[test]
fn sealed_source_late_child_report_does_not_rewrite_parent_acceptance_or_child_seal_links() {
    let directory = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(directory.path().join("engram.sqlite3")).unwrap();
    let root = store
        .create_work(
            &root_request("sealed-source-child", "parent", 0),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let decomposition = store
        .decompose_work(
            &crate::domain::DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![child("child", crate::ChildRequirement::Required, "Child")],
                prerequisites: Vec::new(),
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "decompose".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let work = decomposition.children[0].clone();
    let child_claim = claim(&mut store, &work, "runner", "claim-child", 2, 3600);
    let note = evidence(&mut store, &work, &child_claim, "runner", "child-note", 3);
    checkpoint(
        &mut store,
        &work,
        &child_claim,
        "runner",
        "child-checkpoint",
        4,
        std::slice::from_ref(&note),
    );
    let (mut fixture, host, grant, child_seal) = prepared_on(Fixture {
        store,
        directory,
        work,
        claim: child_claim,
        evidence: note,
    });
    let root = fixture.store.get_work_item(root.work_id).unwrap();
    let parent_claim = claim(
        &mut fixture.store,
        &root,
        "runner",
        "claim-parent",
        51,
        3600,
    );
    let note = evidence(
        &mut fixture.store,
        &root,
        &parent_claim,
        "runner",
        "parent-note",
        52,
    );
    let input = request(
        &root,
        cut(&fixture.store, &root),
        "runner",
        Mode::SameSession,
        vec![verdict(
            1,
            AcceptanceVerdict::Pass,
            AcceptanceBasis::Judgment,
            std::slice::from_ref(&note),
        )],
        53,
    );
    record(&mut fixture.store, &input).unwrap();
    let parent_before = complete_evaluated(
        &mut fixture.store,
        &root,
        &parent_claim,
        "runner",
        &note,
        None,
        "parent-done",
        54,
    )
    .unwrap();
    let parent_seal = load_work_run(&fixture.store.connection, parent_claim.run_id)
        .unwrap()
        .completion_seal
        .unwrap();
    let child_before: CompletionSeal = fixture.store.get(&child_seal).unwrap().unwrap();
    assert!(parent_before.required_child_seals.contains(&child_seal));
    close(
        &mut fixture,
        &host,
        &grant,
        &[quiet(
            &host,
            "late-child",
            Some(workspace("workspace-B", "B", Some(9))),
        )],
    );
    let snapshot = test_database_shape_snapshot(&fixture.store.connection).unwrap();
    assert_eq!(
        fixture
            .store
            .sealed_source_page(&child_seal, None, None, 32)
            .unwrap()
            .diagnostic
            .differences,
        1
    );
    let parent = fixture
        .store
        .sealed_source_page(&parent_seal, None, None, 32)
        .unwrap();
    assert_eq!(parent.diagnostic.total, 0);
    assert!(parent.diagnostic.judged.is_none());
    assert_eq!(
        fixture
            .store
            .get::<CompletionSeal>(&parent_seal)
            .unwrap()
            .unwrap(),
        parent_before
    );
    assert_eq!(
        fixture
            .store
            .get::<CompletionSeal>(&child_seal)
            .unwrap()
            .unwrap(),
        child_before
    );
    assert_eq!(
        snapshot,
        test_database_shape_snapshot(&fixture.store.connection).unwrap()
    );
}

#[test]
fn sealed_source_unadmitted_finished_run_and_watcher_reports_keep_their_original_accounting() {
    let (mut fixture, _, _, seal) = prepared("sealed-source-unadmitted");
    let diagnostic = fixture
        .store
        .sealed_source_page(&seal, None, None, 0)
        .unwrap()
        .diagnostic;
    let observer = bind_observer(&mut fixture, "observer", 55);
    let policy = SqliteStore::load_active_control_policy(&fixture.store.connection).unwrap();
    let input = ExecutionObserveInput {
        idempotency_key: "late-unadmitted".into(),
        binding: ControlWorkBinding {
            root_execution_id: diagnostic.root_execution,
            work_id: fixture.work.work_id,
            run_id: diagnostic.run,
            work_revision: fixture.claim.accepted_work_revision,
            claim_id: diagnostic.claim,
            claim_fence: diagnostic.claim_fence,
        },
        root_basis: ObservationRootBasis {
            capture_run_cut: diagnostic.evaluated_cut.unwrap(),
            latest_event: diagnostic.named_root_binding,
            state: NamedRootState::Bound {
                workspace_id: "workspace-B".into(),
                generation: 9,
                named_at: at(20),
            },
        },
        observed_interval: ObservedInterval {
            from: at(47),
            through: at(49),
        },
        occurrence: ObservedOccurrence::InterTurnChange {
            source_change: ObservedSourceChange::ContentComparison {
                workspace_id: "workspace-B".into(),
                baseline: MeasuredBaseline {
                    workspace_id: "workspace-B".into(),
                    source_revision: "A".into(),
                    observed_at: at(47),
                },
                sighting: MeasuredSighting {
                    source_basis: workspace("workspace-B", "B", Some(9)),
                    observed_at: at(49),
                },
            },
        },
        causality: ObservationCausality::Unknown {},
        policy_basis: ObservationPolicyBasis::AccountIfEligible {
            project_policy_epoch: policy.epoch,
            policy: policy.policy_id,
            obligation_rule_set: policy.obligation_rule_set,
        },
    };
    let mut submit = |input: ExecutionObserveInput| {
        fixture
            .store
            .record_unadmitted_execution_observation(
                &fixture.work.project_id,
                &observer.status.session_id,
                &observer.connection_token,
                &observer.routing_token,
                &actor("observer"),
                input,
                at(70),
            )
            .unwrap()
    };
    let first = submit(input.clone());
    let duplicate = submit(input.clone());
    assert_eq!(first.observation, duplicate.observation);
    assert_eq!(
        first.accounting,
        ObservationAccounting::AuditOnly {
            reason: ObservationAuditReason::FinishedRun
        }
    );
    assert_eq!(first.opened_obligations, Vec::<ObjectId>::new());
    let mut watcher = input;
    watcher.idempotency_key = "watcher-only".into();
    watcher.occurrence = ObservedOccurrence::InterTurnChange {
        source_change: ObservedSourceChange::WatcherOnly {
            workspace_id: "workspace-B".into(),
            observed_at: at(49),
        },
    };
    submit(watcher);
    let page = fixture
        .store
        .sealed_source_page(&seal, None, None, 32)
        .unwrap();
    assert_eq!(page.diagnostic.differences, 1);
    assert_eq!(page.diagnostic.incomparable, 1);
    assert_eq!(
        page.diagnostic.first_difference.unwrap().observation,
        first.observation
    );
    assert!(
        fixture
            .store
            .work_run_obligations(fixture.claim.run_id)
            .unwrap()
            .iter()
            .all(|obligation| obligation.state != crate::WorkObligationState::Open)
    );
}
