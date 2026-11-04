use super::*;

/// A binding belongs to the claim, not to whoever holds it: renewal, a
/// handoff and recovery by another holder keep it, because the claim goes
/// on. Releasing the claim ends it, although the next claim of the run
/// reuses the claim id.
#[test]
fn a_binding_survives_holder_changes_on_its_claim_but_not_its_release() {
    let (mut store, work, claim, _) = named_work();
    assert_eq!(active_generation(&store, &claim), Some(9));
    let renewed = super::claim(&mut store, &work, "runner", "renew-runner", 5, 300);
    assert_eq!(renewed.claim_id, claim.claim_id);
    assert_eq!(active_generation(&store, &renewed), Some(9), "renewal");
    let work = store.get_work_item(work.work_id).expect("item");
    let offer = store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: work.work_id,
                run_id: renewed.run_id,
                expected_work_revision: work.revision,
                from: renewed.holder.clone(),
                to: SessionId("second".into()),
                claim_id: renewed.claim_id,
                claim_fence: renewed.fence,
                ttl_seconds: 300,
                checkpoint_summary: "handing the run to second".into(),
                actor: actor("runner"),
                idempotency_key: "offer-to-second".into(),
                offered_at: at(6),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("offer handoff");
    let handed = store
        .accept_work_handoff(
            &AcceptWorkHandoffRequest {
                work_id: work.work_id,
                offer_id: offer.offer_id,
                to: SessionId("second".into()),
                actor: actor("second"),
                idempotency_key: "accept-as-second".into(),
                accepted_at: at(7),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("accept handoff");
    assert_eq!(handed.claim_id, claim.claim_id);
    assert_eq!(active_generation(&store, &handed), Some(9), "handoff");
    let work = store.get_work_item(work.work_id).expect("item");
    let recovered = super::claim(&mut store, &work, "third", "recover-as-third", 2_000, 300);
    assert_eq!(recovered.holder, SessionId("third".into()));
    assert_eq!(recovered.claim_id, claim.claim_id);
    assert_eq!(active_generation(&store, &recovered), Some(9), "recovery");
    release(&mut store, &work, &recovered, "release-third", 2_001);
    let work = store.get_work_item(work.work_id).expect("item");
    let reclaimed = super::claim(&mut store, &work, "third", "reclaim-as-third", 2_002, 300);
    assert_eq!(reclaimed.claim_id, claim.claim_id);
    assert_eq!(
        active_generation(&store, &reclaimed),
        None,
        "release ends the binding"
    );
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

/// After its release a claim is unbound. A host that missed the release
/// may go on stating the old generation; that sighting is accepted as
/// recorded but is unbound: when the claim later names a fresh generation,
/// it is neither judged inside the root nor displaced, and like any unbound
/// change it is satisfied by a later matching check, while a change from
/// before the claim's first name is still displaced.
#[test]
fn a_released_generation_never_displaces_a_later_change() {
    let (mut store, work, claim, earlier) = named_work();
    release(&mut store, &work, &claim, "release-runner", 5);
    let work = store.get_work_item(work.work_id).expect("item");
    let claim = super::claim(&mut store, &work, "runner", "reclaim-runner", 6, 300);
    assert_eq!(active_generation(&store, &claim), None);
    let stale = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "stale-generation-in-D",
        7,
        Some(basis("workspace-D", "D7", Some(9))),
        None,
    );
    name_root(&mut store, &work, &claim, 10, 8);
    assert_eq!(active_generation(&store, &claim), Some(10));
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-B-10",
        VerificationKind::Test,
        VerificationResult::Passed,
        9,
        basis("workspace-B", "B9", Some(10)),
    );
    assert_eq!(
        obligation(&store, &claim, &Pick::Criterion),
        (WorkObligationState::Satisfied, Some(checked.clone()))
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-B-10",
        10,
        std::slice::from_ref(&checked),
    );
    let seal = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &checked,
        "complete-B-10",
        11,
    )
    .expect("the B check completes the work");
    assert_eq!(seal.foreign_workspace_changes, vec![earlier.clone()]);
    let records = store
        .work_run_obligations(claim.run_id)
        .expect("terminal obligations");
    let resolution_of = |change: &ObjectId| {
        records
            .iter()
            .find(|record| &record.obligation.triggering_observation == change)
            .and_then(|record| record.resolution.as_ref())
            .map(|event| event.resolution.clone())
    };
    assert!(matches!(
        resolution_of(&earlier),
        Some(WorkObligationResolution::Displaced { .. })
    ));
    assert!(matches!(
        resolution_of(&stale),
        Some(WorkObligationResolution::Satisfied { evidence, .. }) if evidence == checked
    ));
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

/// After the claim names a later generation, a check under the earlier one
/// no longer carries the bound criterion: `done` asks for a passing check in
/// the current named root.
#[test]
fn after_a_rename_a_check_under_the_old_generation_carries_no_bound_criterion() {
    let (mut store, work, claim, _) = named_work();
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-under-9",
        VerificationKind::Test,
        VerificationResult::Passed,
        5,
        basis("workspace-B", "B5", Some(9)),
    );
    assert_eq!(
        obligation(&store, &claim, &Pick::Criterion),
        (WorkObligationState::Satisfied, Some(checked.clone()))
    );
    name_root(&mut store, &work, &claim, 10, 6);
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-after-rename",
        7,
        std::slice::from_ref(&checked),
    );
    let refused = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &checked,
        "complete-after-rename",
        8,
    );
    assert!(
        matches!(
            &refused,
            Err(StoreError::WorkCompletionRefused { reason, .. })
                if reason.contains("requires a passing check in the current named source root")
        ),
        "{refused:?}"
    );
}

