use super::{NativeEvidenceCaptureRequest, NativeEvidenceFeed};
use crate::storage::work::test_support::{actor, at, claim, root_request};
use crate::storage::{SqliteStore, test_database_shape_snapshot};
use crate::task_sync::{evidence::inventory, origin::EnrollRequest};
use crate::{DevelopmentNoopRedactor, FeedId, ObjectId, ProjectId, SessionId};

mod content;
mod disclosure;
mod generic_export;
mod proofs;

struct Fixture {
    store: SqliteStore,
    // Close SQLite before removing its owned fixture directory on Windows.
    _home: crate::test_support::TempHome,
    request: NativeEvidenceCaptureRequest,
    work: crate::WorkId,
    original: ObjectId,
    bytes: Vec<u8>,
}

fn project() -> ProjectId {
    ProjectId("native-pack-project".into())
}

fn fixture() -> Fixture {
    fixture_with_refs(Vec::new())
}

fn fixture_with_refs(refs: Vec<String>) -> Fixture {
    let home = crate::test_support::temp_home().unwrap();
    let database = crate::project_database_path(home.path(), &project());
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    let mut store = SqliteStore::open(&database).unwrap();
    let item = store
        .create_work(
            &root_request(&project().0, "native-work", 1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let held = claim(&mut store, &item, "runner", "claim", 2, 600);
    let item = store.get_work_item(item.work_id).unwrap();
    let original = store
        .record_work_evidence(
            &crate::RecordWorkEvidenceRequest {
                work_id: item.work_id,
                run_id: held.run_id,
                expected_work_revision: item.revision,
                holder: SessionId("runner".into()),
                claim_id: held.claim_id,
                claim_fence: held.fence,
                summary: "Shared native finding".into(),
                refs,
                actor: actor("runner"),
                idempotency_key: "native-check".into(),
                recorded_at: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let origin = store
        .enroll_task_sync_origin(
            &project(),
            &actor("collector"),
            &EnrollRequest {
                operation: uuid::Uuid::new_v4(),
                predecessor: None,
            },
            at(4),
        )
        .unwrap();
    let feed = FeedId::RunExecution(held.run_id);
    let through =
        crate::storage::work::feeds::current_run_feed_cut_on(&store.connection, held.run_id)
            .unwrap()
            .position;
    let bytes = store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id=?1",
            [original.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    Fixture {
        _home: home,
        store,
        work: item.work_id,
        original: original.clone(),
        bytes,
        request: NativeEvidenceCaptureRequest {
            capture: ObjectId::mint(),
            origin: origin.origin_id,
            profile: crate::task_sync::progress::ExportProfile {
                include_notes: true,
                origin_machine: Some("native-test".into()),
            },
            roots: vec![original],
            feeds: vec![NativeEvidenceFeed { feed, through }],
            maximum_pack_records: 2,
        },
    }
}

fn capture(f: &mut Fixture) -> super::NativeEvidenceCapture {
    f.store
        .capture_native_evidence(
            &project(),
            &SessionId("collector".into()),
            &actor("collector"),
            &f.request,
            &DevelopmentNoopRedactor,
        )
        .unwrap()
}

#[test]
fn native_capture_preserves_real_execution_bytes_and_imports_no_live_authority() {
    let mut f = fixture();
    assert!(
        f.store
            .capture_evidence_packs(&project())
            .unwrap()
            .is_none()
    );
    let captured = capture(&mut f);
    assert!(!captured.replayed);
    let page = f
        .store
        .export_native_evidence_capture(&project(), &captured.cursor, 32, &f.request.profile)
        .unwrap();
    assert_eq!(page.next, None);
    inventory::plan_batch(&page.packs, |_| Ok(None)).unwrap();
    let original = page
        .packs
        .iter()
        .flat_map(crate::task_sync::evidence::EvidencePack::records)
        .find(|record| record.object_id() == &f.original)
        .unwrap();
    assert_eq!(original.canonical_bytes(), f.bytes);
    assert_eq!(original.origin(), &f.request.origin);
    super::super::require_native(&f.store.connection, &f.original).unwrap();
    assert!(
        page.packs
            .iter()
            .flat_map(crate::task_sync::evidence::EvidencePack::records)
            .any(|record| record.kind() == crate::task_sync::evidence::RecordKind::WorkEvent)
    );
    assert_eq!(captured.feed_cuts.len(), 1);
    let mut destination = SqliteStore::open_in_memory().unwrap();
    let admissions = page
        .packs
        .iter()
        .map(|_| ObjectId::mint())
        .collect::<Vec<_>>();
    destination
        .retain_evidence_batch(
            &project(),
            &page.packs.iter().zip(&admissions).collect::<Vec<_>>(),
        )
        .unwrap();
    assert!(destination.get_work_item(f.work).is_err());
    assert!(super::super::require_native(&destination.connection, &f.original).is_err());
    let feeds: i64 = destination
        .connection
        .query_row("SELECT count(*) FROM work_feed_entries", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(feeds, 0);
    let report = destination.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
    let report = f.store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
}

#[test]
fn native_capture_exact_retry_preserves_pack_and_witness_ids_and_changed_selection_refuses() {
    let mut f = fixture();
    let first = capture(&mut f);
    let before = test_database_shape_snapshot(&f.store.connection).unwrap();
    let repeated = capture(&mut f);
    assert!(repeated.replayed);
    assert_eq!(first.packs, repeated.packs);
    assert_eq!(first.feed_cuts, repeated.feed_cuts);
    assert_eq!(first.governing, repeated.governing);
    assert_eq!(
        before,
        test_database_shape_snapshot(&f.store.connection).unwrap()
    );
    f.request.maximum_pack_records += 1;
    assert!(
        f.store
            .capture_native_evidence(
                &project(),
                &SessionId("collector".into()),
                &actor("collector"),
                &f.request,
                &DevelopmentNoopRedactor
            )
            .is_err()
    );
    assert_eq!(
        before,
        test_database_shape_snapshot(&f.store.connection).unwrap()
    );
}

#[test]
fn native_capture_missing_feed_context_refuses_atomically() {
    let mut f = fixture();
    let (kind, feed) = crate::storage::work::query::feed_parts(&f.request.feeds[0].feed);
    f.store
        .connection
        .execute(
            "DELETE FROM work_feed_entries WHERE feed_kind=?1 AND feed_id=?2 AND position=1",
            rusqlite::params![kind, feed],
        )
        .unwrap();
    let before = test_database_shape_snapshot(&f.store.connection).unwrap();
    let error = f
        .store
        .capture_native_evidence(
            &project(),
            &SessionId("collector".into()),
            &actor("collector"),
            &f.request,
            &DevelopmentNoopRedactor,
        )
        .unwrap_err();
    assert!(error.to_string().contains("gap"), "{error}");
    assert_eq!(
        before,
        test_database_shape_snapshot(&f.store.connection).unwrap()
    );
    assert!(
        f.store
            .capture_evidence_packs(&project())
            .unwrap()
            .is_none()
    );
}

#[test]
fn native_capture_preserves_a_historical_dependency_id_and_bytes() {
    let mut f = fixture();
    let original: crate::WorkEvidence = serde_json::from_slice(&f.bytes).unwrap();
    let historical = crate::CanonicalObject::freeze(&original).unwrap();
    assert_eq!(historical.key().as_str().len(), 64);
    let run = original.run_id;
    let cut =
        crate::storage::work::feeds::current_run_feed_cut_on(&f.store.connection, run).unwrap();
    // Retain an existing historical original without renaming it. The selected
    // native checkpoint depends on it even though it is outside the feed cut.
    let tx = f.store.connection.transaction().unwrap();
    SqliteStore::insert_object(&tx, "work_evidence", &historical).unwrap();
    let checkpoint = crate::CanonicalObject::mint(&crate::WorkCheckpoint {
        schema_version: crate::domain::SCHEMA_VERSION,
        work_id: f.work,
        run_id: run,
        claim_id: original.claim_id,
        claim_fence: original.claim_fence,
        acknowledged_run_position: cut,
        summary: "checkpoint with retained historical evidence".into(),
        evidence: vec![historical.key().clone()],
        actor: actor("runner"),
        created_at: at(5),
    })
    .unwrap();
    SqliteStore::insert_object(&tx, "work_checkpoint", &checkpoint).unwrap();
    crate::storage::work::feeds::append_to_work_feeds(
        &tx,
        &project(),
        f.work,
        Some(run),
        Some(f.work),
        "work_checkpoint",
        &checkpoint,
    )
    .unwrap();
    tx.commit().unwrap();
    f.request.roots = vec![checkpoint.key().clone()];
    f.request.feeds[0].through =
        crate::storage::work::feeds::current_run_feed_cut_on(&f.store.connection, run)
            .unwrap()
            .position;
    let captured = capture(&mut f);
    let page = f
        .store
        .export_native_evidence_capture(&project(), &captured.cursor, 32, &f.request.profile)
        .unwrap();
    inventory::plan_batch(&page.packs, |_| Ok(None)).unwrap();
    let retained = page
        .packs
        .iter()
        .flat_map(crate::task_sync::evidence::EvidencePack::records)
        .find(|record| record.object_id() == historical.key())
        .unwrap();
    assert_eq!(retained.canonical_bytes(), historical.bytes());
    assert_eq!(retained.origin(), &f.request.origin);
    let mut destination = SqliteStore::open_in_memory().unwrap();
    let admissions = page
        .packs
        .iter()
        .map(|_| ObjectId::mint())
        .collect::<Vec<_>>();
    destination
        .retain_evidence_batch(
            &project(),
            &page.packs.iter().zip(&admissions).collect::<Vec<_>>(),
        )
        .unwrap();
    let received: Vec<u8> = destination
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id=?1",
            [historical.key().as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(received, historical.bytes());
    assert!(super::super::require_native(&destination.connection, historical.key()).is_err());
}

#[test]
fn native_capture_continuation_reads_whole_retained_packs_without_advancing_capture() {
    let mut f = fixture();
    let captured = capture(&mut f);
    assert!(captured.packs.len() > 1);
    let before = test_database_shape_snapshot(&f.store.connection).unwrap();
    let mut cursor = captured.cursor.clone();
    let mut ids = Vec::new();
    loop {
        let page = f
            .store
            .export_native_evidence_capture(&project(), &cursor, 1, &f.request.profile)
            .unwrap();
        assert_eq!(page.packs.len(), 1);
        ids.push(page.packs[0].pack_id().clone());
        match page.next {
            Some(next) => cursor = next,
            None => break,
        }
    }
    assert_eq!(ids, captured.packs);
    assert_eq!(
        before,
        test_database_shape_snapshot(&f.store.connection).unwrap()
    );
    let first = f
        .store
        .export_native_evidence_capture(&project(), &captured.cursor, 1, &f.request.profile)
        .unwrap();
    assert_eq!(first.packs[0].pack_id(), &ids[0]);
}
