//! A host's read of one claim's named root on its run, for a claim it may no
//! longer hold: the state the session status derives, the newest recorded
//! root event by its real id, and the run's lifecycle, from one snapshot;
//! and a host's read of a named root's initial sighting at a cut.

use super::*;
use crate::domain::{
    InitialSighting, NAMED_ROOT_SIGHTING_READ_SCHEMA_VERSION, NamedRootAtCut, NamedRootEndReason,
    NamedRootRead, NamedRootSightingRead, NamedRootSightingReadRefusal, NamedRootState,
    WorkClaimId, WorkClaimState, WorkRunState,
};

/// The control credentials a read presents.
#[derive(Clone)]
struct Credentials {
    project_id: ProjectId,
    session_id: SessionId,
    connection_token: String,
    routing_token: String,
}

impl Credentials {
    fn of(host: &HostSession) -> Self {
        Self {
            project_id: host.project_id.clone(),
            session_id: host.session_id.clone(),
            connection_token: host.connection_token.clone(),
            routing_token: host.routing_token.clone(),
        }
    }
}

fn read(
    store: &SqliteStore,
    host: &Credentials,
    run_id: WorkRunId,
    claim_id: WorkClaimId,
) -> Result<NamedRootRead, StoreError> {
    store.read_named_root(
        &host.project_id,
        &host.session_id,
        &host.connection_token,
        &host.routing_token,
        run_id,
        claim_id,
    )
}

fn read_claim(store: &SqliteStore, host: &HostSession, claim: &WorkClaim) -> NamedRootRead {
    read(store, &Credentials::of(host), claim.run_id, claim.claim_id).expect("named-root read")
}

/// The state the session status and the turn receipts derive now.
fn derived(store: &SqliteStore, claim: &WorkClaim) -> NamedRootState {
    crate::storage::work::named_root_state_on(
        &store.connection,
        claim.run_id,
        claim.claim_id,
        i64::MAX,
    )
    .expect("named-root state")
}

fn head(store: &SqliteStore, run_id: WorkRunId) -> i64 {
    store
        .work_feed_head(&FeedId::RunExecution(run_id))
        .expect("run feed head")
}

/// The event the read names is the recorded one: its id, position,
/// generation and kind are the receipt's.
fn assert_event(read: &NamedRootRead, receipt: &crate::domain::NamedRootBindingReceipt) {
    let event = read.latest_event.as_ref().expect("a recorded root event");
    assert_eq!(event.event, receipt.event);
    assert_eq!(event.position, receipt.position);
    assert_eq!(event.generation, receipt.generation);
    assert_eq!(event.kind, receipt.kind);
    assert_eq!(event.workspace_id, receipt.workspace_id);
}

/// The identities, the cut and the derived state every read must agree on.
fn assert_consistent(
    store: &SqliteStore,
    read: &NamedRootRead,
    work: &WorkItem,
    claim: &WorkClaim,
) {
    assert_eq!(read.project_id, work.project_id);
    assert_eq!(read.work_id, work.work_id);
    assert_eq!(read.run_id, claim.run_id);
    assert_eq!(read.claim_id, claim.claim_id);
    let run = store.get_work_run(claim.run_id).expect("run");
    assert_eq!(read.root_execution_id, run.root_execution_id);
    assert_eq!(read.run.state, run.state);
    assert_eq!(read.run.generation, run.generation);
    assert_eq!(read.read_cut.feed, FeedId::RunExecution(claim.run_id));
    assert_eq!(read.read_cut.position, head(store, claim.run_id));
    assert_eq!(read.named_root, derived(store, claim));
    if let Some(event) = &read.latest_event {
        assert_eq!(event.position.feed, FeedId::RunExecution(claim.run_id));
        assert!(event.position.position <= read.read_cut.position);
    }
}

#[test]
fn a_claim_never_named_reads_none_without_an_event() {
    let (fixture, work, claim, host) = bound_host();
    let read = read_claim(&fixture.store, &host, &claim);
    assert_eq!(read.named_root, NamedRootState::NoRoot);
    assert_eq!(read.latest_event, None);
    assert_eq!(read.claim.state, WorkClaimState::Active);
    assert_eq!(read.claim.holder, claim.holder);
    assert_eq!(read.claim.fence, claim.fence);
    assert_consistent(&fixture.store, &read, &work, &claim);
}

// A release: unbound by release, with the bound event still named.
#[test]
fn a_released_claim_reads_unbound_with_its_last_bound_event() {
    let (mut fixture, work, claim, host) = bound_host();
    let named = host_binds(
        &mut fixture.store,
        &host,
        &claim,
        "workspace-B",
        9,
        NamedRootBindingKind::Bound,
        11,
        "name-B-9",
        11,
    )
    .expect("host names B");
    release_runner(&mut fixture, &work, &claim, 20);
    let read = read_claim(&fixture.store, &host, &claim);
    assert!(
        matches!(
            read.named_root,
            NamedRootState::UnboundByRelease {
                last_generation: 9,
                ..
            }
        ),
        "{read:?}"
    );
    assert_eq!(read.claim.state, WorkClaimState::Released);
    assert_event(&read, &named);
    assert_consistent(&fixture.store, &read, &work, &claim);
}