/// A later name displaces a change made in an earlier name's own root. The
/// claim moves its root from B at generation 9 to C at generation 10; the
/// change made in B under 9 is displaced and disclosed together with the
/// change from before any name, and a check in C completes the work.
#[test]
fn a_later_name_displaces_a_change_made_in_the_earlier_root() {
    let (mut store, work, claim, earlier) = named_work();
    let in_b = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "change-in-B-under-9",
        5,
        Some(basis("workspace-B", "B5", Some(9))),
        None,
    );
    name_root_in(&mut store, &work, &claim, "workspace-C", 10, 6);
    assert_eq!(active_generation(&store, &claim), Some(10));
    for change in [&earlier, &in_b] {
        assert_eq!(
            stock_completion_action(&store, &claim, change),
            WorkObligationCompletionAction::DoneDisplaces
        );
    }
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-C-10",
        VerificationKind::Test,
        VerificationResult::Passed,
        7,
        basis("workspace-C", "C7", Some(10)),
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-C-10",
        8,
        std::slice::from_ref(&checked),
    );
    let seal = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &checked,
        "complete-C-10",
        9,
    )
    .expect("the check in C completes the work");
    let mut displaced = seal.foreign_workspace_changes.clone();
    displaced.sort();
    let mut expected = vec![earlier, in_b];
    expected.sort();
    assert_eq!(displaced, expected);
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

/// A later name never discharges a change that was foreign to the root
/// bound when it was recorded. Workspace A reported a change while B was
/// bound at generation 9; after the claim names generation 10 that change
/// is still open, and `done` asks for an explicit human waiver.
#[test]
fn a_later_name_keeps_a_foreign_change_under_the_earlier_root_open() {
    let (mut store, work, claim, _) = named_work();
    let foreign = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "foreign-under-9",
        5,
        Some(basis("workspace-A", "A5", Some(9))),
        None,
    );
    name_root(&mut store, &work, &claim, 10, 6);
    assert_eq!(
        stock_completion_action(&store, &claim, &foreign),
        WorkObligationCompletionAction::WaiverOnly
    );
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-B-10",
        VerificationKind::Test,
        VerificationResult::Passed,
        7,
        basis("workspace-B", "B7", Some(10)),
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-B-10",
        8,
        std::slice::from_ref(&checked),
    );
    let refused = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &checked,
        "complete-B-10",
        9,
    );
    assert!(
        matches!(
            &refused,
            Err(StoreError::WorkCompletionRefused { reason, .. })
                if reason.contains("foreign workspace workspace-A") && reason.contains("human waiver")
        ),
        "{refused:?}"
    );
    assert_eq!(
        obligation(&store, &claim, &Pick::TriggeredBy(&foreign)),
        (WorkObligationState::Open, None)
    );
}

