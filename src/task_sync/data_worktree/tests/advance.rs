use super::*;
use crate::task_sync::{ledger::RetainedCommit, retention};

fn retain(fixture: &Fixture, parent: Option<&str>) -> RetainedCommit {
    retain_parents(fixture, &parent.into_iter().collect::<Vec<_>>())
}

fn retain_parents(fixture: &Fixture, parents: &[&str]) -> RetainedCommit {
    let tree = oid_line(&run(&fixture.data, &["write-tree"])).unwrap();
    let mut args = vec!["commit-tree", &tree, "-m", "Publish resolved data"];
    for parent in parents {
        args.extend(["-p", *parent]);
    }
    let oid = oid_line(&run(&fixture.data, &args)).unwrap();
    let observed =
        git::observe_commit(&oid, &run(&fixture.data, &["cat-file", "commit", &oid])).unwrap();
    let operation = uuid::Uuid::new_v4();
    let retained = RetainedCommit {
        oid,
        tree,
        parents: observed.parents,
        retention_ref: format!("refs/engram/task-sync/exchange/{operation}/commit"),
        raw_bytes_fingerprint: observed.raw_bytes_fingerprint,
    };
    let selected = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data").unwrap();
    let guard = retention::audit::AuditReservation::acquire(&selected.git_directory).unwrap();
    retention::reserve_commit(&selected.git_directory, operation, &retained, &guard).unwrap();
    run(
        &fixture.data,
        &[
            "update-ref",
            "--no-deref",
            &retained.retention_ref,
            &retained.oid,
            &"0".repeat(retained.oid.len()),
        ],
    );
    retained
}

fn stage(fixture: &Fixture, value: &str) {
    fs::create_dir_all(fixture.data.join(".engram")).unwrap();
    fs::write(fixture.data.join(".engram/project.json"), value).unwrap();
    run(&fixture.data, &["add", ".engram/project.json"]);
}

#[test]
fn retained_data_advance_replays_original_commit_and_keeps_code_unchanged() {
    let fixture = Fixture::new();
    let code_before = fixture.code_snapshot();
    stage(&fixture, "{\"selection\":1}\n");
    let first = retain(&fixture, None);
    let selected = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data").unwrap();
    let raw_commit = run(&fixture.data, &["cat-file", "commit", &first.oid]);
    let index_before = fs::read(selected.git_directory.join("index")).unwrap();
    let published = selected.advance_retained(&first, None).unwrap();
    assert_eq!(published.head.as_ref(), Some(&first.oid));
    // Both the original observation and a fresh observation replay after a lost response.
    selected.advance_retained(&first, None).unwrap();
    published.advance_retained(&first, None).unwrap();
    assert_eq!(
        run(&fixture.data, &["cat-file", "commit", &first.oid]),
        raw_commit
    );
    assert_eq!(
        fs::read(selected.git_directory.join("index")).unwrap(),
        index_before
    );
    assert_eq!(run(&fixture.data, &["rev-list", "--count", "HEAD"]), b"1\n");
    stage(&fixture, "{\"selection\":2}\n");
    let second = retain(&fixture, Some(&first.oid));
    let published = published
        .advance_retained(&second, Some(&first.oid))
        .unwrap();
    assert_eq!(published.head.as_ref(), Some(&second.oid));
    assert_eq!(run(&fixture.data, &["rev-list", "--count", "HEAD"]), b"2\n");
    stage(&fixture, "{\"selection\":3}\n");
    let peer = retain(&fixture, Some(&first.oid));
    assert!(
        published
            .advance_retained(&peer, Some(&second.oid))
            .is_err(),
        "a divergent peer alone is not a local fast-forward"
    );
    let resolved = retain_parents(&fixture, &[&second.oid, &peer.oid]);
    let merged = published
        .advance_retained(&resolved, Some(&second.oid))
        .unwrap();
    assert_eq!(merged.head.as_ref(), Some(&resolved.oid));
    assert_eq!(run(&fixture.data, &["rev-list", "--count", "HEAD"]), b"4\n");
    assert_eq!(fixture.code_snapshot(), code_before);
}

#[test]
fn retained_data_advance_preserves_changed_index_missing_pin_and_competing_head() {
    let fixture = Fixture::new();
    stage(&fixture, "{\"selection\":1}\n");
    let first = retain(&fixture, None);
    let selected = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data").unwrap();
    stage(&fixture, "{\"selection\":2}\n");
    let index = fs::read(selected.git_directory.join("index")).unwrap();
    assert!(
        selected
            .advance_retained(&first, None)
            .unwrap_err()
            .to_string()
            .contains("staged data tree differs")
    );
    assert_eq!(
        fs::read(selected.git_directory.join("index")).unwrap(),
        index
    );
    assert_eq!(
        DataWorktree::observe(&fixture.code, &fixture.data, "engram-data")
            .unwrap()
            .head,
        None
    );
    stage(&fixture, "{\"selection\":1}\n");
    run(&fixture.data, &["update-ref", "-d", &first.retention_ref]);
    assert!(
        selected
            .advance_retained(&first, None)
            .unwrap_err()
            .to_string()
            .contains("pin changed")
    );
    run(
        &fixture.data,
        &["update-ref", &first.retention_ref, &first.oid],
    );
    let competing = retain(&fixture, None);
    // Make a distinct retained commit even if this machine's clock has not ticked.
    let tree = first.tree.clone();
    let other = oid_line(&run(
        &fixture.data,
        &["commit-tree", &tree, "-m", "Competing data"],
    ))
    .unwrap();
    run(
        &fixture.data,
        &["update-ref", "refs/heads/engram-data", &other],
    );
    let error = selected.advance_retained(&competing, None).unwrap_err();
    assert!(error.to_string().contains("competing head"));
    assert_eq!(
        oid_line(&run(&fixture.data, &["rev-parse", "HEAD"])).unwrap(),
        other
    );
    assert_eq!(
        fs::read_to_string(fixture.data.join(".engram/project.json")).unwrap(),
        "{\"selection\":1}\n"
    );
}
