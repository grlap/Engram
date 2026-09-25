//! Reads that compare rows from several statements, each run in autocommit
//! across a commit another connection makes between two of its statements.
//! Each read reports the state from before the commit instead of calling the
//! mix of two commits corruption.

use super::*;
use crate::domain::WorkBlockerKind;
use crate::storage::concurrent_commit::FEED_SNAPSHOT_HEAD;
use crate::storage::work::planning::validated_current_work_relation_basis;
use crate::test_support::TempHome;
use crate::{AcceptWorkHandoffRequest, CancelWorkHandoffRequest};

/// A claimed root item, a writer that holds the claim, and a second
/// connection to read with.
struct Claimed {
    _directory: TempHome,
    writer: SqliteStore,
    reader: SqliteStore,
    work: WorkItem,
    held: WorkClaim,
}

fn claimed(project: &str) -> Claimed {
    let directory = crate::test_support::temp_home().expect("directory");
    let path = directory.path().join("engram.sqlite3");
    let mut writer = SqliteStore::open(&path).expect("writer store");
    let work = writer
        .create_work(&root_request(project, "root", 0), &DevelopmentNoopRedactor)
        .expect("root");
    let held = claim(&mut writer, &work, "holder", "claim", 1, 3_600);
    let work = writer.get_work_item(work.work_id).expect("claimed item");
    let reader = SqliteStore::open(&path).expect("reader store");
    Claimed {
        _directory: directory,
        writer,
        reader,
        work,
        held,
    }
}

/// An unclaimed root item, which project planning may block, a writer and a
/// second connection to read with.
fn unclaimed(project: &str) -> (TempHome, SqliteStore, SqliteStore, WorkItem) {
    let directory = crate::test_support::temp_home().expect("directory");
    let path = directory.path().join("engram.sqlite3");
    let mut writer = SqliteStore::open(&path).expect("writer store");
    let work = writer
        .create_work(&root_request(project, "root", 0), &DevelopmentNoopRedactor)
        .expect("root");
    let reader = SqliteStore::open(&path).expect("reader store");
    (directory, writer, reader, work)
}

