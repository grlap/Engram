//! A host's read of one claim's named root on its run, for a claim it may no
//! longer hold: the state the session status derives, the newest recorded
//! root event by its real id, and the run's lifecycle, from one snapshot.

use super::*;
use crate::domain::{
    NamedRootEndReason, NamedRootRead, NamedRootState, WorkClaimId, WorkClaimState, WorkRunState,
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
