//! A registered record sent again after its claim or root moved: Engram
//! accepts it while the registration's holder and workspace continue, and
//! says which refusals may still clear.

use super::*;

/// Records a named-root event for the claim at `second`, as the host does
/// when it names a root again after a restart (`Bound`) or clears it
/// (`Ended`, naming the root as it was named at `named_second`).
fn name_root(
    f: &mut Fixture,
    claim: &WorkClaim,
    (workspace, generation): (&str, i64),
    kind: NamedRootBindingKind,
    named_second: i64,
    second: i64,
) {
    let end_reason =
        (kind == NamedRootBindingKind::Ended).then_some(NamedRootEndReason::ExplicitClear);
    f.store
        .bind_named_root(
            &f.project,
            &f.session,
            &f.connection,
            crate::storage::HostSessionAuthority::Work,
            claim.claim_id,
            claim.fence,
            workspace,
            generation,
            at(named_second),
            kind,
            end_reason,
            &mut actor("runner"),
            &format!("root-{workspace}-{generation}-{second}"),
            at(second),
        )
        .unwrap();
}

/// The fixture on the live ingress: a new write states its input as a
/// shared `source_id`.
fn live_fixture() -> Fixture {
    let mut f = fixture();
    f.use_shared_source();
    f
}

fn work(f: &Fixture) -> WorkItem {
    f.store
        .get_work_item(f.request.registration.binding.work_id)
        .unwrap()
}

