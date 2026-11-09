use super::*;

#[test]
fn shared_input_kept_root_accepts_new_checks_after_claim_changes() {
    for transition in ["recover", "recover-other", "renew", "handoff"] {
        let (_root, mut f, identity) = shared_fixture(true, false);
        f.store
            .set_acceptance_evaluation_policy(
                &AcceptanceEvaluationPolicy {
                    allowed_modes: vec![AcceptanceEvaluationMode::IndependentSession],
                    mechanical_basis: MechanicalBasis::Observed,
                    require_source_freshness: true,
                },
                &actor("policy-admin"),
                "kept-root-policy",
                None,
                at(5),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        let original = f.request.registration.binding.clone();
        let work = f.store.get_work_item(original.work_id).unwrap();
        let named = f
            .store
            .read_named_root(
                &f.project,
                &f.session,
                &f.connection,
                crate::storage::HostSessionAuthority::Work,
                original.run_id,
                original.claim_id,
            )
            .unwrap()
            .latest_event
            .unwrap();
        let original_event = f
            .store
            .get::<NamedRootBindingEvent>(&named.event)
            .unwrap()
            .unwrap();
        let holder = if matches!(transition, "recover-other" | "handoff") {
            "successor"
        } else {
            "runner"
        };
        let held = if transition == "handoff" {
            let offer = f
                .store
                .offer_work_handoff(
                    &OfferWorkHandoffRequest {
                        work_id: work.work_id,
                        run_id: original.run_id,
                        expected_work_revision: work.revision,
                        from: SessionId("runner".into()),
                        to: SessionId(holder.into()),
                        claim_id: original.claim_id,
                        claim_fence: original.claim_fence,
                        ttl_seconds: 7200,
                        checkpoint_summary: "keep the named source root".into(),
                        actor: actor("runner"),
                        idempotency_key: "kept-root-offer".into(),
                        offered_at: at(7),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
            f.store
                .accept_work_handoff(
                    &AcceptWorkHandoffRequest {
                        work_id: work.work_id,
                        offer_id: offer.offer_id,
                        to: SessionId(holder.into()),
                        actor: actor(holder),
                        idempotency_key: "kept-root-accept".into(),
                        accepted_at: at(8),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
            f.store.claim_fixture(work.work_id)
        } else {
            claim(
                &mut f.store,
                &work,
                holder,
                "kept-root-claim",
                if transition == "renew" { 7 } else { 4000 },
                7200,
            )
        };
        assert_eq!(held.claim_id, original.claim_id);
        if transition != "renew" {
            assert!(held.fence > original.claim_fence);
        }
        f.session = SessionId(holder.into());
        f.connection = f
            .store
            .resume_work_connection(&f.session, at(4001))
            .unwrap();
        let current = f.store.get_work_item(original.work_id).unwrap();
        f.request.registration.binding.claim_fence = held.fence;
        f.request.registration.binding.work_revision = current.revision;
        f.request.registration.initiating_session_id = f.session.clone();
        f.request.registration.registered_at = at(4001);
        f.request.run.started_at = at(4001);
        f.request.run.finished_at = at(4005);
        f.request.checks[0].started_at = Some(at(4002));
        f.request.checks[0].completed_at = Some(at(4005));
        let receipt = f
            .store
            .record_registered_verification(
                &f.project,
                &f.session,
                &f.connection,
                crate::storage::HostSessionAuthority::Work,
                &actor(holder),
                &f.request,
                at(4006),
            )
            .unwrap_or_else(|error| panic!("new check after {transition}: {error}"));
        let check = receipt.verifications[0].record_id.clone();
        let evidence = f.store.load_verification_evidence(&check).unwrap();
        assert_eq!(evidence.binding, f.request.registration.binding);
        assert_eq!(evidence.session_id, f.session);
        let mut input = evaluation(&f, &check, "unused");
        input.source_basis = Some(AcceptanceSourceBasis {
            workspace_id: Some(f.request.registration.root.workspace_id.clone()),
            identity: VerificationSourceIdentity::SourceId {
                source_id: identity.clone(),
            },
        });
        input.recorded_at = at(4007);
        let evaluated = record_evaluation(&mut f, &input).unwrap();
        let decision = evaluated.record.registered_input_decision.as_ref().unwrap();
        assert_eq!(decision.verification, check);
        assert_eq!(decision.named_root_binding, named.event);
        checkpoint(
            &mut f.store,
            &current,
            &held,
            holder,
            "kept-root-checkpoint",
            4008,
            std::slice::from_ref(&check),
        );
        let mut complete =
            completion_request(&current, &held, holder, &check, "kept-root-done", 4009);
        complete.acceptance.clear();
        complete.source_fingerprint = None;
        complete.source_id = Some(identity);
        let sealed = f
            .store
            .complete_work(&complete, &DevelopmentNoopRedactor)
            .unwrap();
        assert_eq!(sealed.acceptance_evaluation, Some(evaluated.evaluation));
        assert_eq!(
            f.store
                .get::<NamedRootBindingEvent>(&named.event)
                .unwrap()
                .unwrap(),
            original_event
        );
        let report = f.store.verify_all().unwrap();
        assert!(report.is_healthy(), "{transition}: {report:?}");
    }
}
