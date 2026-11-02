use super::super::session::{
    MAX_PROCESS_DEFAULT_SESSION_RECLAIMS_PER_CREATION,
    PROCESS_DEFAULT_SESSION_RECLAMATION_CANDIDATES_SQL,
};
use super::super::test_support::*;
use super::super::*;
use super::*;
use crate::storage::test_support::open_work_host;

mod claims;
mod evidence_page;
mod gate_evidence;
mod next_ready;
mod sessions;
mod stale_binding;
mod status;
mod typed_integrity;

#[test]
fn named_root_binding_is_claim_scoped_replayed_and_explicit_clear_is_session_scoped() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let work = store
        .create_work(
            &root_request("project-a", "named-root-work", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("work");
    let claim = claim(&mut store, &work, "runner", "named-root-claim", 2, 300);
    let host = open_work_host(&mut store, "runner", at(3));
    let mut host_actor = actor("runner");
    let named = store
        .bind_named_root(
            &work.project_id,
            &host.session_id,
            &host.connection_token,
            crate::storage::HostSessionAuthority::Work,
            claim.claim_id,
            claim.fence,
            "workspace-B",
            7,
            at(3),
            crate::domain::NamedRootBindingKind::Bound,
            None,
            &mut host_actor,
            "name-generation-7",
            at(4),
        )
        .expect("name root from host session without a work binding");
    assert_eq!(
        named.position.feed,
        crate::domain::FeedId::RunExecution(claim.run_id)
    );
    let replay = store
        .bind_named_root(
            &work.project_id,
            &host.session_id,
            &host.connection_token,
            crate::storage::HostSessionAuthority::Work,
            claim.claim_id,
            claim.fence,
            "workspace-B",
            7,
            at(3),
            crate::domain::NamedRootBindingKind::Bound,
            None,
            &mut host_actor,
            "name-generation-7",
            at(5),
        )
        .expect("exact retry");
    assert_eq!(replay, named);
    let peer = open_work_host(&mut store, "peer", at(4));
    let mut peer_actor = actor("peer");
    assert!(matches!(
        store.bind_named_root(
            &work.project_id,
            &peer.session_id,
            &peer.connection_token,
            crate::storage::HostSessionAuthority::Work,
            claim.claim_id,
            claim.fence,
            "workspace-B",
            7,
            at(3),
            crate::domain::NamedRootBindingKind::Ended,
            Some(crate::domain::NamedRootEndReason::ExplicitClear),
            &mut peer_actor,
            "peer-clear",
            at(5),
        ),
        Err(StoreError::NamedRootBindingRefused(_))
    ));
    let ended = store
        .bind_named_root(
            &work.project_id,
            &peer.session_id,
            &peer.connection_token,
            crate::storage::HostSessionAuthority::Work,
            claim.claim_id,
            claim.fence,
            "workspace-B",
            7,
            at(3),
            crate::domain::NamedRootBindingKind::Ended,
            Some(crate::domain::NamedRootEndReason::RootInvalid),
            &mut peer_actor,
            "host-ended-root",
            at(5),
        )
        .expect("host ends an invalid root from another bound session");
    assert!(ended.position.position > named.position.position);
    assert!(matches!(
        store.bind_named_root(
            &work.project_id,
            &host.session_id,
            &host.connection_token,
            crate::storage::HostSessionAuthority::Work,
            claim.claim_id,
            claim.fence,
            "workspace-B",
            7,
            at(3),
            crate::domain::NamedRootBindingKind::Bound,
            None,
            &mut host_actor,
            "stale-generation",
            at(6),
        ),
        Err(StoreError::NamedRootBindingRefused(_))
    ));
}
