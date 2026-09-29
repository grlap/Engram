use std::collections::BTreeMap;

use super::*;

/// One restored-record row read back from a store: its bound id, its object
/// kind and its canonical bytes, keyed by item and generation.
type RestoredRows = BTreeMap<(String, i64), (String, String, Vec<u8>)>;

fn restored_rows(store: &SqliteStore) -> RestoredRows {
    store
        .connection
        .prepare(
            "SELECT record.work_id, record.generation_index, record.record_id,
                    object.object_kind, object.canonical_json
             FROM work_restored_records record
             JOIN objects object ON object.object_id = record.record_id
             ORDER BY record.work_id, record.generation_index",
        )
        .expect("prepare restored rows")
        .query_map([], |row| {
            Ok((
                (row.get::<_, String>(0)?, row.get::<_, i64>(1)?),
                (
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ),
            ))
        })
        .expect("query restored rows")
        .collect::<Result<_, _>>()
        .expect("read restored rows")
}

fn load_into(
    directory: &std::path::Path,
    name: &str,
    project: &ProjectId,
    bytes: &[u8],
    loader: &str,
    second: i64,
) -> SqliteStore {
    let mut store = SqliteStore::open(directory.join(name)).expect("fresh store");
    store
        .load_work_graph_snapshot(
            project,
            &actor(loader),
            bytes,
            false,
            at(second),
            &DevelopmentNoopRedactor,
        )
        .expect("load snapshot into a fresh store");
    store
}

fn save(
    store: &mut SqliteStore,
    project: &ProjectId,
    second: i64,
) -> crate::WorkGraphSnapshotDocument {
    store
        .save_work_graph_snapshot(
            project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(second),
            &DevelopmentNoopRedactor,
        )
        .expect("save snapshot")
        .document
}

