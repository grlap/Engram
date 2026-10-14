use super::*;
use crate::{
    ChildRequirement, WorkId, WorkItemKind, WorkLifecycle,
    task_sync::{FORMAT, ProjectDocument, TaskDocument},
};
use std::{cell::Cell, fs};

mod prepared;
mod recovery;
mod state_limit;

fn tree(count: usize) -> TaskTree {
    let mut tasks = (0..count)
        .map(|index| TaskDocument {
            id: WorkId::new(),
            external_ref: None,
            title: format!("task {index}"),
            outcome: "Deliver work".into(),
            acceptance: vec!["Implemented".into()],
            acceptance_bindings: Vec::new(),
            evaluation_mode: None,
            kind: WorkItemKind::Task,
            priority: 1,
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
            activity: BTreeMap::default(),
        })
        .collect::<Vec<_>>();
    tasks.sort_by_key(|task| task.id.0);
    TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: ProjectId("incremental".into()),
            notes_included: false,
        },
        tasks,
    }
}

fn capture(tree: TaskTree) -> ExportCapture {
    ExportCapture {
        tree,
        omissions: NoteOmissions::default(),
        activity_omissions: NoteOmissions::default(),
        cut: ExportCut {
            position: 0,
            anchor: None,
        },
        full_reason: None,
    }
}

fn profile() -> ExportProfile {
    ExportProfile {
        include_notes: false,
        origin_machine: None,
    }
}

fn producer() -> ObjectId {
    ObjectId::from_canonical_bytes(b"derived test producer")
}

#[test]
fn task_prepare_differential_mutation_table_serializes_only_final_changed_tasks() {
    let original = tree(1000);
    let bytes = original.documents().unwrap();
    let mut cases = Vec::new();
    cases.push(("unchanged and runtime-only inventory", original.clone(), 0));
    let mut revised = original.clone();
    for _ in 0..20 {
        revised.tasks[500].title.push('!');
    }
    cases.push(("repeated update", revised, 1));
    let mut created = original.clone();
    created.tasks.extend(tree(1).tasks);
    created.tasks.sort_by_key(|task| task.id.0);
    cases.push(("create", created, 1));
    let mut removed = original.clone();
    removed.tasks.remove(500);
    cases.push(("remove", removed, 0));
    let mut fields = original.clone();
    fields.tasks[0].title.push_str(" revised");
    fields.tasks[0].outcome.push_str(" revised");
    fields.tasks[0].acceptance.push("Second".into());
    fields.tasks[0].kind = WorkItemKind::Bug;
    fields.tasks[0].priority = 2;
    fields.tasks[0].labels = vec!["shared".into()];
    fields.tasks[0].assigned_to = Some("next-worker".into());
    fields.tasks[0].external_ref = Some("asserted-ticket".into());
    fields.tasks[0].prerequisites = vec![fields.tasks[1].id];
    fields.tasks[0].child_requirement = ChildRequirement::Optional;
    fields.tasks[2].parent_id = Some(fields.tasks[0].id);
    cases.push(("planning fields and relations", fields, 2));
    for (name, candidate, count) in cases {
        // Both paths consume this exact immutable typed snapshot, not two reads.
        let (incremental, serialized) = select_documents(&candidate, Some(&bytes)).unwrap();
        let (full, full_count) = select_documents(&candidate, None).unwrap();
        assert_eq!(incremental, full, "{name}");
        assert_eq!(full, candidate.documents().unwrap(), "{name}");
        assert_eq!(serialized, count, "{name}");
        assert_eq!(full_count, candidate.tasks.len());
    }
}

