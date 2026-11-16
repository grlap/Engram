use super::*;
use crate::{
    ObjectId, ProjectId,
    task_sync::{
        self,
        memories::{self, Head, ImportProvenance, KeyHistory, Revision, Source},
    },
};
use std::{collections::BTreeMap, fmt::Write as _};

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
fn memory_bytes_an_import_counts_are_the_files_on_disk_not_a_reencoding() {
    let (_fixture, data) = fixture();
    let selected = BTreeMap::from([("rule".into(), history("original", None))]);
    memories::directory::write(&data, &BTreeMap::new(), &selected).unwrap();
    // The same documents, padded with valid JSON whitespace.
    let key = data.checkout.join(".engram").join("memories").join("rule");
    let mut on_disk = 0_u64;
    for entry in std::fs::read_dir(&key).unwrap() {
        let path = entry.unwrap().path();
        let padded = [std::fs::read(&path).unwrap().as_slice(), &[b' '; 4096]].concat();
        std::fs::write(&path, &padded).unwrap();
        on_disk += u64::try_from(padded.len()).unwrap();
    }
    let held = memories::directory::publication_lock(&data).unwrap();
    let (read, bytes) = memories::directory::read_locked_with_bytes(&data, &held).unwrap();
    assert_eq!(read, selected, "the padded files hold the same histories");
    assert_eq!(bytes, on_disk);
}

#[test]
fn memory_publication_beside_an_admitted_task_route_reads_no_task_file() {
    let (_fixture, data) = fixture();
    // A task file no reader can decode: only a publisher that reads the
    // task files again meets it.
    let tasks = data.checkout.join(".engram").join("tasks");
    std::fs::create_dir_all(&tasks).unwrap();
    std::fs::write(
        tasks.join(format!("{}.json", uuid::Uuid::new_v4())),
        b"not a task",
    )
    .unwrap();
    let selected = BTreeMap::from([("rule".into(), history("original", None))]);
    let held = memories::directory::publication_lock(&data).unwrap();
    assert!(
        memories::directory::write_locked(&data, &BTreeMap::new(), &selected, false, &held)
            .is_err()
    );
    memories::directory::write_admitted(
        &data,
        &BTreeMap::new(),
        &selected,
        false,
        &held,
        &ProjectId("data/fixture".into()),
        0,
    )
    .unwrap();
    assert_eq!(
        memories::directory::read_locked(&data, &held).unwrap(),
        selected
    );
}

#[test]
fn memory_sync_import_reader_uses_the_publication_fence() {
    let (_fixture, data) = fixture();
    let selected = BTreeMap::from([("rule".into(), history("original", None))]);
    memories::directory::write(&data, &BTreeMap::new(), &selected).unwrap();
    let held = memories::directory::publication_lock(&data).unwrap();
    // An export holding the fence prevents both public import readers from
    // inspecting an in-progress publication; encompassing imports read once
    // under their own fence instead of reacquiring it between components.
    assert!(memories::directory::read(&data).is_err());
    assert!(task_sync::directory::read_resolved_data_folder_with_memories(&data).is_err());
    let folder =
        task_sync::directory::read_resolved_data_folder_with_memories_locked(&data, &held).unwrap();
    assert_eq!(
        folder.tree.project.project_id,
        ProjectId("data/fixture".into())
    );
    assert_eq!(
        memories::directory::read_locked(&data, &held).unwrap(),
        selected
    );
    drop(held);
    assert_eq!(memories::directory::read(&data).unwrap(), selected);
    assert!(task_sync::directory::read_resolved_data_folder_with_memories(&data).is_ok());
}

#[test]
fn memory_sync_task_only_export_refuses_memory_capable_head_before_creating_files() {
    for populated in [false, true] {
        let (fixture, data) = fixture();
        if populated {
            let selected = BTreeMap::from([("rule".into(), history("preserved", None))]);
            memories::directory::write(&data, &BTreeMap::new(), &selected).unwrap();
        }
        let tasks = task_sync::directory::read_resolved_data_folder_with_memories(&data).unwrap();
        run(&fixture.data, &["add", ".engram"]);
        run(
            &fixture.data,
            &["commit", "-m", "Record memory-capable data"],
        );
        let data = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data").unwrap();
        let project_before = fs::read(data.checkout.join(".engram/project.json")).unwrap();
        let error = task_sync::directory::write_data_folder(&data, &tasks.tree, &tasks.profile)
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<crate::storage::StoreError>(),
            Some(crate::storage::StoreError::TaskSyncRefused {
                refusal: task_sync::TaskSyncRefusal::MemoryProfileRequiresOptIn,
                ..
            })
        ));
        assert_eq!(
            fs::read(data.checkout.join(".engram/project.json")).unwrap(),
            project_before
        );
        fs::rename(
            data.checkout.join(".engram"),
            fixture.home.path().join("retained-memory-component"),
        )
        .unwrap();
        assert!(
            task_sync::directory::write_data_folder(&data, &tasks.tree, &tasks.profile).is_err()
        );
        assert!(!data.checkout.join(".engram").exists());
    }
}

