//! The claim's named-root state that the host transport reports in the
//! session status and in a turn's begin receipt.

use super::*;
use crate::domain::{NamedRootState, holds_refused_source_text, is_refused_source_text_char};

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

/// The state returned by the ordinary host read for the captured association.
fn observed_state(store: &SqliteStore, host: &HostSession) -> NamedRootState {
    store
        .read_named_root(
            &host.project_id,
            &host.session_id,
            &host.connection_token,
            crate::storage::HostSessionAuthority::Work,
            host.binding.run_id,
            host.binding.claim_id,
        )
        .unwrap()
        .named_root
}

fn observe_association(
    store: &SqliteStore,
    host: &mut HostSession,
    work: &WorkItem,
    claim: &WorkClaim,
) -> NamedRootState {
    host.binding = fixture_binding(store, work, claim);
    observed_state(store, host)
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
        observed_state(&fixture.store, &host),
        NamedRootState::NoRoot
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
    assert_eq!(observed_state(&fixture.store, &host), named.clone());
    assert_eq!(
        observe_association(&fixture.store, &mut host, &work, &claim),
        named.clone(),
        "a replayed bind reports the state at the replay"
    );

    release_runner(&mut fixture, &work, &claim, 20);
    let released = NamedRootState::UnboundByRelease {
        last_generation: 9,
        released_at_position: release_position(&fixture.store, &claim),
    };
    assert_eq!(state(&fixture.store, &claim), released);
    assert_eq!(
        observed_state(&fixture.store, &host),
        released.clone(),
        "the session still names the released claim"
    );
    assert_eq!(
        observe_association(&fixture.store, &mut host, &work, &claim),
        released.clone(),
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
        observe_association(&fixture.store, &mut host, &current, &reclaimed),
        released.clone(),
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
        observe_association(&fixture.store, &mut host, &current, &reclaimed),
        bound("workspace-B", 10, 23),
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

fn object_count(store: &SqliteStore) -> i64 {
    store
        .connection
        .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
        .expect("object count")
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
