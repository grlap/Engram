use super::*;
use crate::{
    ChildRequirement, ProjectId, WorkId, WorkItemKind, WorkLifecycle,
    task_sync::{FORMAT, ProjectDocument, TaskDocument},
};

mod parent_precedence;
mod publication;
mod refusals;

fn profile() -> crate::task_sync::progress::ExportProfile {
    crate::task_sync::progress::ExportProfile {
        include_notes: false,
        origin_machine: None,
    }
}

fn candidate() -> TaskTree {
    TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: ProjectId("directory-test".into()),
            notes_included: false,
        },
        tasks: vec![TaskDocument {
            id: WorkId(uuid::Uuid::new_v4()),
            external_ref: None,
            title: "task".into(),
            outcome: "Keep escaped CR\rLF\ncontent".into(),
            acceptance: vec!["preserve bytes".into()],
            acceptance_bindings: Vec::new(),
            evaluation_mode: None,
            kind: WorkItemKind::Task,
            priority: 2,
            labels: Vec::new(),
            assigned_to: None,
            deferred_until: None,
            lifecycle: WorkLifecycle::Open,
            parent_id: None,
            child_requirement: ChildRequirement::Required,
            prerequisites: Vec::new(),
            blockers: Vec::new(),
            superseded_by: None,
            disposal_reason: None,
            completion: None,
            notes: Vec::new(),
            activity: std::collections::BTreeMap::default(),
        }],
    }
}

/// The structured code of a store refusal, through any added context.
fn refusal_code(error: &anyhow::Error) -> serde_json::Value {
    let error = error
        .downcast_ref::<crate::storage::StoreError>()
        .unwrap_or_else(|| panic!("not a store refusal: {error:#}"));
    crate::store_error_value(error)["error"]["code"].clone()
}

#[test]
fn task_sync_directory_preserves_crlf_string_content_and_existing_bytes() {
    let fixture = crate::test_support::temp_home().unwrap();
    let path = fixture.path().join("candidate");
    let tree = candidate();
    write(&path, &tree).unwrap();
    let documents = tree.documents().unwrap();
    for (relative, bytes) in &documents {
        assert_eq!(fs::read(path.join(relative)).unwrap(), *bytes);
        let crlf = std::str::from_utf8(bytes).unwrap().replace('\n', "\r\n");
        fs::write(path.join(relative), crlf).unwrap();
    }
    assert_eq!(read(&path).unwrap(), tree);
    let before = fs::read(path.join("project.json")).unwrap();
    let task_path = path.join(format!("tasks/{}.json", tree.tasks[0].id.0));
    let task_before = fs::read(&task_path).unwrap();
    write(&path, &tree).unwrap();
    assert_eq!(fs::read(path.join("project.json")).unwrap(), before);
    assert_eq!(fs::read(&task_path).unwrap(), task_before);
    let mut divergent = tree;
    divergent.tasks[0].title = "different".into();
    assert!(write(&path, &divergent).is_err());
    assert_eq!(fs::read(&task_path).unwrap(), task_before);
    assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 1);
}

#[test]
fn folder_exchange_writes_a_complete_envelope_without_overwriting() {
    let fixture = crate::test_support::temp_home().unwrap();
    let path = fixture.path().join("candidate");
    let tree = candidate();
    let exported = write_folder(&path, &tree, &profile()).unwrap();
    let read_back = read_folder(&path).unwrap();
    assert_eq!(read_back, exported);
    assert_eq!(read_back.tree, tree);
    assert_eq!(
        read_back.export_basis,
        crate::task_sync::progress::documents_fingerprint(&tree.documents().unwrap())
    );
    let before = fs::read(path.join("project.json")).unwrap();
    write_folder(&path, &tree, &profile()).unwrap();
    assert_eq!(fs::read(path.join("project.json")).unwrap(), before);
    let mut divergent = tree;
    divergent.tasks[0].title = "different".into();
    assert!(write_folder(&path, &divergent, &profile()).is_err());
    assert_eq!(fs::read(path.join("project.json")).unwrap(), before);
}

