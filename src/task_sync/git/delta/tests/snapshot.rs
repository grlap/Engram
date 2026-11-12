use super::*;
use crate::task_sync::git::{read_task_snapshot, snapshot::read_with_limit};

fn snapshot(fixture: &Fixture, commit: &str) -> Result<crate::task_sync::git::CommitTaskSnapshot> {
    read_task_snapshot(&fixture.root, &fixture.tree.project.project_id, commit)
}

#[test]
fn bounded_snapshot_refuses_exhausted_budget_and_reads_the_selected_commit_without_writes() {
    let fixture = Fixture::new(2);
    let commit = fixture.commit();
    let mut dirty = fixture.tree.tasks[0].clone();
    dirty.title = "Uncommitted task differs".into();
    fixture.write(&dirty);
    let identity = fixture.identity();
    reads();
    assert!(
        crate::task_sync::git::read_task_snapshot_bounded(
            &fixture.root,
            &fixture.tree.project.project_id,
            &commit,
            std::time::Duration::ZERO,
        )
        .is_err()
    );
    assert_eq!(task_reads(), Vec::<String>::new());
    assert_eq!(fixture.identity(), identity);
    let selected = crate::task_sync::git::read_task_snapshot_bounded(
        &fixture.root,
        &fixture.tree.project.project_id,
        &commit,
        std::time::Duration::from_secs(30),
    )
    .unwrap();
    assert_eq!(selected.tree, fixture.tree);
    assert_eq!(fixture.identity(), identity);
}

#[test]
fn data_policy_edits_preserve_task_snapshots_and_proportional_delta_reads() {
    let fixture = Fixture::new(2);
    let base = fixture.commit();
    fs::write(
        fixture.root.join(".engram/.gitignore"),
        "# metadata policy\n",
    )
    .unwrap();
    fs::write(
        fixture.root.join(".engram/.gitattributes"),
        "* text eol=lf -filter\n",
    )
    .unwrap();
    let policies = fixture.commit();
    reads();
    assert_eq!(snapshot(&fixture, &policies).unwrap().tree, fixture.tree);
    reads();
    let delta = fixture.read(&base, &policies).unwrap();
    assert!(delta.upserts.is_empty() && delta.removed.is_empty());
    assert!(
        task_reads().is_empty(),
        "policy changes must not read task blobs"
    );
    let mut changed = fixture.tree.tasks[0].clone();
    changed.title = "Changed with metadata policy".into();
    fixture.write(&changed);
    fs::write(
        fixture.root.join(".engram/.gitignore"),
        "# changed policy\n",
    )
    .unwrap();
    let incoming = fixture.commit();
    reads();
    let delta = fixture.read(&policies, &incoming).unwrap();
    assert_eq!(delta.upserts.get(&changed.id), Some(&changed));
    assert_eq!(task_reads(), vec![Fixture::relative(changed.id)]);
    let blob = fixture.text(&["rev-parse", "HEAD:.engram/.gitignore"]);
    fixture.git(&[
        "update-index",
        "--cacheinfo",
        "120000",
        &blob,
        ".engram/.gitignore",
    ]);
    fixture.git(&["commit", "-m", "Unsafe policy mode"]);
    let unsafe_commit = fixture.text(&["rev-parse", "HEAD"]);
    assert!(snapshot(&fixture, &unsafe_commit).is_err());
    assert!(fixture.read(&incoming, &unsafe_commit).is_err());
}

