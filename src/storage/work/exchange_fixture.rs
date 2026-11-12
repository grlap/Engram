//! Test-only access for exchange integration tests outside storage. It builds
//! a real native history and reads the storage-private evidence partition.

use super::test_support::{actor, at, claim, root_request};
use crate::storage::{NativeEvidenceCaptureRequest, NativeEvidenceFeed, SqliteStore};
use crate::task_sync::origin::EnrollRequest;
use crate::{DevelopmentNoopRedactor, FeedId, ObjectId, ProjectId};

/// One claimed work item with recorded evidence, an enrolled producing origin
/// and a capture request over its exact run feed.
pub(crate) struct NativeSender {
    pub(crate) request: NativeEvidenceCaptureRequest,
    pub(crate) original: ObjectId,
}

/// `profile` is the exchange binding's explicit disclosure selection.
pub(crate) fn native_sender(
    store: &mut SqliteStore,
    project: &ProjectId,
    profile: &crate::task_sync::progress::ExportProfile,
) -> NativeSender {
    let item = store
        .create_work(
            &root_request(&project.0, "native-evidence-sender", 1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let held = claim(store, &item, "runner", "sender-claim", 2, 600);
    let item = store.get_work_item(item.work_id).unwrap();
    // Disclosable prose: no refs, as the disclosure guard requires.
    let original = store
        .record_work_evidence(
            &crate::RecordWorkEvidenceRequest {
                work_id: item.work_id,
                run_id: held.run_id,
                expected_work_revision: item.revision,
                holder: crate::SessionId("runner".into()),
                claim_id: held.claim_id,
                claim_fence: held.fence,
                summary: "Shared native finding".into(),
                refs: Vec::new(),
                actor: actor("runner"),
                idempotency_key: "sender-check".into(),
                recorded_at: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let origin = store
        .enroll_task_sync_origin(
            project,
            &actor("collector"),
            &EnrollRequest {
                operation: uuid::Uuid::new_v4(),
                predecessor: None,
            },
            at(4),
        )
        .unwrap();
    let through = super::feeds::current_run_feed_cut_on(&store.connection, held.run_id)
        .unwrap()
        .position;
    NativeSender {
        request: NativeEvidenceCaptureRequest {
            capture: ObjectId::mint(),
            origin: origin.origin_id,
            profile: profile.clone(),
            roots: vec![original.clone()],
            feeds: vec![NativeEvidenceFeed {
                feed: FeedId::RunExecution(held.run_id),
                through,
            }],
            maximum_pack_records: 2,
        },
        original,
    }
}

/// Capture the sender's request as its enrolling collector.
pub(crate) fn capture(
    store: &mut SqliteStore,
    project: &ProjectId,
    request: &NativeEvidenceCaptureRequest,
) -> crate::storage::NativeEvidenceCapture {
    store
        .capture_native_evidence(
            project,
            &crate::SessionId("collector".into()),
            &actor("collector"),
            request,
            &DevelopmentNoopRedactor,
        )
        .unwrap()
}

/// The stored kind and canonical bytes of one object, if present.
pub(crate) fn object(store: &SqliteStore, id: &ObjectId) -> Option<(String, Vec<u8>)> {
    use rusqlite::OptionalExtension;
    store
        .connection
        .query_row(
            "SELECT object_kind,canonical_json FROM objects WHERE object_id=?1",
            [id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .unwrap()
}

/// Whether an evidence admission imported this original.
pub(crate) fn imported_only(store: &SqliteStore, id: &ObjectId) -> bool {
    super::task_sync::evidence::is_imported_only(&store.connection, id).unwrap()
}

/// Native execution state an imported pack must never grow: run-execution
/// feed entries, runs and claims. Exchange bookkeeping has its own feed.
pub(crate) fn native_counts(store: &SqliteStore) -> [i64; 3] {
    [
        "SELECT count(*) FROM work_feed_entries WHERE feed_kind='run_execution'",
        "SELECT count(*) FROM work_runs",
        "SELECT count(*) FROM work_claims",
    ]
    .map(|sql| {
        store
            .connection
            .query_row(sql, [], |row| row.get(0))
            .unwrap()
    })
}
