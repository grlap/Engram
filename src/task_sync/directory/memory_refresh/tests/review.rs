use super::super::{
    EVENTS, INTERVENTION, MemoryTaskExport, TEST_LIMITS, final_limits, intended_documents,
    raw_tasks, raw_tasks_with_limits,
};
use super::{Fixture, git, interrupted, inventory};
use crate::task_sync::{directory, memories, progress::ExportProfile};
use std::{collections::BTreeMap, fs};

#[test]
fn memory_task_refresh_intervening_edits_and_new_files_are_preserved() {
    for (stage, change) in [
        ("before_task_mutation", "edit"),
        ("before_task_removal", "new"),
        ("before_task_removal", "removed"),
        ("before_project_rename", "project"),
    ] {
        let fixture = Fixture::new();
        let (tree, selected) = fixture.changed();
        let path = if change == "new" {
            fixture
                .folder()
                .join(format!("tasks/{}.json", uuid::Uuid::new_v4()))
        } else if change == "removed" {
            fixture
                .folder()
                .join(format!("tasks/{}.json", fixture.tree.tasks[1].id.0))
        } else if change == "project" {
            fixture.folder().join("project.json")
        } else {
            fixture
                .folder()
                .join(format!("tasks/{}.json", fixture.tree.tasks[0].id.0))
        };
        let output = path.clone();
        INTERVENTION.with(|slot| {
            *slot.borrow_mut() = Some((
                stage.into(),
                Box::new(move || fs::write(output, b"intervening foreign edit").unwrap()),
            ))
        });
        let error = fixture.publish(&tree, &selected, false).unwrap_err();
        assert_eq!(
            directory::tests::refusal_code(&error),
            "memory_task_export_component_changed"
        );
        assert_eq!(fs::read(path).unwrap(), b"intervening foreign edit");
        assert!(directory::memory_profile_recovery_pending(&fixture.observe()).unwrap());
    }
}

#[test]
fn memory_task_refresh_intervening_index_change_refuses() {
    let fixture = Fixture::new();
    let (tree, selected) = fixture.changed();
    let data = fixture.data.clone();
    INTERVENTION.with(|slot| {
        *slot.borrow_mut() = Some((
            "before_task_mutation".into(),
            Box::new(move || {
                fs::write(data.join("operator-file"), b"operator change").unwrap();
                git(&data, &["add", "operator-file"]);
            }),
        ))
    });
    let before = fs::read(fixture.folder().join("project.json")).unwrap();
    let error = fixture.publish(&tree, &selected, false).unwrap_err();
    assert_eq!(
        directory::tests::refusal_code(&error),
        "memory_task_export_index_changed"
    );
    assert!(
        error
            .to_string()
            .contains("commit the intended staged data or unstage")
    );
    assert_eq!(
        fs::read(fixture.folder().join("project.json")).unwrap(),
        before
    );
    assert!(
        String::from_utf8(git(&fixture.data, &["diff", "--cached", "--name-only"]))
            .unwrap()
            .contains("operator-file")
    );
}

#[test]
fn memory_task_refresh_durability_orders_fence_before_writes_and_removes_it_last() {
    let fixture = Fixture::new();
    let (tree, selected) = fixture.changed();
    EVENTS.with(|events| *events.borrow_mut() = Some(Vec::new()));
    fixture.publish(&tree, &selected, false).unwrap();
    let stages = EVENTS.with(|events| events.borrow_mut().take().unwrap());
    let ordered = [
        "fence_directory_synced",
        "before_task_mutation",
        "task_entries_synced",
        "project_entry_synced",
        "memory_entries_synced",
        "components_synced",
        "fence_removal_synced",
    ];
    let positions: Vec<_> = ordered
        .iter()
        .map(|stage| stages.iter().position(|seen| seen == stage).unwrap())
        .collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(!directory::memory_profile_recovery_pending(&fixture.observe()).unwrap());
}

struct ResetLimits;
impl Drop for ResetLimits {
    fn drop(&mut self) {
        TEST_LIMITS.with(|limits| limits.set(None));
    }
}

