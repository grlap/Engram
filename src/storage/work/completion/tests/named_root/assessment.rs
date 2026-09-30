//! A verification record's reconstructed assessment: what the current
//! matching rules say it satisfies at its own run-feed position, beside each
//! obligation's recorded end.

use super::*;
use crate::control::{ObligationAssessment, ObligationSkip};
use crate::domain::{ExecutionOutcome, VerificationEvidenceMismatch as Mismatch};
use crate::storage::{
    AssessmentBoundary, RecordedObligationEnd as Recorded, VerificationAssessment,
};

/// One run of a check, as the host records it: a producer observation of
/// `suite` with `outcome`, and a verification of it with `result`.
struct Run<'a> {
    key: &'a str,
    suite: &'a str,
    outcome: ExecutionOutcome,
    result: VerificationResult,
}

/// Appends `run`'s producer and verification at `second` in `source_basis`
/// within the caller's checkpoint.
fn record_check(
    transaction: &rusqlite::Transaction<'_>,
    work: &WorkItem,
    claim: &WorkClaim,
    run: &Run<'_>,
    second: i64,
    source_basis: &ExecutionSourceBasis,
) -> ObjectId {
    use crate::domain::{
        ControlWorkBinding, EffectClass, ExecutionObservation, VerificationEvidence,
    };
    let binding = ControlWorkBinding {
        root_execution_id: load_work_run(transaction, claim.run_id)
            .expect("run")
            .root_execution_id,
        work_id: work.work_id,
        run_id: claim.run_id,
        work_revision: claim.accepted_work_revision,
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
    };
    let mut run_actor = actor("runner");
    run_actor.run_id = Some(claim.run_id.0.to_string());
    let producer = append_control_execution_observation_on(
        transaction,
        &ExecutionObservation {
            schema_version: SCHEMA_VERSION,
            project_id: work.project_id.clone(),
            binding: binding.clone(),
            session_id: SessionId("runner".into()),
            grant_id: format!("grant-{}", run.key),
            observation_id: format!("check-{}", run.key),
            action_fingerprint: check_fingerprint(run.suite),
            effect: EffectClass::Observe,
            outcome: run.outcome,
            source_changed: false,
            reported_source_change: None,
            obligation_rule_set: active_rule_set_id(transaction),
            source_basis: Some(source_basis.clone()),
            observed_at: Some(at(second)),
            actor: run_actor.clone(),
            recorded_at: at(second),
        },
    )
    .expect("producer");
    append_control_verification_evidence_on(
        transaction,
        &VerificationEvidence {
            schema_version: SCHEMA_VERSION,
            project_id: work.project_id.clone(),
            binding,
            session_id: SessionId("runner".into()),
            producer_observation: producer,
            source_basis: source_basis.clone(),
            environment: None,
            check_kind: VerificationKind::Test,
            check_fingerprint: check_fingerprint(run.suite),
            result: run.result,
            completed_at: at(second),
            summary: format!("host observed {}", run.key),
            refs: Vec::new(),
            actor: run_actor,
            recorded_at: at(second),
        },
    )
    .expect("verification")
}

fn passed<'a>(key: &'a str, suite: &'a str) -> Run<'a> {
    Run {
        key,
        suite,
        outcome: ExecutionOutcome::Succeeded,
        result: VerificationResult::Passed,
    }
}

/// Every candidate of `record` on one page.
fn assess(store: &SqliteStore, work: &WorkItem, record: &ObjectId) -> VerificationAssessment {
    page(store, work, record, None, usize::MAX)
}

fn page(
    store: &SqliteStore,
    work: &WorkItem,
    record: &ObjectId,
    after: Option<AssessmentBoundary>,
    limit: usize,
) -> VerificationAssessment {
    store
        .verification_assessment(work.work_id, record, after, limit)
        .expect("assessment read")
        .expect("a verification record of the item")
}