/// A later name never makes a foreign change its own. Workspace A reports a
/// change while B is bound at generation 9; the claim then names A itself at
/// generation 10, directly or after a release and a reclaim. A check in A
/// under 10 does not satisfy that change, it is not waived as untested, and
/// `done` asks for an explicit human waiver.
#[test]
fn naming_the_foreign_workspace_later_does_not_discharge_its_change() {
    for through_release in [false, true] {
        let (mut store, work, claim, _) = named_work();
        let foreign = source_mutation_from_basis(
            &mut store,
            &work,
            &claim,
            "runner",
            "foreign-under-9",
            5,
            Some(basis("workspace-A", "A5", Some(9))),
            None,
        );
        let claim = if through_release {
            release(&mut store, &work, &claim, "release-runner", 6);
            let work = store.get_work_item(work.work_id).expect("item");
            super::claim(&mut store, &work, "runner", "reclaim-runner", 7, 300)
        } else {
            claim
        };
        name_root_in(&mut store, &work, &claim, "workspace-A", 10, 8);
        let checked = host_verification_from_basis(
            &mut store,
            &work,
            &claim,
            "runner",
            "test-in-A-10",
            VerificationKind::Test,
            VerificationResult::Passed,
            9,
            basis("workspace-A", "A9", Some(10)),
        );
        assert_eq!(
            obligation(&store, &claim, &Pick::TriggeredBy(&foreign)),
            (WorkObligationState::Open, None),
            "through release: {through_release}"
        );
        checkpoint(
            &mut store,
            &work,
            &claim,
            "runner",
            "checkpoint-A-10",
            10,
            std::slice::from_ref(&checked),
        );
        let refused = complete(
            &mut store,
            &work,
            &claim,
            "runner",
            &checked,
            "complete-A-10",
            11,
        );
        assert!(
            matches!(
                &refused,
                Err(StoreError::WorkCompletionRefused { reason, .. })
                    if reason.contains("foreign workspace workspace-A")
                        && reason.contains("human waiver")
            ),
            "through release: {through_release}: {refused:?}"
        );
        assert_eq!(
            obligation(&store, &claim, &Pick::TriggeredBy(&foreign)),
            (WorkObligationState::Open, None),
            "through release: {through_release}"
        );
    }
}

/// A change in the root's own workspace from before the binding is part of
/// the source the root now holds: a later check of the root's newest
/// sighting satisfies it, so it is neither untested nor displaced.
#[test]
fn a_change_in_the_root_before_its_binding_is_satisfied_by_a_later_check() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut request = root_request("project-a", "named-root-work", 1);
    request.acceptance = vec!["run a test".into()];
    request.acceptance_bindings = vec![AcceptanceBinding {
        criterion: 1,
        requirement: VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: None,
        },
    }];
    let work = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("work");
    let claim = claim(&mut store, &work, "runner", "named-root-claim", 2, 300);
    let before = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "change-in-B-before-name",
        3,
        Some(basis("workspace-B", "B3", None)),
        None,
    );
    name_root(&mut store, &work, &claim, 9, 4);
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-B",
        VerificationKind::Test,
        VerificationResult::Passed,
        5,
        basis("workspace-B", "B3", Some(9)),
    );
    for pick in [Pick::Criterion, Pick::TriggeredBy(&before)] {
        assert_eq!(
            obligation(&store, &claim, &pick),
            (WorkObligationState::Satisfied, Some(checked.clone()))
        );
    }
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-B",
        6,
        std::slice::from_ref(&checked),
    );
    let seal = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &checked,
        "complete-B",
        7,
    )
    .expect("the B check completes the work");
    assert!(
        seal.foreign_workspace_changes.is_empty(),
        "{:?}",
        seal.foreign_workspace_changes
    );
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