#[test]
fn task_prepare_differential_journal_table_covers_pins_producer_and_fallbacks() {
    for scenario in [
        "unchanged",
        "missing",
        "corrupt",
        "divergent",
        "output missing",
        "producer",
        "profile",
        "alias",
    ] {
        let home = crate::test_support::temp_home().unwrap();
        let store = home.path().join("store.db");
        fs::write(&store, b"route only").unwrap();
        let progress = home.path().join("progress");
        let first_output = home.path().join("first");
        let output = home.path().join("second");
        let mut snapshot = tree(3);
        let mut selected = profile();
        if scenario == "alias" {
            selected = ExportProfile {
                include_notes: true,
                origin_machine: Some("host-a".into()),
            };
            snapshot.project.notes_included = true;
        }
        prepare_with_producer(
            &first_output,
            &progress,
            &store,
            &snapshot.project.project_id,
            &selected,
            false,
            &producer(),
            |_| Ok(capture(snapshot.clone())),
        )
        .unwrap();
        let baseline = journal::load(&progress).unwrap().current.unwrap();
        match scenario {
            "missing" => fs::remove_dir_all(&baseline.directory).unwrap(),
            "corrupt" => fs::write(baseline.directory.join("state.json"), b"invalid").unwrap(),
            "divergent" => {
                fs::write(baseline.directory.join("candidate/project.json"), b"{}").unwrap();
            }
            "output missing" => fs::remove_dir_all(&first_output).unwrap(),
            "profile" => {
                selected.include_notes = true;
                selected.origin_machine = Some("host-a".into());
                snapshot.project.notes_included = true;
            }
            "alias" => selected.origin_machine = Some("host-b".into()),
            _ => {}
        }
        let different = ObjectId::from_canonical_bytes(b"different test producer");
        let producing = if scenario == "producer" {
            &different
        } else {
            &producer()
        };
        let result = prepare_with_producer(
            &output,
            &progress,
            &store,
            &snapshot.project.project_id,
            &selected,
            false,
            producing,
            |_| Ok(capture(snapshot.clone())),
        );
        if matches!(scenario, "profile" | "alias") {
            assert!(
                result.unwrap_err().to_string().contains("pinned"),
                "{scenario}"
            );
            assert!(!output.exists());
            continue;
        }
        let receipt = result.unwrap();
        assert_eq!(
            directory::read_documents(&output).unwrap(),
            snapshot.documents().unwrap(),
            "{scenario}"
        );
        assert_eq!(
            receipt.path,
            if scenario == "unchanged" {
                "incremental"
            } else {
                "full"
            },
            "{scenario}"
        );
        assert_eq!(
            receipt.serialized_tasks,
            if scenario == "unchanged" {
                0
            } else {
                snapshot.tasks.len()
            },
            "{scenario}"
        );
    }
}

#[test]
fn task_prepare_pending_retry_keeps_original_bytes_after_later_store_changes() {
    for output_exists in [false, true] {
        let home = crate::test_support::temp_home().unwrap();
        let store = home.path().join("store.db");
        fs::write(&store, b"route").unwrap();
        let progress = home.path().join("progress");
        let output = home.path().join("candidate");
        let snapshot = tree(2);
        prepare_with_producer(
            &output,
            &progress,
            &store,
            &snapshot.project.project_id,
            &profile(),
            false,
            &producer(),
            |_| {
                let mut captured = capture(snapshot.clone());
                captured
                    .activity_omissions
                    .counts
                    .insert("restricted-task".into(), 2);
                Ok(captured)
            },
        )
        .unwrap();
        let original = directory::read_documents(&output).unwrap();
        let generation = journal::load(&progress).unwrap().current.unwrap();
        // Simulate process exit after pending publication, before output or completion.
        fs::remove_file(generation.directory.join("complete")).unwrap();
        if !output_exists {
            fs::remove_dir_all(&output).unwrap();
        }
        let calls = Cell::new(0);
        let receipt = prepare_with_producer(
            &output,
            &progress,
            &store,
            &snapshot.project.project_id,
            &profile(),
            false,
            &producer(),
            |_| {
                calls.set(calls.get() + 1);
                let mut later = snapshot.clone();
                later.tasks[0].title.push_str(" later");
                Ok(capture(later))
            },
        )
        .unwrap();
        assert!(receipt.recovered);
        assert_eq!(calls.get(), 0);
        assert_eq!(receipt.operation, generation.meta.operation);
        assert_eq!(
            receipt.activity_omissions,
            generation.meta.activity_omissions
        );
        assert_eq!(
            receipt.activity_omissions.counts.get("restricted-task"),
            Some(&2)
        );
        assert_eq!(directory::read_documents(&output).unwrap(), original);
        let mut later = snapshot.clone();
        later.tasks[0].title.push_str(" later");
        let next = home.path().join("later");
        let receipt = prepare_with_producer(
            &next,
            &progress,
            &store,
            &snapshot.project.project_id,
            &profile(),
            false,
            &producer(),
            |_| Ok(capture(later.clone())),
        )
        .unwrap();
        assert_eq!(receipt.serialized_tasks, 1);
        assert_eq!(
            directory::read_documents(&next).unwrap(),
            later.documents().unwrap()
        );
    }
}