/// The assessment and recorded end of the obligation `pick` selects.
fn row_for(
    store: &SqliteStore,
    claim: &WorkClaim,
    view: &VerificationAssessment,
    pick: &Pick<'_>,
) -> (ObligationAssessment, Recorded) {
    let records = store
        .work_run_obligations(claim.run_id)
        .expect("obligations");
    let rows = view
        .rows
        .iter()
        .filter(|row| {
            let record = records
                .iter()
                .find(|record| record.obligation.obligation_id == row.obligation_id)
                .expect("a row is an obligation of the run");
            match pick {
                Pick::Criterion => row.criterion.is_some(),
                Pick::TriggeredBy(change) => {
                    row.criterion.is_none() && &record.obligation.triggering_observation == *change
                }
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 1, "exactly one row is selected");
    (rows[0].assessment, rows[0].recorded)
}

/// Every row whose obligation this record satisfied reads as a match, and the
/// satisfaction's evaluated cut is the position the read reconstructs at.
fn assert_recorded_satisfaction_matches(
    store: &SqliteStore,
    claim: &WorkClaim,
    record: &ObjectId,
    view: &VerificationAssessment,
) {
    for row in &view.rows {
        if row.recorded == Recorded::SatisfiedByThisRecord {
            assert_eq!(row.assessment, ObligationAssessment::Matches, "{row:?}");
        }
    }
    for obligation in store
        .work_run_obligations(claim.run_id)
        .expect("obligations")
    {
        if let Some(WorkObligationResolution::Satisfied {
            evidence,
            evaluated_cut,
        }) = obligation
            .resolution
            .as_ref()
            .map(|event| &event.resolution)
            && evidence == record
        {
            assert_eq!(evaluated_cut.position, view.cut_position);
            assert_eq!(view.cut_position, view.record_position + 1);
        }
    }
}

/// A failed and a passed record of one check in one checkpoint, in either
/// order, under a named root: the passed record satisfies what it matches and
/// the failed one reads as not passed or, after it, as already closed. Done
/// then completes when the passed record is the newer one, displacing the
/// change from before the root was named, and refuses the bound criterion
/// when the failed record is newer.
#[test]
fn a_failed_and_a_passed_record_of_one_check_read_by_their_own_positions() {
    for failed_first in [true, false] {
        let (mut store, work, claim, foreign) = named_work();
        let rooted = basis("workspace-B", "B5", Some(9));
        let change = source_mutation_from_basis(
            &mut store,
            &work,
            &claim,
            "runner",
            "b-change",
            5,
            Some(rooted.clone()),
            None,
        );
        let failing = Run {
            key: "failed-run",
            suite: "suite",
            outcome: ExecutionOutcome::Failed,
            result: VerificationResult::Failed,
        };
        let passing = passed("passed-run", "suite");
        let transaction = begin(&mut store);
        let (failed, pass) = if failed_first {
            let failed = record_check(&transaction, &work, &claim, &failing, 6, &rooted);
            let pass = record_check(&transaction, &work, &claim, &passing, 6, &rooted);
            (failed, pass)
        } else {
            let pass = record_check(&transaction, &work, &claim, &passing, 6, &rooted);
            let failed = record_check(&transaction, &work, &claim, &failing, 6, &rooted);
            (failed, pass)
        };
        transaction.commit().expect("commit the checkpoint");
        let context = format!("failed first: {failed_first}");

        let pass_view = assess(&store, &work, &pass);
        assert_recorded_satisfaction_matches(&store, &claim, &pass, &pass_view);
        for pick in [Pick::Criterion, Pick::TriggeredBy(&change)] {
            assert_eq!(
                row_for(&store, &claim, &pass_view, &pick),
                (
                    ObligationAssessment::Matches,
                    Recorded::SatisfiedByThisRecord
                ),
                "{context}"
            );
        }
        assert_eq!(
            row_for(&store, &claim, &pass_view, &Pick::TriggeredBy(&foreign)),
            (
                ObligationAssessment::Skipped(ObligationSkip::ForeignOrDisplaced),
                Recorded::Open
            ),
            "{context}"
        );

        let failed_view = assess(&store, &work, &failed);
        let failed_status = if failed_first {
            ObligationAssessment::Mismatch(Mismatch::ResultNotPassed)
        } else {
            ObligationAssessment::Skipped(ObligationSkip::AlreadyClosed)
        };
        for pick in [Pick::Criterion, Pick::TriggeredBy(&change)] {
            assert_eq!(
                row_for(&store, &claim, &failed_view, &pick),
                (failed_status, Recorded::SatisfiedByAnotherRecord),
                "{context}"
            );
        }

        checkpoint(
            &mut store,
            &work,
            &claim,
            "runner",
            "checkpoint",
            7,
            std::slice::from_ref(&pass),
        );
        let done = complete(&mut store, &work, &claim, "runner", &pass, "done", 8);
        if !failed_first {
            // The obligations stay satisfied, but the criterion's newest
            // record of its check failed, so done refuses the criterion.
            assert!(
                matches!(
                    &done,
                    Err(StoreError::WorkBoundVerificationRefused { reason, cause, .. })
                        if reason.contains("criterion 1")
                            && reason.contains("contradicted by newer verification evidence")
                            && cause.mismatch == Mismatch::ResultNotPassed
                            && cause.verification == failed
                ),
                "{context}: {done:?}"
            );
            assert!(
                store.verify_all().expect("doctor").is_healthy(),
                "{context}"
            );
            continue;
        }
        done.unwrap_or_else(|error| panic!("{context}: done completes: {error:?}"));
        let after_done = assess(&store, &work, &pass);
        assert_eq!(
            row_for(&store, &claim, &after_done, &Pick::TriggeredBy(&foreign)),
            (
                ObligationAssessment::Skipped(ObligationSkip::ForeignOrDisplaced),
                Recorded::Displaced
            ),
            "{context}: the recorded end is read as stored"
        );
        assert!(
            store.verify_all().expect("doctor").is_healthy(),
            "{context}"
        );
    }
}

/// A check whose producer outcome is unknown records an indeterminate result,
/// which matches no obligation: the matcher's first mismatch is the result.
#[test]
fn an_unknown_producer_outcome_reads_as_result_not_passed() {
    let (mut store, work, claim, _) = named_work();
    let rooted = basis("workspace-B", "B5", Some(9));
    let change = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "b-change",
        5,
        Some(rooted.clone()),
        None,
    );
    let transaction = begin(&mut store);
    let record = record_check(
        &transaction,
        &work,
        &claim,
        &Run {
            key: "unknown-run",
            suite: "suite",
            outcome: ExecutionOutcome::Unknown,
            result: VerificationResult::Indeterminate,
        },
        6,
        &rooted,
    );
    transaction.commit().expect("commit");
    let view = assess(&store, &work, &record);
    for pick in [Pick::Criterion, Pick::TriggeredBy(&change)] {
        assert_eq!(
            row_for(&store, &claim, &view, &pick),
            (
                ObligationAssessment::Mismatch(Mismatch::ResultNotPassed),
                Recorded::Open
            )
        );
    }
}

/// A rule that pins its check refuses a passing record of another suite, and
/// the row says the rule is pinned.
#[test]
fn a_pinned_rule_reads_a_fingerprint_mismatch() {
    let (mut store, work, claim, _) = named_work_in(pinned_store("pinned-suite"));
    let rooted = basis("workspace-B", "B5", Some(9));
    let change = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "b-change",
        5,
        Some(rooted.clone()),
        None,
    );
    let transaction = begin(&mut store);
    let record = record_check(
        &transaction,
        &work,
        &claim,
        &passed("other-run", "other-suite"),
        6,
        &rooted,
    );
    transaction.commit().expect("commit");
    let view = assess(&store, &work, &record);
    assert_eq!(
        row_for(&store, &claim, &view, &Pick::TriggeredBy(&change)),
        (
            ObligationAssessment::Mismatch(Mismatch::CheckFingerprintMismatch),
            Recorded::Open
        )
    );
    let pinned = view
        .rows
        .iter()
        .find(|row| {
            row.criterion.is_none()
                && row.assessment
                    != ObligationAssessment::Skipped(ObligationSkip::ForeignOrDisplaced)
        })
        .expect("the pinned rule's row");
    assert!(pinned.pinned);
    assert_eq!(pinned.rule.rule_id, "operator-pinned-suite");
    assert_eq!(
        row_for(&store, &claim, &view, &Pick::Criterion),
        (
            ObligationAssessment::Matches,
            Recorded::SatisfiedByThisRecord
        ),
        "the unpinned criterion takes any passing test"
    );
}

