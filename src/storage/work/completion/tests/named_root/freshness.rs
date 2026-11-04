use super::*;

/// A check the host verified only after a change whose root is unknown,
/// but that ran before it, cannot account for that change: what counts is
/// when the check ran, its producer's place on the run feed, not when its
/// verification was recorded. This holds whether the verification cites a
/// producer stored in an earlier checkpoint or arrives in the same
/// checkpoint as the change.
#[test]
fn a_check_run_before_an_unknown_root_change_cannot_account_for_it() {
    for same_checkpoint in [false, true] {
        let (mut store, work, claim, _) = named_work();
        let check_basis = || basis("workspace-B", "B5", Some(9));
        let (unknown, check) = if same_checkpoint {
            let transaction = begin(&mut store);
            let producer = host_check_producer(
                &transaction,
                &work,
                &claim,
                "runner",
                "early-check",
                5,
                check_basis(),
            );
            let change = source_mutation_observation(
                &transaction,
                &work,
                &claim,
                "runner",
                "unknown-root",
                6,
                None,
                None,
            );
            let unknown = append_source_mutation_on(&transaction, &change);
            let check = host_verification_of_producer(
                &transaction,
                &work,
                &claim,
                "runner",
                "early-check",
                5,
                7,
                check_basis(),
                producer,
            );
            transaction.commit().expect("commit the checkpoint");
            (unknown, check)
        } else {
            let transaction = begin(&mut store);
            let producer = host_check_producer(
                &transaction,
                &work,
                &claim,
                "runner",
                "early-check",
                5,
                check_basis(),
            );
            transaction.commit().expect("commit the producer");
            let unknown = source_mutation_from_basis(
                &mut store,
                &work,
                &claim,
                "runner",
                "unknown-root",
                6,
                None,
                None,
            );
            let transaction = begin(&mut store);
            let check = host_verification_of_producer(
                &transaction,
                &work,
                &claim,
                "runner",
                "early-check",
                5,
                7,
                check_basis(),
                producer,
            );
            transaction.commit().expect("commit the late verification");
            (unknown, check)
        };
        for pick in [Pick::Criterion, Pick::TriggeredBy(&unknown)] {
            assert_eq!(
                obligation(&store, &claim, &pick),
                (WorkObligationState::Open, None),
                "same checkpoint: {same_checkpoint}"
            );
        }
        checkpoint(
            &mut store,
            &work,
            &claim,
            "runner",
            "checkpoint-early-check",
            8,
            std::slice::from_ref(&check),
        );
        assert!(
            matches!(
                complete(
                    &mut store,
                    &work,
                    &claim,
                    "runner",
                    &check,
                    "complete-early",
                    9
                ),
                Err(StoreError::WorkCompletionRefused { .. })
            ),
            "same checkpoint: {same_checkpoint}"
        );
    }
}

/// A change whose root is unknown, after a known change in the named root,
/// leaves that change's revision behind. A fresh check in the root after
/// both, at the root's newest revision, accounts for the two changes and
/// the bound criterion, and the work completes.
#[test]
fn a_fresh_check_after_an_unknown_root_change_accounts_for_an_older_named_root_change() {
    let (mut store, work, claim, _) = named_work();
    let known = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "change-in-B",
        5,
        Some(basis("workspace-B", "B5", Some(9))),
        None,
    );
    let unknown = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "unknown-root-after-B",
        6,
        None,
        None,
    );
    let fresh = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "fresh-test-in-B",
        VerificationKind::Test,
        VerificationResult::Passed,
        7,
        basis("workspace-B", "B7", Some(9)),
    );
    for pick in [
        Pick::Criterion,
        Pick::TriggeredBy(&known),
        Pick::TriggeredBy(&unknown),
    ] {
        assert_eq!(
            obligation(&store, &claim, &pick),
            (WorkObligationState::Satisfied, Some(fresh.clone()))
        );
    }
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-fresh-B",
        8,
        std::slice::from_ref(&fresh),
    );
    complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &fresh,
        "complete-fresh-B",
        9,
    )
    .expect("the fresh check accounts for both changes");
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

#[test]
fn a_check_in_the_named_root_seals_and_discloses_an_earlier_foreign_change() {
    let (mut store, work, claim, earlier) = named_work();
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-B",
        VerificationKind::Test,
        VerificationResult::Passed,
        5,
        basis("workspace-B", "B5", Some(9)),
    );
    let obligations = store
        .work_run_obligations(claim.run_id)
        .expect("obligations");
    assert_eq!(
        obligations
            .iter()
            .filter(|record| record.state == WorkObligationState::Satisfied)
            .count(),
        1
    );
    assert_eq!(
        obligations
            .iter()
            .filter(|record| record.state == WorkObligationState::Open)
            .count(),
        1
    );
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
    .expect("B check satisfies the bound criterion");
    assert_eq!(seal.foreign_workspace_changes, vec![earlier]);
    let records = store
        .work_run_obligations(claim.run_id)
        .expect("terminal obligations");
    assert!(records.iter().any(|record| matches!(
        record.resolution.as_ref().map(|event| &event.resolution),
        Some(WorkObligationResolution::Displaced { trigger_workspace_id, .. })
            if trigger_workspace_id == "workspace-A"
    )));
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

