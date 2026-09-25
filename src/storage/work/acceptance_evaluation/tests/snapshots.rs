//! The completion pre-check and the evaluation status compare the item, the
//! policy and the newest evaluation record. A revision and a fresh
//! evaluation committed partway through such a read must not make it report
//! a stale evaluation for a store that was consistent at every commit.

use super::*;
use crate::storage::concurrent_commit::read_across_a_concurrent_commit;

/// The statement that reads the run's newest evaluation record.
const NEWEST_EVALUATION: &[&str] =
    &["WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_kind = ?2"];

/// An evaluated item with one fresh passing evaluation at its current
/// revision, a writer that holds its claim, and a second connection.
struct Evaluated {
    _directory: crate::test_support::TempHome,
    writer: SqliteStore,
    reader: SqliteStore,
    work: WorkItem,
    claim: WorkClaim,
    gate: ObjectId,
}

fn evaluated(project: &str) -> Evaluated {
    let Fixture {
        mut store,
        directory,
        work,
        claim,
        ..
    } = fixture(project);
    enable(
        &mut store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-snapshot",
        5,
    );
    let gate = gate(&mut store, &work, &claim, "runner", "cargo-test", &[], 6);
    let pass = request(
        &work,
        cut(&store, &work),
        "runner",
        Mode::SameSession,
        vec![verdict(
            1,
            AcceptanceVerdict::Pass,
            AcceptanceBasis::Asserted,
            std::slice::from_ref(&gate),
        )],
        7,
    );
    record(&mut store, &pass).expect("first evaluation");
    let reader = SqliteStore::open(directory.path().join("engram.sqlite3")).expect("reader store");
    Evaluated {
        _directory: directory,
        writer: store,
        reader,
        work,
        claim,
        gate,
    }
}

/// The holder revises the item, then records a fresh passing evaluation at
/// the new revision.
fn revise_and_re_evaluate(
    mut writer: SqliteStore,
    work: WorkItem,
    claim: WorkClaim,
    gate: ObjectId,
) -> impl FnOnce() -> Result<(), String> {
    move || {
        let revised = revise(
            &mut writer,
            &work,
            &claim,
            WorkRevisionPatch {
                title: Some("revised partway through the read".into()),
                ..empty_patch()
            },
            "snapshot-revise",
            8,
        )
        .map_err(|error| error.to_string())?;
        let pass = request(
            &revised,
            cut(&writer, &revised),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Asserted,
                std::slice::from_ref(&gate),
            )],
            9,
        );
        record(&mut writer, &pass)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

#[test]
fn completion_readiness_reads_the_item_and_its_newest_evaluation_from_one_commit() {
    let Evaluated {
        _directory,
        writer,
        reader,
        work,
        claim,
        gate,
    } = evaluated("project-readiness-snapshot");
    let read = read_across_a_concurrent_commit(
        &reader,
        |reader| reader.acceptance_evaluation_readiness(work.work_id, claim.run_id, None),
        NEWEST_EVALUATION,
        revise_and_re_evaluate(writer, work.clone(), claim.clone(), gate),
    );
    match read.expect("one commit state") {
        AcceptanceEvaluationReadiness::Ready(evaluation) => {
            assert_eq!(evaluation.work_revision, work.revision);
        }
        other => panic!("the evaluation was fresh when the read began: {other:?}"),
    }
    match reader
        .acceptance_evaluation_readiness(work.work_id, claim.run_id, None)
        .expect("after")
    {
        AcceptanceEvaluationReadiness::Ready(evaluation) => {
            assert_eq!(evaluation.work_revision, work.revision + 1);
        }
        other => panic!("the fresh re-evaluation is ready: {other:?}"),
    }
}

#[test]
fn evaluation_status_reads_the_item_and_its_newest_evaluation_from_one_commit() {
    let Evaluated {
        _directory,
        writer,
        reader,
        work,
        claim,
        gate,
    } = evaluated("project-status-snapshot");
    let read = read_across_a_concurrent_commit(
        &reader,
        |reader| reader.acceptance_evaluation_status(work.work_id, None),
        NEWEST_EVALUATION,
        revise_and_re_evaluate(writer, work.clone(), claim, gate),
    );
    let status = read
        .expect("one commit state")
        .expect("an evaluation exists");
    assert_eq!(status.record.work_revision, work.revision);
    assert!(status.stale.is_none(), "{:?}", status.stale);
    let after = reader
        .acceptance_evaluation_status(work.work_id, None)
        .expect("after")
        .expect("an evaluation exists");
    assert_eq!(after.record.work_revision, work.revision + 1);
    assert!(after.stale.is_none(), "{:?}", after.stale);
}
