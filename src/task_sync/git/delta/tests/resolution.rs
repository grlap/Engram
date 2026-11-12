//! Actual Git snapshots feed the resolved graph/delta comparison; no writer or
//! remote publication is exercised here.

use super::*;
use crate::task_sync::{
    delta::{DELTA_PAYLOAD_FORMAT, DeltaCandidatePayload, OutputMembership},
    exchange::delta::resolution::validate_snapshots,
    git::{CommitTaskSnapshot, read_task_snapshot},
    inventory::InventoryRoot,
};

fn payload(
    f: &Fixture,
    prior: &str,
    next: &str,
    snapshot: &CommitTaskSnapshot,
) -> DeltaCandidatePayload {
    let delta = f.read(prior, next).unwrap();
    DeltaCandidatePayload {
        format: DELTA_PAYLOAD_FORMAT.into(),
        project_id: f.tree.project.project_id.clone(),
        operation: uuid::Uuid::new_v4(),
        output: OutputMembership {
            inventory_root: snapshot
                .tree
                .tasks
                .iter()
                .try_fold(InventoryRoot::ZERO, InventoryRoot::including)
                .unwrap(),
            profile: snapshot.envelope.profile.clone(),
        },
        documents: delta
            .upserts
            .into_iter()
            .map(|(id, task)| (id, Some(task)))
            .chain(delta.removed.into_iter().map(|id| (id, None)))
            .collect(),
    }
}

#[test]
fn resolved_git_admission_compares_actual_additions_removals_and_unchanged_graph_context() {
    let mut f = Fixture::new(3);
    let prior = f.commit();
    let original = read_task_snapshot(&f.root, &f.tree.project.project_id, &prior).unwrap();
    f.tree.tasks[0].title = "Agent merged planning".into();
    f.write(&f.tree.tasks[0]);
    let removed = f.tree.tasks[1].id;
    fs::remove_file(f.root.join(Fixture::relative(removed))).unwrap();
    let mut added = task();
    added.prerequisites.push(f.tree.tasks[2].id);
    f.write(&added);
    let next = f.commit();
    let merged = read_task_snapshot(&f.root, &f.tree.project.project_id, &next).unwrap();
    let payload = payload(&f, &prior, &next, &merged);
    let before = f.identity();
    validate_snapshots(&original, &merged, &payload).unwrap();
    assert_eq!(payload.documents.len(), 3);
    assert_eq!(payload.documents[&removed], None);
    assert!(!payload.documents.contains_key(&f.tree.tasks[2].id));
    assert_ne!(
        payload.output.inventory_root,
        original
            .tree
            .tasks
            .iter()
            .try_fold(InventoryRoot::ZERO, InventoryRoot::including)
            .unwrap()
    );
    assert_eq!(f.identity(), before);
}

#[test]
fn resolved_git_admission_refuses_omitted_changed_bytes_false_deletions_and_claimed_root() {
    let mut f = Fixture::new(2);
    let prior = f.commit();
    let original = read_task_snapshot(&f.root, &f.tree.project.project_id, &prior).unwrap();
    f.tree.tasks[0].title = "Changed selected bytes".into();
    f.write(&f.tree.tasks[0]);
    let next = f.commit();
    let merged = read_task_snapshot(&f.root, &f.tree.project.project_id, &next).unwrap();
    let payload = payload(&f, &prior, &next, &merged);
    let before = f.identity();
    for scenario in ["missing", "bytes", "deletion", "root", "profile", "project"] {
        let mut changed = payload.clone();
        match scenario {
            "missing" => changed.documents.clear(),
            "bytes" => {
                changed
                    .documents
                    .get_mut(&f.tree.tasks[0].id)
                    .unwrap()
                    .as_mut()
                    .unwrap()
                    .title = "Other bytes".into();
            }
            "deletion" => {
                changed.documents.insert(f.tree.tasks[1].id, None);
            }
            "root" => changed.output.inventory_root = InventoryRoot::ZERO,
            "profile" => changed.output.profile.include_notes = true,
            "project" => changed.project_id = ProjectId("another/project".into()),
            _ => unreachable!(),
        }
        assert!(
            validate_snapshots(&original, &merged, &changed).is_err(),
            "{scenario}"
        );
        assert_eq!(f.identity(), before);
    }
}

#[test]
fn resolved_git_admission_needs_complete_graph_when_deletion_breaks_an_unchanged_child() {
    let mut f = Fixture::new(2);
    let parent = f.tree.tasks[0].id;
    f.tree.tasks[1].parent_id = Some(parent);
    f.write(&f.tree.tasks[1]);
    let prior = f.commit();
    read_task_snapshot(&f.root, &f.tree.project.project_id, &prior).unwrap();
    fs::remove_file(f.root.join(Fixture::relative(parent))).unwrap();
    let next = f.commit();
    // Selected changed fields alone are valid. The unchanged child makes the
    // complete merged graph invalid, so it never reaches recipe comparison.
    let delta = f.read(&prior, &next).unwrap();
    assert!(delta.upserts.is_empty());
    assert_eq!(delta.removed, BTreeSet::from([parent]));
    let before = f.identity();
    assert!(read_task_snapshot(&f.root, &f.tree.project.project_id, &next).is_err());
    assert_eq!(f.identity(), before);
}