/// Losing the root does not discharge a change foreign to it either.
/// Workspace A reports a change while B is bound at generation 9; the claim
/// then loses B, by a release and a reclaim or by the host ending B, and a
/// check in A runs while the claim is unbound. That check does not satisfy
/// the change, whether the claim stays unbound or later names A itself and
/// checks it there, and `done` asks for an explicit human waiver.
#[test]
fn an_unbound_interval_does_not_discharge_a_foreign_change() {
    for unbind in [Unbind::Release, Unbind::End] {
        for rename in [false, true] {
            let context = format!("{unbind:?}, later name: {rename}");
            let (mut store, work, claim, _) = named_work();
            let foreign = source_mutation_from_basis(
                &mut store,
                &work,
                &claim,
                "runner",
                "foreign-under-9",
                5,
                Some(basis("workspace-A", "A5", Some(9))),
                None,
            );
            let claim = match unbind {
                Unbind::Release => {
                    release(&mut store, &work, &claim, "release-runner", 6);
                    let work = store.get_work_item(work.work_id).expect("item");
                    super::claim(&mut store, &work, "runner", "reclaim-runner", 7, 300)
                }
                Unbind::End => {
                    end_root(&mut store, &work, &claim, "workspace-B", 9, 4, 6);
                    claim
                }
            };
            assert_eq!(active_generation(&store, &claim), None, "{context}");
            let unbound_check = host_verification_from_basis(
                &mut store,
                &work,
                &claim,
                "runner",
                "test-in-A-unbound",
                VerificationKind::Test,
                VerificationResult::Passed,
                8,
                basis("workspace-A", "A5", None),
            );
            assert_eq!(
                obligation(&store, &claim, &Pick::TriggeredBy(&foreign)),
                (WorkObligationState::Open, None),
                "{context}"
            );
            let checked = if rename {
                name_root_in(&mut store, &work, &claim, "workspace-A", 10, 9);
                host_verification_from_basis(
                    &mut store,
                    &work,
                    &claim,
                    "runner",
                    "test-in-A-10",
                    VerificationKind::Test,
                    VerificationResult::Passed,
                    10,
                    basis("workspace-A", "A5", Some(10)),
                )
            } else {
                unbound_check
            };
            assert_eq!(
                obligation(&store, &claim, &Pick::TriggeredBy(&foreign)),
                (WorkObligationState::Open, None),
                "{context}"
            );
            checkpoint(
                &mut store,
                &work,
                &claim,
                "runner",
                "checkpoint-A",
                11,
                std::slice::from_ref(&checked),
            );
            let refused = complete(
                &mut store,
                &work,
                &claim,
                "runner",
                &checked,
                "complete-A",
                12,
            );
            assert!(
                matches!(
                    &refused,
                    Err(StoreError::WorkCompletionRefused { reason, .. })
                        if reason.contains("foreign workspace workspace-A")
                            && reason.contains("human waiver")
                ),
                "{context}: {refused:?}"
            );
            assert_eq!(
                obligation(&store, &claim, &Pick::TriggeredBy(&foreign)),
                (WorkObligationState::Open, None),
                "{context}"
            );
            let report = store.verify_all().expect("doctor");
            assert!(report.is_healthy(), "{context}: {report:?}");
        }
    }
}

