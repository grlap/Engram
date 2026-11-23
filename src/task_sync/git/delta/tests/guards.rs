use super::*;

#[test]
fn full_commit_ids_cannot_be_refs_tags_or_cross_format_abbreviations() {
    let sha1 = Fixture::new(1);
    let commit = sha1.commit();
    let alias = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    sha1.git(&["update-ref", &format!("refs/heads/{alias}"), &commit]);
    assert_eq!(
        code(&sha1.read(&alias, &commit).unwrap_err()),
        "task_sync_invalid_graph"
    );
    sha1.git(&["tag", "-a", "fixture-tag", "-m", "Tag fixture", &commit]);
    let tag = sha1.text(&["rev-parse", "fixture-tag"]);
    assert_eq!(
        code(&sha1.read(&tag, &commit).unwrap_err()),
        "task_sync_invalid_graph"
    );
    let sha256 = Fixture::with_format(1, "sha256");
    let full = sha256.commit();
    assert_eq!(full.len(), 64);
    assert!(sha256.read(&full, &full).unwrap().upserts.is_empty());
    assert_eq!(
        code(&sha256.read(&full[..40], &full).unwrap_err()),
        "task_sync_invalid_graph"
    );
}

#[test]
fn unchanged_metadata_modes_are_checked_in_each_basis() {
    let fixture = Fixture::new(1);
    let ordinary = fixture.commit();
    let blob = fixture.text(&["rev-parse", "HEAD:.engram/project.json"]);
    for mode in ["120000", "100755"] {
        fixture.git(&[
            "update-index",
            "--cacheinfo",
            mode,
            &blob,
            ".engram/project.json",
        ]);
        fixture.git(&["commit", "-m", "Change metadata mode"]);
        let invalid = fixture.text(&["rev-parse", "HEAD"]);
        for (old, new) in [
            (&invalid, &invalid),
            (&ordinary, &invalid),
            (&invalid, &ordinary),
        ] {
            assert_eq!(
                code(&fixture.read(old, new).unwrap_err()),
                "task_sync_destination_not_file"
            );
        }
        fixture.git(&[
            "update-index",
            "--cacheinfo",
            "100644",
            &blob,
            ".engram/project.json",
        ]);
        fixture.git(&["commit", "-m", "Restore metadata mode"]);
    }
    let relative = Fixture::relative(fixture.tree.tasks[0].id);
    fixture.git(&["update-index", "--force-remove", &relative]);
    fixture.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        "100644",
        &blob,
        ".engram/tasks",
    ]);
    fixture.git(&["commit", "-m", "Replace task tree with blob"]);
    let invalid = fixture.text(&["rev-parse", "HEAD"]);
    for (old, new) in [
        (&invalid, &invalid),
        (&ordinary, &invalid),
        (&invalid, &ordinary),
    ] {
        assert_eq!(
            code(&fixture.read(old, new).unwrap_err()),
            "task_sync_destination_not_directory"
        );
    }
}

#[test]
fn origin_alias_changes_are_admitted_and_embedded_bases_remain_unverified() {
    let fixture = Fixture::new(1);
    fixture.profile(true, Some("first"));
    let prior = fixture.commit();
    fixture.profile(true, Some("second"));
    let mut document = fixture.tree.tasks[0].clone();
    document.title = "Merged planning text".into();
    fixture.write(&document);
    let incoming = fixture.commit();
    let delta = fixture.read(&prior, &incoming).unwrap();
    assert_eq!(delta.prior.profile.origin_machine.as_deref(), Some("first"));
    assert_eq!(
        delta.incoming.profile.origin_machine.as_deref(),
        Some("second")
    );
    assert_eq!(delta.prior.export_basis, delta.incoming.export_basis);
    assert_eq!(delta.upserts, BTreeMap::from([(document.id, document)]));
}