#[test]
fn memory_task_refresh_union_limits_allow_interruption_and_recovery() {
    for dimension in ["tasks", "bytes"] {
        let fixture = Fixture::new();
        let (tree, selected) = fixture.changed();
        let (desired, _) = intended_documents(&MemoryTaskExport {
            tree: &tree,
            profile: &ExportProfile {
                include_notes: false,
                origin_machine: None,
            },
            basis: &fixture.selected,
            selected: &selected,
            recovery_basis: None,
        })
        .unwrap();
        let old = raw_tasks(&fixture.folder(), &desired["project.json"]).unwrap();
        let bound = old
            .values()
            .map(Vec::len)
            .sum::<usize>()
            .max(desired.values().map(Vec::len).sum())
            + inventory(&fixture.folder())
                .iter()
                .filter(|(path, _)| matches!(path.to_str(), Some(".gitignore" | ".gitattributes")))
                .map(|(_, bytes)| bytes.len())
                .sum::<usize>();
        let limits = if dimension == "tasks" {
            directory::Limits {
                tasks: 2,
                ..directory::LIMITS
            }
        } else {
            directory::Limits {
                tree_bytes: bound,
                ..directory::LIMITS
            }
        };
        let _reset = ResetLimits;
        TEST_LIMITS.with(|slot| slot.set(Some(limits)));
        interrupted(&fixture, &tree, &selected, "before_task_removal");
        let union = raw_tasks(&fixture.folder(), &desired["project.json"]).unwrap();
        assert_eq!(union.len(), 4); // three tasks and the old envelope
        let error =
            raw_tasks_with_limits(&fixture.folder(), &desired["project.json"], final_limits())
                .unwrap_err();
        let expected = if dimension == "tasks" {
            "task_sync_destination_task_limit"
        } else {
            "task_sync_destination_tree_limit"
        };
        assert_eq!(directory::tests::refusal_code(&error), expected);
        fixture.publish(&tree, &selected, true).unwrap();
        assert_eq!(
            raw_tasks_with_limits(&fixture.folder(), &desired["project.json"], final_limits())
                .unwrap(),
            desired
        );
        assert_eq!(
            memories::directory::read(&fixture.observe()).unwrap(),
            selected
        );
    }
}

#[test]
fn memory_task_refresh_final_limit_refusal_precedes_publication() {
    let fixture = Fixture::new();
    let (mut tree, selected) = fixture.changed();
    let mut additional = tree.tasks[0].clone();
    additional.id = crate::WorkId(uuid::Uuid::new_v4());
    tree.tasks.push(additional);
    tree.tasks.sort_by_key(|task| task.id);
    let _reset = ResetLimits;
    TEST_LIMITS.with(|slot| {
        slot.set(Some(directory::Limits {
            tasks: 2,
            ..directory::LIMITS
        }))
    });
    let before = inventory(&fixture.folder());
    let error = fixture.publish(&tree, &selected, false).unwrap_err();
    assert_eq!(
        directory::tests::refusal_code(&error),
        "task_sync_destination_task_limit"
    );
    assert_eq!(inventory(&fixture.folder()), before);
    assert!(!directory::memory_profile_recovery_pending(&fixture.observe()).unwrap());
}

#[test]
fn memory_task_refresh_staged_complete_retry_refuses_without_mutation() {
    let fixture = Fixture::new();
    let (tree, selected) = fixture.changed();
    fixture.publish(&tree, &selected, false).unwrap();
    git(&fixture.data, &["add", ".engram"]);
    let before = inventory(&fixture.folder());
    let index = git(&fixture.data, &["ls-files", "--stage", "-z"]);
    let error = fixture.publish(&tree, &selected, false).unwrap_err();
    assert_eq!(
        directory::tests::refusal_code(&error),
        "memory_task_export_index_changed"
    );
    assert_eq!(inventory(&fixture.folder()), before);
    assert_eq!(git(&fixture.data, &["ls-files", "--stage", "-z"]), index);
}

#[test]
fn memory_task_refresh_empty_advertised_memory_component_is_supported() {
    let fixture = Fixture::new();
    for path in inventory(&fixture.folder().join("memories")).keys() {
        fs::remove_file(fixture.folder().join("memories").join(path)).unwrap();
    }
    fs::remove_dir(fixture.folder().join("memories/rule")).unwrap();
    git(&fixture.data, &["add", ".engram"]);
    git(
        &fixture.data,
        &[
            "-c",
            "commit.gpgSign=false",
            "commit",
            "-m",
            "Publish empty memory selection",
        ],
    );
    let (tree, _) = fixture.changed();
    fixture.publish(&tree, &BTreeMap::new(), false).unwrap();
    assert_eq!(
        memories::directory::read(&fixture.observe()).unwrap().len(),
        0
    );
    assert_eq!(
        directory::read_resolved_data_folder_with_memories(&fixture.observe())
            .unwrap()
            .tree,
        tree
    );
}

