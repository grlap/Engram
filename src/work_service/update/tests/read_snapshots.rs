//! Views the service assembles from several store reads: its guidance, its
//! protocol basis, the focus view and the receipts built after a commit.
//! Each runs across a commit another connection makes partway through, and
//! everything it returns comes from before that commit.

use super::*;
use crate::storage::WorkNoteCapture;
use crate::storage::concurrent_commit::read_across_a_concurrent_commit;
use crate::test_support::TempHome;

/// A root claimed by this service's session, with its database path.
fn claimed_root(project: &str) -> (TempHome, LocalWorkService, std::path::PathBuf, WorkItem) {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let service = LocalWorkService::new(
        database.clone(),
        ProjectId(project.into()),
        "agent".into(),
        SessionId("holder".into()),
        Some("protocol-test".into()),
    );
    let work = match service
        .work_propose(root_input("Snapshot reads", "snapshot-root"), at(0))
        .expect("root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) | WorkProposeResult::Plan(_) => panic!("expected root"),
    };
    service
        .select_work(&work.short_ref, at(1))
        .expect("select the root");
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(3_600),
                recovery_reason: None,
                idempotency_key: "snapshot-claim".into(),
            },
            at(1),
        )
        .expect("claim");
    let work = SqliteStore::open(&database)
        .expect("store")
        .get_work_item(work.work_id)
        .expect("claimed root");
    (directory, service, database, work)
}

