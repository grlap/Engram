use super::*;
use crate::{
    ChildRequirement, ProjectId, WorkId, WorkItemKind, WorkLifecycle,
    task_sync::{FORMAT, ProjectDocument, TaskDocument},
};

fn candidate() -> TaskTree {
    TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: ProjectId("directory-test".into()),
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
        }],
    }
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
    assert!(
        read(&path)
            .unwrap_err()
            .to_string()
            .contains("tasks directory missing")
    );
    fs::create_dir(path.join("tasks")).unwrap();
    fs::write(path.join("unexpected"), b"not ignored").unwrap();
    assert!(read(&path).is_err());
    fs::remove_file(path.join("unexpected")).unwrap();
    let task_path = path.join(format!("tasks/{}.json", tree.tasks[0].id.0));
    fs::write(
        &task_path,
        vec![b' '; usize::try_from(MAX_FILE_BYTES).unwrap() + 1],
    )
    .unwrap();
    assert!(read(&path).is_err());
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
    assert!(write(&path, &candidate()).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"owned existing file");
    assert!(write(&fixture.path().join("missing/candidate"), &candidate()).is_err());
    assert!(!fixture.path().join("missing").exists());
}