#[test]
fn task_prepare_lock_refuses_immediately_and_releases_without_unlinking() {
    let home = crate::test_support::temp_home().unwrap();
    let progress = home.path().join("progress");
    let first = lock::ProgressLock::acquire(&progress).unwrap();
    let error = lock::ProgressLock::acquire(&progress).err().unwrap();
    assert!(error.to_string().contains("another exporter holds"));
    assert!(error.to_string().contains("prepare.lock"));
    drop(first);
    let _second = lock::ProgressLock::acquire(&progress).unwrap();
    assert!(progress.join("prepare.lock").is_file());
}

#[test]
fn task_prepare_journal_retains_only_predecessor_and_current() {
    let home = crate::test_support::temp_home().unwrap();
    let store = home.path().join("store.db");
    fs::write(&store, b"route").unwrap();
    let progress = home.path().join("progress");
    let snapshot = tree(1);
    for step in 0..5 {
        let output = home.path().join(format!("candidate-{step}"));
        prepare_with_producer(
            &output,
            &progress,
            &store,
            &snapshot.project.project_id,
            &profile(),
            false,
            &producer(),
            |_| Ok(capture(snapshot.clone())),
        )
        .unwrap();
        assert_eq!(
            journal::load(&progress)
                .unwrap()
                .current
                .unwrap()
                .meta
                .sequence,
            step + 1
        );
        assert_eq!(
            fs::read_dir(&progress).unwrap().count(),
            1 + (step + 1).min(2) as usize
        );
        assert!(output.exists());
    }
}

#[test]
fn task_prepare_rebaseline_changes_profile_and_missing_predecessor_catches_up() {
    let home = crate::test_support::temp_home().unwrap();
    let store = home.path().join("store.db");
    fs::write(&store, b"route").unwrap();
    let progress = home.path().join("progress");
    let mut snapshot = tree(2);
    for step in 0..2 {
        prepare_with_producer(
            &home.path().join(format!("candidate-{step}")),
            &progress,
            &store,
            &snapshot.project.project_id,
            &profile(),
            false,
            &producer(),
            |_| Ok(capture(snapshot.clone())),
        )
        .unwrap();
    }
    let current = journal::load(&progress).unwrap().current.unwrap();
    let predecessor = fs::read_dir(&progress)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir() && *path != current.directory)
        .unwrap();
    assert!(
        fs::canonicalize(&predecessor)
            .unwrap()
            .starts_with(fs::canonicalize(home.path()).unwrap())
    );
    fs::remove_dir_all(predecessor).unwrap();
    snapshot.tasks[0].title.push_str(" after interruption");
    let output = home.path().join("caught-up");
    let receipt = prepare_with_producer(
        &output,
        &progress,
        &store,
        &snapshot.project.project_id,
        &profile(),
        false,
        &producer(),
        |previous| {
            assert!(previous.is_none());
            Ok(capture(snapshot.clone()))
        },
    )
    .unwrap();
    assert_eq!(receipt.path, "full");
    assert!(receipt.reason.contains("predecessor"));
    assert_eq!(
        directory::read_documents(&output).unwrap(),
        snapshot.documents().unwrap()
    );
    snapshot.project.notes_included = true;
    let notes = ExportProfile {
        include_notes: true,
        origin_machine: Some("host-a".into()),
    };
    let changed = home.path().join("new-profile");
    let receipt = prepare_with_producer(
        &changed,
        &progress,
        &store,
        &snapshot.project.project_id,
        &notes,
        true,
        &producer(),
        |previous| {
            assert!(previous.is_none());
            Ok(capture(snapshot.clone()))
        },
    )
    .unwrap();
    assert_eq!(receipt.path, "full");
    assert_eq!(receipt.reason, "explicit rebaseline");
    assert_eq!(
        directory::read_documents(&changed).unwrap(),
        snapshot.documents().unwrap()
    );
}