#[test]
fn deadline_and_repository_failures_do_not_request_an_unhelpful_rebuild() {
    let fixture = Fixture::new(1);
    let prior = fixture.commit();
    let expired = Reader {
        repository: &fixture.root,
        started: Instant::now().checked_sub(BOUND).unwrap(),
        total_bound: BOUND,
    };
    let error = expired.commit_tree(&prior, "prior").unwrap_err();
    assert_eq!(code(&error), "task_sync_git_query_failed");
    assert!(error.to_string().contains("deadline"));
    assert!(!error.to_string().contains("rebuild"));
    let not_repository = fixture.root.parent().unwrap().join("not-a-repository");
    fs::create_dir(&not_repository).unwrap();
    let error = read_task_delta(
        &not_repository,
        &fixture.tree.project.project_id,
        &prior,
        &prior,
    )
    .unwrap_err();
    assert_eq!(code(&error), "task_sync_git_query_failed");
    assert!(!error.to_string().contains("rebuild"));
}

#[test]
fn oversized_blobs_and_other_responses_have_distinct_bounded_refusals() {
    let fixture = Fixture::new(1);
    let prior = fixture.commit();
    let relative = Fixture::relative(fixture.tree.tasks[0].id);
    fs::write(fixture.root.join(&relative), vec![b'x'; 1024 * 1024 + 1]).unwrap();
    let incoming = fixture.commit();
    let error = fixture.read(&prior, &incoming).unwrap_err();
    assert_eq!(code(&error), "task_sync_destination_file_limit");
    let refusal = error.downcast_ref::<StoreError>().unwrap();
    assert!(refusal.to_string().contains(&relative));
    let structured = crate::store_error_value(refusal);
    assert!(
        structured["error"]["message"]
            .as_str()
            .unwrap()
            .contains(&relative)
    );
    let blob = fixture.text(&["rev-parse", &format!("HEAD:{relative}")]);
    let reader = Reader {
        repository: &fixture.root,
        started: Instant::now(),
        total_bound: BOUND,
    };
    let error = reader.query(&["cat-file", "-p", &blob]).unwrap_err();
    assert_eq!(code(&error), "task_sync_destination_tree_limit");
}

#[test]
fn aggregate_payload_limit_counts_real_selected_blob_bytes() {
    let fixture = Fixture::new(2);
    let prior = fixture.commit();
    let mut total = 0;
    for original in &fixture.tree.tasks {
        let mut document = original.clone();
        document.title = "Changed task".into();
        fixture.write(&document);
        total += task_sync::pretty(&document).unwrap().len();
    }
    let incoming = fixture.commit();
    let changes = || {
        parse_changes(&fixture.git(&[
            "diff-tree",
            "-r",
            "--raw",
            "-z",
            "--no-commit-id",
            "--no-renames",
            "--no-abbrev",
            &prior,
            &incoming,
            "--",
            ".engram",
        ]))
        .unwrap()
    };
    let reader = Reader {
        repository: &fixture.root,
        started: Instant::now(),
        total_bound: BOUND,
    };
    // Exercise the production accumulator with a small budget, keeping the
    // fixture proportional rather than writing 128 MiB in the ordinary suite.
    assert_eq!(
        read_documents(
            &reader,
            &changes(),
            &fixture.tree.project.project_id,
            false,
            total
        )
        .unwrap()
        .0
        .len(),
        2
    );
    let error = read_documents(
        &reader,
        &changes(),
        &fixture.tree.project.project_id,
        false,
        total - 1,
    )
    .unwrap_err();
    assert_eq!(code(&error), "task_sync_destination_tree_limit");
}

fn changes_between(fixture: &Fixture, prior: &str, incoming: &str) -> Vec<Change> {
    parse_changes(&fixture.git(&[
        "diff-tree",
        "-r",
        "--raw",
        "-z",
        "--no-commit-id",
        "--no-renames",
        "--no-abbrev",
        prior,
        incoming,
        "--",
        ".engram",
    ]))
    .unwrap()
}