#[test]
fn memory_sync_export_refuses_committed_ignore_exclusion_until_explicit_update() {
    let (fixture, data) = fixture();
    let path = data.checkout.join(".engram/.gitignore");
    fs::write(&path, "/*\n!/.gitignore\n!/project.json\n!/tasks/\n").unwrap();
    run(&fixture.data, &["add", ".engram"]);
    run(
        &fixture.data,
        &["commit", "-m", "Record original ignore policy"],
    );
    let data = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data").unwrap();
    let mut value = history("historical", None);
    let (_, mut revision) = value.revisions.pop_first().unwrap();
    let id = crate::ObjectId::from_canonical_bytes(b"historical native object fixture");
    revision.provenance.revision_id = id.clone();
    value.head = Head::Active {
        chain: vec![id.clone()],
    };
    value.revisions.insert(id, revision);
    let selected = BTreeMap::from([("rule".into(), value)]);
    let error = memories::directory::write(&data, &BTreeMap::new(), &selected).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("memory_ignore_policy_excludes_tree")
    );
    assert!(!data.checkout.join(".engram/memories").exists());
    let mut allow = "/*\n!/.gitignore\n!/project.json\n!/tasks/\n!/memories/\n/memories/*\n!/memories/*/\n/memories/*/*\n!/memories/*/head.json\n".to_owned();
    for count in [32, 64] {
        writeln!(allow, "!/memories/*/{}.json", "[0-9a-f]".repeat(count)).unwrap();
    }
    fs::write(&path, allow).unwrap();
    assert!(
        memories::directory::write(&data, &BTreeMap::new(), &selected)
            .unwrap_err()
            .to_string()
            .contains("memory_ignore_policy_uncommitted")
    );
    run(&fixture.data, &["add", ".engram/.gitignore"]);
    run(
        &fixture.data,
        &["commit", "-m", "Admit project memory paths"],
    );
    let data = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data").unwrap();
    memories::directory::write(&data, &BTreeMap::new(), &selected).unwrap();
    assert_eq!(memories::directory::read(&data).unwrap(), selected);
    memories::directory::recover(&data, &selected, &selected).unwrap();
    run(&fixture.data, &["add", ".engram"]);
    run(
        &fixture.data,
        &["commit", "-m", "Record historical memory identity"],
    );
    let data = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data").unwrap();
    assert_eq!(
        memories::directory::committed(&data, &ProjectId("data/fixture".into())).unwrap(),
        selected
    );
}

#[test]
fn memory_sync_empty_head_basis_does_not_block_fresh_task_only_export() {
    let (fixture, data) = fixture();
    let original = task_sync::directory::read_resolved_data_folder_with_memories(&data).unwrap();
    let project_path = data.checkout.join(".engram/project.json");
    let mut envelope: serde_json::Value =
        serde_json::from_slice(&fs::read(&project_path).unwrap()).unwrap();
    envelope.as_object_mut().unwrap().remove("memory_profile");
    fs::write(&project_path, serde_json::to_vec(&envelope).unwrap()).unwrap();
    run(&fixture.data, &["add", ".engram"]);
    run(
        &fixture.data,
        &["commit", "-m", "Record task-only component"],
    );
    let data = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data").unwrap();
    assert!(
        memories::directory::committed(&data, &original.tree.project.project_id)
            .unwrap()
            .is_empty()
    );
    fs::rename(
        data.checkout.join(".engram"),
        fixture.home.path().join("old-component"),
    )
    .unwrap();
    task_sync::directory::write_data_folder(&data, &original.tree, &original.profile).unwrap();
    assert_eq!(
        task_sync::directory::read_resolved_data_folder(&data)
            .unwrap()
            .tree,
        original.tree
    );
}