// One snapshot file loaded into two independent fresh stores. An inherited
// record keeps the id and bytes the file gives it; a native layer is minted
// anew by each load, from the file's content alone. Every expected value is
// read from the saved document or the stores at run time.
//
// What makes each group of assertions fail:
// - fixture shape and note: a save that stops carrying inherited records as
//   restored, or emits the new history some other way; these guard the
//   fixture, not the load;
// - key sets: a load that drops or adds a generation;
// - object kind: a row bound to an object of another kind;
// - inherited ids: a load that re-mints inherited records (criterion 1);
// - inherited bytes: a load that rewrites an inherited record, for example by
//   rebuilding it from the current item instead of storing the file's JSON
//   (criterion 1);
// - agreement with the store the file was saved from: a save or load that
//   changes an inherited id or its bytes on the way (criterion 1);
// - distinct native ids: native ids derived from content instead of minted,
//   which come out equal in both stores (criterion 2);
// - native ids apart from inherited ones: a native layer stored under an
//   inherited record's id (criterion 2);
// - equal native bytes: loader actor or load time leaking into a native
//   record, because the two loads use different ones (criterion 2).
#[test]
fn one_snapshot_loaded_into_two_fresh_stores_keeps_inherited_ids_and_mints_native_ones() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let project = ProjectId("snapshot-restored-identity".into());
    let mut source = SqliteStore::open(directory.path().join("source.db")).expect("source store");
    let root = create_root(&mut source, &project, "Identity root", "identity-root");
    let prerequisite = create_root(
        &mut source,
        &project,
        "Identity prerequisite",
        "identity-prerequisite",
    );
    source
        .add_work_prerequisite(
            &ChangeWorkPrerequisiteRequest {
                work_id: root.work_id,
                prerequisite_id: prerequisite.work_id,
                expected_revision: root.revision,
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner-session"),
                idempotency_key: "identity-prerequisite-edge".into(),
                changed_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("add prerequisite edge");
    let first = save(&mut source, &project, 3);
    let mut restored = load_into(
        directory.path(),
        "restored.db",
        &project,
        &serde_json::to_vec_pretty(&first).expect("serialize first snapshot"),
        "load-session",
        4,
    );

    // New history on one item of the restored store becomes a native layer on
    // top of that item's inherited record.
    let claim = restored
        .claim_work(
            &ClaimWorkRequest {
                work_id: prerequisite.work_id,
                expected_work_revision: 1,
                expected_run_id: None,
                holder: crate::SessionId("note-session".into()),
                ttl_seconds: 900,
                recovery_reason: None,
                actor: actor("note-session"),
                idempotency_key: "identity-claim".into(),
                claimed_at: at(5),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("claim restored prerequisite");
    let note = "the restored prerequisite gains a native layer";
    restored
        .record_work_evidence(
            &RecordWorkEvidenceRequest {
                work_id: prerequisite.work_id,
                run_id: claim.run_id,
                expected_work_revision: claim.accepted_work_revision,
                holder: claim.holder.clone(),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                summary: note.into(),
                refs: Vec::new(),
                actor: actor("note-session"),
                idempotency_key: "identity-note".into(),
                recorded_at: at(6),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("record note on the restored prerequisite");
    let document = save(&mut restored, &project, 7);

    // The exact shape: both items inherit generation 0, and only the changed
    // item carries a native generation 1 holding the new note.
    let shape: Vec<(WorkId, u64, bool)> = document
        .body
        .records
        .iter()
        .map(|record| {
            (
                record.work_id,
                u64::try_from(record.generation_index).expect("generation index"),
                matches!(
                    record.payload,
                    crate::WorkGraphSnapshotRecordPayload::Native { .. }
                ),
            )
        })
        .collect();
    let mut expected_shape = vec![
        (root.work_id, 0, false),
        (prerequisite.work_id, 0, false),
        (prerequisite.work_id, 1, true),
    ];
    expected_shape.sort_by_key(|(work_id, generation, _)| (work_id.0.to_string(), *generation));
    let mut sorted_shape = shape;
    sorted_shape.sort_by_key(|(work_id, generation, _)| (work_id.0.to_string(), *generation));
    assert_eq!(sorted_shape, expected_shape);
    let native_history = document
        .body
        .records
        .iter()
        .find_map(|record| match &record.payload {
            crate::WorkGraphSnapshotRecordPayload::Native { history } => Some(history),
            crate::WorkGraphSnapshotRecordPayload::Restored { .. } => None,
        })
        .expect("native layer");
    assert!(
        native_history
            .notes
            .iter()
            .any(|recorded| recorded.summary == note),
        "the native layer carries the new note"
    );

    // Two independent loads of the same bytes, by different loaders at
    // different times.
    let bytes = serde_json::to_vec_pretty(&document).expect("serialize snapshot");
    let first_load = restored_rows(&load_into(
        directory.path(),
        "first-load.db",
        &project,
        &bytes,
        "first-loader",
        20,
    ));
    let second_load = restored_rows(&load_into(
        directory.path(),
        "second-load.db",
        &project,
        &bytes,
        "second-loader",
        30,
    ));

    let document_keys: Vec<(String, i64)> = {
        let mut keys: Vec<_> = document
            .body
            .records
            .iter()
            .map(|record| {
                (
                    record.work_id.0.to_string(),
                    i64::try_from(record.generation_index).expect("generation index"),
                )
            })
            .collect();
        keys.sort();
        keys
    };
    assert_eq!(
        first_load.keys().cloned().collect::<Vec<_>>(),
        document_keys
    );
    assert_eq!(
        second_load.keys().cloned().collect::<Vec<_>>(),
        document_keys
    );
    let origin = restored_rows(&restored);
    let inherited_ids: Vec<String> = document
        .body
        .records
        .iter()
        .filter_map(|record| match &record.payload {
            crate::WorkGraphSnapshotRecordPayload::Restored { object_id, .. } => {
                Some(object_id.as_str().to_owned())
            }
            crate::WorkGraphSnapshotRecordPayload::Native { .. } => None,
        })
        .collect();

    for record in &document.body.records {
        let key = (
            record.work_id.0.to_string(),
            i64::try_from(record.generation_index).expect("generation index"),
        );
        let (first_id, first_kind, first_bytes) = &first_load[&key];
        let (second_id, second_kind, second_bytes) = &second_load[&key];
        assert_eq!(first_kind, "work_restored_record");
        assert_eq!(second_kind, "work_restored_record");
        match &record.payload {
            crate::WorkGraphSnapshotRecordPayload::Restored {
                object_id,
                canonical_json,
            } => {
                // Criterion 1: the file's id and bytes, in both stores.
                let expected = CanonicalObject::identified(object_id, canonical_json)
                    .expect("canonical inherited record");
                assert_eq!(first_id, object_id.as_str(), "{key:?}");
                assert_eq!(second_id, object_id.as_str(), "{key:?}");
                assert_eq!(first_bytes.as_slice(), expected.bytes(), "{key:?}");
                assert_eq!(second_bytes.as_slice(), expected.bytes(), "{key:?}");
                // The same row as in the store the file was saved from, a
                // reference independent of the loader's canonicalization.
                let (origin_id, origin_kind, origin_bytes) = &origin[&key];
                assert_eq!(origin_kind, "work_restored_record", "{key:?}");
                assert_eq!(first_id, origin_id, "{key:?}");
                assert_eq!(first_bytes, origin_bytes, "{key:?}");
                assert_eq!(second_bytes, origin_bytes, "{key:?}");
            }
            crate::WorkGraphSnapshotRecordPayload::Native { .. } => {
                // Criterion 2: a fresh id per load, the same bytes in both.
                assert_ne!(first_id, second_id, "{key:?}");
                assert!(!inherited_ids.contains(first_id), "{key:?}");
                assert!(!inherited_ids.contains(second_id), "{key:?}");
                assert_eq!(first_bytes, second_bytes, "{key:?}");
            }
        }
    }
}
