use super::*;
use crate::domain::{
    NoteRequest, NoteVisibility, ReviseWorkRequest, Sensitivity, WorkPlanningAuthority,
    WorkRevisionPatch,
};

fn memory(f: &mut Fixture, sensitivity: Sensitivity) -> (ObjectId, Vec<u8>) {
    f.store
        .focus_work_session(&project(), &SessionId("runner".into()), f.work, at(5))
        .unwrap();
    let receipt = f
        .store
        .insert_historical_memory_fixture(
            &NoteRequest {
                project_id: project(),
                task_id: None,
                work_id: Some(f.work),
                prose: "Shared native finding".into(),
                visibility: NoteVisibility::Shared,
                kind: None,
                authority: None,
                sensitivity: Some(sensitivity),
                title: None,
                tags: Vec::new(),
                evidence: Vec::new(),
                refs: Vec::new(),
                actor: actor("runner"),
                idempotency_key: "native-note".into(),
                created_at: at(5),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let run = serde_json::from_slice::<crate::WorkEvidence>(&f.bytes)
        .unwrap()
        .run_id;
    f.request.feeds[0].through =
        crate::storage::work::feeds::current_run_feed_cut_on(&f.store.connection, run)
            .unwrap()
            .position;
    let bytes = f
        .store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id=?1",
            [receipt.version.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    (receipt.version, bytes)
}

fn restrict(f: &mut Fixture) {
    let item = f.store.get_work_item(f.work).unwrap();
    let held = crate::storage::work::query::load_work_claim_optional(
        &f.store.connection,
        item.active_run_id.unwrap(),
    )
    .unwrap()
    .unwrap();
    f.store
        .revise_work(
            &ReviseWorkRequest {
                work_id: f.work,
                expected_revision: item.revision,
                patch: WorkRevisionPatch {
                    add_labels: vec!["restricted".into()],
                    ..WorkRevisionPatch::default()
                },
                authority: WorkPlanningAuthority::Claim {
                    run_id: held.run_id,
                    holder: SessionId("runner".into()),
                    claim_id: held.claim_id,
                    claim_fence: held.fence,
                },
                actor: actor("runner"),
                idempotency_key: "restrict-note".into(),
                updated_at: at(12),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
}

fn complete_for_capture(f: &mut Fixture, second: i64) -> ObjectId {
    use crate::storage::work::test_support::{checkpoint, complete};

    let item = f.store.get_work_item(f.work).unwrap();
    let held = crate::storage::work::query::load_work_claim_optional(
        &f.store.connection,
        item.active_run_id.unwrap(),
    )
    .unwrap()
    .unwrap();
    checkpoint(
        &mut f.store,
        &item,
        &held,
        "runner",
        "disclosure-checkpoint",
        second,
        std::slice::from_ref(&f.original),
    );
    let seal = complete(
        &mut f.store,
        &item,
        &held,
        "runner",
        &f.original,
        "disclosure-completion",
        second + 1,
    )
    .unwrap();
    f.request.roots = vec![
        f.store
            .work_event_tail(f.work, 1)
            .unwrap()
            .pop()
            .unwrap()
            .object_id,
    ];
    f.request.feeds[0].through = f.store.work_feed_head(&f.request.feeds[0].feed).unwrap();
    seal.accepted_work_revision_hash
}

fn reopen(f: &mut Fixture) {
    let item = f.store.get_work_item(f.work).unwrap();
    f.store
        .reopen_work(
            &crate::ReopenWorkRequest {
                work_id: f.work,
                expected_work_revision: item.revision,
                reason: "restrict disclosure after capturing completed history".into(),
                actor: actor("human"),
                idempotency_key: "disclosure-reopen".into(),
                reopened_at: at(9),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let item = f.store.get_work_item(f.work).unwrap();
    claim(&mut f.store, &item, "runner", "reopened-claim", 10, 600);
}

pub(super) fn assert_generic_refused(
    store: &SqliteStore,
    cursor: &crate::storage::work::task_sync::evidence::PackCursor,
) {
    let error = store.export_evidence_packs(cursor).err().unwrap();
    assert!(
        error.to_string().contains("profile-bound export"),
        "{error}"
    );
}

#[test]
fn reference_bearing_evidence_refuses_capture_without_retaining_packs() {
    let mut f = fixture_with_refs(vec!["cargo:test".into()]);
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
    assert!(error.to_string().contains("note disclosure refused"));
    assert_eq!(
        test_database_shape_snapshot(&f.store.connection).unwrap(),
        before
    );
    assert!(
        f.store
            .capture_evidence_packs(&project())
            .unwrap()
            .is_none()
    );
    let original: Vec<u8> = f
        .store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id=?1",
            [f.original.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(original, f.bytes);
}

#[test]
fn notes_off_refuses_native_memory_and_evidence_summary_without_retaining_packs() {
    for with_memory in [false, true] {
        let mut f = fixture();
        if with_memory {
            memory(&mut f, Sensitivity::Internal);
        }
        f.request.profile.include_notes = false;
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
        assert!(error.to_string().contains("note disclosure refused"));
        assert_eq!(
            test_database_shape_snapshot(&f.store.connection).unwrap(),
            before
        );
        assert!(
            f.store
                .capture_evidence_packs(&project())
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn disclosed_native_memory_keeps_original_bytes_and_profile_bound_replay() {
    let mut f = fixture();
    let (id, bytes) = memory(&mut f, Sensitivity::Internal);
    let first = capture(&mut f);
    let page = f
        .store
        .export_native_evidence_capture(&project(), &first.cursor, 32, &f.request.profile)
        .unwrap();
    let original = page
        .packs
        .iter()
        .flat_map(crate::task_sync::evidence::EvidencePack::records)
        .find(|record| record.object_id() == &id)
        .unwrap();
    assert_eq!(original.canonical_bytes(), bytes);
    let before = test_database_shape_snapshot(&f.store.connection).unwrap();
    let repeated = capture(&mut f);
    assert!(repeated.replayed);
    assert_eq!(repeated.packs, first.packs);
    assert_eq!(
        test_database_shape_snapshot(&f.store.connection).unwrap(),
        before
    );
    let mut profile = f.request.profile.clone();
    profile.include_notes = false;
    assert!(
        f.store
            .export_native_evidence_capture(&project(), &first.cursor, 32, &profile)
            .err()
            .unwrap()
            .to_string()
            .contains("disclosure profile differs")
    );
    f.request.profile = profile;
    assert!(
        f.store
            .capture_native_evidence(
                &project(),
                &SessionId("collector".into()),
                &actor("collector"),
                &f.request,
                &DevelopmentNoopRedactor
            )
            .unwrap_err()
            .to_string()
            .contains("retry intent differs")
    );
    assert_eq!(
        test_database_shape_snapshot(&f.store.connection).unwrap(),
        before
    );
}

#[test]
fn restricted_memory_and_task_labels_refuse_capture_atomically() {
    for sensitive_memory in [false, true] {
        let mut f = fixture();
        memory(
            &mut f,
            if sensitive_memory {
                Sensitivity::Restricted
            } else {
                Sensitivity::Internal
            },
        );
        if !sensitive_memory {
            restrict(&mut f);
        }
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
        assert!(error.to_string().contains("disclosure refused"));
        assert_eq!(
            test_database_shape_snapshot(&f.store.connection).unwrap(),
            before
        );
        assert!(
            f.store
                .capture_evidence_packs(&project())
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn newly_restricted_task_blocks_export_and_capture_replay_without_rewriting_history() {
    let mut f = fixture();
    memory(&mut f, Sensitivity::Internal);
    let captured = capture(&mut f);
    let generic = f.store.capture_evidence_packs(&project()).unwrap().unwrap();
    let permitted = f
        .store
        .export_native_evidence_capture(&project(), &captured.cursor, 32, &f.request.profile)
        .unwrap();
    let original_packs = permitted
        .packs
        .iter()
        .map(|pack| (pack.pack_id().clone(), pack.to_bytes().unwrap()))
        .collect::<Vec<_>>();
    restrict(&mut f);
    let before = test_database_shape_snapshot(&f.store.connection).unwrap();
    assert_generic_refused(&f.store, &generic);
    assert_generic_refused(
        &f.store,
        &f.store.capture_evidence_packs(&project()).unwrap().unwrap(),
    );
    assert!(
        f.store
            .export_native_evidence_capture(&project(), &captured.cursor, 32, &f.request.profile)
            .err()
            .unwrap()
            .to_string()
            .contains("disclosure refused")
    );
    assert!(
        f.store
            .capture_native_evidence(
                &project(),
                &SessionId("collector".into()),
                &actor("collector"),
                &f.request,
                &DevelopmentNoopRedactor
            )
            .unwrap_err()
            .to_string()
            .contains("disclosure refused")
    );
    assert_eq!(
        test_database_shape_snapshot(&f.store.connection).unwrap(),
        before
    );
    for (id, bytes) in original_packs {
        let retained = crate::storage::work::task_sync::evidence::load_retained_pack_on(
            &f.store.connection,
            &project(),
            &id,
        )
        .unwrap();
        assert_eq!(retained.to_bytes().unwrap(), bytes);
    }
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn every_single_record_page_rechecks_task_disclosure_without_companion_records() {
    let mut f = fixture();
    let revision = complete_for_capture(&mut f, 6);
    f.request.maximum_pack_records = 1;
    let captured = capture(&mut f);
    let generic = f.store.capture_evidence_packs(&project()).unwrap().unwrap();
    let before = test_database_shape_snapshot(&f.store.connection).unwrap();
    let mut revision_page = false;
    let mut originals = Vec::new();
    for after in 0..captured.packs.len() {
        let cursor = super::super::NativeEvidenceCursor {
            after,
            ..captured.cursor.clone()
        };
        let page = f
            .store
            .export_native_evidence_capture(&project(), &cursor, 1, &f.request.profile)
            .unwrap();
        assert_eq!(page.packs.len(), 1);
        assert_eq!(page.packs[0].pack_id(), &captured.packs[after]);
        assert_eq!(page.packs[0].records().len(), 1);
        let record = &page.packs[0].records()[0];
        if record.object_id() == &revision {
            assert_eq!(
                record.kind(),
                crate::task_sync::evidence::RecordKind::WorkItemRevision
            );
            revision_page = true;
        }
        let stored: Vec<u8> = f
            .store
            .connection
            .query_row(
                "SELECT canonical_json FROM objects WHERE object_id=?1",
                [record.object_id().as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(record.canonical_bytes(), stored);
        assert_eq!(record.origin(), &f.request.origin);
        originals.push((record.object_id().clone(), stored));
        let repeated = f
            .store
            .export_native_evidence_capture(&project(), &cursor, 1, &f.request.profile)
            .unwrap();
        assert_eq!(repeated.packs[0], page.packs[0]);
        assert_eq!(repeated.next, page.next);
        let generic_page = crate::storage::work::task_sync::evidence::PackCursor {
            capture: captured.packs[after].clone(),
            after: after
                .checked_sub(1)
                .map(|index| captured.packs[index].clone()),
            ..generic.clone()
        };
        assert_generic_refused(&f.store, &generic_page);
    }
    assert!(revision_page, "exercise a task-revision-only page");
    assert_eq!(
        test_database_shape_snapshot(&f.store.connection).unwrap(),
        before
    );

    reopen(&mut f);
    restrict(&mut f);
    let before = test_database_shape_snapshot(&f.store.connection).unwrap();
    for after in 0..captured.packs.len() {
        let cursor = super::super::NativeEvidenceCursor {
            after,
            ..captured.cursor.clone()
        };
        let error = f
            .store
            .export_native_evidence_capture(&project(), &cursor, 1, &f.request.profile)
            .err()
            .unwrap();
        assert!(error.to_string().contains("disclosure refused"), "{error}");
        let generic_page = crate::storage::work::task_sync::evidence::PackCursor {
            capture: captured.packs[after].clone(),
            after: after
                .checked_sub(1)
                .map(|index| captured.packs[index].clone()),
            ..generic.clone()
        };
        assert_generic_refused(&f.store, &generic_page);
    }
    assert_eq!(
        test_database_shape_snapshot(&f.store.connection).unwrap(),
        before
    );
    for (id, bytes) in originals {
        let retained: Vec<u8> = f
            .store
            .connection
            .query_row(
                "SELECT canonical_json FROM objects WHERE object_id=?1",
                [id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, bytes);
    }
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn captured_task_restrictions_refuse_without_native_task_or_companion_records() {
    let mut f = fixture();
    restrict(&mut f);
    let id = complete_for_capture(&mut f, 13);
    let bytes: Vec<u8> = f
        .store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id=?1 AND object_kind='work_item_revision'",
            [id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    let item: crate::WorkItem = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(item.work_id, f.work);
    assert!(item.labels.iter().any(|label| label == "restricted"));
    let record = crate::task_sync::evidence::RecordEnvelope::from_canonical(
        f.request.origin.clone(),
        id.clone(),
        "work_item_revision",
        bytes.clone(),
    )
    .unwrap();
    let destination = SqliteStore::open_in_memory().unwrap();
    let before = test_database_shape_snapshot(&destination.connection).unwrap();
    let empty = std::collections::BTreeMap::new();
    let error = super::super::disclosure::validate(
        &destination.connection,
        &f.request.profile,
        std::iter::once(&record),
        &empty,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("task disclosure refused"),
        "{error}"
    );
    // The same captured restriction must also guard a page with no task snapshot.
    let labels = std::collections::BTreeMap::from([(f.work, vec!["restricted".into()])]);
    assert!(
        super::super::disclosure::validate(
            &destination.connection,
            &f.request.profile,
            std::iter::empty(),
            &labels,
        )
        .is_err()
    );
    assert_eq!(record.object_id(), &id);
    assert_eq!(record.canonical_bytes(), bytes);
    assert_eq!(
        test_database_shape_snapshot(&destination.connection).unwrap(),
        before
    );
}

#[test]
fn run_activity_requires_an_explicit_machine_disclosure() {
    let mut f = fixture();
    f.request.profile.include_notes = false;
    f.request.profile.origin_machine = None;
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
    assert!(error.to_string().contains("activity disclosure refused"));
    assert_eq!(
        test_database_shape_snapshot(&f.store.connection).unwrap(),
        before
    );
}
