use super::*;
use chrono::TimeZone;

fn staged(f: &Fixture, value: &str, head: Option<&str>) -> StagedCommitRequest {
    fs::create_dir_all(f.data.join(".engram")).unwrap();
    fs::write(f.data.join(".engram/project.json"), value).unwrap();
    run(&f.data, &["add", ".engram/project.json"]);
    StagedCommitRequest {
        operation: uuid::Uuid::new_v4(),
        tree: oid_line(&run(&f.data, &["write-tree"])).unwrap(),
        expected_head: head.map(str::to_owned),
        parents: head.into_iter().map(str::to_owned).collect(),
        author_name: "Explicit data author".into(),
        author_email: "explicit@example.invalid".into(),
        authored_at: chrono::Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
        message: "Record selected project data\n".into(),
    }
}

#[test]
fn staged_data_commit_replays_original_bytes_without_advancing_branch_or_changing_code_index() {
    let f = Fixture::new();
    let code_before = f.code_snapshot();
    let request = staged(&f, "{\"selection\":1}\n", None);
    let selected = DataWorktree::observe(&f.code, &f.data, "engram-data").unwrap();
    let index = fs::read(selected.git_directory.join("index")).unwrap();
    let retained = selected.create_retained(&request).unwrap();
    let raw = run(&f.data, &["cat-file", "commit", &retained.oid]);
    assert!(
        String::from_utf8(raw.clone())
            .unwrap()
            .contains("author Explicit data author <explicit@example.invalid>")
    );
    assert_eq!(retained.tree, request.tree);
    assert_eq!(
        DataWorktree::observe(&f.code, &f.data, "engram-data")
            .unwrap()
            .head,
        None
    );
    assert_eq!(selected.create_retained(&request).unwrap(), retained);
    assert_eq!(
        fs::read(selected.git_directory.join("index")).unwrap(),
        index
    );
    let advanced = selected.advance_retained(&retained, None).unwrap();
    assert_eq!(advanced.create_retained(&request).unwrap(), retained);
    assert_eq!(run(&f.data, &["cat-file", "commit", &retained.oid]), raw);
    assert_eq!(run(&f.data, &["rev-list", "--count", "HEAD"]), b"1\n");
    assert_eq!(f.code_snapshot(), code_before);
}

#[test]
fn staged_data_commit_preserves_changed_input_and_missing_pin_without_repair() {
    for case in ["message", "index", "branch", "pin", "author", "tree-kind"] {
        let f = Fixture::new();
        let mut request = staged(&f, "{\"selection\":1}\n", None);
        let selected = DataWorktree::observe(&f.code, &f.data, "engram-data").unwrap();
        let retained = selected.create_retained(&request).unwrap();
        match case {
            "message" => request.message = "Different selected operation\n".into(),
            "index" => {
                staged(&f, "{\"selection\":2}\n", None);
            }
            "branch" => {
                run(&f.data, &["symbolic-ref", "HEAD", "refs/heads/other-data"]);
            }
            "pin" => {
                run(
                    &f.data,
                    &["update-ref", "--no-deref", "-d", &retained.retention_ref],
                );
            }
            "author" => request.author_name = "Injected\nparent forged".into(),
            "tree-kind" => request.tree = retained.oid.clone(),
            _ => unreachable!(),
        }
        let index = fs::read(selected.git_directory.join("index")).unwrap();
        let refs = run(
            &f.data,
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        );
        let code = f.code_snapshot();
        let objects = run(
            &f.data,
            &[
                "cat-file",
                "--batch-all-objects",
                "--batch-check=%(objectname)",
            ],
        );
        assert!(selected.create_retained(&request).is_err(), "{case}");
        assert_eq!(
            fs::read(selected.git_directory.join("index")).unwrap(),
            index,
            "{case}"
        );
        assert_eq!(
            run(
                &f.data,
                &["for-each-ref", "--format=%(refname) %(objectname)"]
            ),
            refs,
            "{case}"
        );
        assert_eq!(f.code_snapshot(), code, "{case}");
        assert_eq!(
            run(
                &f.data,
                &[
                    "cat-file",
                    "--batch-all-objects",
                    "--batch-check=%(objectname)"
                ]
            ),
            objects,
            "{case}"
        );
        assert_eq!(
            crate::task_sync::retention::commit_selection(
                &selected.git_directory,
                request.operation
            )
            .unwrap(),
            Some(retained),
            "{case}"
        );
    }
}

#[test]
fn staged_data_commit_retains_both_resolved_data_parents_and_refuses_code_ancestry() {
    let f = Fixture::new();
    let code_before = f.code_snapshot();
    let first_request = staged(&f, "{\"selection\":1}\n", None);
    let selected = DataWorktree::observe(&f.code, &f.data, "engram-data").unwrap();
    let first = selected.create_retained(&first_request).unwrap();
    let selected = selected.advance_retained(&first, None).unwrap();
    let peer_request = staged(&f, "{\"selection\":2}\n", Some(&first.oid));
    let peer = selected.create_retained(&peer_request).unwrap();
    let local_request = staged(&f, "{\"selection\":3}\n", Some(&first.oid));
    let local = selected.create_retained(&local_request).unwrap();
    let selected = selected.advance_retained(&local, Some(&first.oid)).unwrap();
    let mut resolved_request = staged(&f, "{\"selection\":4}\n", Some(&local.oid));
    resolved_request.parents.push(peer.oid.clone());
    let resolved = selected.create_retained(&resolved_request).unwrap();
    assert_eq!(resolved.parents, vec![local.oid.clone(), peer.oid]);
    let selected = selected
        .advance_retained(&resolved, Some(&local.oid))
        .unwrap();
    let mut unsafe_request = staged(&f, "{\"selection\":4}\n", Some(&resolved.oid));
    unsafe_request
        .parents
        .push(oid_line(&run(&f.code, &["rev-parse", "HEAD"])).unwrap());
    let refs = run(
        &f.data,
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    );
    assert!(selected.create_retained(&unsafe_request).is_err());
    assert_eq!(
        run(
            &f.data,
            &["for-each-ref", "--format=%(refname) %(objectname)"]
        ),
        refs
    );
    assert_eq!(run(&f.data, &["rev-list", "--count", "HEAD"]), b"4\n");
    assert_eq!(f.code_snapshot(), code_before);
}