#[test]
fn later_named_root_and_foreign_changes_do_not_inherit_an_older_pass() {
    let (mut store, work, claim, _) = named_work();
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-B-before-change",
        VerificationKind::Test,
        VerificationResult::Passed,
        5,
        basis("workspace-B", "B5", Some(9)),
    );
    source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "foreign-after-name",
        6,
        Some(basis("workspace-A", "A6", Some(9))),
        None,
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-foreign",
        7,
        std::slice::from_ref(&checked),
    );
    assert!(matches!(
        complete(&mut store, &work, &claim, "runner", &checked, "complete-foreign", 8),
        Err(StoreError::WorkCompletionRefused { reason, .. }) if reason.contains("foreign workspace")
    ));
}

#[test]
fn a_named_root_change_after_the_check_needs_a_new_check() {
    let (mut store, work, claim, _) = named_work();
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-B-before-change",
        VerificationKind::Test,
        VerificationResult::Passed,
        5,
        basis("workspace-B", "B5", Some(9)),
    );
    source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "B-after-check",
        6,
        Some(basis("workspace-B", "B6", Some(9))),
        None,
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-new-B",
        7,
        std::slice::from_ref(&checked),
    );
    let refused = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &checked,
        "complete-stale-B",
        8,
    );
    assert!(
        matches!(
            &refused,
            Err(StoreError::WorkBoundVerificationRefused { reason, cause, .. })
                if reason.contains("latest source change")
                    && cause.verification == checked
                    && cause.mismatch == crate::domain::VerificationEvidenceMismatch::StaleSourceRevision
                    && cause.remedy == crate::domain::BoundVerificationRemedy::RunCurrentCheck
        ),
        "{refused:?}"
    );
}

#[test]
fn an_unknown_root_change_needs_a_fresh_named_root_check() {
    let (mut store, work, claim, _) = named_work();
    let old_check = host_verification_from_basis(
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
    source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "unknown-after-name",
        6,
        None,
        None,
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-unknown",
        7,
        std::slice::from_ref(&old_check),
    );
    let refused = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &old_check,
        "complete-unknown",
        8,
    );
    assert!(
        matches!(
            &refused,
            Err(StoreError::WorkCompletionRefused { reason, .. }) if reason.contains("unknown workspace")
        ),
        "{refused:?}"
    );

    let fresh_check = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-after-unknown",
        VerificationKind::Test,
        VerificationResult::Passed,
        9,
        basis("workspace-B", "B9", Some(9)),
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-fresh",
        10,
        &[old_check, fresh_check.clone()],
    );
    complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &fresh_check,
        "complete-fresh",
        11,
    )
    .expect("fresh named-root check accounts for unknown source change");
}

#[test]
fn a_foreign_check_cannot_satisfy_a_named_root_source_change() {
    let (mut store, work, claim, _) = named_work();
    let change_in_b = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "change-in-B",
        5,
        Some(basis("workspace-B", "same-revision", Some(9))),
        None,
    );
    let foreign_check = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-A",
        VerificationKind::Test,
        VerificationResult::Passed,
        6,
        basis("workspace-A", "same-revision", Some(9)),
    );
    // Each obligation the foreign check might wrongly satisfy stays open;
    // the fixture's earlier foreign change keeps its own obligation open too,
    // so a check over all of them would prove nothing.
    for pick in [Pick::Criterion, Pick::TriggeredBy(&change_in_b)] {
        assert_eq!(
            obligation(&store, &claim, &pick),
            (WorkObligationState::Open, None)
        );
    }
    let correct_check = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-B",
        VerificationKind::Test,
        VerificationResult::Passed,
        7,
        basis("workspace-B", "same-revision", Some(9)),
    );
    for pick in [Pick::Criterion, Pick::TriggeredBy(&change_in_b)] {
        assert_eq!(
            obligation(&store, &claim, &pick),
            (WorkObligationState::Satisfied, Some(correct_check.clone()))
        );
    }
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-foreign-check",
        8,
        &[foreign_check, correct_check.clone()],
    );
    complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &correct_check,
        "complete-with-B",
        9,
    )
    .expect("B check satisfies B source change");
}

/// A check accounts only for a change it ran after. A host that states a
/// released generation for a change in workspace D records an unbound
/// change; a check in the current root that ran before that change, though
/// its verification was recorded after it, does not satisfy it, and a later
/// check does.
#[test]
fn a_check_run_before_an_unbound_change_cannot_account_for_it() {
    let (mut store, work, claim, _) = named_work();
    release(&mut store, &work, &claim, "release-runner", 5);
    let work = store.get_work_item(work.work_id).expect("item");
    let claim = super::claim(&mut store, &work, "runner", "reclaim-runner", 6, 300);
    name_root(&mut store, &work, &claim, 10, 7);
    let early_basis = || basis("workspace-B", "B8", Some(10));
    let transaction = begin(&mut store);
    let producer = host_check_producer(
        &transaction,
        &work,
        &claim,
        "runner",
        "early-check",
        8,
        early_basis(),
    );
    transaction.commit().expect("commit the producer");
    let stale = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "stale-generation-in-D",
        9,
        Some(basis("workspace-D", "D9", Some(9))),
        None,
    );
    let transaction = begin(&mut store);
    host_verification_of_producer(
        &transaction,
        &work,
        &claim,
        "runner",
        "early-check",
        8,
        10,
        early_basis(),
        producer,
    );
    transaction.commit().expect("commit the late verification");
    assert_eq!(
        obligation(&store, &claim, &Pick::TriggeredBy(&stale)),
        (WorkObligationState::Open, None)
    );
    let late = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "late-check",
        VerificationKind::Test,
        VerificationResult::Passed,
        11,
        basis("workspace-B", "B8", Some(10)),
    );
    assert_eq!(
        obligation(&store, &claim, &Pick::TriggeredBy(&stale)),
        (WorkObligationState::Satisfied, Some(late))
    );
}