/// The claim's holder edits the work's title, which changes its revision.
fn edit_title(f: &mut Fixture, claim: &WorkClaim, key: &str, second: i64) {
    let work = work(f);
    f.store
        .revise_work(
            &ReviseWorkRequest {
                work_id: work.work_id,
                expected_revision: work.revision,
                patch: WorkRevisionPatch {
                    title: Some(format!("Edited: {key}")),
                    ..WorkRevisionPatch::default()
                },
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
        .unwrap();
}

fn refused_field(result: Result<RegisteredVerificationReceipt, StoreError>) -> String {
    match result {
        Err(StoreError::RegisteredVerificationRefused { field, .. }) => field,
        other => panic!("expected a refusal naming its field, got {other:?}"),
    }
}

/// The run ends after the claim's original expiry, with the claim lapsed, and
/// the same session recovers it at a higher fence: the record is accepted.
#[test]
fn a_record_is_accepted_after_the_same_session_recovers_its_lapsed_claim() {
    let mut f = live_fixture();
    f.request.run.finished_at = at(3999);
    f.request.checks[0].completed_at = Some(at(3999));
    let work = work(&f);
    let recovered = claim(&mut f.store, &work, "runner", "recover", 4000, 3600);
    assert!(recovered.fence > f.request.registration.binding.claim_fence);
    let receipt = f.record_at(at(4001)).unwrap();
    assert_eq!(receipt.verifications.len(), 1);
}

/// A run longer than the claim's time to live: the holder renews the claim
/// while the run goes on, the run ends after the claim's original expiry, and
/// its record is accepted under the registered claim and fence.
#[test]
fn a_run_past_the_original_expiry_is_accepted_after_a_renewal_during_the_run() {
    let mut f = live_fixture();
    let binding = f.request.registration.binding.clone();
    let work = work(&f);
    let renewed = claim(&mut f.store, &work, "runner", "heartbeat", 3000, 3600);
    assert_eq!(
        (renewed.claim_id, renewed.fence),
        (binding.claim_id, binding.claim_fence)
    );
    assert!(renewed.expires_at > at(4001));
    f.request.run.finished_at = at(4000);
    f.request.checks[0].completed_at = Some(at(4000));
    let receipt = f.record_at(at(4001)).unwrap();
    assert_eq!(receipt.verifications.len(), 1);
}

/// The same session recovered its claim before the run was registered, and
/// the registration still names the earlier fence: no other session held the
/// claim, so the record is accepted.
#[test]
fn a_registration_naming_an_earlier_fence_of_the_same_holder_is_accepted() {
    let mut f = live_fixture();
    let work = work(&f);
    claim(&mut f.store, &work, "runner", "recover", 4000, 3600);
    f.request.registration.registered_at = at(4001);
    f.request.run.started_at = at(4001);
    f.request.run.finished_at = at(4005);
    f.request.checks[0].started_at = Some(at(4002));
    f.request.checks[0].completed_at = Some(at(4005));
    assert_eq!(f.record_at(at(4006)).unwrap().verifications.len(), 1);
}

/// The host states registration time to the millisecond. A root named again
/// within the registration's millisecond was there when the run was
/// registered.
#[test]
fn a_root_named_within_the_registration_millisecond_counts_as_registered() {
    let mut f = live_fixture();
    let binding = f.request.registration.binding.clone();
    let named_at = at(5) + chrono::Duration::microseconds(600);
    f.store
        .bind_named_root(
            &f.project,
            &f.session,
            &f.connection,
            crate::storage::HostSessionAuthority::Work,
            binding.claim_id,
            binding.claim_fence,
            "workspace-a",
            2,
            named_at,
            NamedRootBindingKind::Bound,
            None,
            &mut actor("runner"),
            "rename-within-the-millisecond",
            named_at,
        )
        .unwrap();
    f.request.registration.root.generation = 2;
    assert_eq!(f.record().unwrap().verifications.len(), 1);
}

/// While a handoff offer is open, holder writes wait: the record is refused
/// as one that may still clear, and accepted once the offer has expired.
#[test]
fn an_open_handoff_offer_refuses_with_claim_live_until_it_expires() {
    let mut f = live_fixture();
    let binding = f.request.registration.binding.clone();
    let work = work(&f);
    f.store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: work.work_id,
                run_id: binding.run_id,
                expected_work_revision: work.revision,
                from: f.session.clone(),
                to: SessionId("successor".into()),
                claim_id: binding.claim_id,
                claim_fence: binding.claim_fence,
                ttl_seconds: 60,
                checkpoint_summary: "offer the run".into(),
                actor: actor("runner"),
                idempotency_key: "open-offer".into(),
                offered_at: at(6),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let before = f.snapshot();
    assert_eq!(refused_field(f.record()), "claim_live");
    assert_eq!(f.snapshot(), before, "a refusal stores nothing");
    assert_eq!(f.record_at(at(100)).unwrap().verifications.len(), 1);
}

/// A root ended because its session was gone at restore is never named again
/// by that session, so the record is refused for good.
#[test]
fn a_root_ended_for_a_session_gone_at_restore_refuses_the_record_for_good() {
    let mut f = live_fixture();
    let binding = f.request.registration.binding.clone();
    f.store
        .bind_named_root(
            &f.project,
            &f.session,
            &f.connection,
            crate::storage::HostSessionAuthority::Work,
            binding.claim_id,
            binding.claim_fence,
            "workspace-a",
            1,
            at(4),
            NamedRootBindingKind::Ended,
            Some(NamedRootEndReason::SessionGoneAtRestore),
            &mut actor("runner"),
            "gone-at-restore",
            at(6),
        )
        .unwrap();
    let before = f.snapshot();
    assert_eq!(refused_field(f.record()), "root");
    assert_eq!(f.snapshot(), before);
}

/// While the claim has lapsed and nobody has recovered it, the record of a run
/// that ended after the original expiry is refused as one that may still
/// clear, and nothing is stored; the same key and body are accepted once the
/// session recovers the claim.
#[test]
fn a_lapsed_claim_refuses_with_claim_live_and_the_same_record_is_accepted_after_recovery() {
    let mut f = live_fixture();
    f.request.run.finished_at = at(3650);
    f.request.checks[0].completed_at = Some(at(3650));
    let before = f.snapshot();
    assert_eq!(refused_field(f.record_at(at(3700))), "claim_live");
    assert_eq!(f.snapshot(), before, "a refusal stores nothing");
    let work = work(&f);
    claim(&mut f.store, &work, "runner", "recover", 3800, 3600);
    assert_eq!(f.record_at(at(3801)).unwrap().verifications.len(), 1);
}

/// A root that ended after registration, or was named again on the same
/// workspace, which always takes a later generation, ends the binding: the
/// record's checks could only credit under the generation it names. The
/// refusal names the root even when the work was also edited.
#[test]
fn a_root_ended_or_named_again_since_registration_refuses_the_record_for_good() {
    for (case, edited) in [
        ("ended", false),
        ("named-again", false),
        ("ended", true),
        ("named-again", true),
    ] {
        let mut f = live_fixture();
        let work = work(&f);
        let recovered = claim(&mut f.store, &work, "runner", "restart", 4000, 3600);
        name_root(
            &mut f,
            &recovered,
            ("workspace-a", 1),
            NamedRootBindingKind::Ended,
            4,
            4001,
        );
        if case == "named-again" {
            name_root(
                &mut f,
                &recovered,
                ("workspace-a", 2),
                NamedRootBindingKind::Bound,
                4002,
                4002,
            );
        }
        if edited {
            edit_title(&mut f, &recovered, "edit-after-root", 4003);
        }
        let case = format!("{case}, edited={edited}");
        let before = f.snapshot();
        assert_eq!(refused_field(f.record_at(at(4004))), "root", "{case}");
        assert_eq!(f.snapshot(), before, "{case}: a refusal stores nothing");
    }
}

/// Decomposes `parent` into one child with the given requirement.
fn decompose_one(
    store: &mut SqliteStore,
    parent: &WorkItem,
    requirement: ChildRequirement,
    key: &str,
) -> WorkItem {
    use crate::storage::work::test_support::{child, delegated};
    store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: parent.work_id,
                expected_parent_revision: parent.revision,
                children: vec![child(key, requirement, key)],
                prerequisites: Vec::new(),
                authority: delegated("project-a", "planner"),
                actor: actor("planner"),
                idempotency_key: key.into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap()
        .children
        .remove(0)
}

/// An optional child's claim lapses and its parent completes: the claim can
/// never be recovered, so the record is refused for good rather than as one
/// that may still clear. That holds for a request stamped before the claim
/// lapsed, as one is when it waits for the write lock while the parent
/// completes. The parent is the root, whose completion ends the root
/// execution, or a mid-level item, whose completion leaves it running. The
/// refusal names the claim even when the child was also edited.
#[test]
fn a_claim_under_a_completed_parent_refuses_the_record_for_good() {
    for parent_is_root in [true, false] {
        for edited in [false, true] {
            refuses_under_a_completed_parent(parent_is_root, edited);
        }
    }
}

fn refuses_under_a_completed_parent(parent_is_root: bool, edited: bool) {
    use crate::storage::work::test_support::{checkpoint, complete, evidence};
    let mut store = SqliteStore::open_in_memory().unwrap();
    let root = store
        .create_work(
            &root_request("project-a", "parent-work", 1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let parent = if parent_is_root {
        root
    } else {
        decompose_one(&mut store, &root, ChildRequirement::Required, "mid")
    };
    let parent = store.get_work_item(parent.work_id).unwrap();
    let leaf = decompose_one(&mut store, &parent, ChildRequirement::Optional, "optional");
    let mut f = fixture_for_work(store, &leaf, 0);
    f.use_shared_source();
    let child_claim = f
        .store
        .current_work_claim(f.request.registration.binding.work_id)
        .unwrap()
        .unwrap();
    if edited {
        edit_title(&mut f, &child_claim, "child-edit", 5);
    }
    let child_item = work(&f);
    let child_claim = f
        .store
        .current_work_claim(child_item.work_id)
        .unwrap()
        .unwrap();
    let contributed = evidence(
        &mut f.store,
        &child_item,
        &child_claim,
        "runner",
        "child-evidence",
        6,
    );
    checkpoint(
        &mut f.store,
        &child_item,
        &child_claim,
        "runner",
        "child-checkpoint",
        7,
        std::slice::from_ref(&contributed),
    );
    let parent = f.store.get_work_item(parent.work_id).unwrap();
    let parent_claim = claim(
        &mut f.store,
        &parent,
        "root-agent",
        "parent-claim",
        4000,
        100,
    );
    let parent_evidence = evidence(
        &mut f.store,
        &parent,
        &parent_claim,
        "root-agent",
        "parent-evidence",
        4001,
    );
    checkpoint(
        &mut f.store,
        &parent,
        &parent_claim,
        "root-agent",
        "parent-checkpoint",
        4002,
        std::slice::from_ref(&parent_evidence),
    );
    complete(
        &mut f.store,
        &parent,
        &parent_claim,
        "root-agent",
        &parent_evidence,
        "parent-done",
        4003,
    )
    .unwrap();
    assert_eq!(
        work(&f).revision,
        child_item.revision,
        "completing the parent leaves the open child's revision"
    );
    assert_eq!(
        child_item.revision != f.request.registration.binding.work_revision,
        edited
    );
    let before = f.snapshot();
    for second in [3000, 4004] {
        let case = format!("parent_is_root={parent_is_root}, edited={edited} at {second}");
        assert_eq!(refused_field(f.record_at(at(second))), "claim", "{case}");
        assert_eq!(f.snapshot(), before, "{case}: a refusal stores nothing");
    }
}

/// A root bound to another workspace after registration ends the binding,
/// even when it comes back to the registered workspace.
#[test]
fn a_root_detour_to_another_workspace_refuses_the_record_for_good() {
    let mut f = live_fixture();
    let binding = f.request.registration.binding.clone();
    let held = f
        .store
        .current_work_claim(binding.work_id)
        .unwrap()
        .unwrap();
    name_root(
        &mut f,
        &held,
        ("workspace-b", 2),
        NamedRootBindingKind::Bound,
        6,
        6,
    );
    name_root(
        &mut f,
        &held,
        ("workspace-a", 3),
        NamedRootBindingKind::Bound,
        7,
        7,
    );
    let before = f.snapshot();
    assert_eq!(refused_field(f.record()), "root");
    assert_eq!(f.snapshot(), before);
}

/// Another session recovers the lapsed claim: the record is refused for good,
/// even after the first session takes the claim back.
#[test]
fn a_claim_taken_by_another_session_refuses_the_record_for_good() {
    let mut f = live_fixture();
    let work = work(&f);
    claim(&mut f.store, &work, "other", "takeover", 4000, 3600);
    let work = self::work(&f);
    claim(&mut f.store, &work, "runner", "return", 8000, 3600);
    let before = f.snapshot();
    assert_eq!(refused_field(f.record_at(at(8001))), "claim");
    assert_eq!(f.snapshot(), before);
}

/// The session hands the claim to another session after registering: the
/// handoff ended the registered binding, and the refusal names the claim even
/// after the successor edits the work.
#[test]
fn a_handoff_to_another_session_refuses_the_record_for_good() {
    let mut f = live_fixture();
    let binding = f.request.registration.binding.clone();
    let work = work(&f);
    let offer = f
        .store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: work.work_id,
                run_id: binding.run_id,
                expected_work_revision: work.revision,
                from: f.session.clone(),
                to: SessionId("successor".into()),
                claim_id: binding.claim_id,
                claim_fence: binding.claim_fence,
                ttl_seconds: 7200,
                checkpoint_summary: "hand the run over".into(),
                actor: actor("runner"),
                idempotency_key: "offer".into(),
                offered_at: at(6),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    f.store
        .accept_work_handoff(
            &AcceptWorkHandoffRequest {
                work_id: work.work_id,
                offer_id: offer.offer_id,
                to: SessionId("successor".into()),
                actor: actor("successor"),
                idempotency_key: "accept".into(),
                accepted_at: at(7),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let before = f.snapshot();
    assert_eq!(refused_field(f.record()), "claim");
    assert_eq!(f.snapshot(), before);
    let successor = f.store.current_work_claim(work.work_id).unwrap().unwrap();
    edit_title(&mut f, &successor, "successor-edit", 8);
    assert_ne!(
        self::work(&f).revision,
        f.request.registration.binding.work_revision
    );
    let before = f.snapshot();
    assert_eq!(refused_field(f.record()), "claim");
    assert_eq!(f.snapshot(), before);
}

/// The session releases its claim and claims the item again: the release
/// ended the registered binding.
#[test]
fn a_release_and_reclaim_by_the_same_session_refuses_the_record_for_good() {
    let mut f = live_fixture();
    let binding = f.request.registration.binding.clone();
    let work = work(&f);
    f.store
        .release_work(
            &ReleaseWorkRequest {
                work_id: work.work_id,
                run_id: binding.run_id,
                expected_work_revision: work.revision,
                holder: f.session.clone(),
                claim_id: binding.claim_id,
                claim_fence: binding.claim_fence,
                reason: "let it go".into(),
                waiver_reason: Some("no contribution before release".into()),
                actor: actor("runner"),
                idempotency_key: "release".into(),
                released_at: at(6),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let work = self::work(&f);
    claim(&mut f.store, &work, "runner", "reclaim", 7, 3600);
    let before = f.snapshot();
    assert_eq!(refused_field(f.record()), "claim");
    assert_eq!(f.snapshot(), before);
}