// A completion: none, and the bound event stays named, so a host tells it
// from a claim never named. The old run reads the same after the item is
// reopened onto a new run that a new claim holds.
#[test]
fn a_completed_claim_reads_none_with_its_bound_event_after_the_work_moves_on() {
    let mut fixture = fixture("project-a");
    let (work, claim) = (fixture.work.clone(), fixture.claim.clone());
    let host = HostSession::bind(&mut fixture.store, &work, &claim, 6);
    let named = host_binds(
        &mut fixture.store,
        &host,
        &claim,
        "workspace-B",
        9,
        NamedRootBindingKind::Bound,
        11,
        "name-B-9",
        11,
    )
    .expect("host names B");
    let current = fixture.store.get_work_item(work.work_id).expect("item");
    let evidence = fixture.evidence.clone();
    checkpoint_then_complete(
        &mut fixture.store,
        &current,
        &claim,
        "runner",
        std::slice::from_ref(&evidence),
        true,
        None,
        "complete-named-work",
        14,
    )
    .expect("complete");
    let completed = read_claim(&fixture.store, &host, &claim);
    assert_eq!(completed.named_root, NamedRootState::NoRoot);
    assert_eq!(completed.run.state, WorkRunState::Completed);
    assert_eq!(completed.claim.state, WorkClaimState::Completed);
    assert_event(&completed, &named);
    assert_consistent(&fixture.store, &completed, &work, &claim);

    let done = fixture.store.get_work_item(work.work_id).expect("item");
    let reopened = fixture
        .store
        .reopen_work(
            &crate::domain::ReopenWorkRequest {
                work_id: done.work_id,
                expected_work_revision: done.revision,
                reason: "more to do".into(),
                actor: actor("planner"),
                idempotency_key: "reopen-named-work".into(),
                reopened_at: at(20),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("reopen");
    assert_ne!(
        reopened.run_id, claim.run_id,
        "the reopen started a new run"
    );
    let reopened = fixture.store.get_work_item(work.work_id).expect("item");
    let next = super::claim(
        &mut fixture.store,
        &reopened,
        "second",
        "claim-new-run",
        21,
        3_600,
    );
    assert_ne!(next.run_id, claim.run_id);
    let old = read_claim(&fixture.store, &host, &claim);
    assert_eq!(old, completed, "the old run reads as it did");
    assert_eq!(read_claim(&fixture.store, &host, &next).latest_event, None);
}

// An end: none with the ended event. A release after the end keeps both: the
// end decides, and the ended event stays the newest.
#[test]
fn an_ended_root_reads_none_with_its_ended_event_through_a_release() {
    let (mut fixture, work, claim, host) = bound_host();
    host_binds(
        &mut fixture.store,
        &host,
        &claim,
        "workspace-B",
        9,
        NamedRootBindingKind::Bound,
        11,
        "name-B-9",
        11,
    )
    .expect("host names B");
    let ended = host_binds(
        &mut fixture.store,
        &host,
        &claim,
        "workspace-B",
        9,
        NamedRootBindingKind::Ended,
        11,
        "end-B-9",
        12,
    )
    .expect("host ends B");
    let read_after_end = read_claim(&fixture.store, &host, &claim);
    assert_eq!(read_after_end.named_root, NamedRootState::NoRoot);
    assert_event(&read_after_end, &ended);
    assert_eq!(
        read_after_end
            .latest_event
            .as_ref()
            .and_then(|event| event.end_reason),
        Some(NamedRootEndReason::ExplicitClear)
    );
    release_runner(&mut fixture, &work, &claim, 20);
    let read_after_release = read_claim(&fixture.store, &host, &claim);
    assert_eq!(read_after_release.named_root, NamedRootState::NoRoot);
    assert_event(&read_after_release, &ended);
    assert_eq!(read_after_release.claim.state, WorkClaimState::Released);
    assert_consistent(&fixture.store, &read_after_release, &work, &claim);
}

// A fresh name at a higher generation after a release and a re-claim of the
// same claim id: bound at the new generation, which is the newest event.
#[test]
fn a_root_named_again_at_a_higher_generation_reads_bound_at_it() {
    let (mut fixture, work, claim, host) = bound_host();
    host_binds(
        &mut fixture.store,
        &host,
        &claim,
        "workspace-B",
        9,
        NamedRootBindingKind::Bound,
        11,
        "name-B-9",
        11,
    )
    .expect("host names B");
    release_runner(&mut fixture, &work, &claim, 20);
    let current = fixture.store.get_work_item(work.work_id).expect("item");
    let reclaimed = super::claim(
        &mut fixture.store,
        &current,
        "runner",
        "reclaim-runner",
        22,
        3_600,
    );
    assert_eq!(reclaimed.claim_id, claim.claim_id);
    let renamed = host_binds(
        &mut fixture.store,
        &host,
        &reclaimed,
        "workspace-C",
        10,
        NamedRootBindingKind::Bound,
        23,
        "name-C-10",
        23,
    )
    .expect("host names C at a fresh generation");
    let read = read_claim(&fixture.store, &host, &reclaimed);
    assert_eq!(
        read.named_root,
        NamedRootState::Bound {
            workspace_id: "workspace-C".into(),
            generation: 10,
            named_at: at(23),
        }
    );
    assert_event(&read, &renamed);
    assert_consistent(&fixture.store, &read, &current, &reclaimed);
}

// A holder long gone, its claim expired and never recovered, still reads
// bound: expiry is disclosed in the claim, never derived into an end. Another
// host session with no work binding reads it.
#[test]
fn a_still_bound_root_reads_bound_after_its_holder_is_gone() {
    let (mut fixture, work, claim, host) = bound_host();
    let named = host_binds(
        &mut fixture.store,
        &host,
        &claim,
        "workspace-B",
        9,
        NamedRootBindingKind::Bound,
        11,
        "name-B-9",
        11,
    )
    .expect("host names B");
    let observer = crate::storage::test_support::bind_control_for(
        &mut fixture.store,
        "observer",
        "observe-old-claims",
        &[EffectClass::Observe],
        at(30_000),
    );
    assert!(observer.status.work_binding.is_none());
    let reader = Credentials {
        project_id: work.project_id.clone(),
        session_id: observer.status.session_id.clone(),
        connection_token: observer.connection_token.clone(),
        routing_token: observer.routing_token.clone(),
    };
    assert_ne!(reader.session_id, host.session_id);
    let read =
        read(&fixture.store, &reader, claim.run_id, claim.claim_id).expect("named-root read");
    assert!(read.claim.expires_at < at(30_000), "the claim has expired");
    assert_eq!(read.claim.state, WorkClaimState::Active);
    assert_eq!(
        read.named_root,
        NamedRootState::Bound {
            workspace_id: "workspace-B".into(),
            generation: 9,
            named_at: at(11),
        }
    );
    assert_event(&read, &named);
    assert_consistent(&fixture.store, &read, &work, &claim);
}

// A claim of another run, an unknown run or claim, and a session with the
// wrong credentials are refused, never read as a state.
#[test]
fn a_claim_that_does_not_belong_to_the_run_is_refused() {
    let (mut fixture, work, claim, host) = bound_host();
    let other = fixture
        .store
        .create_work(
            &root_request("project-a", "create-other-work", 7),
            &DevelopmentNoopRedactor,
        )
        .expect("other work");
    let other_claim = super::claim(
        &mut fixture.store,
        &other,
        "second",
        "claim-other",
        8,
        3_600,
    );
    assert_ne!(other_claim.run_id, claim.run_id);
    let credentials = Credentials::of(&host);
    let refused = |result: Result<NamedRootRead, StoreError>| match result {
        Err(StoreError::NamedRootReadRefused(reason)) => reason,
        other => panic!("expected a named-root read refusal, got {other:?}"),
    };
    assert_eq!(
        refused(read(
            &fixture.store,
            &credentials,
            claim.run_id,
            other_claim.claim_id
        )),
        "the claim does not belong to the run"
    );
    assert_eq!(
        refused(read(
            &fixture.store,
            &credentials,
            other_claim.run_id,
            claim.claim_id
        )),
        "the claim does not belong to the run"
    );
    assert_eq!(
        refused(read(
            &fixture.store,
            &credentials,
            WorkRunId(uuid::Uuid::from_u128(7)),
            claim.claim_id
        )),
        "the run is unknown in this store"
    );
    assert_eq!(
        refused(read(
            &fixture.store,
            &credentials,
            claim.run_id,
            WorkClaimId(uuid::Uuid::from_u128(7))
        )),
        "the claim does not belong to the run"
    );
    // The session's own credentials keep their refusals.
    let mut wrong = Credentials::of(&host);
    wrong.routing_token = "wrong-routing".into();
    assert!(matches!(
        read_claim_result(&fixture.store, &wrong, &claim),
        Err(StoreError::ControlSessionTokenMismatch(_))
    ));
    let mut wrong = Credentials::of(&host);
    wrong.connection_token = "wrong-connection".into();
    assert!(matches!(
        read_claim_result(&fixture.store, &wrong, &claim),
        Err(StoreError::ControlConnectionSuperseded(_))
    ));
    let mut wrong = Credentials::of(&host);
    wrong.project_id = ProjectId("project-b".into());
    assert!(matches!(
        read_claim_result(&fixture.store, &wrong, &claim),
        Err(StoreError::ControlSessionNotBound(_))
    ));
    assert_consistent(
        &fixture.store,
        &read_claim(&fixture.store, &host, &claim),
        &work,
        &claim,
    );
}

fn read_claim_result(
    store: &SqliteStore,
    host: &Credentials,
    claim: &WorkClaim,
) -> Result<NamedRootRead, StoreError> {
    read(store, host, claim.run_id, claim.claim_id)
}

// Reads and refusals write nothing: every table's rows are byte-identical
// afterwards, an issued turn grant that has not begun included, which the
// session status would expire.
#[test]
fn reads_and_refusals_write_nothing() {
    let (mut fixture, _work, claim, mut host) = bound_host();
    host_binds(
        &mut fixture.store,
        &host,
        &claim,
        "workspace-B",
        9,
        NamedRootBindingKind::Bound,
        11,
        "name-B-9",
        11,
    )
    .expect("host names B");
    let _outstanding = host.grant(&mut fixture.store, &[EffectClass::Observe], false, 12);
    let before =
        crate::storage::test_database_shape_snapshot(&fixture.store.connection).expect("before");
    read_claim(&fixture.store, &host, &claim);
    let _ = read(
        &fixture.store,
        &Credentials::of(&host),
        WorkRunId(uuid::Uuid::from_u128(7)),
        claim.claim_id,
    );
    let mut wrong = Credentials::of(&host);
    wrong.routing_token = "wrong-routing".into();
    let _ = read_claim_result(&fixture.store, &wrong, &claim);
    let after =
        crate::storage::test_database_shape_snapshot(&fixture.store.connection).expect("after");
    assert_eq!(after, before, "the read wrote nothing");
}

// One snapshot: a release another connection commits after the read has
// started, before it looks up the run, reaches none of the read; a read after
// it sees the release.
#[test]
fn a_release_committed_mid_read_reaches_none_of_it() {
    let (fixture, work, claim, host) = bound_host();
    let Fixture {
        store: mut writer,
        directory,
        ..
    } = fixture;
    host_binds(
        &mut writer,
        &host,
        &claim,
        "workspace-B",
        9,
        NamedRootBindingKind::Bound,
        11,
        "name-B-9",
        11,
    )
    .expect("host names B");
    let reader = SqliteStore::open(directory.path().join("engram.sqlite3")).expect("reader");
    let credentials = Credentials::of(&host);
    let before = read(&reader, &credentials, claim.run_id, claim.claim_id).expect("before");
    let current = writer.get_work_item(work.work_id).expect("item");
    let releasing = claim.clone();
    let during = crate::storage::concurrent_commit::read_across_a_concurrent_commit(
        &reader,
        |reader| read(reader, &credentials, claim.run_id, claim.claim_id),
        &["EXISTS(SELECT 1 FROM work_runs WHERE run_id"],
        move || {
            writer
                .release_work(
                    &crate::domain::ReleaseWorkRequest {
                        work_id: current.work_id,
                        run_id: releasing.run_id,
                        expected_work_revision: current.revision,
                        holder: releasing.holder.clone(),
                        claim_id: releasing.claim_id,
                        claim_fence: releasing.fence,
                        reason: "stepping away mid-read".into(),
                        waiver_reason: None,
                        actor: actor("runner"),
                        idempotency_key: "release-mid-read".into(),
                        released_at: at(20),
                    },
                    &DevelopmentNoopRedactor,
                )
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
    );
    assert_eq!(during.expect("one snapshot, no false corruption"), before);
    let after = read(&reader, &credentials, claim.run_id, claim.claim_id).expect("after");
    assert_eq!(after.claim.state, WorkClaimState::Released);
    assert!(
        matches!(after.named_root, NamedRootState::UnboundByRelease { .. }),
        "{after:?}"
    );
    assert!(after.read_cut.position > before.read_cut.position);
    drop(reader);
    drop(directory);
}

// One snapshot, later in the read: a fresh name at a higher generation that
// another connection commits after the run and claim reads, just before the
// read takes its run-feed cut, reaches neither the cut, the state nor the
// event; a read after it sees the new generation.
#[test]
fn a_rename_committed_before_the_cut_reaches_none_of_the_read() {
    let (fixture, _work, claim, host) = bound_host();
    let Fixture {
        store: mut writer,
        directory,
        ..
    } = fixture;
    host_binds(
        &mut writer,
        &host,
        &claim,
        "workspace-B",
        9,
        NamedRootBindingKind::Bound,
        11,
        "name-B-9",
        11,
    )
    .expect("host names B");
    let reader = SqliteStore::open(directory.path().join("engram.sqlite3")).expect("reader");
    let credentials = Credentials::of(&host);
    let before = read(&reader, &credentials, claim.run_id, claim.claim_id).expect("before");
    let renaming = claim.clone();
    let during = crate::storage::concurrent_commit::read_across_a_concurrent_commit(
        &reader,
        |reader| read(reader, &credentials, claim.run_id, claim.claim_id),
        &[
            "SELECT position FROM work_feed_heads",
            "feed_kind = 'run_execution'",
        ],
        move || {
            host_binds(
                &mut writer,
                &host,
                &renaming,
                "workspace-C",
                10,
                NamedRootBindingKind::Bound,
                20,
                "name-C-10",
                20,
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
        },
    );
    assert_eq!(during.expect("one snapshot, no false corruption"), before);
    let after = read(&reader, &credentials, claim.run_id, claim.claim_id).expect("after");
    assert!(
        matches!(
            after.named_root,
            NamedRootState::Bound { generation: 10, .. }
        ),
        "{after:?}"
    );
    assert_eq!(
        after.latest_event.as_ref().map(|event| event.generation),
        Some(10)
    );
    assert!(after.read_cut.position > before.read_cut.position);
    drop(reader);
    drop(directory);
}

// A run of another project in the same store is refused, whatever its claim.
#[test]
fn a_run_of_another_project_is_refused() {
    let (mut fixture, _work, _claim, host) = bound_host();
    let foreign = fixture
        .store
        .create_work(
            &root_request("project-b", "create-foreign-work", 7),
            &DevelopmentNoopRedactor,
        )
        .expect("work of another project");
    let foreign_claim = super::claim(
        &mut fixture.store,
        &foreign,
        "runner",
        "claim-foreign",
        8,
        3_600,
    );
    match read(
        &fixture.store,
        &Credentials::of(&host),
        foreign_claim.run_id,
        foreign_claim.claim_id,
    ) {
        Err(StoreError::NamedRootReadRefused(reason)) => {
            assert_eq!(reason, "the run belongs to another project");
        }
        other => panic!("expected a named-root read refusal, got {other:?}"),
    }
}

// A run whose feed exists but whose row is missing is a damaged store: the
// read reports a storage error, never an unknown run and never a state.
#[test]
fn a_run_missing_its_row_is_a_storage_error_not_an_unknown_run() {
    let (fixture, _work, claim, host) = bound_host();
    fixture
        .store
        .connection
        .execute_batch("PRAGMA foreign_keys = OFF")
        .expect("allow the damage");
    let removed = fixture
        .store
        .connection
        .execute(
            "DELETE FROM work_runs WHERE run_id = ?1",
            [claim.run_id.0.to_string()],
        )
        .expect("remove the run row");
    assert_eq!(removed, 1);
    match read(
        &fixture.store,
        &Credentials::of(&host),
        claim.run_id,
        claim.claim_id,
    ) {
        Err(StoreError::InvalidWorkProjection(reason)) => {
            assert!(reason.contains("has a run feed but no run row"), "{reason}");
        }
        other => panic!("expected a storage error, got {other:?}"),
    }
}

// A host's read of a named root's initial sighting agrees with recording:
// wherever the read finds a bound root without a sighting, recording an
// evaluation at that cut is refused for want of one, and wherever it finds
// no root or a sighting, recording is not refused for that reason. The read
// writes nothing and needs no routing token or live holder.
fn sighting_read(
    store: &SqliteStore,
    work: &WorkItem,
    run_id: WorkRunId,
    run_cut: Option<i64>,
) -> NamedRootSightingRead {
    let snapshot = test_database_shape_snapshot(&store.connection).expect("snapshot");
    let read = store
        .read_named_root_sighting(&work.project_id, &work.short_ref, run_id, run_cut)
        .expect("the read answers");
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("snapshot"),
        snapshot,
        "the read writes nothing"
    );
    assert_eq!(read.schema_version, NAMED_ROOT_SIGHTING_READ_SCHEMA_VERSION);
    assert_eq!(read.work_id, work.work_id);
    assert_eq!(read.run_id, run_id);
    read
}

/// Recording's answer to a passing evaluation at `through`.
fn record_at(
    store: &mut SqliteStore,
    work: &WorkItem,
    session: &str,
    evidence: &ObjectId,
    through: i64,
    key: &str,
    second: i64,
) -> Result<AcceptanceEvaluationReceipt, StoreError> {
    let mut input = request(
        work,
        through,
        session,
        Mode::SameSession,
        vec![verdict(
            1,
            AcceptanceVerdict::Pass,
            AcceptanceBasis::Judgment,
            std::slice::from_ref(evidence),
        )],
        second,
    );
    input.attempt_key = Some(key.into());
    record(store, &input)
}

/// Whether recording refused for want of the root's initial sighting.
fn refused_for_no_sighting(answer: &Result<AcceptanceEvaluationReceipt, StoreError>) -> bool {
    match answer {
        Err(StoreError::AcceptanceEvaluationAdmissionRefused { cause, .. }) => matches!(
            **cause,
            AcceptanceEvaluationAdmissionCause::SourceRoot(ref root)
                if root.mismatch == EvaluationRootMismatch::NoInitialSighting
        ),
        _ => false,
    }
}

/// A claimed item with evaluation enabled and a host control session bound
/// for the runner.
fn evaluated_fixture() -> (Fixture, WorkItem, WorkClaim, ObjectId) {
    let mut fixture = fixture("project-a");
    enable(
        &mut fixture.store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-evaluation",
        5,
    );
    let (work, claim, note) = (
        fixture.work.clone(),
        fixture.claim.clone(),
        fixture.evidence.clone(),
    );
    (fixture, work, claim, note)
}

#[test]
fn without_a_named_root_the_read_finds_none_and_recording_checks_no_sighting() {
    let (mut fixture, work, claim, note) = evaluated_fixture();
    let read = sighting_read(&fixture.store, &work, claim.run_id, None);
    assert_eq!(read.root, NamedRootAtCut::None {});
    assert_eq!(read.current_binding, None);
    assert!(!read.binding_changed);
    assert_eq!(read.read_cut, read.head_cut);
    let through = read.read_cut;
    record_at(
        &mut fixture.store,
        &work,
        "runner",
        &note,
        through,
        "no-root",
        10,
    )
    .expect("recording records the evaluation");
}

#[test]
fn a_named_root_without_a_sighting_reads_absent_and_recording_refuses_until_it_is_sighted() {
    let (mut fixture, work, claim, note) = evaluated_fixture();
    let host = bind_control_for(
        &mut fixture.store,
        "runner",
        "named-evaluation-host",
        &[crate::domain::EffectClass::Observe],
        at(8),
    );
    name_root(&mut fixture.store, &work, &claim, &host, 9, 8);
    let before = sighting_read(&fixture.store, &work, claim.run_id, None);
    let NamedRootAtCut::Bound {
        workspace_id,
        generation,
        binding_event,
        binding_position,
        sighting,
    } = &before.root
    else {
        panic!("a bound root: {before:?}");
    };
    assert_eq!(workspace_id, "workspace-B");
    assert_eq!(*generation, 9);
    assert_eq!(sighting, &InitialSighting::Absent {});
    assert_eq!(before.current_binding.as_ref(), Some(binding_event));
    assert!(!before.binding_changed);
    assert!(*binding_position <= before.read_cut);
    assert!(refused_for_no_sighting(&record_at(
        &mut fixture.store,
        &work,
        "runner",
        &note,
        before.read_cut,
        "before-sighting",
        10
    )));

    host_verification_from_basis(
        &mut fixture.store,
        &work,
        &claim,
        "runner",
        "sighting-B",
        VerificationKind::Test,
        VerificationResult::Passed,
        11,
        source("workspace-B", 9),
    );
    let after = sighting_read(&fixture.store, &work, claim.run_id, None);
    let NamedRootAtCut::Bound {
        sighting: InitialSighting::Present {
            position, revision, ..
        },
        ..
    } = &after.root
    else {
        panic!("a sighted root: {after:?}");
    };
    assert_eq!(revision, "revision-B");
    assert!(*position > before.read_cut && *position <= after.read_cut);
    // The earlier cut still reads absent: a later record is never a
    // sighting at an earlier cut.
    let earlier = sighting_read(&fixture.store, &work, claim.run_id, Some(before.read_cut));
    assert_eq!(earlier.read_cut, before.read_cut);
    assert!(matches!(
        earlier.root,
        NamedRootAtCut::Bound {
            sighting: InitialSighting::Absent {},
            ..
        }
    ));
    record_at(
        &mut fixture.store,
        &work,
        "runner",
        &note,
        after.read_cut,
        "after-sighting",
        12,
    )
    .expect("recording records the evaluation");
}

#[test]
fn a_quiet_checkpoint_in_the_root_is_a_sighting() {
    let (mut fixture, work, claim, note) = evaluated_fixture();
    let mut host = HostSession::bind(&mut fixture.store, &work, &claim, 6);
    let store = &mut fixture.store;
    host_binds(
        store,
        &host,
        &claim,
        "workspace-B",
        9,
        NamedRootBindingKind::Bound,
        30,
        "name-B",
        30,
    )
    .expect("host names B");
    assert!(matches!(
        checkpoint_basis(
            &mut host,
            store,
            workspace("workspace-B", "R2", Some(9)),
            false,
            40,
        ),
        Ok(ControlTurnCheckpointDecision::Checkpointed { .. })
    ));
    let read = sighting_read(store, &work, claim.run_id, None);
    assert!(
        matches!(
            &read.root,
            NamedRootAtCut::Bound {
                sighting: InitialSighting::Present { revision, .. },
                ..
            } if revision == "R2"
        ),
        "{read:?}"
    );
    record_at(
        store,
        &work,
        "runner",
        &note,
        read.read_cut,
        "quiet-sighting",
        50,
    )
    .expect("recording records the evaluation");
}

#[test]
fn an_accounted_unadmitted_change_in_the_root_is_a_sighting() {
    use crate::domain::{
        ExecutionObserveInput, MeasuredBaseline, MeasuredSighting, ObservationCausality,
        ObservationPolicyBasis, ObservationRootBasis, ObservedInterval, ObservedOccurrence,
        ObservedSourceChange,
    };
    let (mut fixture, work, claim, note) = evaluated_fixture();
    let host = HostSession::bind(&mut fixture.store, &work, &claim, 6);
    let store = &mut fixture.store;
    host_binds(
        store,
        &host,
        &claim,
        "workspace-B",
        9,
        NamedRootBindingKind::Bound,
        30,
        "name-B",
        30,
    )
    .expect("host names B");
    let before = sighting_read(store, &work, claim.run_id, None);
    assert!(matches!(
        before.root,
        NamedRootAtCut::Bound {
            sighting: InitialSighting::Absent {},
            ..
        }
    ));
    // The host reports a change in B between turns, against the root state
    // it read at the capture cut.
    let root = store
        .read_named_root(
            &host.project_id,
            &host.session_id,
            &host.connection_token,
            &host.routing_token,
            claim.run_id,
            claim.claim_id,
        )
        .expect("the host reads its root");
    let run = load_work_run(&store.connection, claim.run_id).expect("run");
    let policy = SqliteStore::load_active_control_policy(&store.connection).expect("policy");
    let input = ExecutionObserveInput {
        idempotency_key: "unadmitted-change-in-B".into(),
        binding: crate::domain::ControlWorkBinding {
            root_execution_id: run.root_execution_id,
            work_id: claim.work_id,
            run_id: claim.run_id,
            work_revision: claim.accepted_work_revision,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
        },
        root_basis: ObservationRootBasis {
            capture_run_cut: root.read_cut.position,
            latest_event: root.latest_event.as_ref().map(|event| event.event.clone()),
            state: root.named_root.clone(),
        },
        observed_interval: ObservedInterval {
            from: at(38),
            through: at(39),
        },
        occurrence: ObservedOccurrence::InterTurnChange {
            source_change: ObservedSourceChange::ContentComparison {
                workspace_id: "workspace-B".into(),
                baseline: MeasuredBaseline {
                    workspace_id: "workspace-B".into(),
                    source_revision: "revision-before".into(),
                    observed_at: at(38),
                },
                sighting: MeasuredSighting {
                    source_basis: workspace("workspace-B", "R3", Some(9)),
                    observed_at: at(39),
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
    store
        .record_unadmitted_execution_observation(
            &host.project_id,
            &host.session_id,
            &host.connection_token,
            &host.routing_token,
            &actor(&host.session_id.0),
            input,
            at(40),
        )
        .expect("an accounted change in B");
    let after = sighting_read(store, &work, claim.run_id, None);
    assert!(
        matches!(
            &after.root,
            NamedRootAtCut::Bound {
                sighting: InitialSighting::Present { revision, .. },
                ..
            } if revision == "R3"
        ),
        "{after:?}"
    );
    record_at(
        store,
        &work,
        "runner",
        &note,
        after.read_cut,
        "unadmitted-sighting",
        50,
    )
    .expect("recording records the evaluation");
}

#[test]
fn after_a_release_and_a_new_name_an_older_generation_sighting_does_not_count() {
    let (mut fixture, work, claim, note) = evaluated_fixture();
    let host = HostSession::bind(&mut fixture.store, &work, &claim, 6);
    host_binds(
        &mut fixture.store,
        &host,
        &claim,
        "workspace-B",
        9,
        NamedRootBindingKind::Bound,
        9,
        "name-B-9",
        9,
    )
    .expect("host names B");
    let mut host = host;
    assert!(matches!(
        checkpoint_basis(
            &mut host,
            &mut fixture.store,
            stated(Some(9), Some(SourceRootState::Named)),
            false,
            10,
        ),
        Ok(ControlTurnCheckpointDecision::Checkpointed { .. })
    ));
    let sighted = sighting_read(&fixture.store, &work, claim.run_id, None);
    assert!(matches!(
        sighted.root,
        NamedRootAtCut::Bound {
            sighting: InitialSighting::Present { .. },
            ..
        }
    ));

    release_runner(&mut fixture, &work, &claim, 20);
    let released = sighting_read(&fixture.store, &work, claim.run_id, None);
    assert_eq!(released.root, NamedRootAtCut::None {});
    assert_eq!(released.current_binding, None);
    // The binding recording would select at the earlier cut is gone at the
    // head.
    let at_sighted = sighting_read(&fixture.store, &work, claim.run_id, Some(sighted.read_cut));
    assert!(at_sighted.binding_changed);

    let current = fixture.store.get_work_item(work.work_id).expect("item");
    let reclaimed =
        super::super::claim(&mut fixture.store, &current, "second", "reclaim", 21, 3_600);
    assert_eq!(reclaimed.claim_id, claim.claim_id);
    let second = HostSession::bind(&mut fixture.store, &current, &reclaimed, 22);
    host_binds(
        &mut fixture.store,
        &second,
        &reclaimed,
        "workspace-B",
        10,
        NamedRootBindingKind::Bound,
        23,
        "name-B-10",
        23,
    )
    .expect("host names B again, at a higher generation");
    let renamed = sighting_read(&fixture.store, &current, claim.run_id, None);
    let NamedRootAtCut::Bound {
        generation,
        sighting,
        binding_event,
        ..
    } = &renamed.root
    else {
        panic!("a bound root: {renamed:?}");
    };
    assert_eq!(*generation, 10);
    assert_eq!(sighting, &InitialSighting::Absent {});
    assert_eq!(renamed.current_binding.as_ref(), Some(binding_event));
    let current = fixture.store.get_work_item(work.work_id).expect("item");
    assert!(refused_for_no_sighting(&record_at(
        &mut fixture.store,
        &current,
        "second",
        &note,
        renamed.read_cut,
        "after-rename",
        24
    )));
}

#[test]
fn a_binding_that_moved_after_the_cut_is_reported_and_an_ended_root_reads_none() {
    let (mut fixture, work, claim, note) = evaluated_fixture();
    let host = bind_control_for(
        &mut fixture.store,
        "runner",
        "named-evaluation-host",
        &[crate::domain::EffectClass::Observe],
        at(8),
    );
    name_root(&mut fixture.store, &work, &claim, &host, 9, 8);
    host_verification_from_basis(
        &mut fixture.store,
        &work,
        &claim,
        "runner",
        "sighting-B",
        VerificationKind::Test,
        VerificationResult::Passed,
        9,
        source("workspace-B", 9),
    );
    let first = sighting_read(&fixture.store, &work, claim.run_id, None);
    name_root(&mut fixture.store, &work, &claim, &host, 10, 10);
    let moved = sighting_read(&fixture.store, &work, claim.run_id, Some(first.read_cut));
    assert!(moved.binding_changed);
    assert_ne!(moved.current_binding, first.current_binding);
    assert!(matches!(
        moved.root,
        NamedRootAtCut::Bound {
            generation: 9,
            sighting: InitialSighting::Present { .. },
            ..
        }
    ));
    // Recording refuses that cut as moved, before it looks for a sighting.
    let answer = record_at(
        &mut fixture.store,
        &work,
        "runner",
        &note,
        first.read_cut,
        "cut-before-rename",
        11,
    );
    assert!(
        matches!(
            answer,
            Err(StoreError::AcceptanceEvaluationBasisMoved {
                moved: EvaluationBasisMove::SourceChanged,
                ..
            })
        ),
        "{answer:?}"
    );

    fixture
        .store
        .bind_named_root(
            &work.project_id,
            &host.status.session_id,
            &host.connection_token,
            &host.routing_token,
            claim.claim_id,
            claim.fence,
            "workspace-B",
            10,
            at(10),
            NamedRootBindingKind::Ended,
            Some(crate::domain::NamedRootEndReason::ExplicitClear),
            &mut actor("runner"),
            "end-B-10",
            at(12),
        )
        .expect("the host ends B");
    let ended = sighting_read(&fixture.store, &work, claim.run_id, None);
    assert_eq!(ended.root, NamedRootAtCut::None {});
    assert_eq!(ended.current_binding, None);
}

#[test]
fn a_read_outside_its_item_or_its_run_feed_is_refused_with_a_typed_code() {
    let (mut fixture, work, claim, _) = evaluated_fixture();
    let store = &mut fixture.store;
    let refusal = |result: Result<NamedRootSightingRead, StoreError>| match result {
        Err(StoreError::NamedRootSightingReadRefused { refusal, .. }) => refusal,
        other => panic!("a typed refusal, not {other:?}"),
    };
    let head = cut(store, &work);
    assert_eq!(
        refusal(store.read_named_root_sighting(
            &work.project_id,
            "w-000000000000",
            claim.run_id,
            None
        )),
        NamedRootSightingReadRefusal::InvalidWorkRef
    );
    assert_eq!(
        refusal(store.read_named_root_sighting(&work.project_id, "  ", claim.run_id, None)),
        NamedRootSightingReadRefusal::InvalidWorkRef
    );
    let other = store
        .create_work(
            &root_request("project-a", "create-other-work", 30),
            &DevelopmentNoopRedactor,
        )
        .expect("another item");
    let other_claim = super::super::claim(store, &other, "runner", "claim-other", 31, 3_600);
    assert_eq!(
        refusal(store.read_named_root_sighting(
            &work.project_id,
            &work.short_ref,
            other_claim.run_id,
            None
        )),
        NamedRootSightingReadRefusal::WrongRun
    );
    assert_eq!(
        refusal(store.read_named_root_sighting(
            &work.project_id,
            &work.short_ref,
            WorkRunId(uuid::Uuid::now_v7()),
            None
        )),
        NamedRootSightingReadRefusal::WrongRun
    );
    for bad in [-1, head + 1] {
        assert_eq!(
            refusal(store.read_named_root_sighting(
                &work.project_id,
                &work.short_ref,
                claim.run_id,
                Some(bad)
            )),
            NamedRootSightingReadRefusal::InvalidCut,
            "{bad}"
        );
    }
    // Cut 0 is the lowest cut recording takes, and the read takes it too.
    let first = store
        .read_named_root_sighting(&work.project_id, &work.short_ref, claim.run_id, Some(0))
        .expect("cut 0 is inside the run feed");
    assert_eq!(first.read_cut, 0);
    assert_eq!(first.root, NamedRootAtCut::None {});
    assert!(!first.binding_changed);
    // The same item, named by its full id, reads at the head.
    let by_id = store
        .read_named_root_sighting(
            &work.project_id,
            &work.work_id.0.to_string(),
            claim.run_id,
            Some(head),
        )
        .expect("the full id names the item");
    assert_eq!(by_id.read_cut, head);
    assert_eq!(by_id.head_cut, head);
}

/// Every path of a JSON value, objects joined with dots.
fn json_paths(
    prefix: &str,
    value: &serde_json::Value,
    paths: &mut std::collections::BTreeSet<String>,
) {
    if let serde_json::Value::Object(object) = value {
        for (key, child) in object {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            json_paths(&path, child, paths);
        }
    } else {
        paths.insert(prefix.to_owned());
    }
}

/// The documented result table names exactly the fields the three result
/// shapes carry, and lists the state values they take.
#[test]
fn the_documented_result_table_matches_every_result_shape() {
    let doc = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/features/behavioral-control-plane.md"
    ));
    let table = doc
        .split("<!-- named-root-sighting-read:begin -->")
        .nth(1)
        .and_then(|rest| rest.split("<!-- named-root-sighting-read:end -->").next())
        .expect("the result table is marked");
    let rows: Vec<(String, String)> = table
        .lines()
        .filter_map(|line| {
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            let field = cells.get(1)?.strip_prefix('`')?.strip_suffix('`')?;
            Some((field.to_owned(), (*cells.get(2)?).to_owned()))
        })
        .collect();
    let documented: std::collections::BTreeSet<String> =
        rows.iter().map(|(field, _)| field.clone()).collect();

    let object = ObjectId::from_canonical_bytes(b"event");
    let shape = |root: NamedRootAtCut| NamedRootSightingRead {
        schema_version: NAMED_ROOT_SIGHTING_READ_SCHEMA_VERSION,
        project_id: ProjectId("project-a".into()),
        work_id: WorkId(uuid::Uuid::now_v7()),
        run_id: WorkRunId(uuid::Uuid::now_v7()),
        read_cut: 3,
        head_cut: 4,
        current_binding: Some(object.clone()),
        binding_changed: true,
        root,
    };
    let bound = |sighting| NamedRootAtCut::Bound {
        workspace_id: "workspace-B".into(),
        generation: 9,
        binding_event: object.clone(),
        binding_position: 2,
        sighting,
    };
    let mut emitted = std::collections::BTreeSet::new();
    for read in [
        shape(NamedRootAtCut::None {}),
        shape(bound(InitialSighting::Absent {})),
        shape(bound(InitialSighting::Present {
            record: object.clone(),
            position: 3,
            revision: "R1".into(),
        })),
    ] {
        let value = serde_json::to_value(&read).expect("serialize");
        json_paths("", &value, &mut emitted);
        // Each shape reads back as itself; nothing is dropped.
        let back: NamedRootSightingRead = serde_json::from_value(value).expect("read back");
        assert_eq!(back, read);
    }
    assert_eq!(documented, emitted);
    let values = |field: &str| {
        rows.iter()
            .find(|(name, _)| name == field)
            .map(|(_, kind)| kind.clone())
            .expect("documented")
    };
    for state in ["`none`", "`bound`"] {
        assert!(values("root.state").contains(state), "{state}");
    }
    for state in ["`absent`", "`present`"] {
        assert!(values("root.sighting.state").contains(state), "{state}");
    }
    // An unknown field is refused at every level, never ignored.
    let present = serde_json::to_value(shape(bound(InitialSighting::Present {
        record: object.clone(),
        position: 3,
        revision: "R1".into(),
    })))
    .expect("serialize");
    let none = serde_json::to_value(shape(NamedRootAtCut::None {})).expect("serialize");
    let mut cases = Vec::new();
    let mut extra = none.clone();
    extra["root"]["sighting"] = serde_json::json!({ "state": "absent" });
    cases.push(extra);
    let mut extra = none;
    extra["reader"] = serde_json::json!("host");
    cases.push(extra);
    let mut extra = present.clone();
    extra["root"]["extra"] = serde_json::json!(1);
    cases.push(extra);
    let mut extra = present;
    extra["root"]["sighting"]["extra"] = serde_json::json!(1);
    cases.push(extra);
    for extra in cases {
        assert!(
            serde_json::from_value::<NamedRootSightingRead>(extra.clone()).is_err(),
            "{extra}"
        );
    }
}