#[test]
fn memory_sync_committed_memories_refuse_invalid_envelopes_without_losing_peer_keys() {
    for problem in [
        "absent",
        "malformed",
        "incomplete",
        "mode",
        "missing capability",
        "null capability",
        "unsupported capability",
        "foreign",
    ] {
        let (fixture, data) = fixture();
        let original =
            task_sync::directory::read_resolved_data_folder_with_memories(&data).unwrap();
        let selected = BTreeMap::from([("rule".into(), history("peer history", None))]);
        memories::directory::write(&data, &BTreeMap::new(), &selected).unwrap();
        let project_path = data.checkout.join(".engram/project.json");
        let valid = fs::read(&project_path).unwrap();
        match problem {
            "absent" => fs::remove_file(&project_path).unwrap(),
            "malformed" => fs::write(&project_path, "invalid envelope").unwrap(),
            _ => {
                let mut envelope: serde_json::Value = serde_json::from_slice(&valid).unwrap();
                match problem {
                    "incomplete" => envelope["folder_exchange"]["complete"] = false.into(),
                    "missing capability" => {
                        envelope.as_object_mut().unwrap().remove("memory_profile");
                    }
                    "null capability" => envelope["memory_profile"] = serde_json::Value::Null,
                    "unsupported capability" => envelope["memory_profile"] = "unknown".into(),
                    "foreign" => envelope["project"]["project_id"] = "another/project".into(),
                    "mode" => {}
                    _ => unreachable!(),
                }
                fs::write(&project_path, serde_json::to_vec(&envelope).unwrap()).unwrap();
            }
        }
        run(&fixture.data, &["add", ".engram"]);
        if problem == "mode" {
            run(
                &fixture.data,
                &["update-index", "--chmod=+x", ".engram/project.json"],
            );
        }
        run(
            &fixture.data,
            &["commit", "-m", "Record invalid committed envelope"],
        );
        let data = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data").unwrap();
        let saved = fixture.home.path().join("saved-component");
        fs::rename(data.checkout.join(".engram"), &saved).unwrap();
        let error =
            memories::directory::committed(&data, &original.tree.project.project_id).unwrap_err();
        let code = if problem == "foreign" {
            "task_sync_origin_mismatch"
        } else {
            "memory_export_basis_invalid"
        };
        assert_eq!(
            crate::store_error_value(error.downcast_ref::<crate::storage::StoreError>().unwrap())["error"]
                ["code"],
            code,
            "{problem}: {error:#}"
        );
        assert!(!data.checkout.join(".engram").exists());
        if problem != "foreign" {
            let error =
                task_sync::directory::write_data_folder(&data, &original.tree, &original.profile)
                    .unwrap_err();
            assert_eq!(
                crate::store_error_value(
                    error.downcast_ref::<crate::storage::StoreError>().unwrap()
                )["error"]["code"],
                "memory_export_basis_invalid",
                "{problem}: {error:#}"
            );
            assert!(!data.checkout.join(".engram").exists());
        }
        fs::rename(&saved, data.checkout.join(".engram")).unwrap();
        fs::write(&project_path, &valid).unwrap();
        run(&fixture.data, &["add", ".engram"]);
        run(
            &fixture.data,
            &["update-index", "--chmod=-x", ".engram/project.json"],
        );
        run(
            &fixture.data,
            &["commit", "-m", "Repair committed envelope"],
        );
        let data = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data").unwrap();
        assert_eq!(
            memories::directory::committed(&data, &original.tree.project.project_id).unwrap(),
            selected,
            "{problem}"
        );
    }
}