#[test]
fn memory_task_refresh_missing_memory_directory_names_component_missing() {
    let fixture = Fixture::new();
    for path in inventory(&fixture.folder().join("memories")).keys() {
        fs::remove_file(fixture.folder().join("memories").join(path)).unwrap();
    }
    fs::remove_dir(fixture.folder().join("memories/rule")).unwrap();
    fs::remove_dir(fixture.folder().join("memories")).unwrap();
    let (tree, selected) = fixture.changed();
    let before = inventory(&fixture.folder());
    let error = fixture.publish(&tree, &selected, false).unwrap_err();
    assert_eq!(
        directory::tests::refusal_code(&error),
        "memory_export_component_missing"
    );
    assert_eq!(inventory(&fixture.folder()), before);
}

#[test]
fn memory_task_refresh_autocrlf_checkout_diagnoses_eol_policy() {
    let fixture = Fixture::new();
    git(&fixture.data, &["config", "core.autocrlf", "true"]);
    // An explicit path override models a legacy task checkout without eol=lf.
    let info = fixture.observe().common_directory.join("info");
    fs::create_dir_all(&info).unwrap();
    fs::write(
        info.join("attributes"),
        b".engram/tasks/*.json text eol=crlf\n",
    )
    .unwrap();
    git(&fixture.data, &["checkout-index", "--force", "--all"]);
    let (tree, selected) = fixture.changed();
    let before = inventory(&fixture.folder());
    let error = fixture.publish(&tree, &selected, false).unwrap_err();
    assert_eq!(
        directory::tests::refusal_code(&error),
        "memory_task_export_component_changed"
    );
    assert!(error.to_string().contains("CRLF"));
    assert!(error.to_string().contains("eol=lf"));
    assert_eq!(inventory(&fixture.folder()), before);
}

#[test]
fn memory_task_refresh_hashes_component_files_in_one_bounded_batch() {
    let fixture = Fixture::new();
    let observed = fixture.observe();
    let basis = crate::task_sync::git::read_task_publication_basis(
        &fixture.data,
        &fixture.tree.project.project_id,
        observed.head.as_deref().unwrap(),
    )
    .unwrap()
    .unwrap();
    let paths: Vec<_> = basis.memory_files.keys().cloned().collect();
    assert_eq!(paths.len(), 2);
    let queries = std::rc::Rc::new(std::cell::Cell::new(0));
    let capture = queries.clone();
    crate::task_sync::git::BEFORE_QUERY.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |name| {
            if name == "hash-object" {
                capture.set(capture.get() + 1);
            }
        }))
    });
    let result = super::super::require_file_identities(
        &observed,
        &paths,
        &basis.memory_files,
        crate::task_sync::TaskSyncRefusal::MemoryExportComponentChanged,
    );
    crate::task_sync::git::BEFORE_QUERY.with(|hook| *hook.borrow_mut() = None);
    result.unwrap();
    assert_eq!(queries.get(), 1);
}

#[test]
fn memory_task_refresh_memory_recovery_refuses_with_named_metadata() {
    let fixture = Fixture::new();
    let (tree, selected) = fixture.changed();
    interrupted(&fixture, &tree, &selected, "fence_synced");
    let id = crate::ObjectId::mint();
    fs::write(
        fixture.folder().join(format!("memories/rule/{id}.json")),
        b"foreign memory body",
    )
    .unwrap();
    let before = inventory(&fixture.folder());
    let error = fixture.publish(&tree, &selected, true).unwrap_err();
    assert_eq!(
        directory::tests::refusal_code(&error),
        "memory_task_export_recovery_mismatch"
    );
    assert!(
        error
            .to_string()
            .contains("check=memory_publication_validation")
    );
    assert!(!error.to_string().contains("foreign memory body"));
    assert_eq!(inventory(&fixture.folder()), before);
}
