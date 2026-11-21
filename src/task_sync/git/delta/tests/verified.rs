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

#[test]
fn staged_merge_membership_reads_only_the_changed_task_documents() {
    let fixture = Fixture::new(12);
    let base_root = root(&fixture.tree.tasks);
    let base = fixture.commit();
    let original = &fixture.tree.tasks[0];
    let removed = &fixture.tree.tasks[1];
    let mut edited = original.clone();
    edited.title = "Resolved task planning".into();
    fixture.write(&edited);
    fs::remove_file(fixture.root.join(Fixture::relative(removed.id))).unwrap();
    let added = task();
    fixture.write(&added);
    fixture.git(&["add", "--all"]);
    let staged = fixture.text(&["write-tree"]);
    reads();

    let (merged_root, count) = merged_task_membership(
        &fixture.root,
        &fixture.tree.project.project_id,
        &base,
        base_root,
        12,
        &staged,
        std::time::Duration::from_secs(60),
    )
    .unwrap();

    let expected = base_root
        .excluding(original)
        .unwrap()
        .including(&edited)
        .unwrap()
        .excluding(removed)
        .unwrap()
        .including(&added)
        .unwrap();
    assert_eq!((merged_root, count), (expected, 12));
    let mut opened = reads();
    opened.sort();
    let mut selected = [original.id, original.id, removed.id, added.id].map(Fixture::relative);
    selected.sort();
    assert_eq!(opened, selected);
    assert_eq!(fixture.text(&["rev-parse", "HEAD"]), base);
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
fn a_delta_whose_path_listing_outgrows_a_general_query_is_read_whole() {
    let fixture = Fixture::new(10);
    let base_root = root(&fixture.tree.tasks);
    let base = fixture.commit();
    let mut tasks = fixture.tree.tasks.clone();
    // Each raw diff record is well over 128 bytes, so this many additions
    // list more than the 1 MiB a general Git query may return.
    for _ in 0..8_500 {
        let added = task();
        fixture.write(&added);
        tasks.push(added);
    }
    let incoming = fixture.commit();
    let delta = verified(&fixture, &base, base_root, &incoming).unwrap();
    assert_eq!(delta.changed.len(), 8_500);
    assert_eq!(delta.root, root(&tasks));
}

#[test]
fn selected_reads_of_more_tasks_than_one_command_line_holds_return_every_document() {
    let fixture = Fixture::new(1_200);
    let commit = fixture.commit();
    let absent = WorkId::new();
    let selected = fixture
        .tree
        .tasks
        .iter()
        .map(|task| task.id)
        .chain([absent])
        .collect::<Vec<_>>();
    let read = read_tasks_at(
        &fixture.root,
        &fixture.tree.project.project_id,
        &commit,
        selected,
        // The production deadline: one batched blob read, not one per task.
        std::time::Duration::from_secs(60),
    )
    .unwrap();
    assert_eq!(read.len(), 1_201);
    assert_eq!(read[&absent], None);
    for task in &fixture.tree.tasks {
        assert_eq!(read[&task.id].as_ref(), Some(task));
    }
}

#[test]
fn a_shallow_clone_cannot_prove_descent_and_names_the_unshallow_remedy() {
    let fixture = Fixture::new(2);
    let first = fixture.commit();
    let mut edited = fixture.tree.tasks[0].clone();
    edited.title = "Edited after the first commit".into();
    fixture.write(&edited);
    let second = fixture.commit();
    // Both commits are branch tips of a depth-one clone: present, but with
    // the history between them cut, so descent cannot be proved there.
    fixture.git(&["branch", "earlier", &first]);
    let home = fixture.root.parent().unwrap();
    let shallow = home.join("shallow-clone");
    // A plain Git process: the hardened command forbids every transport, and
    // a shallow clone needs the file transport.
    let clone = std::process::Command::new("git")
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", home.join("global.config"))
        .env_remove("GIT_ALLOW_PROTOCOL")
        .args([
            "-c",
            "protocol.file.allow=always",
            "clone",
            "--quiet",
            "--no-local",
            "--depth",
            "1",
            "--no-single-branch",
        ])
        .arg(&fixture.root)
        .arg(&shallow)
        .output()
        .unwrap();
    assert!(
        clone.status.success(),
        "{}",
        String::from_utf8_lossy(&clone.stderr)
    );
    let error = read_verified_task_delta(
        &shallow,
        &fixture.tree.project.project_id,
        &first,
        root(&fixture.tree.tasks),
        &second,
        std::time::Duration::from_secs(60),
    )
    .unwrap_err();
    assert_eq!(
        code(&error),
        "task_sync_source_history_unavailable",
        "{error:#}"
    );
    assert!(format!("{error:#}").contains("--unshallow"), "{error:#}");
    // The complete history proves the same pair.
    verified(&fixture, &first, root(&fixture.tree.tasks), &second).unwrap();
}

#[test]
fn a_commit_with_an_entry_the_format_does_not_define_refuses_like_a_complete_read() {
    let fixture = Fixture::new(2);
    let base = fixture.commit();
    let undefined = fixture.root.join(".engram/memories");
    fs::create_dir_all(&undefined).unwrap();
    fs::write(undefined.join("note.json"), "{\"body\": \"shared\"}\n").unwrap();
    let mut edited = fixture.tree.tasks[0].clone();
    edited.title = "Edited beside an undefined entry".into();
    fixture.write(&edited);
    let incoming = fixture.commit();
    // The delta and the complete snapshot admit the same folder layout, so a
    // delta never accepts what the named rebuild would then refuse.
    assert!(verified(&fixture, &base, root(&fixture.tree.tasks), &incoming).is_err());
    assert!(
        crate::task_sync::git::read_task_snapshot(
            &fixture.root,
            &fixture.tree.project.project_id,
            &incoming
        )
        .is_err()
    );
}

#[test]
fn a_changed_policy_file_keeps_the_per_file_limit_and_its_removal_is_read_no_further() {
    let fixture = Fixture::new(2);
    let base_root = root(&fixture.tree.tasks);
    let base = fixture.commit();
    // A policy file one byte over the per-file limit the complete reader
    // applies: the delta refuses it by the same limit.
    let limit = usize::try_from(crate::task_sync::directory::MAX_FILE_BYTES).unwrap();
    let oversized = format!("#{}", "a".repeat(limit));
    fs::write(fixture.root.join(".engram/.gitignore"), oversized).unwrap();
    let grown = fixture.commit();
    let error = verified(&fixture, &base, base_root, &grown).unwrap_err();
    assert_eq!(
        code(&error),
        "task_sync_destination_file_limit",
        "{error:#}"
    );
    // Its removal reads no blob of it and is admitted.
    fs::remove_file(fixture.root.join(".engram/.gitignore")).unwrap();
    let removed = fixture.commit();
    reads();
    let delta = verified(&fixture, &grown, base_root, &removed).unwrap();
    assert_eq!(task_reads(), Vec::<String>::new());
    assert!(delta.changed.is_empty());
    assert_eq!(delta.root, base_root);
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