/// A record is read at its own position: after the host names a newer
/// generation of the root, an earlier record still reads as it matched then,
/// while a record of the old generation reads as stale against the change it
/// did not follow.
#[test]
fn a_moved_root_generation_reads_stale_and_earlier_records_keep_their_cut() {
    let (mut store, work, claim, _) = named_work();
    let generation_9 = basis("workspace-B", "B5", Some(9));
    source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "b-change",
        5,
        Some(generation_9.clone()),
        None,
    );
    let transaction = begin(&mut store);
    let early = record_check(
        &transaction,
        &work,
        &claim,
        &passed("early-run", "suite"),
        6,
        &generation_9,
    );
    transaction.commit().expect("commit");

    name_root(&mut store, &work, &claim, 10, 7);
    let later_change = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "b-change-10",
        8,
        Some(basis("workspace-B", "B7", Some(10))),
        None,
    );
    let transaction = begin(&mut store);
    let old_generation = record_check(
        &transaction,
        &work,
        &claim,
        &passed("old-generation-run", "suite"),
        9,
        &basis("workspace-B", "B7", Some(9)),
    );
    transaction.commit().expect("commit");

    let view = assess(&store, &work, &old_generation);
    assert_eq!(
        row_for(&store, &claim, &view, &Pick::TriggeredBy(&later_change)),
        (
            ObligationAssessment::Mismatch(Mismatch::StaleSourceRevision),
            Recorded::Open
        )
    );
    let early_view = assess(&store, &work, &early);
    assert_recorded_satisfaction_matches(&store, &claim, &early, &early_view);
    assert_eq!(
        row_for(
            &store,
            &claim,
            &early_view,
            &Pick::TriggeredBy(&later_change)
        ),
        (
            ObligationAssessment::Skipped(ObligationSkip::NotYetDefined),
            Recorded::Open
        ),
        "an obligation opened after the record was not defined for it"
    );
    assert_eq!(
        row_for(&store, &claim, &early_view, &Pick::Criterion),
        (
            ObligationAssessment::Matches,
            Recorded::SatisfiedByThisRecord
        ),
        "the earlier record still matches under the generation of its own position"
    );
}