#[test]
fn folder_exchange_writes_retained_historical_notes_without_reapplying_admission() {
    use crate::task_sync::{
        SharedActor,
        notes::{SharedNote, SharedNoteKind},
    };

    let fixture = crate::test_support::temp_home().unwrap();
    let path = fixture.path().join("candidate");
    let mut tree = candidate();
    tree.project.notes_included = true;
    tree.tasks[0].labels.push("SeCrEt".into());
    tree.tasks[0].notes.push(SharedNote {
        id: crate::ObjectId::mint(),
        kind: SharedNoteKind::Generic,
        body: "root=/private".into(),
        author: SharedActor {
            actor_id: "fixture".into(),
            actor_kind: "agent".into(),
        },
        recorded_at: chrono::Utc::now(),
        origin_machine: "fixture-host".into(),
    });
    let profile = crate::task_sync::progress::ExportProfile {
        include_notes: true,
        origin_machine: Some("fixture-host".into()),
    };

    let exported = write_folder(&path, &tree, &profile).unwrap();
    assert_eq!(exported.tree, tree);
    assert!(read_folder(&path).is_err());
}

#[test]
fn folder_exchange_reads_a_complete_zero_task_envelope_without_a_tasks_directory() {
    let fixture = crate::test_support::temp_home().unwrap();
    let path = fixture.path().join("candidate");
    let mut tree = candidate();
    tree.tasks.clear();
    tree.project.notes_included = true;
    let notes_profile = crate::task_sync::progress::ExportProfile {
        include_notes: true,
        origin_machine: Some("directory-test".into()),
    };
    let exported = write_folder(&path, &tree, &notes_profile).unwrap();
    fs::remove_dir(path.join("tasks")).unwrap();
    assert_eq!(read_folder(&path).unwrap(), exported);
}

#[test]
fn folder_exchange_refuses_missing_or_inconsistent_basis() {
    let fixture = crate::test_support::temp_home().unwrap();
    let path = fixture.path().join("candidate");
    let tree = candidate();
    write_folder(&path, &tree, &profile()).unwrap();
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(path.join("project.json")).unwrap()).unwrap();
    value["folder_exchange"]["complete"] = serde_json::Value::Bool(false);
    fs::write(
        path.join("project.json"),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    assert!(read_folder(&path).is_err());
}

#[test]
fn task_sync_directory_refuses_partial_unknown_and_oversized_input() {
    let fixture = crate::test_support::temp_home().unwrap();
    let path = fixture.path().join("candidate");
    fs::create_dir(&path).unwrap();
    let tree = candidate();
    fs::write(
        path.join("project.json"),
        tree.documents().unwrap()["project.json"].clone(),
    )
    .unwrap();
    let error = read(&path).unwrap_err();
    assert!(error.to_string().contains("tasks directory missing"));
    assert_eq!(refusal_code(&error), "task_sync_destination_missing_tasks");
    fs::create_dir(path.join("tasks")).unwrap();
    fs::write(path.join("unexpected"), b"not ignored").unwrap();
    let error = read(&path).unwrap_err();
    assert_eq!(
        error.to_string(),
        "local work input is invalid: unknown task directory entry"
    );
    assert_eq!(refusal_code(&error), "task_sync_destination_unknown_entry");
    fs::remove_file(path.join("unexpected")).unwrap();
    let task_path = path.join(format!("tasks/{}.json", tree.tasks[0].id.0));
    fs::write(
        &task_path,
        vec![b' '; usize::try_from(MAX_FILE_BYTES).unwrap() + 1],
    )
    .unwrap();
    let error = read(&path).unwrap_err();
    assert_eq!(
        error.root_cause().to_string(),
        "local work input is invalid: task candidate file must be an ordinary bounded file"
    );
    assert_eq!(refusal_code(&error), "task_sync_destination_file_limit");
    fs::write(&task_path, b"<<<<<<< unresolved conflict").unwrap();
    assert!(read(&path).is_err());
    fs::write(
        &task_path,
        tree.documents().unwrap().values().last().unwrap(),
    )
    .unwrap();
    fs::rename(&task_path, path.join("tasks/wrong-name.json")).unwrap();
    assert!(read(&path).is_err());
}

#[test]
fn task_sync_directory_refuses_non_directory_destination_without_overwriting() {
    let fixture = crate::test_support::temp_home().unwrap();
    let path = fixture.path().join("candidate");
    fs::write(&path, b"owned existing file").unwrap();
    let error = write(&path, &candidate()).unwrap_err();
    assert_eq!(refusal_code(&error), "task_sync_destination_not_directory");
    assert_eq!(fs::read(&path).unwrap(), b"owned existing file");
    assert!(write(&fixture.path().join("missing/candidate"), &candidate()).is_err());
    assert!(!fixture.path().join("missing").exists());
}

