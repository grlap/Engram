//! A host records execution it observed without admission: the record is
//! history under the host's attribution, never a grant, a begin, a turn
//! result, a claim renewal or credit.

use super::super::test_support::*;
use super::super::*;
use super::UNADMITTED_OBSERVATION_KIND;
use crate::domain::{
    ExecutionObservationDecision, ExecutionObservationReceipt, ExecutionObserveInput,
    ExecutionSourceBasis, MeasuredBaseline, MeasuredSighting, NamedRootState,
    ObservationAccounting, ObservationAdmission, ObservationAuditReason, ObservationCausality,
    ObservationPolicyBasis, ObservationRootBasis, ObservedCheck, ObservedCheckCredit,
    ObservedInterval, ObservedOccurrence, ObservedSourceChange, ProjectPolicyEpoch,
    RecordedOccurrence, UnadmittedExecutionObservation, VerificationKind, VerificationResult,
    WorkClaim,
};
use crate::storage::test_support::{TestControlBinding, bind_control_for};

mod accounting;
mod doctor;
mod matching;
mod readers;
mod refusals;

pub(super) struct Fixture {
    pub(super) store: SqliteStore,
    pub(super) work: WorkItem,
    pub(super) claim: WorkClaim,
    pub(super) host: TestControlBinding,
    pub(super) binding: ControlWorkBinding,
}

/// A claimed item, and a host control session of another session that
/// observes it: the observer never holds the claim.
pub(super) fn fixture() -> Fixture {
    fixture_on(SqliteStore::open_in_memory().expect("store"))
}

pub(super) fn fixture_on(store: SqliteStore) -> Fixture {
    fixture_of(store, &root_request("project-a", "observed-work", 1))
}

/// [`fixture_on`] for the item `request` creates.
pub(super) fn fixture_of(mut store: SqliteStore, request: &CreateWorkRequest) -> Fixture {
    let work = store
        .create_work(request, &DevelopmentNoopRedactor)
        .expect("work");
    let claim = claim(&mut store, &work, "runner", "observed-claim", 2, 300);
    let host = bind_control_for(
        &mut store,
        "observer",
        "observer-bind",
        &[EffectClass::Observe],
        at(3),
    );
    let run = load_work_run(&store.connection, claim.run_id).expect("run");
    let binding = ControlWorkBinding {
        root_execution_id: run.root_execution_id,
        work_id: work.work_id,
        run_id: run.run_id,
        work_revision: claim.accepted_work_revision,
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
    };
    Fixture {
        store,
        work,
        claim,
        host,
        binding,
    }
}

pub(super) fn run_head(fixture: &Fixture) -> i64 {
    fixture
        .store
        .work_feed_head(&FeedId::RunExecution(fixture.claim.run_id))
        .expect("run feed head")
}

pub(super) fn content_change(from: &str, to: &str) -> ObservedSourceChange {
    ObservedSourceChange::ContentComparison {
        workspace_id: "workspace-A".into(),
        baseline: MeasuredBaseline {
            workspace_id: "workspace-A".into(),
            source_revision: from.into(),
            observed_at: at(4),
        },
        sighting: MeasuredSighting {
            source_basis: ExecutionSourceBasis {
                workspace_id: "workspace-A".into(),
                source_revision: to.into(),
                source_root_generation: None,
                source_root_state: None,
            },
            observed_at: at(5),
        },
    }
}

pub(super) fn passed_check(id: &str) -> ObservedCheck {
    ObservedCheck {
        host_check_id: id.into(),
        check_kind: VerificationKind::Test,
        observed_result: VerificationResult::Passed,
        started_at: Some(at(4)),
        finished_at: Some(at(5)),
        observed_at: at(5),
        source_basis: None,
        host_evidence_ref: Some("termal://check-log/1".into()),
    }
}

