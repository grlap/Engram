use super::*;
use crate::{
    ObjectId, ProjectId,
    task_sync::{
        self,
        memories::{self, Head, ImportProvenance, KeyHistory, Revision, Source},
    },
};
use std::collections::BTreeMap;

fn head_bytes(history: &KeyHistory) -> Vec<u8> {
    #[derive(serde::Serialize)]
    struct HeadFile<'a> {
        format: &'a str,
        project_id: &'a ProjectId,
        key: &'a str,
        head: &'a Head,
    }
    serde_json::to_vec_pretty(&HeadFile {
        format: memories::FORMAT,
        project_id: &history.project_id,
        key: &history.key,
        head: &history.head,
    })
    .unwrap()
}

fn history(body: &str, previous: Option<ObjectId>) -> KeyHistory {
    let id = ObjectId::mint();
    KeyHistory {
        format: memories::FORMAT.into(),
        project_id: ProjectId("data/fixture".into()),
        key: "rule".into(),
        head: Head::Active {
            chain: vec![id.clone()],
        },
        revisions: BTreeMap::from([(
            id.clone(),
            Revision {
                provenance: ImportProvenance {
                    format: memories::FORMAT.into(),
                    project_id: ProjectId("data/fixture".into()),
                    key: "rule".into(),
                    revision_id: id,
                    previous,
                    body_sha256: memories::body_sha256(body),
                    retiring_target: None,
                    source_clear: false,
                    source: Source {
                        actor_id: "fixture-author".into(),
                        actor_kind: "agent".into(),
                        actor_context: None,
                        reason: "fixture".into(),
                        remembered_at: chrono::Utc::now(),
                    },
                },
                body: body.into(),
            },
        )]),
    }
}

fn fixture() -> (Fixture, DataWorktree) {
    let fixture = Fixture::new();
    let data = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data").unwrap();
    let tree = task_sync::TaskTree {
        project: task_sync::ProjectDocument {
            format: task_sync::FORMAT.into(),
            project_id: ProjectId("data/fixture".into()),
            notes_included: false,
        },
        tasks: Vec::new(),
    };
    task_sync::directory::write_data_folder(
        &data,
        &tree,
        &task_sync::progress::ExportProfile {
            include_notes: false,
            origin_machine: None,
        },
    )
    .unwrap();
    task_sync::directory::enable_memory_profile(&data).unwrap();
    (fixture, data)
}

fn append(first: &KeyHistory, body: &str) -> KeyHistory {
    let Head::Active { chain } = &first.head else {
        unreachable!()
    };
    let added = history(body, chain.last().cloned());
    let mut result = first.clone();
    let Head::Active { chain } = &mut result.head else {
        unreachable!()
    };
    let Head::Active { chain: next } = &added.head else {
        unreachable!()
    };
    chain.extend(next.clone());
    result.revisions.extend(added.revisions);
    result
}

