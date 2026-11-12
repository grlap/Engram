use super::{ANCESTRY_VISITS, DECODES, derive, load, rebuild, verify};
use crate::storage::work::task_sync::{producing_origin, tests::state};
use crate::storage::work::test_support::{actor, at};
use crate::task_sync::origin::{Enrollment, PRODUCING_ORIGIN_KIND};
use crate::{CanonicalObject, ObjectId, ProjectId, SqliteStore, StoreError};

fn enrollment(project: &ProjectId, predecessor: Option<ObjectId>) -> Enrollment {
    Enrollment {
        project_id: project.clone(),
        predecessor_origin_id: predecessor,
        enrollment_operation_id: uuid::Uuid::new_v4(),
        actor: actor("origin-view-test"),
        enrolled_at: at(1),
    }
}

fn append(store: &mut SqliteStore, record: &Enrollment) -> ObjectId {
    let object = CanonicalObject::mint(record).unwrap();
    let tx = store.begin_work_mutation().unwrap();
    producing_origin::insert_enrollment_on(&tx, &object).unwrap();
    tx.commit().unwrap();
    object.key().clone()
}

fn objects(store: &SqliteStore) -> Vec<(String, Vec<u8>)> {
    store
        .connection
        .prepare("SELECT object_id,canonical_json FROM objects ORDER BY object_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn origin_head_reads_one_record_despite_unrelated_enrollments_and_long_lineage() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let project = ProjectId("selected".into());
    let mut head = None;
    for _ in 0..512 {
        head = Some(append(&mut store, &enrollment(&project, head)));
    }
    for index in 0..2000 {
        append(
            &mut store,
            &enrollment(&ProjectId(format!("unrelated-{index}")), None),
        );
    }
    let expected = load(&store.connection, &project).unwrap().unwrap();
    assert_eq!(Some(&expected.origin_id), head.as_ref());
    // A selected read must not decode even malformed disconnected history.
    let unrelated: String = store.connection.query_row(
        "SELECT origin_id FROM work_task_sync_producing_origin_heads WHERE project_id='unrelated-0'", [], |row| row.get(0),
    ).unwrap();
    store
        .connection
        .execute(
            "UPDATE objects SET canonical_json=?1 WHERE object_id=?2",
            rusqlite::params![b"{\"actor\":\"malformed\"}".as_slice(), unrelated],
        )
        .unwrap();
    let before = state(&store);
    DECODES.with(|count| count.set(0));
    assert_eq!(load(&store.connection, &project).unwrap(), Some(expected));
    assert_eq!(DECODES.with(std::cell::Cell::get), 1);
    assert_eq!(state(&store), before);
    // Restore the damaged fixture for exhaustive derivation and count the
    // actual predecessor traversal, including shared completed suffixes.
    let record = enrollment(&ProjectId("unrelated-0".into()), None);
    let object =
        CanonicalObject::identified(&ObjectId::from_stored(unrelated.clone()).unwrap(), &record)
            .unwrap();
    store
        .connection
        .execute(
            "UPDATE objects SET canonical_json=?1 WHERE object_id=?2",
            rusqlite::params![object.bytes(), unrelated],
        )
        .unwrap();
    DECODES.with(|count| count.set(0));
    ANCESTRY_VISITS.with(|count| count.set(0));
    let mut checked = 0;
    let rows = derive(&store.connection, &mut checked).unwrap();
    assert_eq!(rows.members.len(), 2512);
    assert_eq!(checked, 2512);
    assert_eq!(DECODES.with(std::cell::Cell::get), 2512);
    assert_eq!(ANCESTRY_VISITS.with(std::cell::Cell::get), 2512);
}

#[test]
fn origin_projection_refuses_missing_stale_and_feed_proofs_then_repairs_without_identity_changes() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("origin.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let project = ProjectId("repair".into());
    let first = append(&mut store, &enrollment(&project, None));
    let head = append(&mut store, &enrollment(&project, Some(first.clone())));
    let canonical = objects(&store);
    let witness = home.path().join("task-sync-origin-registration.json");
    std::fs::write(&witness, b"independent private witness").unwrap();
    for damage in [
        "DELETE FROM work_task_sync_producing_origin_heads",
        "UPDATE work_task_sync_producing_origin_heads SET origin_id=(SELECT origin_id FROM work_task_sync_producing_origin_members WHERE predecessor_id IS NULL)",
        "UPDATE work_task_sync_producing_origin_members SET operation_id=operation_id||'-wrong'",
        "UPDATE work_task_sync_producing_origin_members SET feed_position=feed_position+1",
        "DELETE FROM work_task_sync_producing_origin_heads; DELETE FROM work_task_sync_producing_origin_members",
    ] {
        store
            .connection
            .execute_batch(damage)
            .unwrap_or_else(|error| panic!("damage {damage}: {error}"));
        let before = state(&store);
        assert!(
            load(&store.connection, &project)
                .unwrap_err()
                .to_string()
                .contains("repair projections")
        );
        assert_eq!(state(&store), before);
        let report = store.verify_all().unwrap();
        assert!(
            report
                .invalid_work_records
                .iter()
                .any(|label| label == "task_sync_producing_origin:projection differs"),
            "{report:?}"
        );
        drop(store);
        assert!(
            SqliteStore::repair_rebuildable_projections(&database)
                .unwrap_or_else(|error| panic!("repair after {damage}: {error}"))
                .is_healthy()
        );
        store = SqliteStore::open(&database).unwrap();
        assert_eq!(
            load(&store.connection, &project)
                .unwrap()
                .unwrap()
                .origin_id,
            head
        );
        assert_eq!(objects(&store), canonical);
        assert_eq!(
            std::fs::read(&witness).unwrap(),
            b"independent private witness"
        );
    }
    let before = state(&store);
    assert_eq!(
        load(&store.connection, &ProjectId("never-enrolled".into())).unwrap(),
        None
    );
    assert_eq!(state(&store), before);
}

#[test]
fn origin_append_projection_failure_and_duplicate_operation_roll_back_canonical_and_feed() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let project = ProjectId("atomic".into());
    let record = enrollment(&project, None);
    append(&mut store, &record);
    let before = state(&store);
    let duplicate = CanonicalObject::mint(&Enrollment {
        project_id: ProjectId("other".into()),
        ..record
    })
    .unwrap();
    let tx = store.begin_work_mutation().unwrap();
    assert!(
        producing_origin::insert_enrollment_on(&tx, &duplicate)
            .unwrap_err()
            .to_string()
            .contains("operation already")
    );
    drop(tx);
    assert_eq!(state(&store), before);
    store.connection.execute_batch(
        "CREATE TRIGGER fail_origin_projection BEFORE UPDATE ON work_task_sync_producing_origin_heads
         BEGIN SELECT RAISE(ABORT,'injected origin view failure'); END;",
    ).unwrap();
    let before = state(&store);
    let current = load(&store.connection, &project).unwrap().unwrap();
    let next = CanonicalObject::mint(&enrollment(&project, Some(current.origin_id))).unwrap();
    let tx = store.begin_work_mutation().unwrap();
    assert!(matches!(
        producing_origin::insert_enrollment_on(&tx, &next),
        Err(StoreError::Sqlite(_))
    ));
    drop(tx);
    assert_eq!(state(&store), before);
}