#[test]
fn complete_cold_snapshot_reads_two_thousand_documents_and_keeps_unverified_envelope() {
    use sha2::{Digest, Sha256};
    let fixture = Fixture::new(2000);
    fixture.commit();
    let mut changed = fixture.tree.tasks[0].clone();
    changed.title = "Merged task text without a regenerated envelope".into();
    fixture.write(&changed);
    let commit = fixture.commit();
    let identity = fixture.identity();
    let mut dirty = changed.clone();
    dirty.title = "Uncommitted local change".into();
    fixture.write(&dirty);
    let dirty_identity = fixture.identity();
    reads();
    let actual = snapshot(&fixture, &commit).unwrap();
    assert_eq!(actual.tree.tasks.len(), 2000);
    assert_eq!(
        actual.tree.tasks.iter().find(|task| task.id == changed.id),
        Some(&changed)
    );
    let blobs = reads();
    assert_eq!(blobs.len(), 2001);
    assert_eq!(blobs.into_iter().collect::<BTreeSet<_>>().len(), 2001);
    assert_eq!(actual.commit.oid, commit);
    assert_eq!(
        actual.commit.tree,
        fixture.text(&["rev-parse", "HEAD^{tree}"])
    );
    assert_eq!(
        actual.commit.parents,
        vec![fixture.text(&["rev-parse", "HEAD^"])]
    );
    assert_eq!(
        actual.commit.raw_bytes_fingerprint.as_str(),
        format!(
            "{:x}",
            Sha256::digest(fixture.git(&["cat-file", "commit", &commit]))
        )
    );
    let mut expected = fixture.tree.clone();
    expected.tasks[0] = changed;
    assert_eq!(
        actual.tree.documents().unwrap(),
        expected.documents().unwrap()
    );
    assert_eq!(
        actual.envelope.export_basis,
        directory::folder_basis_from_bytes(
            &fs::read(fixture.root.join(".engram/project.json")).unwrap()
        )
        .unwrap()
        .export_basis
    );
    assert_eq!(fixture.identity(), dirty_identity);
    assert_ne!(identity, dirty_identity);
}

#[test]
fn complete_snapshot_preserves_merge_parents_and_accepts_an_explicit_empty_tree() {
    let fixture = Fixture::new(2);
    let base = fixture.commit();
    fixture.git(&["checkout", "-b", "other"]);
    let mut other = fixture.tree.tasks[0].clone();
    other.title = "Other side".into();
    fixture.write(&other);
    let other_commit = fixture.commit();
    fixture.git(&["checkout", "main"]);
    let mut main = fixture.tree.tasks[1].clone();
    main.title = "Main side".into();
    fixture.write(&main);
    let main_commit = fixture.commit();
    fixture.git(&["merge", "--no-ff", "--no-edit", "other"]);
    let merged = fixture.text(&["rev-parse", "HEAD"]);
    let actual = snapshot(&fixture, &merged).unwrap();
    assert_eq!(actual.commit.parents, vec![main_commit, other_commit]);
    assert!(actual.tree.tasks.contains(&main));
    assert!(actual.tree.tasks.contains(&other));
    assert_eq!(snapshot(&fixture, &base).unwrap().tree.tasks.len(), 2);
    let empty = Fixture::new(0);
    let commit = empty.commit();
    assert_eq!(
        snapshot(&empty, &commit).unwrap().tree.tasks,
        [] as [TaskDocument; 0]
    );
}

#[test]
fn complete_snapshot_requires_exact_sha1_or_sha256_commit_objects() {
    for format in ["sha1", "sha256"] {
        let fixture = Fixture::with_format(1, format);
        let commit = fixture.commit();
        assert_eq!(snapshot(&fixture, &commit).unwrap().commit.oid, commit);
        for invalid in [&commit[..12], "HEAD"] {
            assert_eq!(
                code(&snapshot(&fixture, invalid).unwrap_err()),
                "task_sync_invalid_graph"
            );
        }
        if format == "sha256" {
            assert_eq!(
                code(&snapshot(&fixture, &commit[..40]).unwrap_err()),
                "task_sync_invalid_graph"
            );
        }
        fixture.git(&["tag", "-a", "not-a-commit", "-m", "Tag", &commit]);
        let tag = fixture.text(&["rev-parse", "not-a-commit"]);
        assert_eq!(
            code(&snapshot(&fixture, &tag).unwrap_err()),
            "task_sync_invalid_graph"
        );
    }
}

#[test]
fn complete_snapshot_refuses_unknown_paths_and_unchanged_unsafe_modes() {
    let fixture = Fixture::new(1);
    fixture.commit();
    let relative = Fixture::relative(fixture.tree.tasks[0].id);
    let blob = fixture.text(&["rev-parse", &format!("HEAD:{relative}")]);
    for mode in ["100755", "120000"] {
        fixture.git(&["update-index", "--cacheinfo", mode, &blob, &relative]);
        fixture.git(&["commit", "-m", "Unsafe task mode"]);
        let commit = fixture.text(&["rev-parse", "HEAD"]);
        assert_eq!(
            code(&snapshot(&fixture, &commit).unwrap_err()),
            "task_sync_destination_not_file"
        );
    }
    fixture.git(&["update-index", "--cacheinfo", "100644", &blob, &relative]);
    fixture.git(&["commit", "-m", "Ordinary task mode"]);
    for path in [
        ".engram/unexpected.json".to_owned(),
        format!(".engram/tasks/{}.json", fixture.tree.tasks[0].id.0.simple()),
    ] {
        fs::write(fixture.root.join(&path), b"{}").unwrap();
        let commit = fixture.commit();
        assert_eq!(
            code(&snapshot(&fixture, &commit).unwrap_err()),
            "task_sync_destination_unknown_entry"
        );
        fs::remove_file(fixture.root.join(path)).unwrap();
    }
}

