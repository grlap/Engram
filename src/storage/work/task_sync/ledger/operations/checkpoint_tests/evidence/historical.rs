use super::*;
use crate::storage::work::task_sync::evidence::{
    load_retained_pack_on, retained_pack_load_probe as probe,
};

type Originals = BTreeMap<ObjectId, (String, Vec<u8>)>;

struct History {
    store: SqliteStore,
    _home: crate::test_support::TempHome,
    binding: Binding,
    id: ObjectId,
    tree: crate::task_sync::TaskTree,
    inputs: Vec<InspectedEvidence>,
}

impl History {
    fn new() -> Self {
        let (home, mut store, binding, tree) = initial_fixture();
        let id = store.activate_task_sync_binding(&binding).unwrap();
        let mut inputs = Vec::new();
        for _ in 0..2 {
            let input = retained_input(&mut store, &binding, &id);
            store.apply_task_sync_evidence_import(&input).unwrap();
            assert_ne!(
                Some(input.packs[0].publishing_origin()),
                binding.store.origin.as_ref()
            );
            inputs.push(input);
        }
        Self {
            _home: home,
            store,
            binding,
            id,
            tree,
            inputs,
        }
    }

    fn originals(&self) -> Originals {
        let mut ids = Vec::new();
        for input in &self.inputs {
            ids.extend([
                input.recipe.prepared_id.clone(),
                input.recipe.outcome_id.clone(),
            ]);
            for (pack, selected) in &input.recipe.intent().unwrap().packs {
                ids.extend([pack.clone(), selected.admission.clone()]);
            }
            ids.extend(
                input
                    .packs
                    .iter()
                    .flat_map(EvidencePack::records)
                    .map(|record| record.object_id().clone()),
            );
        }
        ids.into_iter()
            .map(|id| {
                let original = self
                    .store
                    .connection
                    .query_row(
                        "SELECT object_kind,canonical_json FROM main.objects WHERE object_id=?1",
                        [id.as_str()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .unwrap();
                (id, original)
            })
            .collect()
    }
}

#[test]
fn historical_evidence_load_and_ordinary_writer_never_reload_pack_bodies() {
    let mut history = History::new();
    let before = history.originals();
    let loads = probe::loads();
    Ledger::load(&history.store.connection).unwrap();
    let next = super::super::super::tests::input(
        &history.store,
        &history.id,
        &history.binding,
        history.tree.clone(),
    );
    history
        .store
        .prepare_task_sync_exchange(&next, at(80))
        .unwrap();
    assert_eq!(probe::loads(), loads);
    assert_eq!(history.originals(), before);
}

#[test]
fn historical_metadata_load_survives_unreadable_body_but_export_and_doctor_refuse() {
    let history = History::new();
    let before = history.originals();
    let cursor = history
        .store
        .capture_evidence_packs(&history.binding.project)
        .unwrap()
        .unwrap();
    let pack = &history.inputs[0].packs[0];
    let original = pack.records()[0].object_id();
    // Keep the real immutable rows intact. A temporary copy supports the
    // ledger's explicit index hints while exposing an undecodable typed body.
    let indexes = history
        .store
        .connection
        .prepare("SELECT name FROM main.sqlite_master WHERE type='index' AND tbl_name='objects' AND sql IS NOT NULL")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    history
        .store
        .connection
        .execute_batch("CREATE TEMP TABLE objects AS SELECT * FROM main.objects")
        .unwrap();
    for index in indexes {
        let index = index.replace('"', "\"\"");
        history
            .store
            .connection
            .execute_batch(&format!(
                "CREATE INDEX temp.\"{index}\" ON objects(object_id)"
            ))
            .unwrap();
    }
    history
        .store
        .connection
        .execute(
            "UPDATE temp.objects SET canonical_json=?1 WHERE object_id=?2",
            rusqlite::params![b"null".as_slice(), original.as_str()],
        )
        .unwrap();
    let loads = probe::loads();
    Ledger::load(&history.store.connection).unwrap();
    assert_eq!(probe::loads(), loads);
    assert!(
        load_retained_pack_on(
            &history.store.connection,
            &history.binding.project,
            pack.pack_id()
        )
        .is_err()
    );
    assert!(history.store.export_evidence_packs(&cursor).is_err());
    if let Ok(report) = history.store.verify_all() {
        assert!(!report.is_healthy());
    }
    history
        .store
        .connection
        .execute_batch("DROP TABLE temp.objects")
        .unwrap();
    assert_eq!(history.originals(), before);
    assert!(history.store.verify_all().unwrap().is_healthy());
}

#[test]
fn historical_evidence_links_survive_projection_repair_without_changing_receipts() {
    let history = History::new();
    let before = history.originals();
    let History {
        _home: home,
        store,
        binding,
        id,
        tree,
        inputs,
    } = history;
    drop(store);
    let report = SqliteStore::repair_rebuildable_projections(&binding.store.database).unwrap();
    assert!(report.is_healthy(), "{report:?}");
    let history = History {
        store: SqliteStore::open(&binding.store.database).unwrap(),
        _home: home,
        binding,
        id,
        tree,
        inputs,
    };
    let loads = probe::loads();
    Ledger::load(&history.store.connection).unwrap();
    assert_eq!(probe::loads(), loads);
    assert_eq!(history.originals(), before);
    assert!(history.store.verify_all().unwrap().is_healthy());
}

#[test]
fn historical_evidence_requires_exact_canonical_manifest_and_admission_links() {
    let history = History::new();
    let before = history.originals();
    let intent = history.inputs[0].recipe.intent().unwrap();
    let (pack, selected) = intent.packs.first_key_value().unwrap();
    history
        .store
        .connection
        .execute_batch("PRAGMA foreign_keys=OFF")
        .unwrap();
    for fault in [
        "manifest-missing",
        "manifest-kind",
        "manifest-project",
        "manifest-origin",
        "manifest-members",
        "admission-missing",
        "admission-kind",
        "admission-pack",
        "admission-project",
    ] {
        history
            .store
            .connection
            .execute_batch("SAVEPOINT fault")
            .unwrap();
        let id = if fault.starts_with("manifest-") {
            pack
        } else {
            &selected.admission
        };
        if fault.ends_with("-missing") {
            history
                .store
                .connection
                .execute("DELETE FROM objects WHERE object_id=?1", [id.as_str()])
                .unwrap();
        } else if fault.ends_with("-kind") {
            history
                .store
                .connection
                .execute(
                    "UPDATE objects SET object_kind='wrong_receipt_kind' WHERE object_id=?1",
                    [id.as_str()],
                )
                .unwrap();
        } else {
            let mut body: serde_json::Value = serde_json::from_slice(&before[id].1).unwrap();
            match fault {
                "manifest-project" | "admission-project" => {
                    body["project_id"] = "another-project".into();
                }
                "manifest-origin" => {
                    body["publishing_origin"] = ObjectId::mint().to_string().into();
                }
                "manifest-members" => body["members"] = serde_json::json!([]),
                "admission-pack" => body["pack_id"] = ObjectId::mint().to_string().into(),
                _ => unreachable!(),
            }
            history
                .store
                .connection
                .execute(
                    "UPDATE objects SET canonical_json=?1 WHERE object_id=?2",
                    rusqlite::params![
                        crate::canonical::canonical_bytes(&body).unwrap(),
                        id.as_str()
                    ],
                )
                .unwrap();
        }
        assert!(Ledger::load(&history.store.connection).is_err(), "{fault}");
        history
            .store
            .connection
            .execute_batch("ROLLBACK TO fault; RELEASE fault")
            .unwrap();
        assert_eq!(history.originals(), before, "{fault}");
        Ledger::load(&history.store.connection).unwrap();
    }
}

#[test]
fn fresh_evidence_admission_still_refuses_a_changed_same_size_payload() {
    let (_home, mut store, binding, _) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let mut input = retained_input(&mut store, &binding, &id);
    let original = &input.packs[0];
    let record = &original.records()[0];
    let mut enrollment: crate::task_sync::origin::Enrollment =
        serde_json::from_slice(record.canonical_bytes()).unwrap();
    enrollment.enrolled_at = at(3);
    let changed = RecordEnvelope::from_canonical(
        record.origin().clone(),
        record.object_id().clone(),
        record.kind().as_str(),
        crate::canonical::canonical_bytes(&enrollment).unwrap(),
    )
    .unwrap();
    let changed = EvidencePack::prepare(
        original.pack_id().clone(),
        original.project_id().clone(),
        original.publishing_origin().clone(),
        vec![changed],
    )
    .unwrap();
    assert_eq!(
        changed.to_bytes().unwrap().len(),
        original.to_bytes().unwrap().len()
    );
    assert_ne!(changed.to_bytes().unwrap(), original.to_bytes().unwrap());
    input.packs[0] = changed;
    let before = state(&store);
    let error = store.apply_task_sync_evidence_import(&input).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("selected evidence pack content differs"),
        "{error}"
    );
    assert_eq!(state(&store), before);
}
