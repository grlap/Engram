//! A merged commit read as a rooted diff from the original publication feeds
//! the resolved payload comparison; no writer or remote is exercised here.

use super::*;
use crate::task_sync::{
    delta::{DELTA_PAYLOAD_FORMAT, DeltaCandidatePayload, OutputMembership},
    exchange::delta::resolution::validate_resolved_delta,
    git::{CommitTaskSnapshot, VerifiedTaskDelta, read_rooted_task_diff, read_task_snapshot},
    inventory::InventoryRoot,
};

fn membership(snapshot: &CommitTaskSnapshot) -> OutputMembership {
    OutputMembership {
        inventory_root: snapshot
            .tree
            .tasks
            .iter()
            .try_fold(InventoryRoot::ZERO, InventoryRoot::including)
            .unwrap(),
        profile: snapshot.envelope.profile.clone(),
    }
}

fn payload(f: &Fixture, prior: &str, next: &str) -> DeltaCandidatePayload {
    let delta = f.read(prior, next).unwrap();
    let merged = read_task_snapshot(&f.root, &f.tree.project.project_id, next).unwrap();
    DeltaCandidatePayload {
        format: DELTA_PAYLOAD_FORMAT.into(),
        project_id: f.tree.project.project_id.clone(),
        operation: uuid::Uuid::new_v4(),
        output: membership(&merged),
        documents: delta
            .upserts
            .into_iter()
            .map(|(id, task)| (id, Some(task)))
            .chain(delta.removed.into_iter().map(|id| (id, None)))
            .collect(),
    }
}

fn diff(f: &Fixture, original: &OutputMembership, prior: &str, next: &str) -> VerifiedTaskDelta {
    read_rooted_task_diff(
        &f.root,
        &f.tree.project.project_id,
        prior,
        original.inventory_root,
        next,
        std::time::Duration::from_secs(60),
    )
    .unwrap()
}

#[test]
fn resolved_git_admission_compares_actual_additions_removals_and_unchanged_graph_context() {
    let mut f = Fixture::new(3);
    let prior = f.commit();
    let original =
        membership(&read_task_snapshot(&f.root, &f.tree.project.project_id, &prior).unwrap());
    f.tree.tasks[0].title = "Agent merged planning".into();
    f.write(&f.tree.tasks[0]);
    let removed = f.tree.tasks[1].id;
    fs::remove_file(f.root.join(Fixture::relative(removed))).unwrap();
    let mut added = task();
    added.prerequisites.push(f.tree.tasks[2].id);
    f.write(&added);
    let next = f.commit();
    let payload = payload(&f, &prior, &next);
    let before = f.identity();
    reads();
    let delta = diff(&f, &original, &prior, &next);
    // Only the changed paths are opened: the edit's two blobs, the removal's
    // old one and the addition's new one, never the unchanged prerequisite.
    assert_eq!(task_reads().len(), 4);
    validate_resolved_delta(&delta, &original, &payload).unwrap();
    assert_eq!(payload.documents.len(), 3);
    assert_eq!(payload.documents[&removed], None);
    assert!(!payload.documents.contains_key(&f.tree.tasks[2].id));
    assert_ne!(payload.output.inventory_root, original.inventory_root);
    assert_eq!(f.identity(), before);
}

#[test]
fn resolved_git_admission_refuses_omitted_changed_bytes_false_deletions_and_claimed_root() {
    let mut f = Fixture::new(2);
    let prior = f.commit();
    let original =
        membership(&read_task_snapshot(&f.root, &f.tree.project.project_id, &prior).unwrap());
    f.tree.tasks[0].title = "Changed selected bytes".into();
    f.write(&f.tree.tasks[0]);
    let next = f.commit();
    let payload = payload(&f, &prior, &next);
    let delta = diff(&f, &original, &prior, &next);
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
            validate_resolved_delta(&delta, &original, &changed).is_err(),
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