#[test]
fn complete_snapshot_admits_graph_and_disclosure_in_every_document() {
    let fixture = Fixture::new(1);
    let mut task = fixture.tree.tasks[0].clone();
    task.prerequisites = vec![WorkId::new()];
    fixture.write(&task);
    let commit = fixture.commit();
    assert_eq!(
        code(&snapshot(&fixture, &commit).unwrap_err()),
        "task_sync_invalid_graph"
    );
    fixture.write(&fixture.tree.tasks[0]);
    let path = fixture.root.join(".engram/project.json");
    let original = fs::read(&path).unwrap();
    let mut envelope: serde_json::Value = serde_json::from_slice(&original).unwrap();
    envelope["folder_exchange"]["complete"] = false.into();
    fs::write(&path, serde_json::to_vec(&envelope).unwrap()).unwrap();
    let commit = fixture.commit();
    assert!(snapshot(&fixture, &commit).is_err());
    fs::write(path, original).unwrap();
    let mut task = fixture.tree.tasks[0].clone();
    task.title = " invalid whitespace ".into();
    fixture.write(&task);
    let commit = fixture.commit();
    assert_eq!(
        code(&snapshot(&fixture, &commit).unwrap_err()),
        "task_sync_invalid_graph"
    );
    fs::write(fixture.root.join(Fixture::relative(task.id)), b"not JSON").unwrap();
    let commit = fixture.commit();
    assert_eq!(
        code(&snapshot(&fixture, &commit).unwrap_err()),
        "task_sync_invalid_graph"
    );
    let mut task = fixture.tree.tasks[0].clone();
    task.notes.push(super::validation::note());
    fixture.write(&task);
    let commit = fixture.commit();
    assert_eq!(
        code(&snapshot(&fixture, &commit).unwrap_err()),
        "task_sync_invalid_graph"
    );
    fixture.profile(true, Some("fixture"));
    let commit = fixture.commit();
    assert_eq!(
        snapshot(&fixture, &commit).unwrap().tree.tasks[0].notes,
        task.notes
    );
    task.labels = vec!["restricted".into()];
    fixture.write(&task);
    let commit = fixture.commit();
    assert_eq!(
        code(&snapshot(&fixture, &commit).unwrap_err()),
        "task_sync_invalid_graph"
    );
}

#[test]
fn complete_snapshot_bounds_real_metadata_and_task_blob_bytes() {
    let fixture = Fixture::new(2);
    let commit = fixture.commit();
    let bytes = fs::read(fixture.root.join(".engram/project.json"))
        .unwrap()
        .len()
        + fixture
            .tree
            .tasks
            .iter()
            .map(|task| {
                fs::read(fixture.root.join(Fixture::relative(task.id)))
                    .unwrap()
                    .len()
            })
            .sum::<usize>();
    assert!(
        read_with_limit(
            &fixture.root,
            &fixture.tree.project.project_id,
            &commit,
            bytes
        )
        .is_ok()
    );
    let error = read_with_limit(
        &fixture.root,
        &fixture.tree.project.project_id,
        &commit,
        bytes - 1,
    )
    .unwrap_err();
    assert_eq!(code(&error), "task_sync_destination_tree_limit");
    let path = Fixture::relative(fixture.tree.tasks[0].id);
    fs::write(fixture.root.join(&path), vec![b'x'; 1024 * 1024 + 1]).unwrap();
    let commit = fixture.commit();
    let error = snapshot(&fixture, &commit).unwrap_err();
    assert_eq!(code(&error), "task_sync_destination_file_limit");
    assert!(error.to_string().contains(&path));
}