#[test]
fn memory_sync_filesystem_roundtrip_requires_profile_and_preserves_code_checkout() {
    let (fixture, data) = fixture();
    let before = fixture.code_snapshot();
    let first = history("original", None);
    let selected = BTreeMap::from([("rule".into(), first.clone())]);
    memories::directory::write(&data, &BTreeMap::new(), &selected).unwrap();
    assert_eq!(memories::directory::read(&data).unwrap(), selected);
    assert!(task_sync::directory::read_resolved_data_folder(&data).is_err());
    task_sync::directory::read_resolved_data_folder_with_memories(&data).unwrap();
    let next = BTreeMap::from([("rule".into(), append(&first, "later"))]);
    memories::directory::write(&data, &selected, &next).unwrap();
    assert_eq!(memories::directory::read(&data).unwrap(), next);
    assert_eq!(fixture.code_snapshot(), before);
    let head_path = data.checkout.join(".engram/memories/rule/head.json");
    let bytes = fs::read(&head_path).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    value["project_id"] = "foreign-project".into();
    fs::write(&head_path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(memories::directory::read(&data).is_err());
    fs::write(&head_path, bytes).unwrap();
    let project = data.checkout.join(".engram/project.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&project).unwrap()).unwrap();
    value["memory_profile"] = "unsupported-memory-profile".into();
    fs::write(&project, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(memories::directory::read(&data).is_err());
    assert!(task_sync::directory::read_resolved_data_folder_with_memories(&data).is_err());
}

#[test]
fn memory_sync_recovery_handles_partial_revision_pending_head_and_post_head_cleanup() {
    for phase in 0..5 {
        let (_fixture, data) = fixture();
        let first = history("first", None);
        let basis = BTreeMap::from([(
            "rule".into(),
            if phase == 4 {
                append(&first, "divergent branch")
            } else {
                first.clone()
            },
        )]);
        memories::directory::write(&data, &BTreeMap::new(), &basis).unwrap();
        let next = append(&first, "second");
        let selected = BTreeMap::from([("rule".into(), next.clone())]);
        let directory = data.checkout.join(".engram/memories/rule");
        let Head::Active { chain } = &next.head else {
            unreachable!()
        };
        let id = chain.last().unwrap();
        let bytes = serde_json::to_vec_pretty(&next.revisions[id]).unwrap();
        let head = head_bytes(&next);
        match phase {
            0 => fs::write(
                directory.join(format!("{id}.json")),
                &bytes[..bytes.len() / 2],
            )
            .unwrap(),
            1 => fs::write(
                directory.join(format!("revision-{id}.pending")),
                &bytes[..bytes.len() / 2],
            )
            .unwrap(),
            2 => {
                fs::write(directory.join(format!("{id}.json")), &bytes).unwrap();
                fs::write(
                    directory.join(format!("head-{}.pending", uuid::Uuid::new_v4())),
                    &head[..head.len() / 2],
                )
                .unwrap();
            }
            _ => {
                fs::write(directory.join(format!("{id}.json")), &bytes).unwrap();
                fs::write(directory.join("head.json"), &head).unwrap();
            }
        }
        memories::directory::recover(&data, &basis, &selected).unwrap();
        assert_eq!(memories::directory::read(&data).unwrap(), selected);
        memories::directory::recover(&data, &basis, &selected).unwrap();
        assert_eq!(memories::directory::read(&data).unwrap(), selected);
    }
}

#[test]
fn memory_sync_filesystem_refuses_oversized_head_before_publication() {
    let (_fixture, data) = fixture();
    let first = history("first", None);
    let selected = BTreeMap::from([("rule".into(), first)]);
    memories::directory::write(&data, &BTreeMap::new(), &selected).unwrap();
    let path = data.checkout.join(".engram/memories/rule/head.json");
    let oversized = vec![b' '; 1024 * 1024 + 1];
    fs::write(&path, &oversized).unwrap();
    assert!(memories::directory::read(&data).is_err());
    assert!(memories::directory::recover(&data, &selected, &selected).is_err());
    assert_eq!(fs::metadata(path).unwrap().len(), oversized.len() as u64);
}

#[test]
fn memory_sync_cold_git_snapshot_requires_memory_capability() {
    let (fixture, data) = fixture();
    let selected = BTreeMap::from([("rule".into(), history("first", None))]);
    memories::directory::write(&data, &BTreeMap::new(), &selected).unwrap();
    run(&fixture.data, &["add", ".engram"]);
    run(
        &fixture.data,
        &["commit", "-m", "Record memory profile fixture"],
    );
    let commit = String::from_utf8(run(&fixture.data, &["rev-parse", "HEAD"])).unwrap();
    task_sync::git::read_task_snapshot(
        &fixture.data,
        &ProjectId("data/fixture".into()),
        commit.trim(),
    )
    .unwrap();
    let path = fixture.data.join(".engram/project.json");
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value.as_object_mut().unwrap().remove("memory_profile");
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    run(&fixture.data, &["add", ".engram/project.json"]);
    run(
        &fixture.data,
        &["commit", "-m", "Omit memory capability fixture"],
    );
    let commit = String::from_utf8(run(&fixture.data, &["rev-parse", "HEAD"])).unwrap();
    assert!(
        task_sync::git::read_task_snapshot(
            &fixture.data,
            &ProjectId("data/fixture".into()),
            commit.trim()
        )
        .is_err()
    );
}

#[test]
fn memory_sync_recovery_refuses_unknown_bytes_stale_heads_and_immutable_rewrites_without_changes() {
    let (_fixture, data) = fixture();
    let first = history("first", None);
    let basis = BTreeMap::from([("rule".into(), first.clone())]);
    memories::directory::write(&data, &BTreeMap::new(), &basis).unwrap();
    let selected = BTreeMap::from([("rule".into(), append(&first, "second"))]);
    let dir = data.checkout.join(".engram/memories/rule");
    let foreign = dir.join("unrelated.txt");
    fs::write(&foreign, "preserve this").unwrap();
    assert!(memories::directory::recover(&data, &basis, &selected).is_err());
    assert_eq!(fs::read_to_string(&foreign).unwrap(), "preserve this");
    fs::remove_file(&foreign).unwrap();
    let id = first.revisions.keys().next().unwrap();
    let file = dir.join(format!("{id}.json"));
    let original = fs::read(&file).unwrap();
    fs::write(&file, "torn old immutable file").unwrap();
    assert!(memories::directory::recover(&data, &basis, &selected).is_err());
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "torn old immutable file"
    );
    fs::write(file, original).unwrap();
    let unrelated = head_bytes(&history("unrelated selected head", None));
    fs::write(dir.join("head.json"), &unrelated).unwrap();
    assert!(memories::directory::recover(&data, &basis, &selected).is_err());
    assert_eq!(fs::read(dir.join("head.json")).unwrap(), unrelated);
}

#[cfg(unix)]
#[test]
fn memory_sync_filesystem_refuses_symlinked_revision_files() {
    let (fixture, data) = fixture();
    let first = history("first", None);
    let selected = BTreeMap::from([("rule".into(), first.clone())]);
    memories::directory::write(&data, &BTreeMap::new(), &selected).unwrap();
    let outside = fixture.home.path().join("unrelated.json");
    fs::write(&outside, "preserve this").unwrap();
    let id = first.revisions.keys().next().unwrap();
    let path = data
        .checkout
        .join(format!(".engram/memories/rule/{id}.json"));
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&outside, &path).unwrap();
    assert!(memories::directory::read(&data).is_err());
    assert!(memories::directory::recover(&data, &selected, &selected).is_err());
    assert_eq!(fs::read_to_string(outside).unwrap(), "preserve this");
}