/// Project planning blocks the item, which adds a blocker row and an event
/// with a new relation fingerprint.
fn block_by_planner(
    mut writer: SqliteStore,
    work: WorkItem,
) -> impl FnOnce() -> Result<(), String> {
    move || {
        writer
            .add_work_blocker(
                &AddWorkBlockerRequest {
                    work_id: work.work_id,
                    expected_work_revision: work.revision,
                    kind: WorkBlockerKind::Manual,
                    detail: "blocked between the reader's statements".into(),
                    authority: delegated(&work.project_id.0, "planner"),
                    actor: actor("planner"),
                    idempotency_key: "concurrent-block".into(),
                    blocked_at: at(50),
                },
                &DevelopmentNoopRedactor,
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

/// The holder checkpoints, which changes the run and the root execution
/// and appends their events.
fn checkpoint_by_holder(
    mut writer: SqliteStore,
    work: &WorkItem,
    held: WorkClaim,
) -> impl FnOnce() -> Result<(), String> + use<> {
    let (work_id, expected_work_revision) = (work.work_id, work.revision);
    move || {
        writer
            .checkpoint_work(
                &CheckpointWorkRequest {
                    work_id,
                    run_id: held.run_id,
                    expected_work_revision,
                    holder: held.holder.clone(),
                    claim_id: held.claim_id,
                    claim_fence: held.fence,
                    summary: "checkpointed between the reader's statements".into(),
                    evidence: Some(Vec::new()),
                    actor: actor("holder"),
                    idempotency_key: "concurrent-checkpoint".into(),
                    checkpointed_at: at(50),
                },
                &DevelopmentNoopRedactor,
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

#[test]
fn a_run_read_compares_its_row_and_run_feed_event_from_one_commit() {
    let Claimed {
        _directory,
        writer,
        reader,
        work,
        held,
    } = claimed("race-run");
    let before = load_work_run(&reader.connection, held.run_id).expect("run before");
    let read = read_across_a_concurrent_commit(
        &reader,
        |reader| load_work_run(&reader.connection, held.run_id),
        FEED_SNAPSHOT_HEAD,
        checkpoint_by_holder(writer, &work, held.clone()),
    );
    assert_eq!(read.expect("one commit state, no false corruption"), before);
    let after = load_work_run(&reader.connection, held.run_id).expect("run after");
    assert_ne!(after, before, "the checkpoint changed the run");
}

#[test]
fn a_claim_read_compares_its_row_and_run_feed_event_from_one_commit() {
    let Claimed {
        _directory,
        mut writer,
        reader,
        work,
        held,
    } = claimed("race-claim");
    let before = load_work_claim_optional(&reader.connection, held.run_id).expect("claim before");
    let renew_for = work.clone();
    let read = read_across_a_concurrent_commit(
        &reader,
        |reader| load_work_claim_optional(&reader.connection, held.run_id),
        FEED_SNAPSHOT_HEAD,
        move || {
            writer
                .claim_work(
                    &ClaimWorkRequest {
                        work_id: renew_for.work_id,
                        expected_work_revision: renew_for.revision,
                        expected_run_id: renew_for.active_run_id,
                        holder: SessionId("holder".into()),
                        ttl_seconds: 7_200,
                        recovery_reason: None,
                        actor: actor("holder"),
                        idempotency_key: "concurrent-renewal".into(),
                        claimed_at: at(50),
                    },
                    &DevelopmentNoopRedactor,
                )
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
    );
    assert_eq!(read.expect("one commit state, no false corruption"), before);
    let after = load_work_claim_optional(&reader.connection, held.run_id).expect("claim after");
    assert_ne!(after, before, "the renewal changed the claim");
}

#[test]
fn an_active_root_execution_read_compares_its_state_and_root_feed_event_from_one_commit() {
    let Claimed {
        _directory,
        writer,
        reader,
        work,
        held,
    } = claimed("race-active-root");
    let before = active_root_execution_optional(&reader.connection, work.root_id)
        .expect("root before")
        .expect("an active execution");
    let root_id = work.root_id;
    let read = read_across_a_concurrent_commit(
        &reader,
        |reader| active_root_execution_optional(&reader.connection, root_id),
        FEED_SNAPSHOT_HEAD,
        checkpoint_by_holder(writer, &work, held),
    );
    assert_eq!(
        read.expect("one commit state, no false corruption"),
        Some(before.clone())
    );
    let after = active_root_execution_optional(&reader.connection, root_id)
        .expect("root after")
        .expect("an active execution");
    assert_ne!(after, before, "the checkpoint changed the root execution");
}

#[test]
fn a_root_execution_read_by_id_compares_its_state_and_root_feed_event_from_one_commit() {
    let Claimed {
        _directory,
        writer,
        reader,
        work,
        held,
    } = claimed("race-root-by-id");
    let run = load_work_run(&reader.connection, held.run_id).expect("run");
    let before =
        load_root_execution_with_ref(&reader.connection, run.root_execution_id).expect("before");
    let read = read_across_a_concurrent_commit(
        &reader,
        |reader| load_root_execution_with_ref(&reader.connection, run.root_execution_id),
        FEED_SNAPSHOT_HEAD,
        checkpoint_by_holder(writer, &work, held),
    );
    assert_eq!(read.expect("one commit state, no false corruption"), before);
    let after =
        load_root_execution_with_ref(&reader.connection, run.root_execution_id).expect("after");
    assert_ne!(after, before, "the checkpoint changed the root execution");
}

#[test]
fn a_retained_root_execution_read_compares_its_state_and_generation_event_from_one_commit() {
    let Claimed {
        _directory,
        writer,
        reader,
        work,
        held,
    } = claimed("race-retained-root");
    let run = load_work_run(&reader.connection, held.run_id).expect("run");
    let before =
        load_retained_root_execution(&reader.connection, run.root_execution_id).expect("before");
    let read = read_across_a_concurrent_commit(
        &reader,
        |reader| load_retained_root_execution(&reader.connection, run.root_execution_id),
        &["$.root_execution.generation"],
        checkpoint_by_holder(writer, &work, held),
    );
    assert_eq!(read.expect("one commit state, no false corruption"), before);
    let after =
        load_retained_root_execution(&reader.connection, run.root_execution_id).expect("after");
    assert_ne!(after, before, "the checkpoint changed the root execution");
}

#[test]
fn a_handoff_offer_read_compares_each_offer_and_its_event_from_one_commit() {
    let Claimed {
        _directory,
        mut writer,
        reader,
        work,
        held,
    } = claimed("race-handoff");
    let offer = writer
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: work.work_id,
                run_id: held.run_id,
                expected_work_revision: work.revision,
                from: held.holder.clone(),
                to: SessionId("recipient".into()),
                claim_id: held.claim_id,
                claim_fence: held.fence,
                ttl_seconds: 600,
                checkpoint_summary: "offered before the read".into(),
                actor: actor("holder"),
                idempotency_key: "offer".into(),
                offered_at: at(10),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("offer");
    let offered = writer.get_work_item(work.work_id).expect("offered item");
    let before = reader
        .work_handoff_offers(work.work_id)
        .expect("offers before");
    let read = read_across_a_concurrent_commit(
        &reader,
        |reader| reader.work_handoff_offers(work.work_id),
        &["$.handoff_offer.offer_id"],
        move || {
            writer
                .cancel_work_handoff(
                    &CancelWorkHandoffRequest {
                        work_id: offered.work_id,
                        run_id: held.run_id,
                        expected_work_revision: offered.revision,
                        holder: held.holder.clone(),
                        offer_id: offer.offer_id,
                        claim_id: held.claim_id,
                        claim_fence: held.fence,
                        reason: "cancelled between the reader's statements".into(),
                        actor: actor("holder"),
                        idempotency_key: "cancel".into(),
                        cancelled_at: at(50),
                    },
                    &DevelopmentNoopRedactor,
                )
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
    );
    assert_eq!(read.expect("one commit state, no false corruption"), before);
    let after = reader
        .work_handoff_offers(work.work_id)
        .expect("offers after");
    assert_ne!(after, before, "the cancellation changed the offer");
}

#[test]
fn an_obligation_read_compares_its_rows_and_source_changes_from_one_commit() {
    let Claimed {
        _directory,
        mut writer,
        reader,
        work,
        held,
    } = claimed("race-obligations");
    let before = reader
        .work_run_obligations(held.run_id)
        .expect("obligations before");
    let read = read_across_a_concurrent_commit(
        &reader,
        |reader| reader.work_run_obligations(held.run_id),
        &[
            "entry.object_kind = 'execution_observation'",
            "$.source_changed",
        ],
        move || {
            writer.append_source_change_fixture(work.work_id, "concurrent", at(50), "rev-2");
            Ok(())
        },
    );
    assert_eq!(
        read.expect("one commit state, no false corruption").len(),
        before.len()
    );
    let after = reader
        .work_run_obligations(held.run_id)
        .expect("obligations after");
    assert!(
        after.len() > before.len(),
        "the source change opened an obligation"
    );
}

#[test]
fn a_relation_basis_read_compares_its_relations_and_event_fingerprint_from_one_commit() {
    let (_directory, writer, reader, work) = unclaimed("race-relations");
    let before =
        validated_current_work_relation_basis(&reader.connection, work.work_id).expect("before");
    let read = read_across_a_concurrent_commit(
        &reader,
        |reader| validated_current_work_relation_basis(&reader.connection, work.work_id),
        ITEM_FEED_HEAD,
        block_by_planner(writer, work.clone()),
    );
    let basis = |value| serde_json::to_value(value).expect("relation basis");
    assert_eq!(
        basis(read.expect("one commit state, no false corruption")),
        basis(before.clone())
    );
    let after =
        validated_current_work_relation_basis(&reader.connection, work.work_id).expect("after");
    assert_ne!(
        basis(after),
        basis(before),
        "the blocker changed the relations"
    );
}

/// The item status reads the item, its relations and their basis. Across
/// a blocker added mid-read it reports the item and its blockers as they
/// were together before the blocker.
#[test]
fn an_item_status_reads_the_item_and_its_blockers_from_one_commit() {
    let (_directory, writer, reader, work) = unclaimed("race-status");
    let read = read_across_a_concurrent_commit(
        &reader,
        |reader| reader.inspect_work(work.work_id, at(40)),
        &["FROM work_blockers"],
        block_by_planner(writer, work.clone()),
    );
    let status = read.expect("one commit state, no false corruption");
    assert_eq!(status.work, work);
    assert!(
        status.blockers.is_empty(),
        "the blocker committed after the read began"
    );
    let after = reader.inspect_work(work.work_id, at(60)).expect("after");
    assert_eq!(after.work.revision, work.revision + 1);
    assert_eq!(after.blockers.len(), 1);
}

/// The claims a session holds are selected by holder and then reloaded. A
/// handoff accepted between the two must not list the recipient's claim as
/// still held by the offering session.
#[test]
fn held_claims_are_selected_and_loaded_from_one_commit() {
    let Claimed {
        _directory,
        mut writer,
        reader,
        work,
        held,
    } = claimed("race-held");
    let offer = writer
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: work.work_id,
                run_id: held.run_id,
                expected_work_revision: work.revision,
                from: held.holder.clone(),
                to: SessionId("recipient".into()),
                claim_id: held.claim_id,
                claim_fence: held.fence,
                ttl_seconds: 600,
                checkpoint_summary: "offered before the read".into(),
                actor: actor("holder"),
                idempotency_key: "held-offer".into(),
                offered_at: at(10),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("offer");
    let project = work.project_id.clone();
    let read = read_across_a_concurrent_commit(
        &reader,
        |reader| reader.work_claims_held_in_project(&project, &held.holder, at(40)),
        &["FROM work_claims WHERE run_id = ?1"],
        move || {
            writer
                .accept_work_handoff(
                    &AcceptWorkHandoffRequest {
                        work_id: offer.work_id,
                        offer_id: offer.offer_id,
                        to: SessionId("recipient".into()),
                        actor: actor("recipient"),
                        idempotency_key: "held-accept".into(),
                        accepted_at: at(50),
                    },
                    &DevelopmentNoopRedactor,
                )
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
    );
    let listed = read.expect("one commit state, no false inconsistency");
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].0.holder, held.holder,
        "the claim listed is the holder's, from before the acceptance"
    );
    let after = reader
        .work_claims_held_in_project(&project, &held.holder, at(60))
        .expect("after");
    assert!(after.is_empty(), "the acceptance moved the claim away");
}

/// Completion readiness compares the run's last checkpoint with the run
/// feed's head. Another checkpoint of the same evidence committed between
/// the run read and the head read must not make a ready item look behind.
#[test]
fn completion_readiness_reads_the_run_and_its_feed_head_from_one_commit() {
    let Claimed {
        _directory,
        mut writer,
        reader,
        work,
        held,
    } = claimed("race-readiness");
    let evidence_id = evidence(&mut writer, &work, &held, "holder", "readiness-evidence", 5);
    checkpoint(
        &mut writer,
        &work,
        &held,
        "holder",
        "readiness-checkpoint",
        6,
        std::slice::from_ref(&evidence_id),
    );
    let holder = held.holder.clone();
    assert_eq!(
        reader
            .work_completion_readiness(work.work_id, &holder, at(40))
            .expect("before"),
        (true, true)
    );
    let (work_id, revision) = (work.work_id, work.revision);
    let read = read_across_a_concurrent_commit(
        &reader,
        |reader| reader.work_completion_readiness(work_id, &holder, at(40)),
        &["SELECT position FROM work_feed_heads WHERE feed_kind = ?1 AND feed_id = ?2"],
        move || {
            writer
                .checkpoint_work(
                    &CheckpointWorkRequest {
                        work_id,
                        run_id: held.run_id,
                        expected_work_revision: revision,
                        holder: held.holder.clone(),
                        claim_id: held.claim_id,
                        claim_fence: held.fence,
                        summary: "checkpointed again partway through the read".into(),
                        evidence: Some(vec![evidence_id]),
                        actor: actor("holder"),
                        idempotency_key: "readiness-checkpoint-again".into(),
                        checkpointed_at: at(50),
                    },
                    &DevelopmentNoopRedactor,
                )
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
    );
    assert_eq!(read.expect("one commit state"), (true, true));
    assert_eq!(
        reader
            .work_completion_readiness(work_id, &holder, at(60))
            .expect("after"),
        (true, true)
    );
}

/// The obligations open at a run-feed cut are checked against the run's
/// source-change observations. A source change committed partway through
/// must not make that check fail for a cut taken before it.
#[test]
fn obligations_at_a_cut_are_read_against_their_observations_from_one_commit() {
    let Claimed {
        _directory,
        mut writer,
        reader,
        work,
        held,
    } = claimed("race-obligations-at-cut");
    let feed = FeedId::RunExecution(held.run_id);
    let cut = FeedPosition {
        position: reader.work_feed_head(&feed).expect("run head"),
        feed,
    };
    let read = read_across_a_concurrent_commit(
        &reader,
        |reader| reader.open_work_obligations_at_cut(held.run_id, &cut),
        &[
            "entry.object_kind = 'execution_observation'",
            "$.source_changed",
        ],
        move || {
            writer.append_source_change_fixture(work.work_id, "cut", at(50), "rev-2");
            Ok(())
        },
    );
    assert!(
        read.expect("one commit state, no false inconsistency")
            .is_empty(),
        "no obligation was open at the cut"
    );
    assert!(
        reader
            .open_work_obligations_at_cut(held.run_id, &cut)
            .expect("after")
            .is_empty(),
        "the later source change lies past the cut"
    );
}

/// Whether a session needs a recovery waiver compares the claim's holder
/// with the root execution's accounting. A third session that recovers an
/// expired claim waives the old holder and takes the claim in one commit.
/// Before and after that commit the answer is yes, for the old holder and
/// then for the recoverer, neither of them accounted. A read that paired the
/// old holder's claim with the accounting after the waiver would answer no.
#[test]
fn claim_recovery_reads_the_claim_and_its_accounting_from_one_commit() {
    let Claimed {
        _directory,
        mut writer,
        reader,
        work,
        held,
    } = claimed("race-recovery");
    let querier = SessionId("querier".into());
    assert!(
        reader
            .work_claim_recovery_required(work.work_id, &querier)
            .expect("before"),
        "the holder is not accounted"
    );
    let recover = work.clone();
    let read = read_across_a_concurrent_commit(
        &reader,
        |reader| reader.work_claim_recovery_required(work.work_id, &querier),
        &["FROM work_root_executions WHERE root_execution_id = ?1"],
        move || {
            writer
                .claim_work(
                    &ClaimWorkRequest {
                        work_id: recover.work_id,
                        expected_work_revision: recover.revision,
                        expected_run_id: Some(held.run_id),
                        holder: SessionId("recoverer".into()),
                        ttl_seconds: 600,
                        recovery_reason: Some("the holder's claim expired".into()),
                        actor: actor("recoverer"),
                        idempotency_key: "recovery-claim".into(),
                        claimed_at: at(4_000),
                    },
                    &DevelopmentNoopRedactor,
                )
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
    );
    assert!(
        read.expect("one commit state"),
        "the holder's claim with the accounting from before the recovery"
    );
    let after = reader
        .current_work_claim_for_item(&reader.get_work_item(work.work_id).expect("item"))
        .expect("claim after")
        .expect("a claim");
    assert_eq!(after.holder, SessionId("recoverer".into()));
    assert!(
        reader
            .work_claim_recovery_required(work.work_id, &querier)
            .expect("after"),
        "the recoverer is not accounted either"
    );
}
