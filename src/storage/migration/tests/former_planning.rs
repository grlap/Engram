//! Whole-store import names unsupported complete inventories without rewriting them.

use super::*;
use crate::storage::TaskSyncFolderApply;
use crate::task_sync::progress::{ExportProfile, documents_fingerprint};

#[test]
fn former_planning_refusal_counts_each_blob_encoding_without_publishing() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let file = directory.path().join("former.jsonl");
    let target = directory.path().join("current.db");
    populated(&source);
    {
        let store = SqliteStore::open_unresolved(&source).unwrap();
        let project = ProjectId("project-json-transfer".into());
        let mut tree = store.export_task_tree(&project).unwrap();
        tree.tasks[0].title = "private-planning-content".into();
        let fingerprint = documents_fingerprint(&tree.folder_documents().unwrap());
        let body = serde_json::json!({
            "project_id": project,
            "tree": tree,
            "inventory_fingerprint": fingerprint,
            "export_basis": fingerprint,
            "profile": {"include_notes": false, "origin_machine": null},
            "created_at": Utc::now()
        });
        // Only format recognition is under test: keep each original identity,
        // and let the importer refuse before repairing or admitting any record.
        for index in 0..3 {
            let mut body = body.clone();
            if index == 2 {
                body["project_id"] = Json::String("other-project".into());
                body["tree"]["project"]["project_id"] = Json::String("other-project".into());
            }
            let object = crate::CanonicalObject::mint(&body).unwrap();
            SqliteStore::insert_object(&store.connection, "task_sync_import_planning", &object)
                .unwrap();
        }
    }
    let source_before = rows(&source);
    export_json(&source, &file).unwrap();
    let mut document: Vec<Json> = fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    document[0]["engram_export"]["tables"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|table| table["name"] == "objects")
        .unwrap()["columns"]
        .as_array_mut()
        .unwrap()
        .reverse();
    let mut found = 0;
    for line in &mut document {
        if line["row"]["table"] != "objects"
            || line["row"]["values"]["object_kind"] != "task_sync_import_planning"
        {
            continue;
        }
        let cell = &mut line["row"]["values"]["canonical_json"];
        let SqlValue::Blob(bytes) = decode(cell.clone()).unwrap() else {
            unreachable!()
        };
        if found == 1 {
            *cell = serde_json::json!({"text": String::from_utf8(bytes).unwrap()});
        } else if found == 2 {
            use std::fmt::Write as _;
            let mut hex = String::new();
            for byte in bytes {
                write!(&mut hex, "{byte:02x}").unwrap();
            }
            *cell = serde_json::json!({"hex": hex});
        }
        found += 1;
    }
    assert_eq!(found, 3);
    fs::write(
        &file,
        document
            .iter()
            .map(Json::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
    let input_before = fs::read(&file).unwrap();
    let error = import_json(&file, &target).unwrap_err().to_string();
    assert!(
        error.contains("object kind task_sync_import_planning: 3 former complete-tree records"),
        "{error}"
    );
    assert!(
        error.contains("\"other-project\":1") && error.contains("\"project-json-transfer\":2"),
        "{error}"
    );
    assert!(!error.contains("private-planning-content"), "{error}");
    assert!(!target.exists(), "published an unsupported planning format");
    assert_eq!(staging_leftovers(directory.path()), 0);
    assert_eq!(fs::read(&file).unwrap(), input_before);
    assert_eq!(rows(&source), source_before);
}

#[test]
fn current_planning_delta_import_preserves_its_id_and_canonical_bytes() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let file = directory.path().join("current.jsonl");
    let target = directory.path().join("current.db");
    populated(&source);
    {
        let mut store = SqliteStore::open_unresolved(&source).unwrap();
        let project = ProjectId("planning-project".into());
        let profile = ExportProfile {
            include_notes: false,
            origin_machine: None,
        };
        let exported = store.export_task_sync_folder(&project, &profile).unwrap();
        store
            .apply_task_sync_folder(
                TaskSyncFolderApply {
                    project: &project,
                    actor: &actor("planner"),
                    tree: &exported.tree,
                    export_basis: &exported.export_basis,
                    profile: &profile,
                    complete: true,
                    now: Utc::now(),
                },
                "current-planning",
            )
            .unwrap();
        let bytes: Vec<u8> = store
            .connection
            .query_row(
                "SELECT canonical_json FROM objects WHERE object_kind = 'task_sync_import_planning'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let body: Json = serde_json::from_slice(&bytes).unwrap();
        assert!(body.get("tree").is_none());
        assert!(body.get("changes").is_some());
        assert!(store.verify_all().unwrap().is_healthy());
    }
    let before = rows(&source)["objects"].clone();
    export_json(&source, &file).unwrap();
    import_json(&file, &target).unwrap();
    assert_eq!(rows(&target)["objects"], before);
    assert!(
        SqliteStore::open_unresolved(&target)
            .unwrap()
            .verify_all()
            .unwrap()
            .is_healthy()
    );
}
