use super::*;
use crate::task_sync::evidence::{EvidencePack, RecordEnvelope, RecordKind};
use crate::task_sync::origin::Enrollment;
use chrono::{TimeZone, Utc};

const BODY: &str = "Revised project knowledge\nRetained across projection repair";

struct Fixture {
    _directory: crate::test_support::TempHome,
    database: std::path::PathBuf,
    store: SqliteStore,
    native: ObjectId,
    foreign: ObjectId,
    admission: ObjectId,
}

fn fixture() -> Fixture {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("engram.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let project = ProjectId("repair-partition".into());
    let at = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut request = RememberProjectMemoryRequest {
        project_id: project.clone(),
        session_id: SessionId("repair-agent".into()),
        key: Some("repair-knowledge".into()),
        body: "Original project knowledge".into(),
        actor: actor("repair-agent"),
        created_at: at,
        revise: false,
        expected_revision: None,
        retiring_target: crate::domain::ProjectMemoryRetiringTargetChange::Keep,
    };
    store
        .remember_project_memory(&request, &DevelopmentNoopRedactor)
        .unwrap();
    request.revise = true;
    request.expected_revision = Some(1);
    request.body = BODY.into();
    request.created_at += chrono::Duration::seconds(1);
    let revision = store
        .remember_project_memory(&request, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!(revision.revision, 2);
    let native: String = store
        .connection
        .query_row("SELECT version_id FROM memory_heads", [], |row| row.get(0))
        .unwrap();
    let origin = ObjectId::mint();
    let foreign = ObjectId::mint();
    let enrollment = Enrollment {
        project_id: project.clone(),
        predecessor_origin_id: None,
        enrollment_operation_id: uuid::Uuid::new_v4(),
        actor: actor("foreign-agent"),
        enrolled_at: at,
    };
    let record = |id: &ObjectId, kind: RecordKind, value: &serde_json::Value| {
        RecordEnvelope::from_canonical(
            origin.clone(),
            id.clone(),
            kind.as_str(),
            serde_json_canonicalizer::to_vec(value).unwrap(),
        )
        .unwrap()
    };
    let pack = EvidencePack::prepare(
        ObjectId::mint(),
        project.clone(),
        origin.clone(),
        vec![
            record(
                &origin,
                RecordKind::ProducingOrigin,
                &serde_json::to_value(enrollment).unwrap(),
            ),
            record(
                &foreign,
                RecordKind::WorkEvidence,
                &serde_json::json!({
                    "schema_version": crate::schema::SCHEMA_VERSION,
                    "work_id": uuid::Uuid::new_v4(), "run_id": uuid::Uuid::new_v4(),
                    "claim_id": uuid::Uuid::new_v4(), "claim_fence": 1,
                    "summary": "Foreign observation without native execution authority",
                    "refs": [], "actor": actor("foreign-agent"), "created_at": at
                }),
            ),
        ],
    )
    .unwrap();
    let retained = store
        .retain_evidence_pack(&project, &pack, &ObjectId::mint())
        .unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
    Fixture {
        _directory: directory,
        database,
        store,
        native: ObjectId::from_stored(native).unwrap(),
        foreign,
        admission: retained.admission,
    }
}

#[derive(Clone, Copy, Debug)]
enum Damage {
    Missing,
    Obsolete,
    Forged,
}

fn damage(f: &Fixture, kind: Damage) {
    match kind {
        Damage::Missing => f
            .store
            .connection
            .execute_batch("DROP TABLE work_task_sync_evidence_imported;")
            .unwrap(),
        Damage::Obsolete => f
            .store
            .connection
            .execute_batch(
                "DROP TABLE work_task_sync_evidence_imported;
                 CREATE TABLE work_task_sync_evidence_imported (object_id TEXT PRIMARY KEY) STRICT;",
            )
            .unwrap(),
        Damage::Forged => {
            f.store
                .connection
                .execute(
                    "INSERT INTO work_task_sync_evidence_imported(object_id,admission_id) VALUES (?1,?2)",
                    rusqlite::params![f.native.as_str(), f.admission.as_str()],
                )
                .unwrap();
        }
    }
}

#[test]
fn projection_repair_restores_evidence_partition_before_native_memory_reads() {
    for kind in [Damage::Missing, Damage::Obsolete, Damage::Forged] {
        let f = fixture();
        let before = test_database_shape_snapshot(&f.store.connection).unwrap();
        damage(&f, kind);
        let Fixture {
            _directory,
            database,
            store,
            native,
            foreign,
            ..
        } = f;
        drop(store);
        let report = SqliteStore::repair_rebuildable_projections(&database)
            .unwrap_or_else(|error| panic!("repair {kind:?}: {error}"));
        assert!(report.is_healthy(), "{kind:?}: {report:?}");
        SqliteStore::readiness(&database, None).unwrap();
        let store = SqliteStore::open(&database).unwrap();
        let memory = store
            .project_memory_full(
                &ProjectId("repair-partition".into()),
                &SessionId("repair-agent".into()),
                &actor("repair-agent"),
                "repair-knowledge",
                None,
            )
            .unwrap();
        assert_eq!(memory.body, BODY);
        assert_eq!(memory.revision, 2);
        let after = test_database_shape_snapshot(&store.connection).unwrap();
        for table in ["objects", "memory_heads"] {
            assert_eq!(
                before.rows.iter().find(|(name, _)| name == table),
                after.rows.iter().find(|(name, _)| name == table),
                "repair changed durable {table} for {kind:?}"
            );
        }
        crate::storage::work::task_sync::evidence::require_native(&store.connection, &native)
            .unwrap();
        assert!(
            crate::storage::work::task_sync::evidence::require_native(&store.connection, &foreign)
                .is_err()
        );
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn projection_repair_rolls_back_partition_rebuild_for_invalid_durable_memory() {
    let f = fixture();
    damage(&f, Damage::Missing);
    f.store
        .connection
        .execute("UPDATE memory_heads SET title='invalid durable title'", [])
        .unwrap();
    let before = test_database_shape_snapshot(&f.store.connection).unwrap();
    let Fixture {
        _directory,
        database,
        store,
        ..
    } = f;
    drop(store);
    let error = SqliteStore::repair_rebuildable_projections(&database).unwrap_err();
    assert!(
        matches!(error, StoreError::InvalidMemoryProjection(ref message)
            if message.contains("durable memory heads are invalid")),
        "{error}"
    );
    let connection = Connection::open(&database).unwrap();
    assert_eq!(test_database_shape_snapshot(&connection).unwrap(), before);
}