/// An inter-turn change between `from` and `to`, at the run's current head,
/// kept for audit with its cause unknown.
pub(super) fn inter_turn_change(fixture: &Fixture, key: &str) -> ExecutionObserveInput {
    ExecutionObserveInput {
        idempotency_key: key.into(),
        binding: fixture.binding.clone(),
        root_basis: ObservationRootBasis {
            capture_run_cut: run_head(fixture),
            latest_event: None,
            state: NamedRootState::NoRoot,
        },
        observed_interval: ObservedInterval {
            from: at(4),
            through: at(6),
        },
        occurrence: ObservedOccurrence::InterTurnChange {
            source_change: content_change("rev-a", "rev-b"),
        },
        causality: ObservationCausality::Unknown {},
        policy_basis: ObservationPolicyBasis::AuditOnly {},
    }
}

pub(super) fn observe(
    fixture: &mut Fixture,
    input: ExecutionObserveInput,
    second: i64,
) -> Result<ExecutionObservationReceipt, StoreError> {
    let mut observer = actor("observer");
    // The host-control channel records an observation as the kind the agent
    // words record as, so the observing session reads it as its own.
    observer.actor_kind = crate::work_service::WORD_ACTOR_KIND.into();
    observer.run_id = Some(fixture.binding.run_id.0.to_string());
    let host = &fixture.host;
    fixture.store.record_unadmitted_execution_observation(
        &fixture.work.project_id,
        &host.status.session_id,
        &host.connection_token,
        &host.routing_token,
        &observer,
        input,
        at(second),
    )
}

pub(super) fn count(store: &SqliteStore, sql: &str) -> i64 {
    store
        .connection
        .query_row(sql, [], |row| row.get(0))
        .expect("count")
}

/// Every table an observation could touch, counted.
pub(super) fn footprint(store: &SqliteStore) -> [i64; 5] {
    [
        count(store, "SELECT COUNT(*) FROM objects"),
        count(store, "SELECT COUNT(*) FROM work_feed_entries"),
        count(store, "SELECT COUNT(*) FROM control_operation_results"),
        count(store, "SELECT COUNT(*) FROM control_turn_grants"),
        count(store, "SELECT COUNT(*) FROM work_run_obligations"),
    ]
}

