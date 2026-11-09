use super::*;

#[test]
fn kept_root_registration_preserves_authority_and_root_refusals() {
    for case in [
        "workspace",
        "generation",
        "moved",
        "ended",
        "late",
        "nonholder",
        "initiator",
        "stale-fence",
        "released",
    ] {
        let mut f = fixture();
        let binding = f.request.registration.binding.clone();
        let work = f.store.get_work_item(binding.work_id).unwrap();
        let mut now = 10;
        let expected_field = match case {
            "workspace" => {
                f.request.registration.root.workspace_id = "another-workspace".into();
                "root"
            }
            "generation" => {
                f.request.registration.root.generation += 1;
                "root"
            }
            "moved" | "late" => {
                f.store
                    .bind_named_root(
                        &f.project,
                        &f.session,
                        &f.connection,
                        crate::storage::HostSessionAuthority::Work,
                        binding.claim_id,
                        binding.claim_fence,
                        "workspace-b",
                        2,
                        at(6),
                        NamedRootBindingKind::Bound,
                        None,
                        &mut actor("runner"),
                        "moved-root",
                        at(6),
                    )
                    .unwrap();
                if case == "late" {
                    f.request.registration.root.workspace_id = "workspace-b".into();
                    f.request.registration.root.generation = 2;
                } else {
                    f.request.registration.registered_at = at(7);
                    f.request.run.started_at = at(7);
                    f.request.checks[0].started_at = Some(at(8));
                }
                "root"
            }
            "ended" => {
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
                        Some(NamedRootEndReason::ExplicitClear),
                        &mut actor("runner"),
                        "ended-root",
                        at(6),
                    )
                    .unwrap();
                "root"
            }
            "nonholder" => {
                f.session = SessionId("peer".into());
                f.connection = f.store.resume_work_connection(&f.session, at(6)).unwrap();
                f.request.registration.initiating_session_id = f.session.clone();
                "claim"
            }
            "initiator" => {
                f.request.registration.initiating_session_id = SessionId("peer".into());
                "initiating_session_id"
            }
            "stale-fence" => {
                let recovered = claim(&mut f.store, &work, "runner", "recover", 4000, 3600);
                assert!(recovered.fence > binding.claim_fence);
                f.request.registration.registered_at = at(4001);
                f.request.run.started_at = at(4001);
                f.request.run.finished_at = at(4005);
                f.request.checks[0].started_at = Some(at(4002));
                f.request.checks[0].completed_at = Some(at(4005));
                now = 4006;
                "claim"
            }
            "released" => {
                f.store
                    .release_work(
                        &ReleaseWorkRequest {
                            work_id: work.work_id,
                            run_id: binding.run_id,
                            expected_work_revision: work.revision,
                            holder: f.session.clone(),
                            claim_id: binding.claim_id,
                            claim_fence: binding.claim_fence,
                            reason: "end this claim's named root".into(),
                            waiver_reason: Some("no contribution before release".into()),
                            actor: actor("runner"),
                            idempotency_key: "release-root".into(),
                            released_at: at(6),
                        },
                        &DevelopmentNoopRedactor,
                    )
                    .unwrap();
                let current = f.store.get_work_item(work.work_id).unwrap();
                let reclaimed = claim(&mut f.store, &current, "runner", "reclaim", 7, 3600);
                assert_eq!(reclaimed.claim_id, binding.claim_id);
                assert!(reclaimed.fence > binding.claim_fence);
                f.request.registration.binding.claim_fence = reclaimed.fence;
                f.request.registration.binding.work_revision = current.revision;
                f.request.registration.registered_at = at(8);
                f.request.run.started_at = at(8);
                f.request.checks[0].started_at = Some(at(8));
                "root"
            }
            _ => unreachable!(),
        };
        let before = f.snapshot();
        let error = f
            .store
            .record_registered_verification(
                &f.project,
                &f.session,
                &f.connection,
                crate::storage::HostSessionAuthority::Work,
                &actor(&f.session.0),
                &f.request,
                at(now),
            )
            .unwrap_err();
        if case == "nonholder" {
            assert!(
                matches!(error, StoreError::WorkClaimMismatch { work } if work == binding.work_id),
                "{case}: {error}"
            );
        } else {
            assert!(
                matches!(error, StoreError::RegisteredVerificationRefused { ref field, .. }
                if field == expected_field),
                "{case}: {error}"
            );
        }
        assert_eq!(f.snapshot(), before, "{case}: refusal must write nothing");
    }
}

#[test]
fn kept_root_explicit_clear_stays_with_original_naming_session_after_recovery() {
    let mut f = fixture();
    let binding = f.request.registration.binding.clone();
    let work = f.store.get_work_item(binding.work_id).unwrap();
    let held = claim(&mut f.store, &work, "successor", "recover", 4000, 3600);
    let successor = SessionId("successor".into());
    let connection = f
        .store
        .resume_work_connection(&successor, at(4001))
        .unwrap();
    let before = f.snapshot();
    let error = f
        .store
        .bind_named_root(
            &f.project,
            &successor,
            &connection,
            crate::storage::HostSessionAuthority::Work,
            held.claim_id,
            held.fence,
            "workspace-a",
            1,
            at(4),
            NamedRootBindingKind::Ended,
            Some(NamedRootEndReason::ExplicitClear),
            &mut actor("successor"),
            "successor-clear",
            at(4002),
        )
        .unwrap_err();
    assert!(matches!(error, StoreError::NamedRootBindingRefused(_)));
    assert_eq!(f.snapshot(), before);
    let cleared = f
        .store
        .bind_named_root(
            &f.project,
            &f.session,
            &f.connection,
            crate::storage::HostSessionAuthority::Work,
            held.claim_id,
            held.fence,
            "workspace-a",
            1,
            at(4),
            NamedRootBindingKind::Ended,
            Some(NamedRootEndReason::ExplicitClear),
            &mut actor("runner"),
            "original-clear",
            at(4003),
        )
        .unwrap();
    assert_eq!(cleared.kind, NamedRootBindingKind::Ended);
    assert!(f.store.verify_all().unwrap().is_healthy());
}