/// Without a named root, a check of a revision the source has moved on from
/// reads as stale against the change it did not follow.
#[test]
fn a_moved_revision_reads_stale() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let work = store
        .create_work(
            &root_request("project-a", "moved-revision", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("work");
    let claim = claim(&mut store, &work, "runner", "moved-revision-claim", 2, 300);
    source_mutation(&mut store, &work, &claim, "runner", "first", 3, Some("R1"));
    let change = source_mutation(&mut store, &work, &claim, "runner", "second", 4, Some("R2"));
    let stale = host_verification_of(
        &mut store,
        &work,
        &claim,
        "runner",
        "stale-run",
        VerificationKind::Test,
        VerificationResult::Passed,
        5,
        "R1",
    );
    let view = assess(&store, &work, &stale);
    assert_eq!(
        row_for(&store, &claim, &view, &Pick::TriggeredBy(&change)),
        (
            ObligationAssessment::Mismatch(Mismatch::StaleSourceRevision),
            Recorded::Open
        )
    );
}

/// A foreign change recorded after an earlier record did not exist for it:
/// it reads as not yet defined, never as foreign to a root it never met.
#[test]
fn a_later_foreign_change_reads_as_not_yet_defined() {
    let (mut store, work, claim, _) = named_work();
    let rooted = basis("workspace-B", "B5", Some(9));
    source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "b-change",
        5,
        Some(rooted.clone()),
        None,
    );
    let transaction = begin(&mut store);
    let early = record_check(
        &transaction,
        &work,
        &claim,
        &passed("early-run", "suite"),
        6,
        &rooted,
    );
    transaction.commit().expect("commit");
    let later_foreign = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "foreign-after-name",
        7,
        Some(basis("workspace-A", "A7", None)),
        None,
    );
    let view = assess(&store, &work, &early);
    assert_eq!(
        row_for(&store, &claim, &view, &Pick::TriggeredBy(&later_foreign)),
        (
            ObligationAssessment::Skipped(ObligationSkip::NotYetDefined),
            Recorded::Open
        )
    );
}