pub(super) fn stored(store: &SqliteStore, id: &crate::ObjectId) -> UnadmittedExecutionObservation {
    let (kind, bytes): (String, Vec<u8>) = store
        .connection
        .query_row(
            "SELECT object_kind, canonical_json FROM objects WHERE object_id = ?1",
            [id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("stored observation");
    assert_eq!(kind, UNADMITTED_OBSERVATION_KIND);
    CanonicalObject::stored(id, bytes)
        .expect("canonical bytes")
        .decode()
        .expect("decode")
}

#[test]
fn an_inter_turn_change_is_recorded_as_an_unadmitted_fact_with_no_authority() {
    let mut fixture = fixture();
    let head = run_head(&fixture);
    let status_before = fixture
        .store
        .control_status(
            &fixture.work.project_id,
            &fixture.host.status.session_id,
            &fixture.host.connection_token,
            &fixture.host.routing_token,
            at(6),
        )
        .expect("status");
    let input = inter_turn_change(&fixture, "change-1");
    let receipt = observe(&mut fixture, input.clone(), 7).expect("recorded");

    assert_eq!(receipt.decision, ExecutionObservationDecision::Recorded);
    assert_eq!(receipt.admission, ObservationAdmission::Unadmitted);
    assert_eq!(
        receipt.accounting,
        ObservationAccounting::AuditOnly {
            reason: ObservationAuditReason::ExplicitAudit
        }
    );
    assert!(
        receipt.opened_obligations.is_empty(),
        "{:?}",
        receipt.opened_obligations
    );
    assert!(
        receipt.observed_checks.is_empty(),
        "{:?}",
        receipt.observed_checks
    );
    assert_eq!(receipt.causality, ObservationCausality::Unknown {});
    assert_eq!(receipt.binding, fixture.binding);
    // The observer is named separately from the claim's holder.
    assert_eq!(receipt.observing_session.0, "observer");
    assert_ne!(receipt.observing_session, fixture.claim.holder);
    assert_eq!(
        receipt.position,
        FeedPosition {
            feed: FeedId::RunExecution(fixture.claim.run_id),
            position: head + 1,
        }
    );

    let observation = stored(&fixture.store, &receipt.observation);
    assert_eq!(observation.admission, ObservationAdmission::Unadmitted);
    assert_eq!(observation.observing_session.0, "observer");
    assert_eq!(
        observation
            .observer
            .session_id
            .as_ref()
            .map(|s| s.0.as_str()),
        Some("observer")
    );
    assert_eq!(observation.occurrence.reported(), input.occurrence);
    assert_eq!(observation.root_basis, input.root_basis);
    assert_eq!(observation.observed_interval, input.observed_interval);
    assert_eq!(observation.recorded_at, at(7));
    // It sits on the project, root and run feeds, and nowhere else.
    let feeds: Vec<String> = fixture
        .store
        .connection
        .prepare("SELECT feed_kind FROM work_feed_entries WHERE object_id = ?1 ORDER BY feed_kind")
        .expect("prepare")
        .query_map([receipt.observation.as_str()], |row| row.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("feeds");
    assert_eq!(feeds, ["project", "root_work", "run_execution"]);

    // No grant, no begin, no obligation, no session phase change, and the
    // claim is neither renewed nor changed.
    assert_eq!(
        count(&fixture.store, "SELECT COUNT(*) FROM control_turn_grants"),
        0
    );
    assert_eq!(
        count(&fixture.store, "SELECT COUNT(*) FROM work_run_obligations"),
        0
    );
    let status_after = fixture
        .store
        .control_status(
            &fixture.work.project_id,
            &fixture.host.status.session_id,
            &fixture.host.connection_token,
            &fixture.host.routing_token,
            at(8),
        )
        .expect("status");
    assert_eq!(status_after.phase, status_before.phase);
    assert_eq!(status_after.revision, status_before.revision);
    let claim_after = load_work_claim_optional(&fixture.store.connection, fixture.claim.run_id)
        .expect("claim")
        .expect("a claim");
    assert_eq!(claim_after.expires_at, fixture.claim.expires_at);
    assert_eq!(claim_after.fence, fixture.claim.fence);
    assert_eq!(claim_after.revision, fixture.claim.revision);
    // It can never be a verification producer.
    assert!(
        load_control_execution_observation_on(&fixture.store.connection, &receipt.observation)
            .expect("lookup")
            .is_none()
    );
}

#[test]
fn an_identical_retry_returns_the_original_receipt_even_after_reconnecting() {
    let mut fixture = fixture();
    let input = inter_turn_change(&fixture, "change-retry");
    let first = observe(&mut fixture, input.clone(), 7).expect("recorded");
    let before = footprint(&fixture.store);
    let again = observe(&mut fixture, input.clone(), 30).expect("replayed");
    assert_eq!(again, first);
    assert_eq!(
        crate::canonical::canonical_bytes(&again).expect("bytes"),
        crate::canonical::canonical_bytes(&first).expect("bytes")
    );
    assert_eq!(footprint(&fixture.store), before);

    // A new connection for the same observing session replays too.
    fixture.host.connection_token = fixture
        .store
        .resume_control_connection(&fixture.host.status.session_id.clone(), at(31))
        .expect("reconnect");
    let rotated = observe(&mut fixture, input.clone(), 32).expect("replayed after reconnect");
    assert_eq!(rotated, first);

    // The same key with any changed fact is a conflict, never a second fact.
    let mut changed = input;
    changed.observed_interval.from = at(5);
    assert!(matches!(
        observe(&mut fixture, changed, 33),
        Err(StoreError::ControlOperationIdempotencyConflict { operation, key })
            if operation == "execution_observe" && key == "change-retry"
    ));
    assert_eq!(footprint(&fixture.store), before);
}

#[test]
fn an_unadmitted_turn_keeps_every_check_uncredited_and_its_asserted_cause_apart() {
    let mut fixture = fixture();
    let mut claimed_actor = actor("runner");
    claimed_actor.reason = "the host saw the runner's terminal".into();
    let mut input = inter_turn_change(&fixture, "turn-1");
    input.occurrence = ObservedOccurrence::UnadmittedTurn {
        host_turn_ref: "termal-turn-17".into(),
        source_change: None,
        observed_checks: vec![passed_check("cargo-test"), {
            let mut check = passed_check("clippy");
            check.check_kind = VerificationKind::Lint;
            check.observed_result = VerificationResult::Indeterminate;
            check.finished_at = None;
            check
        }],
    };
    input.causality = ObservationCausality::HostAssertion {
        claimed_actor: Box::new(claimed_actor.clone()),
        basis: "terminal ownership".into(),
    };
    let receipt = observe(&mut fixture, input, 7).expect("recorded");
    let ids: Vec<(&str, ObservedCheckCredit)> = receipt
        .observed_checks
        .iter()
        .map(|summary| (summary.host_check_id.as_str(), summary.credit))
        .collect();
    assert_eq!(
        ids,
        [
            ("cargo-test", ObservedCheckCredit::Uncredited),
            ("clippy", ObservedCheckCredit::Uncredited)
        ]
    );
    let observation = stored(&fixture.store, &receipt.observation);
    let RecordedOccurrence::UnadmittedTurn {
        observed_checks, ..
    } = &observation.occurrence
    else {
        panic!("an unadmitted turn");
    };
    assert!(
        observed_checks
            .iter()
            .all(|check| check.credit == ObservedCheckCredit::Uncredited)
    );
    // The asserted cause stays an assertion, apart from the observer.
    assert_eq!(
        observation.causality,
        ObservationCausality::HostAssertion {
            claimed_actor: Box::new(claimed_actor),
            basis: "terminal ownership".into(),
        }
    );
    assert_eq!(observation.observing_session.0, "observer");
    // A passed observed check mints no verification evidence and no gate.
    assert_eq!(
        count(
            &fixture.store,
            "SELECT COUNT(*) FROM objects WHERE object_kind IN ('verification_evidence', 'work_evidence')"
        ),
        0
    );
}

// Even a passed check reported with its source and an artifact reference
// is no evidence: neither an evaluation citation nor a completion link can
// name the record, and nothing verification-shaped is minted from it.
#[test]
fn an_observation_is_never_citable_evidence() {
    let mut fixture = fixture();
    let mut input = inter_turn_change(&fixture, "uncitable");
    let mut check = passed_check("cargo-test");
    check.source_basis = Some(ExecutionSourceBasis {
        workspace_id: "workspace-A".into(),
        source_revision: "rev-b".into(),
        source_root_generation: None,
        source_root_state: None,
    });
    input.occurrence = ObservedOccurrence::UnadmittedTurn {
        host_turn_ref: "turn-3".into(),
        source_change: None,
        observed_checks: vec![check],
    };
    let receipt = observe(&mut fixture, input, 7).expect("recorded");
    let (project, work_id, run_id) = (
        fixture.work.project_id.clone(),
        fixture.work.work_id,
        fixture.claim.run_id,
    );
    assert!(
        !fixture
            .store
            .host_minted_run_evidence(run_id, &receipt.observation)
            .expect("lookup"),
        "an evaluation must not cite it as host-minted evidence"
    );
    let index = fixture
        .store
        .work_record_index(
            &project,
            work_id,
            crate::storage::WorkRecordKind::NotesWithGates,
        )
        .expect("record index");
    let resolved = fixture
        .store
        .resolve_criterion_evidence_classified(
            &project,
            work_id,
            run_id,
            receipt.observation.as_str(),
            &index,
        )
        .expect("resolution");
    assert!(resolved.is_err(), "a completion link must not name it");
    assert_eq!(
        count(
            &fixture.store,
            "SELECT COUNT(*) FROM objects WHERE object_kind IN ('verification_evidence', 'work_evidence')"
        ),
        0
    );
}

#[test]
fn a_report_after_the_claim_expired_is_history_and_renews_nothing() {
    let mut fixture = fixture();
    let input = inter_turn_change(&fixture, "late-report");
    // The claim lapsed long ago; the report still describes what was seen.
    let receipt = observe(&mut fixture, input, 5_000).expect("recorded after expiry");
    assert_eq!(receipt.binding, fixture.binding);
    let claim_after = load_work_claim_optional(&fixture.store.connection, fixture.claim.run_id)
        .expect("claim")
        .expect("a claim");
    assert_eq!(claim_after.expires_at, fixture.claim.expires_at);
    assert_eq!(claim_after.fence, fixture.claim.fence);
}