/// A change whose root cannot be established, recorded while B was bound at
/// generation 9, keeps needing a fresh check in a named root after B ends, is
/// released, or is renamed. A check while the claim is unbound, even one that
/// accounts for a later change, does not satisfy it; `done` neither waives it
/// as untested nor completes; and a fresh check in a later named root
/// accounts for it.
#[test]
fn an_unknown_root_change_outlives_its_binding() {
    for unbind in [Some(Unbind::Release), Some(Unbind::End), None] {
        let context = format!("{unbind:?}");
        let (mut store, work, claim, _) = named_work();
        let before = host_verification_from_basis(
            &mut store,
            &work,
            &claim,
            "runner",
            "test-before-unknown",
            VerificationKind::Test,
            VerificationResult::Passed,
            5,
            basis("workspace-B", "B5", Some(9)),
        );
        let unknown = source_mutation_from_basis(
            &mut store,
            &work,
            &claim,
            "runner",
            "unknown-under-9",
            6,
            None,
            None,
        );
        let (claim, stale) = if let Some(unbind) = unbind {
            let claim = match unbind {
                Unbind::Release => {
                    release(&mut store, &work, &claim, "release-runner", 7);
                    let work = store.get_work_item(work.work_id).expect("item");
                    super::claim(&mut store, &work, "runner", "reclaim-runner", 8, 300)
                }
                Unbind::End => {
                    end_root(&mut store, &work, &claim, "workspace-B", 9, 4, 7);
                    claim
                }
            };
            assert_eq!(active_generation(&store, &claim), None, "{context}");
            // Unbound, the host reports a change it can place, and a
            // check that accounts for it by the rules without a root.
            source_mutation_from_basis(
                &mut store,
                &work,
                &claim,
                "runner",
                "unbound-change",
                9,
                Some(basis("workspace-B", "B9", None)),
                None,
            );
            let unbound_check = host_verification_from_basis(
                &mut store,
                &work,
                &claim,
                "runner",
                "test-unbound",
                VerificationKind::Test,
                VerificationResult::Passed,
                10,
                basis("workspace-B", "B9", None),
            );
            (claim, unbound_check)
        } else {
            name_root(&mut store, &work, &claim, 10, 7);
            (claim, before)
        };
        assert_eq!(
            obligation(&store, &claim, &Pick::TriggeredBy(&unknown)),
            (WorkObligationState::Open, None),
            "{context}"
        );
        assert_eq!(
            stock_completion_action(&store, &claim, &unknown),
            if unbind.is_some() {
                WorkObligationCompletionAction::NameRootCheckOrWaiver
            } else {
                WorkObligationCompletionAction::CheckOrWaiver
            },
            "{context}"
        );
        checkpoint(
            &mut store,
            &work,
            &claim,
            "runner",
            "checkpoint-stale",
            11,
            std::slice::from_ref(&stale),
        );
        let refused = complete(
            &mut store,
            &work,
            &claim,
            "runner",
            &stale,
            "complete-stale",
            12,
        );
        let remedy = if unbind.is_some() {
            "name a root and run a fresh check in it"
        } else {
            "run a fresh check in the named root"
        };
        assert!(
            matches!(
                &refused,
                Err(StoreError::WorkCompletionRefused { reason, .. })
                    if reason.contains("unknown workspace") && reason.contains(remedy)
            ),
            "{context}: {refused:?}"
        );
        assert_eq!(
            obligation(&store, &claim, &Pick::TriggeredBy(&unknown)),
            (WorkObligationState::Open, None),
            "{context}"
        );
        if unbind.is_some() {
            name_root(&mut store, &work, &claim, 10, 13);
        }
        let fresh = host_verification_from_basis(
            &mut store,
            &work,
            &claim,
            "runner",
            "test-in-B-10",
            VerificationKind::Test,
            VerificationResult::Passed,
            14,
            basis("workspace-B", "B14", Some(10)),
        );
        assert_eq!(
            obligation(&store, &claim, &Pick::TriggeredBy(&unknown)),
            (WorkObligationState::Satisfied, Some(fresh.clone())),
            "{context}"
        );
        checkpoint(
            &mut store,
            &work,
            &claim,
            "runner",
            "checkpoint-fresh",
            15,
            &[stale, fresh.clone()],
        );
        complete(
            &mut store,
            &work,
            &claim,
            "runner",
            &fresh,
            "complete-fresh",
            16,
        )
        .unwrap_or_else(|error| panic!("{context}: {error:?}"));
        let report = store.verify_all().expect("doctor");
        assert!(report.is_healthy(), "{context}: {report:?}");
    }
}
