//! Real native capture and foreign retention shared by producer regressions.

use super::{EvidencePack, NativeEvidenceCaptureRequest, NativeEvidenceFeed, RecordEnvelope};
use crate::storage::work::test_support::{actor, at};
use crate::storage::{SqliteStore, test_database_shape_snapshot};
use crate::task_sync::evidence::{EvidenceError, RecordKind, inventory};
use crate::task_sync::origin::EnrollRequest;
use crate::{DevelopmentNoopRedactor, FeedId, ObjectId, ProjectId, SessionId, WorkRunId};

pub(crate) fn project_store(home: &std::path::Path, project: &ProjectId) -> SqliteStore {
    let database = crate::project_database_path(home, project);
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    SqliteStore::open(database).unwrap()
}

fn original_origins(
    store: &SqliteStore,
    project: &ProjectId,
) -> std::collections::BTreeMap<String, String> {
    store
        .connection
        .prepare(
            "SELECT DISTINCT member.object_id,member.origin_id
             FROM work_task_sync_evidence_members member
             JOIN work_task_sync_evidence_packs pack ON pack.pack_id=member.pack_id
             WHERE pack.project_id=?1",
        )
        .unwrap()
        .query_map([&project.0], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<std::result::Result<std::collections::BTreeMap<_, _>, _>>()
        .unwrap()
}

pub(crate) fn capture_roundtrip(
    store: &mut SqliteStore,
    project: &ProjectId,
    run: WorkRunId,
    roots: Vec<ObjectId>,
) -> Vec<EvidencePack> {
    capture_feeds_roundtrip(store, project, vec![FeedId::RunExecution(run)], roots)
}

pub(crate) fn capture_feeds_roundtrip(
    store: &mut SqliteStore,
    project: &ProjectId,
    feeds: Vec<FeedId>,
    roots: Vec<ObjectId>,
) -> Vec<EvidencePack> {
    // Freeze existing provenance before the new capture adds any memberships.
    let original_origins = original_origins(store, project);
    let origin = store
        .enroll_task_sync_origin(
            project,
            &actor("collector"),
            &EnrollRequest {
                operation: uuid::Uuid::new_v4(),
                predecessor: None,
            },
            at(20),
        )
        .unwrap();
    let request = NativeEvidenceCaptureRequest {
        capture: ObjectId::mint(),
        origin: origin.origin_id.clone(),
        profile: crate::task_sync::progress::ExportProfile {
            include_notes: true,
            origin_machine: Some("native-test".into()),
        },
        roots,
        feeds: feeds
            .into_iter()
            .map(|feed| NativeEvidenceFeed {
                through: store.work_feed_head(&feed).unwrap(),
                feed,
            })
            .collect(),
        maximum_pack_records: 16,
    };
    let captured = store
        .capture_native_evidence(
            project,
            &SessionId("collector".into()),
            &actor("collector"),
            &request,
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let page = store
        .export_native_evidence_capture(project, &captured.cursor, 128, &request.profile)
        .unwrap();
    assert!(page.next.is_none());
    let packs = page
        .packs
        .iter()
        .map(|pack| EvidencePack::read(pack.to_bytes().unwrap().as_slice()).unwrap())
        .collect::<Vec<_>>();
    inventory::plan_batch(&packs, |_| Ok(None)).unwrap();
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("received.sqlite3");
    let mut destination = SqliteStore::open(&database).unwrap();
    let admissions = packs.iter().map(|_| ObjectId::mint()).collect::<Vec<_>>();
    destination
        .retain_evidence_batch(project, &packs.iter().zip(&admissions).collect::<Vec<_>>())
        .unwrap();
    let native_rows: i64 = destination
        .connection
        .query_row(
            "SELECT (SELECT count(*) FROM work_items) + (SELECT count(*) FROM work_feed_entries)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(native_rows, 0);
    for record in packs.iter().flat_map(EvidencePack::records) {
        let (_, original) = super::object_on(&store.connection, record.object_id())
            .unwrap()
            .unwrap();
        let (_, received) = super::object_on(&destination.connection, record.object_id())
            .unwrap()
            .unwrap();
        let expected_origin = original_origins
            .get(record.object_id().as_str())
            .map_or(origin.origin_id.as_str(), String::as_str);
        assert_eq!(record.origin().as_str(), expected_origin);
        assert_eq!(record.canonical_bytes(), original);
        assert_eq!(received, original);
    }
    let report = store.verify_all().unwrap();
    assert!(report.is_healthy(), "native: {report:?}");
    let report = destination.verify_all().unwrap();
    assert!(report.is_healthy(), "received: {report:?}");
    drop(destination);
    let report = SqliteStore::repair_rebuildable_projections(&database).unwrap();
    assert!(report.is_healthy(), "repair: {report:?}");
    let destination = SqliteStore::open(&database).unwrap();
    let report = destination.verify_all().unwrap();
    assert!(report.is_healthy(), "reopened: {report:?}");
    packs
}

pub(crate) fn dependency_refusals(packs: &[EvidencePack], dependency: &ObjectId) {
    let wrong_kind_record = packs
        .iter()
        .flat_map(EvidencePack::records)
        .find(|record| record.kind() == RecordKind::ProducingOrigin)
        .unwrap();
    for wrong_kind in [false, true] {
        let altered = packs
            .iter()
            .map(|pack| {
                let mut records = pack
                    .records()
                    .iter()
                    .filter(|record| record.object_id() != dependency)
                    .cloned()
                    .collect::<Vec<_>>();
                if wrong_kind
                    && pack
                        .records()
                        .iter()
                        .any(|record| record.object_id() == dependency)
                {
                    // A valid context kind with no links cannot satisfy a typed
                    // producer/snapshot/witness dependency merely by sharing its id.
                    records.push(
                        RecordEnvelope::from_canonical(
                            wrong_kind_record.origin().clone(),
                            dependency.clone(),
                            wrong_kind_record.kind().as_str(),
                            wrong_kind_record.canonical_bytes().to_vec(),
                        )
                        .unwrap(),
                    );
                }
                EvidencePack::prepare(
                    ObjectId::mint(),
                    pack.project_id().clone(),
                    pack.publishing_origin().clone(),
                    records,
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let error = inventory::plan_batch(&altered, |_| Ok(None)).unwrap_err();
        assert!(
            matches!(error, EvidenceError::Dependency { ref object, .. } if object == dependency),
            "{error}"
        );
        let mut destination = SqliteStore::open_in_memory().unwrap();
        let before = test_database_shape_snapshot(&destination.connection).unwrap();
        let admissions = altered.iter().map(|_| ObjectId::mint()).collect::<Vec<_>>();
        assert!(
            destination
                .retain_evidence_batch(
                    altered[0].project_id(),
                    &altered.iter().zip(&admissions).collect::<Vec<_>>(),
                )
                .is_err()
        );
        assert_eq!(
            before,
            test_database_shape_snapshot(&destination.connection).unwrap()
        );
    }
}