/// The holder offers the item to another session, which adds an offer.
fn offer_by_holder(
    database: &std::path::Path,
    work: &WorkItem,
) -> impl FnOnce() -> Result<(), String> + use<> {
    let mut writer = SqliteStore::open(database).expect("writer store");
    let claim = writer
        .current_work_claim(work.work_id)
        .expect("claim read")
        .expect("claimed");
    let work = work.clone();
    move || {
        writer
            .offer_work_handoff(
                &crate::OfferWorkHandoffRequest {
                    work_id: work.work_id,
                    run_id: claim.run_id,
                    expected_work_revision: work.revision,
                    from: claim.holder.clone(),
                    to: SessionId("recipient".into()),
                    claim_id: claim.claim_id,
                    claim_fence: claim.fence,
                    ttl_seconds: 600,
                    checkpoint_summary: "offered partway through the read".into(),
                    actor: crate::ActorContext {
                        actor_id: "agent".into(),
                        actor_kind: "test_agent".into(),
                        assurance: crate::domain::AssuranceLevel::Asserted,
                        run_id: None,
                        session_id: Some(claim.holder.clone()),
                        source_tool: Some("work_test".into()),
                        source_skill: None,
                        provenance_chain: Vec::new(),
                        reason: "offer partway through a read".into(),
                    },
                    idempotency_key: "snapshot-offer".into(),
                    offered_at: at(50),
                },
                &DevelopmentNoopRedactor,
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

/// A host-observed source change on the claimed run, which opens an
/// obligation.
fn source_change(
    database: &std::path::Path,
    work: &WorkItem,
) -> impl FnOnce() -> Result<(), String> + use<> {
    let mut writer = SqliteStore::open(database).expect("writer store");
    let work_id = work.work_id;
    move || {
        writer.append_source_change_fixture(work_id, "snapshot", at(50), "rev-2");
        Ok(())
    }
}

#[test]
fn guidance_reads_the_item_claim_and_offers_from_one_commit() {
    let (_directory, service, database, work) = claimed_root("snapshot-guidance");
    let reader = SqliteStore::open(&database).expect("reader store");
    let guidance = read_across_a_concurrent_commit(
        &reader,
        |reader| service.work_guidance(reader, work.work_id, at(40)),
        &["FROM work_handoff_offers"],
        offer_by_holder(&database, &work),
    )
    .expect("one commit state");
    assert_eq!(guidance.status.work, work);
    assert!(
        guidance.handoffs.is_empty(),
        "the offer committed after the read began"
    );
    let after = service
        .work_guidance(&reader, work.work_id, at(60))
        .expect("after");
    assert_eq!(after.handoffs.len(), 1);
}

#[test]
fn the_protocol_basis_reads_the_item_claim_and_offers_from_one_commit() {
    let (_directory, service, database, work) = claimed_root("snapshot-basis");
    let reader = SqliteStore::open(&database).expect("reader store");
    let basis = read_across_a_concurrent_commit(
        &reader,
        |reader| service.protocol_basis(reader, true, true, Some(work.work_id), at(40)),
        &["FROM work_handoff_offers"],
        offer_by_holder(&database, &work),
    )
    .expect("one commit state");
    assert_eq!(basis.focused_work.as_ref(), Some(&work));
    assert!(
        basis.handoffs.is_empty(),
        "the offer committed after the read began"
    );
    let after = service
        .protocol_basis(&reader, true, true, Some(work.work_id), at(60))
        .expect("after");
    assert_eq!(after.handoffs.len(), 1);
}

#[test]
fn the_focus_view_reads_its_status_and_obligations_from_one_commit() {
    let (_directory, service, database, work) = claimed_root("snapshot-focus");
    let reader = SqliteStore::open(&database).expect("reader store");
    let view = read_across_a_concurrent_commit(
        &reader,
        |reader| service.focus_view(reader, work.work_id, false, true, at(40)),
        &["FROM work_run_obligations"],
        source_change(&database, &work),
    )
    .expect("one commit state");
    assert!(
        view.obligation_page.items.is_empty(),
        "the source change committed after the read began"
    );
    let after = service
        .focus_view(&reader, work.work_id, false, true, at(60))
        .expect("after");
    assert_eq!(after.obligation_page.items.len(), 1);
}

#[test]
fn an_update_receipt_reads_its_guidance_and_obligations_from_one_commit() {
    let (_directory, service, database, work) = claimed_root("snapshot-update-receipt");
    let reader = SqliteStore::open(&database).expect("reader store");
    let result = read_across_a_concurrent_commit(
        &reader,
        |reader| {
            service.work_update_result(
                reader,
                "revise",
                work.work_id,
                serde_json::json!({ "state": "revised" }),
                at(40),
            )
        },
        &["FROM work_run_obligations"],
        source_change(&database, &work),
    )
    .expect("one commit state");
    assert!(
        result.obligation_page.items.is_empty(),
        "the source change committed after the read began"
    );
}

#[test]
fn a_note_receipt_reads_its_guidance_and_obligations_from_one_commit() {
    let (_directory, service, database, work) = claimed_root("snapshot-note-receipt");
    let reader = SqliteStore::open(&database).expect("reader store");
    let capture = WorkNoteCapture {
        non_holder: false,
        evidence: ObjectId::from_canonical_bytes(b"snapshot note evidence"),
        checkpoint: None,
    };
    let result = read_across_a_concurrent_commit(
        &reader,
        |reader| service.work_note_result(reader, work.work_id, &capture, at(40)),
        &["FROM work_run_obligations"],
        source_change(&database, &work),
    )
    .expect("one commit state");
    assert!(
        result.obligation_page.items.is_empty(),
        "the source change committed after the read began"
    );
}

/// The root this service's session claimed, completed by `done`.
fn completed_root(project: &str) -> (TempHome, LocalWorkService, std::path::PathBuf, WorkItem) {
    let (directory, service, database, claimed) = claimed_root(project);
    service
        .work_complete(completion_input("delivered", "done"), at(2))
        .expect("complete the root");
    let work = SqliteStore::open(&database)
        .expect("store")
        .get_work_item(claimed.work_id)
        .expect("completed root");
    assert_eq!(work.lifecycle, WorkLifecycle::Completed);
    (directory, service, database, work)
}

/// Another connection reopens the completed item, which adds a fresh,
/// unsealed run.
fn reopen_by_another_connection(
    database: &std::path::Path,
    work: &WorkItem,
) -> impl FnOnce() -> Result<(), String> + use<> {
    let mut writer = SqliteStore::open(database).expect("writer store");
    let work = work.clone();
    move || {
        writer
            .reopen_work(
                &crate::ReopenWorkRequest {
                    work_id: work.work_id,
                    expected_work_revision: work.revision,
                    reason: "reopened partway through the read".into(),
                    actor: crate::ActorContext {
                        actor_id: "agent".into(),
                        actor_kind: "test_agent".into(),
                        assurance: crate::domain::AssuranceLevel::Asserted,
                        run_id: None,
                        session_id: Some(SessionId("holder".into())),
                        source_tool: Some("work_test".into()),
                        source_skill: None,
                        provenance_chain: Vec::new(),
                        reason: "reopen partway through a read".into(),
                    },
                    idempotency_key: "snapshot-reopen".into(),
                    reopened_at: at(50),
                },
                &DevelopmentNoopRedactor,
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

// A late note or gate on completed work reads the item, then its newest run
// and that run's seal. A reopen committed between those reads adds a fresh,
// unsealed run; read separately, the run would look like completed work
// without a seal, a broken store. On one snapshot the reads agree, and a
// reopen that landed before them is a revision conflict.
#[test]
fn late_evidence_reads_the_item_its_run_and_seal_from_one_commit() {
    let (_directory, _service, database, work) = completed_root("snapshot-late-evidence");
    let reader = SqliteStore::open(&database).expect("reader store");
    let (run, seal) = read_across_a_concurrent_commit(
        &reader,
        |reader| LocalWorkService::completed_evidence_basis(reader, &work),
        &[
            "FROM work_runs WHERE work_id",
            "ORDER BY generation DESC LIMIT 1",
        ],
        reopen_by_another_connection(&database, &work),
    )
    .expect("the completed basis as of one commit");
    assert_eq!(run.state, WorkRunState::Completed);
    assert_eq!(seal.run_id, run.run_id);
    assert_eq!(seal.work_id, work.work_id);

    assert!(matches!(
        LocalWorkService::completed_evidence_basis(&reader, &work),
        Err(StoreError::WorkRevisionConflict { work: conflicted, expected, .. })
            if conflicted == work.work_id && expected == work.revision
    ));
}