#[test]
fn origin_cold_validation_rejects_foreign_predecessors_cycles_and_competing_heads() {
    for defect in ["foreign", "cycle", "fork", "duplicate-operation"] {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let project = ProjectId("lineage".into());
        let original = enrollment(&project, None);
        let first = append(&mut store, &original);
        let second_id = ObjectId::mint();
        let second = Enrollment {
            project_id: if defect == "foreign" {
                ProjectId("foreign".into())
            } else {
                project.clone()
            },
            predecessor_origin_id: if defect == "fork" || defect == "duplicate-operation" {
                None
            } else {
                Some(first.clone())
            },
            enrollment_operation_id: if defect == "duplicate-operation" {
                original.enrollment_operation_id
            } else {
                uuid::Uuid::new_v4()
            },
            ..original.clone()
        };
        let object = CanonicalObject::identified(&second_id, &second).unwrap();
        let tx = store.begin_work_mutation().unwrap();
        SqliteStore::insert_object(&tx, PRODUCING_ORIGIN_KIND, &object).unwrap();
        crate::storage::work::feeds::append_to_project_feed(
            &tx,
            &second.project_id,
            PRODUCING_ORIGIN_KIND,
            &object,
        )
        .unwrap();
        if defect == "cycle" {
            let changed = CanonicalObject::identified(
                &first,
                &Enrollment {
                    predecessor_origin_id: Some(second_id),
                    ..original
                },
            )
            .unwrap();
            tx.execute(
                "UPDATE objects SET canonical_json=?1 WHERE object_id=?2",
                rusqlite::params![changed.bytes(), first.as_str()],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        let before = state(&store);
        let tx = store.begin_work_mutation().unwrap();
        let findings = rebuild(&tx).unwrap();
        assert_eq!(findings.len(), 1, "{defect}: {findings:?}");
        assert!(findings[0].starts_with("task_sync_producing_origin:"));
        let mut invalid = Vec::new();
        verify(&tx, &mut 0, &mut invalid).unwrap();
        assert_eq!(invalid, findings);
        drop(tx);
        assert_eq!(state(&store), before);
    }
}
