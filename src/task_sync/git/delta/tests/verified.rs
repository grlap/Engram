//! The verified delta opens only changed blobs and derives the incoming root
//! from the recorded one.

use super::*;
use crate::task_sync::inventory::InventoryRoot;

fn root(tasks: &[TaskDocument]) -> InventoryRoot {
    tasks
        .iter()
        .try_fold(InventoryRoot::ZERO, InventoryRoot::including)
        .unwrap()
}

fn verified(
    fixture: &Fixture,
    base: &str,
    base_root: InventoryRoot,
    incoming: &str,
) -> Result<VerifiedTaskDelta> {
    read_verified_task_delta(
        &fixture.root,
        &fixture.tree.project.project_id,
        base,
        base_root,
        incoming,
        std::time::Duration::from_secs(60),
    )
}

#[test]
fn a_verified_delta_reads_old_and_new_changed_blobs_only_and_derives_the_incoming_root() {
    let fixture = Fixture::new(2000);
    let base_root = root(&fixture.tree.tasks);
    let base = fixture.commit();
    let mut tasks = fixture.tree.tasks.clone();
    for task in tasks.iter_mut().take(5) {
        task.title = "Changed planning text".into();
        fixture.write(task);
    }
    let removed = tasks.remove(5);
    fs::remove_file(fixture.root.join(Fixture::relative(removed.id))).unwrap();
    let added = task();
    fixture.write(&added);
    tasks.push(added.clone());
    let incoming = fixture.commit();
    reads();
    let delta = verified(&fixture, &base, base_root, &incoming).unwrap();
    let opened = task_reads();
    // Five edits read old and new, the removal its old blob, the addition its new one.
    assert_eq!(opened.len(), 12, "{opened:?}");
    assert_eq!(delta.changed.len(), 7);
    assert_eq!(delta.root, root(&tasks));
    delta.require_root(root(&tasks)).unwrap();
    assert_eq!(
        delta.changed[&removed.id],
        ChangedTask {
            before: Some(removed.clone()),
            after: None
        }
    );
    assert_eq!(
        delta.changed[&added.id],
        ChangedTask {
            before: None,
            after: Some(added)
        }
    );
    let error = delta.require_root(base_root).unwrap_err();
    assert_eq!(code(&error), "task_sync_inventory_root_mismatch");
    // The same commit is an empty delta with the recorded root.
    reads();
    let same = verified(&fixture, &incoming, delta.root, &incoming).unwrap();
    assert!(same.changed.is_empty());
    assert_eq!(same.root, delta.root);
    assert_eq!(task_reads(), Vec::<String>::new());
}

#[test]
fn a_force_moved_branch_refuses_even_when_its_contents_match() {
    let fixture = Fixture::new(2);
    let first = fixture.commit();
    let mut edited = fixture.tree.tasks[0].clone();
    edited.title = "Published edit".into();
    fixture.write(&edited);
    let published = fixture.commit();
    // The branch is moved back and the same content committed again: a
    // different commit that does not descend from the published one. Its
    // own message keeps it distinct even within the same second, when the
    // same parent, tree and message would give Git the identical commit.
    fixture.git(&["reset", "--hard", &first]);
    fixture.write(&edited);
    fixture.git(&["add", "--all"]);
    fixture.git(&["commit", "-m", "Rewrite the published edit"]);
    let rewritten = fixture.text(&["rev-parse", "HEAD"]);
    assert_ne!(rewritten, published);
    let mut tasks = fixture.tree.tasks.clone();
    tasks[0] = edited;
    let error = verified(&fixture, &published, root(&tasks), &rewritten).unwrap_err();
    assert_eq!(
        code(&error),
        "task_sync_source_history_diverged",
        "{error:#}"
    );
    // Forward from the shared ancestor is fine.
    verified(&fixture, &first, root(&fixture.tree.tasks), &rewritten)
        .unwrap()
        .require_root(root(&tasks))
        .unwrap();
}
