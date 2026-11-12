use super::*;

fn prepare() -> (crate::test_support::TempHome, Fixture, ApplicabilityRequest) {
    let root = repository();
    let mut fixture = Fixture::new(root.path());
    fixture.observe(root.path(), 5);
    let receipt = fixture.record(10);
    let (origin, enrollment) = enrollment();
    let pack = with_witnesses(
        &fixture,
        &origin,
        vec![
            enrollment,
            envelope(&fixture.store, &origin, receipt.origin.as_ref().unwrap()),
            envelope(&fixture.store, &origin, &receipt.operation),
        ],
    );
    retain(&mut fixture.store, &pack);
    let request = intent(&fixture, receipt.verifications[0].record_id.clone(), origin);
    (root, fixture, request)
}

fn current_request(fixture: &Fixture, original: &ApplicabilityRequest) -> ApplicabilityRequest {
    let work = fixture
        .store
        .get_work_item(fixture.binding.work_id)
        .unwrap();
    let claim = fixture.store.claim_fixture(work.work_id);
    ApplicabilityRequest {
        decision: ObjectId::mint(),
        binding: ControlWorkBinding {
            work_revision: work.revision,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            ..fixture.binding.clone()
        },
        original: original.original.clone(),
        origin: original.origin.clone(),
        origin_cut: original.origin_cut.clone(),
        requirement: original.requirement.clone(),
        criterion: original.criterion,
    }
}

fn associate(
    fixture: &mut Fixture,
    request: &ApplicabilityRequest,
    holder: &str,
    second: i64,
) -> ObjectId {
    let mut actor = actor(holder);
    actor.run_id = Some(request.binding.run_id.0.to_string());
    fixture
        .store
        .associate_imported_verification(
            &project(),
            &SessionId(holder.into()),
            &actor,
            request,
            at(second),
        )
        .unwrap()
}

fn handoff(fixture: &mut Fixture, request: &ApplicabilityRequest) {
    let work = fixture
        .store
        .get_work_item(request.binding.work_id)
        .unwrap();
    let offer = fixture
        .store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: work.work_id,
                run_id: request.binding.run_id,
                expected_work_revision: work.revision,
                from: session(),
                to: SessionId("second".into()),
                claim_id: request.binding.claim_id,
                claim_fence: request.binding.claim_fence,
                ttl_seconds: 300,
                checkpoint_summary: "keep the named root".into(),
                actor: actor("runner"),
                idempotency_key: "offer-kept-root".into(),
                offered_at: at(4002),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    fixture
        .store
        .accept_work_handoff(
            &AcceptWorkHandoffRequest {
                work_id: work.work_id,
                offer_id: offer.offer_id,
                to: SessionId("second".into()),
                actor: actor("second"),
                idempotency_key: "accept-kept-root".into(),
                accepted_at: at(4003),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
}

#[test]
fn applicability_preserves_a_named_root_after_recovery_and_holder_change() {
    let (_root, mut fixture, original) = prepare();
    let work = fixture
        .store
        .get_work_item(original.binding.work_id)
        .unwrap();
    let recovered = claim(
        &mut fixture.store,
        &work,
        "runner",
        "recover-kept-root",
        4000,
        3600,
    );
    assert_eq!(recovered.claim_id, original.binding.claim_id);
    assert!(recovered.fence > original.binding.claim_fence);
    let request = current_request(&fixture, &original);
    let id = associate(&mut fixture, &request, "runner", 4001);
    assert!(
        fixture
            .store
            .imported_verification_summary(&id)
            .unwrap()
            .recorded_eligible
    );
    handoff(&mut fixture, &request);
    let request = current_request(&fixture, &original);
    let id = associate(&mut fixture, &request, "second", 4004);
    assert!(
        fixture
            .store
            .imported_verification_summary(&id)
            .unwrap()
            .recorded_eligible
    );
    let wrong = current_request(&fixture, &original);
    let actor = fixture.local_actor();
    let before = crate::storage::test_database_shape_snapshot(&fixture.store.connection).unwrap();
    assert!(
        fixture
            .store
            .associate_imported_verification(&project(), &session(), &actor, &wrong, at(4005))
            .is_err()
    );
    assert_eq!(
        before,
        crate::storage::test_database_shape_snapshot(&fixture.store.connection).unwrap()
    );
    let mut wrong_fence = request;
    wrong_fence.decision = ObjectId::mint();
    wrong_fence.binding.claim_fence -= 1;
    let mut wrong_actor = actor;
    wrong_actor.session_id = Some(SessionId("second".into()));
    assert!(
        fixture
            .store
            .associate_imported_verification(
                &project(),
                &SessionId("second".into()),
                &wrong_actor,
                &wrong_fence,
                at(4005)
            )
            .is_err()
    );
    assert_eq!(
        before,
        crate::storage::test_database_shape_snapshot(&fixture.store.connection).unwrap()
    );
    healthy(&fixture.store);
}

#[test]
fn applicability_reclaim_without_renaming_has_no_named_root() {
    let (_root, mut fixture, original) = prepare();
    let work = fixture
        .store
        .get_work_item(original.binding.work_id)
        .unwrap();
    fixture
        .store
        .release_work(
            &ReleaseWorkRequest {
                work_id: work.work_id,
                run_id: original.binding.run_id,
                expected_work_revision: work.revision,
                holder: session(),
                claim_id: original.binding.claim_id,
                claim_fence: original.binding.claim_fence,
                reason: "end the root binding".into(),
                waiver_reason: Some("fixture has no outstanding contribution".into()),
                actor: actor("runner"),
                idempotency_key: "release-kept-root".into(),
                released_at: at(12),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let work = fixture.store.get_work_item(work.work_id).unwrap();
    let recovered = claim(
        &mut fixture.store,
        &work,
        "runner",
        "reclaim-ended-root",
        13,
        3600,
    );
    assert_eq!(recovered.claim_id, original.binding.claim_id);
    let request = current_request(&fixture, &original);
    let actor = fixture.local_actor();
    let before = crate::storage::test_database_shape_snapshot(&fixture.store.connection).unwrap();
    assert!(
        fixture
            .store
            .associate_imported_verification(&project(), &session(), &actor, &request, at(14))
            .is_err()
    );
    assert_eq!(
        before,
        crate::storage::test_database_shape_snapshot(&fixture.store.connection).unwrap()
    );
    healthy(&fixture.store);
}
