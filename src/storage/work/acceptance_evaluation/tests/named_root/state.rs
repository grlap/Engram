//! The claim's named-root state that the host transport reports in the
//! session status and in a turn's begin receipt.

use super::*;
use crate::domain::NamedRootState;

/// The claim's named-root state as Engram derives it now.
fn state(store: &SqliteStore, claim: &WorkClaim) -> NamedRootState {
    crate::storage::work::named_root_state_on(
        &store.connection,
        claim.run_id,
        claim.claim_id,
        i64::MAX,
    )
    .expect("named-root state")
}

/// The state the host reads in its session status.
fn status_state(
    store: &mut SqliteStore,
    host: &HostSession,
    second: i64,
) -> Option<NamedRootState> {
    store
        .control_status(
            &host.project_id,
            &host.session_id,
            &host.connection_token,
            &host.routing_token,
            at(second),
        )
        .expect("session status")
        .named_root
}

/// The state a `session_bind` result carries. The first bind's key replays
/// that bind; a new key binds the session to `claim` again, and the host
/// takes the new routing token.
fn bind_state(
    store: &mut SqliteStore,
    host: &mut HostSession,
    work: &WorkItem,
    claim: &WorkClaim,
    key: &str,
    second: i64,
) -> Option<NamedRootState> {
    let binding = bind_host_control(store, work, claim, &host.connection_token, key, second);
    host.routing_token = binding.routing_token;
    binding.status.named_root
}

/// Begins and checkpoints one quiet host turn, returning the state its begin
/// receipt carries.
fn begin_state(
    store: &mut SqliteStore,
    host: &mut HostSession,
    second: i64,
) -> Option<NamedRootState> {
    let grant = host.grant(store, &[EffectClass::Observe], false, second);
    let decision = store
        .begin_control_turn(
            &host.project_id,
            &host.session_id,
            &host.connection_token,
            &host.routing_token,
            &grant.grant_id,
            &[],
            &host.key("begin"),
            at(second + 1),
        )
        .expect("begin host turn");
    let ControlTurnBeginDecision::Begin { receipt } = decision else {
        panic!("the turn must begin: {decision:?}");
    };
    assert!(matches!(
        store
            .checkpoint_control_turn(
                &host.project_id,
                &host.session_id,
                &host.connection_token,
                &host.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                &host.key("checkpoint"),
                at(second + 2),
            )
            .expect("checkpoint host turn"),
        ControlTurnCheckpointDecision::Checkpointed { .. }
    ));
    receipt.named_root
}

fn bound(workspace: &str, generation: i64, named_second: i64) -> NamedRootState {
    NamedRootState::Bound {
        workspace_id: workspace.into(),
        generation,
        named_at: at(named_second),
    }
}

/// The run-feed position of the claim's newest release.
fn release_position(store: &SqliteStore, claim: &WorkClaim) -> i64 {
    crate::storage::work::feeds::latest_claim_release_on(
        &store.connection,
        claim.run_id,
        claim.claim_id,
        i64::MAX,
    )
    .expect("release read")
    .expect("the claim was released")
}

