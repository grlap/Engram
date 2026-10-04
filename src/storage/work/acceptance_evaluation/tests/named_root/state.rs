//! The claim's named-root state that the host transport reports in the
//! session status and in a turn's begin receipt.

use super::*;
use crate::domain::{
    EnvironmentComponents, EnvironmentEvidenceInput, ExecutionObservationInput, NamedRootState,
    holds_refused_source_text, is_refused_source_text_char,
};

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

/// Every way a root state can carry what its state does not name: a field no
/// state names, a field of another state, and a repeated tag.
fn tampered_states(state: &str) -> Vec<(String, &'static str)> {
    let open = state.strip_prefix('{').expect("an object");
    vec![
        (
            format!(r#"{{"root_hint":"x",{open}"#),
            "unknown field `root_hint`",
        ),
        (
            format!(r#"{{"last_generation":9,{open}"#),
            "unknown field `last_generation`",
        ),
        (
            format!(r#"{{"state":"bound",{open}"#),
            "duplicate field `state`",
        ),
    ]
}

/// The session status, a named-root read and a stored begin receipt each
/// decode the root state strictly: what its state does not name, or a
/// repeated key, is refused, and the stored receipt replays unchanged once
/// its recorded bytes are back.
#[test]
fn every_decoded_root_state_refuses_what_its_state_does_not_name() {
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
    let named = bound("workspace-B", 9, 11);
    let wire = serde_json::to_string(&named).expect("state");

    let status = fixture
        .store
        .control_status(
            &host.project_id,
            &host.session_id,
            &host.connection_token,
            &host.routing_token,
            at(12),
        )
        .expect("session status");
    assert_eq!(status.named_root, Some(named.clone()));
    let read = fixture
        .store
        .read_named_root(
            &host.project_id,
            &host.session_id,
            &host.connection_token,
            &host.routing_token,
            claim.run_id,
            claim.claim_id,
        )
        .expect("named-root read");
    assert_eq!(read.named_root, named);
    let status_text = serde_json::to_string(&status).expect("status");
    let read_text = serde_json::to_string(&read).expect("read");
    assert_eq!(
        serde_json::from_str::<crate::domain::ControlSessionStatus>(&status_text)
            .expect("status reads back"),
        status
    );
    assert_eq!(
        serde_json::from_str::<crate::domain::NamedRootRead>(&read_text).expect("read reads back"),
        read
    );
    for (state, expected) in tampered_states(&wire) {
        let error = serde_json::from_str::<crate::domain::ControlSessionStatus>(
            &status_text.replacen(&wire, &state, 1),
        )
        .expect_err("tampered status");
        assert!(error.to_string().contains(expected), "{error}");
        let error = serde_json::from_str::<crate::domain::NamedRootRead>(
            &read_text.replacen(&wire, &state, 1),
        )
        .expect_err("tampered read");
        assert!(error.to_string().contains(expected), "{error}");
    }

    let grant = host.grant(&mut fixture.store, &[EffectClass::Observe], false, 13);
    let key = host.key("begin");
    let begin = |store: &mut SqliteStore| {
        store.begin_control_turn(
            &host.project_id,
            &host.session_id,
            &host.connection_token,
            &host.routing_token,
            &grant.grant_id,
            &[],
            &key,
            at(14),
        )
    };
    let receipt = begin(&mut fixture.store).expect("begin host turn");
    assert!(matches!(
        &receipt,
        ControlTurnBeginDecision::Begin { receipt } if receipt.named_root == Some(named.clone())
    ));
    let stored_result = |store: &SqliteStore| -> Vec<u8> {
        store
            .connection
            .query_row(
                "SELECT result_json FROM control_operation_results
                 WHERE operation = 'turn_begin' AND idempotency_key = ?1",
                [&key],
                |row| row.get(0),
            )
            .expect("the stored begin result")
    };
    let recorded = stored_result(&fixture.store);
    let canonical = String::from_utf8(crate::canonical::canonical_bytes(&named).expect("bytes"))
        .expect("utf-8");
    let recorded_text = String::from_utf8(recorded.clone()).expect("utf-8");
    assert!(recorded_text.contains(&canonical), "{recorded_text}");
    let write_result = |store: &SqliteStore, bytes: &[u8]| {
        store
            .connection
            .execute(
                "UPDATE control_operation_results SET result_json = ?1
                 WHERE operation = 'turn_begin' AND idempotency_key = ?2",
                rusqlite::params![bytes, key],
            )
            .expect("store the begin result");
    };
    for (state, expected) in tampered_states(&canonical) {
        write_result(
            &fixture.store,
            recorded_text.replacen(&canonical, &state, 1).as_bytes(),
        );
        let error = begin(&mut fixture.store).expect_err("a tampered receipt is refused");
        assert!(error.to_string().contains(expected), "{error}");
    }
    write_result(&fixture.store, &recorded);
    assert_eq!(
        begin(&mut fixture.store).expect("the recorded receipt replays"),
        receipt
    );
    assert_eq!(stored_result(&fixture.store), recorded);
}

// New host source text, a workspace id or a source revision, refuses a
// control character or a bidirectional formatting control on every admitted
// host path, and root-generation history stays monotone across a release.
/// Every C0 and C1 control character and every character of Unicode's
/// `Bidi_Control` property is refused; other format characters and Windows
/// path spellings are not.
#[test]
fn the_refused_set_is_the_controls_and_the_bidi_controls() {
    let bidi_controls = [
        '\u{061C}', '\u{200E}', '\u{200F}', '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}',
        '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
    ];
    for character in ('\u{0}'..='\u{1F}')
        .chain(['\u{7F}'])
        .chain('\u{80}'..='\u{9F}')
        .chain(bidi_controls)
    {
        assert!(is_refused_source_text_char(character), "{character:?}");
    }
    for character in [
        ' ',
        '~',
        '\u{A0}',
        '\u{AD}',
        '\u{200B}',
        '\u{200C}',
        '\u{200D}',
        '\u{2060}',
        '\u{FE0F}',
        '\u{FEFF}',
        '\u{1F600}',
    ] {
        assert!(!is_refused_source_text_char(character), "{character:?}");
    }
    for kept in [
        "\\\\?\\C:\\work",
        "\\\\?\\UNC\\server\\share\\work",
        "C:\\work\\tree",
        "content-v1:0123abcd",
        "work\u{200D}tree\u{FE0F}",
    ] {
        assert!(!holds_refused_source_text(kept), "{kept:?}");
    }
}

fn observation(host: &HostSession, workspace: &str, revision: &str) -> ExecutionObservationInput {
    ExecutionObservationInput {
        observation_id: host.key("source-text"),
        action_fingerprint: ObjectId::from_canonical_bytes(b"source-text action"),
        effect: EffectClass::MutateLocal,
        outcome: ExecutionOutcome::Succeeded,
        source_changed: false,
        reported_source_change: None,
        source_basis: Some(ExecutionSourceBasis {
            workspace_id: workspace.into(),
            source_revision: revision.into(),
            source_root_generation: None,
            source_root_state: None,
        }),
        observed_at: Some(at(31)),
    }
}

fn environment(workspace: &str, component_workspace: &str) -> EnvironmentEvidenceInput {
    let components = EnvironmentComponents {
        toolchain: "rustc-test".into(),
        sandbox: None,
        workspace_id: component_workspace.into(),
        capability_map_revision: 1,
    };
    EnvironmentEvidenceInput {
        source_basis: ExecutionSourceBasis {
            workspace_id: workspace.into(),
            source_revision: "revision-B".into(),
            source_root_generation: None,
            source_root_state: None,
        },
        environment_fingerprint: CanonicalObject::freeze(&components)
            .expect("freeze components")
            .key()
            .clone(),
        components: Some(components),
        observed_at: at(31),
    }
}

fn object_count(store: &SqliteStore) -> i64 {
    store
        .connection
        .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
        .expect("object count")
}

/// A turn checkpoint whose observation or environment carries refused
/// source text is refused whole, naming the field and recording nothing; the
/// host's resend without that basis checkpoints.
#[test]
fn a_checkpoint_refuses_unsafe_source_text_whole_and_admits_the_resend() {
    let (mut fixture, _work, _claim, mut host) = bound_host();
    let store = &mut fixture.store;
    let grant = host.grant(store, &[EffectClass::MutateLocal], true, 30);
    host.begin(store, &grant, 31);
    let checkpoint = |store: &mut SqliteStore,
                      observations: &[ExecutionObservationInput],
                      environments: &[EnvironmentEvidenceInput],
                      key: &str| {
        store.checkpoint_control_turn_with_evidence(
            &host.project_id,
            &host.session_id,
            &host.connection_token,
            &host.routing_token,
            &grant.grant_id,
            TurnNextIntent::Continue,
            observations,
            &[],
            environments,
            key,
            at(32),
        )
    };
    let cases = [
        (
            vec![observation(&host, "workspace\u{202E}B", "revision-B")],
            Vec::new(),
            "observations[0].source_basis.workspace_id",
        ),
        (
            vec![observation(&host, "workspace-B", "revision\u{0}B")],
            Vec::new(),
            "observations[0].source_basis.source_revision",
        ),
        (
            Vec::new(),
            vec![environment("workspace\u{2069}B", "workspace\u{2069}B")],
            "environment_evidence[0].source_basis.workspace_id",
        ),
        (
            Vec::new(),
            vec![environment("workspace\u{85}B", "workspace\u{85}B")],
            "environment_evidence[0].source_basis.workspace_id",
        ),
    ];
    for (index, (observations, environments, field)) in cases.into_iter().enumerate() {
        let before = object_count(store);
        let refused = checkpoint(
            store,
            &observations,
            &environments,
            &format!("refused-{index}"),
        )
        .expect_err(field);
        assert!(
            matches!(&refused, StoreError::SourceBasisTextRefused { field: named } if named == field),
            "{refused:?}"
        );
        assert_eq!(
            crate::host::store_error_code(&refused),
            "source_basis_text_refused"
        );
        assert_eq!(object_count(store), before, "{field} recorded nothing");
    }
    // The observation-only resend under the refused request's own key: a
    // refusal records no operation under that key, so the resend is a fresh
    // request, not an idempotency conflict.
    let mut resend = observation(&host, "workspace-B", "revision-B");
    resend.source_basis = None;
    resend.observed_at = None;
    assert!(matches!(
        checkpoint(store, &[resend], &[], "refused-0").expect("the resend checkpoints"),
        ControlTurnCheckpointDecision::Checkpointed { .. }
    ));
}

/// Naming a root refuses a workspace with refused text, recording nothing,
/// and keeps a Windows extended path with a zero-width joiner byte for byte.
#[test]
fn naming_a_root_refuses_unsafe_workspace_text_and_keeps_other_text_exact() {
    let (mut fixture, _work, claim, host) = bound_host();
    let before = object_count(&fixture.store);
    let refused = host_binds(
        &mut fixture.store,
        &host,
        &claim,
        "workspace\u{202D}B",
        9,
        NamedRootBindingKind::Bound,
        11,
        "name-unsafe",
        11,
    )
    .expect_err("refused text");
    assert!(
        matches!(
            &refused,
            StoreError::NamedRootBindingRefused(reason)
                if reason.starts_with("workspace_id holds a control or bidirectional formatting character")
        ),
        "{refused:?}"
    );
    assert_eq!(object_count(&fixture.store), before);
    let kept = "\\\\?\\C:\\work\u{200D}tree";
    host_binds(
        &mut fixture.store,
        &host,
        &claim,
        kept,
        9,
        NamedRootBindingKind::Bound,
        11,
        "name-kept",
        11,
    )
    .expect("kept text names the root");
    let state = crate::storage::work::named_root_state_on(
        &fixture.store.connection,
        claim.run_id,
        claim.claim_id,
        i64::MAX,
    )
    .expect("named-root state");
    assert!(
        matches!(&state, NamedRootState::Bound { workspace_id, generation: 9, .. } if workspace_id == kept),
        "{state:?}"
    );
    // Ending a root carries no new host text: it must repeat the stored
    // workspace, so the text rule does not apply and a mismatch is the
    // ordinary binding refusal.
    let ended = host_binds(
        &mut fixture.store,
        &host,
        &claim,
        "workspace\u{202D}B",
        9,
        NamedRootBindingKind::Ended,
        11,
        "end-mismatch",
        12,
    )
    .expect_err("an end must repeat the bound workspace");
    assert!(
        matches!(ended, StoreError::NamedRootBindingRefused(_)),
        "{ended:?}"
    );
    host_binds(
        &mut fixture.store,
        &host,
        &claim,
        kept,
        9,
        NamedRootBindingKind::Ended,
        11,
        "end-kept",
        12,
    )
    .expect("the kept root ends");
}

/// Root-generation history stays monotone across a claim release: the claim
/// id is reused after release, so a re-claim may not name the released
/// generation or an older one again, the released state stays unbound until
/// a larger generation is named, and the store stays sound.
#[test]
fn a_reclaimed_claim_names_only_a_larger_generation_after_its_release() {
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
    .expect("host names B at 9");
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
    assert_eq!(reclaimed.claim_id, claim.claim_id, "the claim id is reused");
    let state = |store: &SqliteStore| {
        crate::storage::work::named_root_state_on(
            &store.connection,
            reclaimed.run_id,
            reclaimed.claim_id,
            i64::MAX,
        )
        .expect("named-root state")
    };
    let released = state(&fixture.store);
    assert!(
        matches!(
            released,
            NamedRootState::UnboundByRelease {
                last_generation: 9,
                ..
            }
        ),
        "{released:?}"
    );
    for (generation, key) in [(9, "rename-B-9"), (5, "rename-B-5")] {
        let refused = host_binds(
            &mut fixture.store,
            &host,
            &reclaimed,
            "workspace-B",
            generation,
            NamedRootBindingKind::Bound,
            23,
            key,
            23,
        )
        .expect_err("a generation no larger than the history is stale");
        assert!(
            matches!(refused, StoreError::NamedRootBindingRefused(_)),
            "{generation}: {refused:?}"
        );
        assert_eq!(
            state(&fixture.store),
            released,
            "{generation} binds nothing"
        );
    }
    host_binds(
        &mut fixture.store,
        &host,
        &reclaimed,
        "workspace-B",
        10,
        NamedRootBindingKind::Bound,
        24,
        "name-B-10",
        24,
    )
    .expect("a larger generation binds");
    assert!(
        matches!(
            state(&fixture.store),
            NamedRootState::Bound { generation: 10, .. }
        ),
        "the fresh generation binds"
    );
    let report = fixture.store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

/// Leaves the stored operation under `key` as an admission made before the
/// text rule left it: the row answers for `intent`, a request the rule now
/// refuses.
fn admitted_before_the_text_rule(
    store: &SqliteStore,
    host: &HostSession,
    operation: &str,
    key: &str,
    intent: &CanonicalObject,
) {
    let rows = store
        .connection
        .execute(
            "UPDATE control_operation_results SET intent_hash = ?1, intent_json = ?2
             WHERE session_id = ?3 AND operation = ?4 AND idempotency_key = ?5",
            rusqlite::params![
                intent.key().as_str(),
                intent.bytes(),
                host.session_id.0,
                operation,
                key
            ],
        )
        .expect("rewrite the stored intent");
    assert_eq!(rows, 1, "{operation} {key}");
}

/// The text rule runs after replay: an exact retry of a checkpoint or a
/// naming admitted before the rule, whose text the rule now refuses, still
/// answers with its stored receipt, while a fresh request with that text is
/// refused.
#[test]
fn an_exact_retry_admitted_before_the_text_rule_replays() {
    let (mut fixture, _work, claim, mut host) = bound_host();
    let store = &mut fixture.store;
    let grant = host.grant(store, &[EffectClass::MutateLocal], true, 30);
    host.begin(store, &grant, 31);
    let checkpoint =
        |store: &mut SqliteStore, observations: &[ExecutionObservationInput], key: &str| {
            store.checkpoint_control_turn_with_evidence(
                &host.project_id,
                &host.session_id,
                &host.connection_token,
                &host.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                observations,
                &[],
                &[],
                key,
                at(32),
            )
        };
    let admitted = checkpoint(
        store,
        &[observation(&host, "workspace-B", "revision-B")],
        "checkpoint-before",
    )
    .expect("checkpointed");
    let retry = [observation(&host, "workspace\u{202E}B", "revision-B")];
    let refused = checkpoint(store, &retry, "checkpoint-fresh").expect_err("refused text");
    assert!(
        matches!(refused, StoreError::SourceBasisTextRefused { .. }),
        "{refused:?}"
    );
    let intent = CanonicalObject::freeze(&crate::storage::ControlTurnCheckpointFingerprint {
        control_schema_version: crate::schema::CONTROL_SCHEMA_VERSION,
        session_id: &host.session_id,
        grant_id: &grant.grant_id,
        next_intent: TurnNextIntent::Continue,
        observations: &retry,
        verification_evidence: &[],
        environment_evidence: &[],
        idempotency_key: "checkpoint-before",
    })
    .expect("checkpoint intent");
    admitted_before_the_text_rule(
        store,
        &host,
        "turn_checkpoint",
        "checkpoint-before",
        &intent,
    );
    let before = object_count(store);
    let replayed = checkpoint(store, &retry, "checkpoint-before").expect("the retry replays");
    assert_eq!(format!("{replayed:?}"), format!("{admitted:?}"));
    assert_eq!(object_count(store), before, "a replay records nothing");

    let named = host_binds(
        store,
        &host,
        &claim,
        "workspace-B",
        9,
        NamedRootBindingKind::Bound,
        33,
        "name-before",
        33,
    )
    .expect("named");
    let unsafe_workspace = "workspace\u{202D}B";
    let refused = host_binds(
        store,
        &host,
        &claim,
        unsafe_workspace,
        10,
        NamedRootBindingKind::Bound,
        33,
        "name-fresh",
        33,
    )
    .expect_err("refused text");
    assert!(
        matches!(&refused, StoreError::NamedRootBindingRefused(reason) if reason.starts_with("workspace_id holds")),
        "{refused:?}"
    );
    let intent = CanonicalObject::freeze(&crate::storage::NamedRootBindingFingerprint {
        control_schema_version: crate::schema::CONTROL_SCHEMA_VERSION,
        session_id: &host.session_id,
        claim_id: &claim.claim_id,
        claim_fence: claim.fence,
        workspace_id: unsafe_workspace,
        generation: 9,
        named_at: at(33),
        kind: NamedRootBindingKind::Bound,
        end_reason: None,
        idempotency_key: "name-before",
    })
    .expect("naming intent");
    admitted_before_the_text_rule(store, &host, "named_root_bind", "name-before", &intent);
    let before = object_count(store);
    let replayed = host_binds(
        store,
        &host,
        &claim,
        unsafe_workspace,
        9,
        NamedRootBindingKind::Bound,
        33,
        "name-before",
        34,
    )
    .expect("the retry replays");
    assert_eq!(replayed, named);
    assert_eq!(object_count(store), before, "a replay records nothing");
}

/// The text rule runs after replay, but the trimmed-field rule runs before
/// it: at a field's start or end, a refused character that is also
/// whitespace (a tab, a line or page break, or U+0085) keeps the checkpoint's
/// trimmed-field refusal, `invalid_control_session`; inside the field it is
/// the text refusal.
#[test]
fn an_edge_whitespace_control_keeps_the_trimmed_field_refusal() {
    let (mut fixture, _work, _claim, mut host) = bound_host();
    let store = &mut fixture.store;
    let grant = host.grant(store, &[EffectClass::MutateLocal], true, 30);
    host.begin(store, &grant, 31);
    let mut checkpoint = |workspace: &str, key: &str| {
        store
            .checkpoint_control_turn_with_evidence(
                &host.project_id,
                &host.session_id,
                &host.connection_token,
                &host.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                &[observation(&host, workspace, "revision-B")],
                &[],
                &[],
                key,
                at(32),
            )
            .expect_err(workspace)
    };
    for (index, edge) in ["workspace-B\t", "\nworkspace-B", "workspace-B\u{85}"]
        .into_iter()
        .enumerate()
    {
        let refused = checkpoint(edge, &format!("edge-{index}"));
        assert!(
            matches!(refused, StoreError::InvalidControlSession(_)),
            "{edge:?}: {refused:?}"
        );
        assert_eq!(
            crate::host::store_error_code(&refused),
            "invalid_control_session"
        );
    }
    let inside = checkpoint("workspace\tB", "inside");
    assert_eq!(
        crate::host::store_error_code(&inside),
        "source_basis_text_refused",
        "{inside:?}"
    );
}
