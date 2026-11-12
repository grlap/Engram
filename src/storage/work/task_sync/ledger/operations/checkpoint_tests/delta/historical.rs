use super::*;
use crate::task_sync::delta::validation_probe;

fn snapshot(store: &SqliteStore) -> BTreeMap<String, Vec<String>> {
    let tables = store
        .connection
        .prepare(
            "SELECT name FROM sqlite_master WHERE type='table'
         AND (name='objects' OR name LIKE 'work_task_sync_%') ORDER BY name",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    tables
        .into_iter()
        .map(|table| {
            let mut statement = store
                .connection
                .prepare(&format!("SELECT * FROM \"{table}\""))
                .unwrap();
            let columns = statement.column_count();
            let mut rows = statement
                .query_map([], |row| {
                    (0..columns)
                        .map(|index| row.get::<_, rusqlite::types::Value>(index))
                        .collect::<Result<Vec<_>, _>>()
                })
                .unwrap()
                .map(|row| format!("{:?}", row.unwrap()))
                .collect::<Vec<_>>();
            rows.sort();
            (table, rows)
        })
        .collect()
}

pub(super) fn assert_roundtrip(mut store: SqliteStore, binding: &Binding) -> SqliteStore {
    let before = snapshot(&store);
    let tasks = store.export_task_tree(&binding.project).unwrap();
    let admissions = validation_probe::admissions();
    Ledger::load(&store.connection).unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
    unrelated(&mut store, &binding.project);
    assert_eq!(validation_probe::admissions(), admissions);
    // The ordinary writer adds its own records. Repair must preserve all of
    // them as well as the original exchange rows and derived projections.
    let after_writer = snapshot(&store);
    assert!(
        before["objects"]
            .iter()
            .all(|row| after_writer["objects"].contains(row))
    );
    drop(store);
    let report = SqliteStore::repair_rebuildable_projections(&binding.store.database).unwrap();
    assert!(report.is_healthy(), "{report:?}");
    let store = SqliteStore::open(&binding.store.database).unwrap();
    Ledger::load(&store.connection).unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
    assert_eq!(validation_probe::admissions(), admissions);
    assert_eq!(snapshot(&store), after_writer);
    let actual = store.export_task_tree(&binding.project).unwrap();
    for original in tasks.tasks {
        assert!(actual.tasks.contains(&original));
    }
    store
}

#[test]
fn admitted_delta_history_reloads_and_rebuilds_without_repeating_payload_admission() {
    let mut f = Fixture::new();
    let applied = f.apply().unwrap();
    let original: Event =
        SqliteStore::get_typed_object_on(&f.store.connection, &applied.prepared, EVENT_KIND)
            .unwrap()
            .unwrap();
    for fault in ["binding", "project", "keys", "task"] {
        let mut changed = original.clone();
        let Transition::DeltaPrepared { payload, .. } = &mut changed.transition else {
            unreachable!()
        };
        match fault {
            "binding" => changed.binding = ObjectId::mint(),
            "project" => payload.project_id = crate::ProjectId("other-project".into()),
            "keys" => payload.documents.clear(),
            "task" => {
                payload.documents.values_mut().flatten().next().unwrap().id = crate::WorkId::new();
            }
            _ => unreachable!(),
        }
        f.store
            .connection
            .execute_batch("SAVEPOINT historical_fault")
            .unwrap();
        f.store
            .connection
            .execute(
                "UPDATE objects SET canonical_json=?1 WHERE object_id=?2",
                rusqlite::params![
                    crate::canonical::canonical_bytes(&changed).unwrap(),
                    applied.prepared.as_str()
                ],
            )
            .unwrap();
        assert!(Ledger::load(&f.store.connection).is_err(), "{fault}");
        f.store
            .connection
            .execute_batch("ROLLBACK TO historical_fault; RELEASE historical_fault")
            .unwrap();
    }
    let Fixture {
        store,
        input,
        selected,
        home,
    } = f;
    let mut f = Fixture {
        store: assert_roundtrip(store, &input.binding),
        input,
        selected,
        home,
    };
    assert_eq!(f.apply().unwrap(), applied);
}

#[test]
fn fresh_delta_still_refuses_changed_bytes_and_invalid_documents_atomically() {
    for invalid_document in [false, true] {
        let mut f = Fixture::new();
        let task = f
            .input
            .payload
            .documents
            .get_mut(&f.selected)
            .unwrap()
            .as_mut()
            .unwrap();
        task.title = if invalid_document {
            String::new()
        } else {
            "changed selected bytes".into()
        };
        if invalid_document {
            f.input.intent.candidate.content_fingerprint =
                CanonicalObject::freeze(&f.input.payload)
                    .unwrap()
                    .key()
                    .clone();
        }
        let before = state(&f.store);
        assert!(f.apply().is_err());
        assert_eq!(state(&f.store), before);
    }
}