/// A claim reports no root until the host names one, then the bound root.
/// A release, and a re-claim of the same claim id after it, report
/// `unbound_by_release` with the last generation and the release's position
/// until the host names a fresh generation; an ended root reports none. The
/// session status, a bind's result, fresh or replayed, and a turn's begin
/// receipt carry the same state.
#[test]
fn a_release_unbinds_the_root_until_a_fresh_name() {
    let (mut fixture, work, claim, mut host) = bound_host();
    assert_eq!(state(&fixture.store, &claim), NamedRootState::NoRoot);
    assert_eq!(
        status_state(&mut fixture.store, &host, 10),
        Some(NamedRootState::NoRoot)
    );
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
    let named = bound("workspace-B", 9, 11);
    assert_eq!(state(&fixture.store, &claim), named);
    assert_eq!(
        status_state(&mut fixture.store, &host, 12),
        Some(named.clone())
    );
    assert_eq!(
        begin_state(&mut fixture.store, &mut host, 13),
        Some(named.clone())
    );
    assert_eq!(
        bind_state(
            &mut fixture.store,
            &mut host,
            &work,
            &claim,
            "bind-host-session",
            14
        ),
        Some(named.clone()),
        "a replayed bind reports the state at the replay"
    );

    release_runner(&mut fixture, &work, &claim, 20);
    let released = NamedRootState::UnboundByRelease {
        last_generation: 9,
        released_at_position: release_position(&fixture.store, &claim),
    };
    assert_eq!(state(&fixture.store, &claim), released);
    assert_eq!(
        status_state(&mut fixture.store, &host, 21),
        Some(released.clone()),
        "the session still names the released claim"
    );
    assert_eq!(
        bind_state(
            &mut fixture.store,
            &mut host,
            &work,
            &claim,
            "bind-host-session",
            21
        ),
        Some(released.clone()),
        "a bind replayed after the release"
    );
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
    assert_eq!(
        state(&fixture.store, &reclaimed),
        released,
        "a re-claim does not bind the old root again"
    );
    assert_eq!(
        bind_state(
            &mut fixture.store,
            &mut host,
            &current,
            &reclaimed,
            "bind-after-reclaim",
            22
        ),
        Some(released.clone()),
        "a fresh bind to the re-claimed claim"
    );

    host_binds(
        &mut fixture.store,
        &host,
        &reclaimed,
        "workspace-B",
        10,
        NamedRootBindingKind::Bound,
        23,
        "name-B-10",
        23,
    )
    .expect("host names B again with a fresh generation");
    assert_eq!(
        state(&fixture.store, &reclaimed),
        bound("workspace-B", 10, 23)
    );
    assert_eq!(
        bind_state(
            &mut fixture.store,
            &mut host,
            &current,
            &reclaimed,
            "bind-after-reclaim",
            23
        ),
        Some(bound("workspace-B", 10, 23)),
        "the fresh bind replayed after the new name"
    );
    host_binds(
        &mut fixture.store,
        &host,
        &reclaimed,
        "workspace-B",
        10,
        NamedRootBindingKind::Ended,
        23,
        "end-B-10",
        24,
    )
    .expect("host ends B");
    assert_eq!(state(&fixture.store, &reclaimed), NamedRootState::NoRoot);
    let report = fixture.store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

/// A holder change without a release keeps the binding: an accepted handoff
/// and a recovery by another holder after the claim lapsed both advance the
/// claim, and neither changes the state.
#[test]
fn a_holder_change_without_a_release_keeps_the_binding() {
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
    let named = bound("workspace-B", 9, 11);

    let current = fixture.store.get_work_item(work.work_id).expect("item");
    let offer = fixture
        .store
        .offer_work_handoff(
            &crate::domain::OfferWorkHandoffRequest {
                work_id: current.work_id,
                run_id: claim.run_id,
                expected_work_revision: current.revision,
                from: claim.holder.clone(),
                to: SessionId("second".into()),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                ttl_seconds: 300,
                checkpoint_summary: "handing the run to second".into(),
                actor: actor("runner"),
                idempotency_key: "offer-to-second".into(),
                offered_at: at(12),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("offer handoff");
    let handed = fixture
        .store
        .accept_work_handoff(
            &crate::domain::AcceptWorkHandoffRequest {
                work_id: current.work_id,
                offer_id: offer.offer_id,
                to: SessionId("second".into()),
                actor: actor("second"),
                idempotency_key: "accept-as-second".into(),
                accepted_at: at(13),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("accept handoff");
    assert_eq!(handed.claim_id, claim.claim_id);
    assert_ne!(handed.fence, claim.fence);
    assert_eq!(state(&fixture.store, &handed), named, "handoff");

    let current = fixture.store.get_work_item(work.work_id).expect("item");
    let recovered = super::claim(
        &mut fixture.store,
        &current,
        "third",
        "recover-as-third",
        20_000,
        300,
    );
    assert_eq!(recovered.holder, SessionId("third".into()));
    assert_eq!(recovered.claim_id, claim.claim_id);
    assert_ne!(recovered.revision, handed.revision);
    assert_eq!(state(&fixture.store, &recovered), named, "recovery");
}

/// A finished claim reports no root: the run completed, or the item was
/// cancelled, after the name, whether or not a release came between. A
/// completion after a release follows a re-claim of the same claim id.
#[test]
fn a_finished_claim_reports_no_root() {
    for (cancel, released_first) in [(false, false), (true, false), (false, true), (true, true)] {
        let case = format!("cancel: {cancel}, released first: {released_first}");
        let mut fixture = fixture("project-a");
        let (work, mut claim) = (fixture.work.clone(), fixture.claim.clone());
        let host = HostSession::bind(&mut fixture.store, &work, &claim, 6);
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
        assert_eq!(
            state(&fixture.store, &claim),
            bound("workspace-B", 9, 11),
            "{case}"
        );
        if released_first {
            release_runner(&mut fixture, &work, &claim, 12);
            assert!(
                matches!(
                    state(&fixture.store, &claim),
                    NamedRootState::UnboundByRelease { .. }
                ),
                "{case}"
            );
            if !cancel {
                let current = fixture.store.get_work_item(work.work_id).expect("item");
                claim = super::claim(
                    &mut fixture.store,
                    &current,
                    "runner",
                    "reclaim-runner",
                    13,
                    3_600,
                );
            }
        }
        let current = fixture.store.get_work_item(work.work_id).expect("item");
        if cancel {
            fixture
                .store
                .dispose_work(
                    &crate::domain::DisposeWorkRequest {
                        work_id: current.work_id,
                        expected_work_revision: current.revision,
                        disposition: crate::domain::WorkDisposition::Cancelled,
                        replacement_id: None,
                        reason: "cancel the named work".into(),
                        actor: actor("runner"),
                        idempotency_key: "cancel-named-work".into(),
                        disposed_at: at(14),
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect("cancel");
        } else {
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
        }
        assert_eq!(
            state(&fixture.store, &claim),
            NamedRootState::NoRoot,
            "{case}"
        );
    }
}

/// An ended root reports none, even when a release follows it, and a
/// re-claim after that release does not change it.
#[test]
fn an_ended_root_stays_none_after_a_release() {
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
    host_binds(
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
    assert_eq!(state(&fixture.store, &claim), NamedRootState::NoRoot);
    release_runner(&mut fixture, &work, &claim, 20);
    assert_eq!(
        state(&fixture.store, &claim),
        NamedRootState::NoRoot,
        "a release after the end"
    );
    let current = fixture.store.get_work_item(work.work_id).expect("item");
    let reclaimed = super::claim(
        &mut fixture.store,
        &current,
        "runner",
        "reclaim-runner",
        21,
        3_600,
    );
    assert_eq!(
        state(&fixture.store, &reclaimed),
        NamedRootState::NoRoot,
        "a re-claim after the release"
    );
}

/// A session bound without work carries no named-root state in its bind
/// result, its status or a turn's begin receipt.
#[test]
fn a_session_without_work_carries_no_state() {
    let mut fixture = fixture("project-a");
    let project_id = fixture.work.project_id.clone();
    let session_id = SessionId("observer".into());
    let connection_token = fixture
        .store
        .resume_control_connection(&session_id, at(6))
        .expect("resume the observer's control connection");
    let control = fixture
        .store
        .bind_control_session(
            &project_id,
            "local-work:observer",
            "Observe without work",
            &session_id,
            &connection_token,
            &actor("observer"),
            ControlAssurance::TurnGated,
            &[EffectClass::Observe],
            1,
            "bind-observer",
            at(6),
        )
        .expect("bind without work");
    assert_eq!(control.status.work_binding, None);
    assert_eq!(control.status.named_root, None);
    let mut host = HostSession {
        project_id: project_id.clone(),
        session_id,
        connection_token,
        routing_token: control.routing_token,
        subject: ResourceSubject::Path {
            project_id,
            segments: vec!["src".into()],
            coverage: ResourceCoverage::Tree,
        },
        basis: ExecutionSourceBasis {
            workspace_id: "workspace-observer".into(),
            source_revision: "content-revision-1".into(),
            source_root_generation: None,
            source_root_state: None,
        },
        turns: 0,
    };
    assert_eq!(status_state(&mut fixture.store, &host, 7), None);
    assert_eq!(begin_state(&mut fixture.store, &mut host, 8), None);
}

/// A turn-begin receipt an earlier build stored without the state replays as
/// stored, without it, and the doctor stays healthy.
#[test]
fn a_begin_receipt_stored_without_the_state_replays_without_it() {
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
    let grant = host.grant(&mut fixture.store, &[EffectClass::Observe], false, 12);
    let key = host.key("begin");
    let begin = |store: &mut SqliteStore| match store
        .begin_control_turn(
            &host.project_id,
            &host.session_id,
            &host.connection_token,
            &host.routing_token,
            &grant.grant_id,
            &[],
            &key,
            at(13),
        )
        .expect("begin host turn")
    {
        ControlTurnBeginDecision::Begin { receipt } => receipt,
        refused @ ControlTurnBeginDecision::Refuse { .. } => {
            panic!("the turn must begin: {refused:?}")
        }
    };
    let receipt = begin(&mut fixture.store);
    assert_eq!(receipt.named_root, Some(bound("workspace-B", 9, 11)));

    let stored: Vec<u8> = fixture
        .store
        .connection
        .query_row(
            "SELECT result_json FROM control_operation_results
             WHERE operation = 'turn_begin' AND idempotency_key = ?1",
            [&key],
            |row| row.get(0),
        )
        .expect("the stored begin result");
    let mut result: serde_json::Value = serde_json::from_slice(&stored).expect("result JSON");
    assert!(
        result["receipt"]
            .as_object_mut()
            .expect("receipt")
            .remove("named_root")
            .is_some()
    );
    fixture
        .store
        .connection
        .execute(
            "UPDATE control_operation_results SET result_json = ?1
             WHERE operation = 'turn_begin' AND idempotency_key = ?2",
            rusqlite::params![serde_json::to_vec(&result).expect("result bytes"), key],
        )
        .expect("store the result as an earlier build wrote it");
    let replayed = begin(&mut fixture.store);
    assert_eq!(replayed.named_root, None);
    assert_eq!(replayed.grant_id, receipt.grant_id);
    let report = fixture.store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}
