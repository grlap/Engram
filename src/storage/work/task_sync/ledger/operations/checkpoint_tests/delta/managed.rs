//! The acceptance fixture measures the actual SQL writer on 2,000 tasks with
//! five edits, an addition and a removal. Git observations remain substituted
//! exactly as in the surrounding incoming writer fixtures.

use super::*;
use crate::task_sync::{retention::audit::journal, setup::DIRECTORY_LISTINGS};

fn changes(
    tree: &crate::task_sync::TaskTree,
) -> BTreeMap<crate::WorkId, Option<crate::task_sync::TaskDocument>> {
    let mut result = BTreeMap::new();
    for task in &tree.tasks[..5] {
        let mut task = task.clone();
        task.title.push_str(" edited");
        result.insert(task.id, Some(task));
    }
    result.insert(tree.tasks[5].id, None);
    let mut added = tree.tasks[6].clone();
    added.id = crate::WorkId::new();
    added.title = "Incoming addition".into();
    result.insert(added.id, Some(added));
    result
}

fn reset() {
    crate::canonical::reset_canonical_decode_count();
    MEMBER_DECODES.with(|count| count.set(0));
    DIRECTORY_LISTINGS.with(|count| count.set((0, 0)));
}

fn report(phase: &str, elapsed: std::time::Duration) {
    let (calls, entries) = DIRECTORY_LISTINGS.with(std::cell::Cell::get);
    eprintln!(
        "MEASURE {phase}: canonical_decodes={} member_decodes={} directory_listings={calls} directory_entries={entries} elapsed_ms={}",
        crate::canonical::canonical_decode_count(),
        MEMBER_DECODES.with(std::cell::Cell::get),
        elapsed.as_millis()
    );
}

#[test]
fn managed_two_thousand_task_delta_and_replay_preserve_selected_costs() {
    let tree = independent_tree(2_000);
    let mut f = Fixture::with_store_changes_mode(
        &tree,
        |_, tree| changes(tree),
        |_, _| {},
        |_, _, _| {},
        true,
    );
    reset();
    let start = std::time::Instant::now();
    let first = f.apply().unwrap();
    report("managed_delta_apply", start.elapsed());
    assert!(
        journal::observe(&f.input.binding.repository.git_directory)
            .unwrap()
            .unwrap()
            .writes
            .is_empty()
    );
    reset();
    let start = std::time::Instant::now();
    let repeated = f.apply().unwrap();
    report("managed_delta_repeat", start.elapsed());
    assert_eq!(repeated, first);
    assert_eq!(MEMBER_DECODES.with(std::cell::Cell::get), 0);
    assert_eq!(DIRECTORY_LISTINGS.with(std::cell::Cell::get), (0, 0));
    reset();
    f.store
        .recover_task_sync_delta_import(&f.input.binding_id, &f.input.intent)
        .unwrap();
    let recovery_cost = DIRECTORY_LISTINGS.with(std::cell::Cell::get);
    // A further thousand disconnected malformed historical selections do not
    // enter ordinary managed custody observation or selected validation.
    let commits = f
        .input
        .binding
        .repository
        .git_directory
        .join("engram-task-sync-commits");
    for _ in 0..1_000 {
        std::fs::write(
            commits.join(format!("{}.json", uuid::Uuid::new_v4())),
            b"unrelated partial selection",
        )
        .unwrap();
    }
    reset();
    let start = std::time::Instant::now();
    f.store
        .recover_task_sync_delta_import(&f.input.binding_id, &f.input.intent)
        .unwrap();
    report(
        "managed_delta_recovery_with_unrelated_history",
        start.elapsed(),
    );
    assert_eq!(DIRECTORY_LISTINGS.with(std::cell::Cell::get), recovery_cost);
    assert!(retention::inventory(&f.input.binding.repository.git_directory).is_err());
}