#[test]
fn task_sync_directory_refuses_a_linked_tasks_directory_and_a_task_that_is_not_a_file() {
    let fixture = crate::test_support::temp_home().unwrap();
    let tree = candidate();
    let documents = tree.documents().unwrap();

    // A tasks directory that is a link is refused without being followed.
    let real = fixture.path().join("real-tasks");
    fs::create_dir(&real).unwrap();
    let linked = fixture.path().join("linked");
    fs::create_dir(&linked).unwrap();
    fs::write(linked.join("project.json"), &documents["project.json"]).unwrap();
    crate::test_support::make_dir_link(&real, &linked.join("tasks"));
    let error = read(&linked).unwrap_err();
    assert_eq!(
        error.to_string(),
        "local work input is invalid: task candidate directory must be an ordinary directory"
    );
    assert_eq!(refusal_code(&error), "task_sync_destination_link");

    // A task entry that is a directory, not a file, is refused.
    let entry = fixture.path().join("entry");
    fs::create_dir_all(entry.join("tasks")).unwrap();
    fs::write(entry.join("project.json"), &documents["project.json"]).unwrap();
    fs::create_dir(entry.join(format!("tasks/{}.json", tree.tasks[0].id.0))).unwrap();
    let error = read(&entry).unwrap_err();
    assert_eq!(
        error.root_cause().to_string(),
        "local work input is invalid: task candidate file must be an ordinary bounded file"
    );
    assert_eq!(refusal_code(&error), "task_sync_destination_not_file");
}

#[test]
fn task_sync_directory_refuses_inline_note_overflow_without_publishing_or_staging() {
    use crate::task_sync::{
        SharedActor,
        notes::{SharedNote, SharedNoteKind},
    };
    let fixture = crate::test_support::temp_home().unwrap();
    let path = fixture.path().join("candidate");
    let mut tree = candidate();
    tree.project.notes_included = true;
    // Each body fits the native note limit; their escaped inline total does not.
    let body = "Progress Ω\r\n".repeat(4000);
    assert!(body.len() < 64 * 1024);
    tree.tasks[0].notes = (0..24)
        .map(|_| SharedNote {
            id: crate::CanonicalObject::mint(&body).unwrap().key().clone(),
            kind: SharedNoteKind::Generic,
            body: body.trim_end().into(),
            author: SharedActor {
                actor_id: "writer".into(),
                actor_kind: "human".into(),
            },
            recorded_at: chrono::DateTime::from_timestamp(1, 0).unwrap(),
            origin_machine: "host-A".into(),
        })
        .collect();
    tree.tasks[0]
        .notes
        .sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    let task_file = format!("tasks/{}.json", tree.tasks[0].id.0);
    assert!(tree.documents().unwrap()[&task_file].len() > usize::try_from(MAX_FILE_BYTES).unwrap());
    let error = write(&path, &tree).unwrap_err();
    assert_eq!(refusal_code(&error), "task_sync_destination_file_limit");
    let error = error.to_string();
    assert!(error.contains("supported task candidate limits"), "{error}");
    assert!(!path.exists());
    assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 0);
}

#[test]
fn task_sync_directory_refuses_empty_notes_profile_before_publication_and_admission() {
    let fixture = crate::test_support::temp_home().unwrap();
    let path = fixture.path().join("candidate");
    let mut tree = candidate();
    tree.tasks.clear();
    tree.project.notes_included = true;
    let error = write(&path, &tree).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("notes_included requires at least one task")
    );
    assert_eq!(refusal_code(&error), "task_sync_invalid_graph");
    assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 0);
    fs::create_dir(&path).unwrap();
    fs::create_dir(path.join("tasks")).unwrap();
    let bytes = super::super::pretty(&tree.project).unwrap();
    fs::write(path.join("project.json"), &bytes).unwrap();
    let error = read(&path).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("notes_included requires at least one task")
    );
    assert_eq!(refusal_code(&error), "task_sync_invalid_graph");
    assert_eq!(fs::read(path.join("project.json")).unwrap(), bytes);
    tree.project.notes_included = false;
    fs::write(
        path.join("project.json"),
        tree.documents().unwrap()["project.json"].clone(),
    )
    .unwrap();
    assert_eq!(read(&path).unwrap(), tree);
}