#[test]
fn a_document_over_its_file_limit_is_named_even_when_it_also_outgrows_the_byte_budget() {
    let fixture = Fixture::new(1);
    let prior = fixture.commit();
    let relative = Fixture::relative(fixture.tree.tasks[0].id);
    fs::write(fixture.root.join(&relative), vec![b'x'; 1024 * 1024 + 1]).unwrap();
    let incoming = fixture.commit();
    let project = &fixture.tree.project.project_id;
    // A cold reader has more time left than one Git process may take, so the
    // size check after a batch overrun must keep to the per-process bound too.
    let cold = Reader::cold(&fixture.root, project).unwrap();
    // The remaining byte budget is far below the one document, so the batch
    // output overruns it before that document's header can be checked.
    let error = read_documents(
        &cold,
        &changes_between(&fixture, &prior, &incoming),
        project,
        false,
        1024,
    )
    .unwrap_err();
    assert_eq!(code(&error), "task_sync_destination_file_limit");
    assert!(error.to_string().contains(&relative));
}

#[test]
fn a_reader_out_of_time_refuses_document_reads_as_a_failed_git_query() {
    let fixture = Fixture::new(1);
    let prior = fixture.commit();
    let mut edited = fixture.tree.tasks[0].clone();
    edited.title = "Changed task".into();
    fixture.write(&edited);
    let incoming = fixture.commit();
    let changes = changes_between(&fixture, &prior, &incoming);
    let reader = Reader {
        repository: &fixture.root,
        started: Instant::now(),
        total_bound: std::time::Duration::ZERO,
    };
    let error = read_documents(
        &reader,
        &changes,
        &fixture.tree.project.project_id,
        false,
        MAX_DELTA_BYTES,
    )
    .unwrap_err();
    assert_eq!(code(&error), "task_sync_git_query_failed");
}

#[test]
fn a_task_reader_keeps_its_minute_and_spends_bounded_or_cold_time_without_resetting() {
    let fixture = Fixture::new(1);
    let commit = fixture.commit();
    let project = &fixture.tree.project.project_id;
    let expected = fixture.text(&["rev-parse", "HEAD^{tree}"]);

    let mut ordinary = Reader::new(&fixture.root, project).unwrap();
    ordinary.started = Instant::now().checked_sub(Duration::from_secs(31)).unwrap();
    assert_eq!(ordinary.commit_tree(&commit, "prior").unwrap(), expected);
    ordinary.started = Instant::now().checked_sub(Duration::from_secs(61)).unwrap();
    let error = ordinary.commit_tree(&commit, "prior").unwrap_err();
    assert_eq!(code(&error), "task_sync_git_query_failed");
    assert!(error.to_string().contains("deadline"));

    // An internal phase can use less than a fresh minute; it must not renew
    // the caller's already spent time or mistake that remainder for an override.
    let mut bounded = Reader::bounded(&fixture.root, project, Duration::from_secs(29)).unwrap();
    assert_eq!(bounded.commit_tree(&commit, "prior").unwrap(), expected);
    bounded.started = Instant::now().checked_sub(Duration::from_secs(30)).unwrap();
    assert!(
        bounded
            .commit_tree(&commit, "prior")
            .unwrap_err()
            .to_string()
            .contains("deadline")
    );

    // Cold reads keep their longer allowance instead of being reduced to a
    // fresh minute when they acquire the common warning observer.
    let mut cold = Reader::cold(&fixture.root, project).unwrap();
    cold.started = Instant::now().checked_sub(Duration::from_secs(90)).unwrap();
    assert_eq!(cold.commit_tree(&commit, "prior").unwrap(), expected);
    cold.started = Instant::now()
        .checked_sub(Duration::from_secs(601))
        .unwrap();
    assert!(
        cold.commit_tree(&commit, "prior")
            .unwrap_err()
            .to_string()
            .contains("deadline")
    );
}