/// A waived obligation reads as already closed for a later record, with its
/// recorded end as waived; the page loads only what it shows, counts every
/// candidate, and continues from its boundary.
#[test]
fn a_waived_obligation_and_a_paged_read() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let work = store
        .create_work(
            &root_request("project-a", "paged-assessment", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("work");
    let claim = claim(&mut store, &work, "runner", "paged-claim", 2, 300);
    let first = source_mutation(
        &mut store,
        &work,
        &claim,
        "runner",
        "change-0",
        3,
        Some("R0"),
    );
    let waived = store
        .work_run_obligations(claim.run_id)
        .expect("obligations")
        .into_iter()
        .find(|record| record.obligation.triggering_observation == first)
        .expect("the first change's obligation");
    store
        .waive_work_obligation(
            &WaiveWorkObligationRequest {
                obligation_id: waived.obligation.obligation_id,
                expected_definition: waived.definition_id.clone(),
                waived_by: "operator".into(),
                reason: "accepted untested".into(),
                actor: actor("operator"),
                idempotency_key: "waive-first-change".into(),
                waived_at: at(4),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("waive");
    for index in 1..5 {
        source_mutation(
            &mut store,
            &work,
            &claim,
            "runner",
            &format!("change-{index}"),
            4 + index,
            Some(&format!("R{index}")),
        );
    }
    let record = host_verification_of(
        &mut store,
        &work,
        &claim,
        "runner",
        "suite",
        VerificationKind::Test,
        VerificationResult::Passed,
        10,
        "R4",
    );
    let whole = assess(&store, &work, &record);
    assert_eq!(whole.total, 5);
    assert_eq!(
        row_for(&store, &claim, &whole, &Pick::TriggeredBy(&first)),
        (
            ObligationAssessment::Skipped(ObligationSkip::AlreadyClosed),
            Recorded::Waived
        )
    );

    let mut paged = Vec::new();
    let mut after = None;
    loop {
        let view = page(&store, &work, &record, after, 2);
        assert_eq!(view.total, 5);
        assert!(view.boundary_found);
        assert_eq!(view.earlier, paged.len());
        assert!(view.rows.len() <= 2, "a page loads only what it shows");
        let Some(last) = view.rows.last() else {
            break;
        };
        after = Some(AssessmentBoundary {
            trigger_position: last.trigger_position,
            obligation_id: last.obligation_id,
        });
        paged.extend(
            view.rows
                .iter()
                .map(|row| (row.obligation_id, row.assessment)),
        );
    }
    assert_eq!(
        paged,
        whole
            .rows
            .iter()
            .map(|row| (row.obligation_id, row.assessment))
            .collect::<Vec<_>>(),
        "the pages together are the whole list, in order"
    );
    let unknown = page(
        &store,
        &work,
        &record,
        Some(AssessmentBoundary {
            trigger_position: 1,
            obligation_id: crate::domain::WorkObligationId(uuid::Uuid::now_v7()),
        }),
        2,
    );
    assert!(!unknown.boundary_found);
}