#[test]
fn memory_sync_task_reexport_preserves_committed_relay_and_refuses_recreated_folder() {
    let (fixture, data) = fixture();
    let rule = history("relayed rule", None);
    let mut other = history("different author", None);
    other.key = "other".into();
    for revision in other.revisions.values_mut() {
        revision.provenance.key = "other".into();
        revision.provenance.source.actor_id = "another-host".into();
    }
    let selected = BTreeMap::from([("rule".into(), rule), ("other".into(), other)]);
    memories::directory::write(&data, &BTreeMap::new(), &selected).unwrap();
    run(&fixture.data, &["add", ".engram"]);
    run(
        &fixture.data,
        &["commit", "-m", "Record relayed memory components"],
    );
    let data = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data").unwrap();
    let mut folder = task_sync::directory::read_resolved_data_folder_with_memories(&data).unwrap();
    folder.tree.project.notes_included = true;
    folder.profile.include_notes = true;
    folder.profile.origin_machine = Some("fixture-host".into());
    let refusal =
        task_sync::directory::write_data_folder(&data, &folder.tree, &folder.profile).unwrap_err();
    assert_eq!(
        crate::store_error_value(
            refusal
                .downcast_ref::<crate::storage::StoreError>()
                .unwrap()
        )["error"]["code"],
        "memory_profile_requires_opt_in"
    );
    assert_eq!(memories::directory::read(&data).unwrap(), selected);
    assert_eq!(
        memories::directory::committed(&data, &folder.tree.project.project_id).unwrap(),
        selected
    );
    let saved = fixture.home.path().join("saved-engram");
    fs::rename(data.checkout.join(".engram"), &saved).unwrap();
    task_sync::directory::write_data_folder_with_memories(&data, &folder.tree, &folder.profile)
        .unwrap();
    let committed = memories::directory::committed(&data, &folder.tree.project.project_id).unwrap();
    assert_eq!(committed, selected);
    let error = memories::directory::preflight(
        &data,
        &committed,
        &selected,
        false,
        &folder.tree,
        &folder.profile,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("memory_export_component_changed")
    );
    fs::rename(
        saved.join("memories"),
        data.checkout.join(".engram/memories"),
    )
    .unwrap();
    memories::directory::preflight(
        &data,
        &committed,
        &selected,
        false,
        &folder.tree,
        &folder.profile,
    )
    .unwrap();
    task_sync::directory::enable_memory_profile(&data).unwrap();
    memories::directory::write(&data, &committed, &selected).unwrap();
    assert_eq!(memories::directory::read(&data).unwrap(), selected);
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
fn memory_sync_capability_recovery_handles_partial_staging_and_completed_rename() {
    for phase in 0..3 {
        let (fixture, data) = fixture();
        let project = data.checkout.join(".engram/project.json");
        let intended = fs::read(&project).unwrap();
        let mut before: serde_json::Value = serde_json::from_slice(&intended).unwrap();
        before.as_object_mut().unwrap().remove("memory_profile");
        let before = serde_json::to_vec_pretty(&before).unwrap();
        let retained = fixture.home.path().join("retained");
        fs::create_dir(&retained).unwrap();
        fs::write(retained.join("project.json"), &before).unwrap();
        if phase != 2 {
            fs::write(&project, &before).unwrap();
        }
        let pending = data
            .checkout
            .join(format!(".engram/project-{}.pending", uuid::Uuid::new_v4()));
        fs::write(
            &pending,
            if phase == 0 {
                &intended[..intended.len() / 2]
            } else {
                &intended
            },
        )
        .unwrap();
        assert!(task_sync::directory::read_resolved_data_folder_with_memories(&data).is_err());
        task_sync::directory::recover_memory_profile(&data, &retained).unwrap();
        assert!(!pending.exists());
        task_sync::directory::read_resolved_data_folder_with_memories(&data).unwrap();
        task_sync::directory::recover_memory_profile(&data, &retained).unwrap();
        assert_eq!(fs::read(&project).unwrap(), intended);
    }
}

#[test]
fn memory_sync_capability_recovery_refuses_foreign_bytes_and_changed_envelopes() {
    let (fixture, data) = fixture();
    let project = data.checkout.join(".engram/project.json");
    let intended = fs::read(&project).unwrap();
    let retained = fixture.home.path().join("retained");
    fs::create_dir(&retained).unwrap();
    fs::write(retained.join("project.json"), &intended).unwrap();
    let pending = data
        .checkout
        .join(format!(".engram/project-{}.pending", uuid::Uuid::new_v4()));
    fs::write(&pending, "unrelated bytes").unwrap();
    assert!(task_sync::directory::recover_memory_profile(&data, &retained).is_err());
    assert_eq!(fs::read_to_string(&pending).unwrap(), "unrelated bytes");
    fs::write(&pending, &intended[..intended.len() / 2]).unwrap();
    let unknown = data.checkout.join(".engram/unrelated.pending");
    fs::write(&unknown, "preserve").unwrap();
    assert!(task_sync::directory::recover_memory_profile(&data, &retained).is_err());
    assert!(pending.exists());
    fs::remove_file(unknown).unwrap();
    let mut changed: serde_json::Value = serde_json::from_slice(&intended).unwrap();
    changed["project"]["project_id"] = "another-project".into();
    fs::write(&project, serde_json::to_vec(&changed).unwrap()).unwrap();
    assert!(task_sync::directory::recover_memory_profile(&data, &retained).is_err());
    assert!(pending.exists());
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
