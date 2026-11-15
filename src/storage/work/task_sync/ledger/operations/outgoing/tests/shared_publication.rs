use super::*;

fn git(root: &std::path::Path, args: &[&str]) -> String {
    let mut command = crate::task_sync::git::command(root, args);
    command
        .env("GIT_AUTHOR_NAME", "Publication fixture")
        .env("GIT_AUTHOR_EMAIL", "publication@example.invalid")
        .env("GIT_COMMITTER_NAME", "Publication fixture")
        .env("GIT_COMMITTER_EMAIL", "publication@example.invalid")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join("absent-global-config"));
    let bytes = crate::task_sync::git::run_query_command(
        command,
        args[0],
        false,
        None,
        crate::operation_time::MINIMUM,
        crate::task_sync::git::QueryStderrPolicy::ErrorOnly,
    )
    .unwrap();
    String::from_utf8(bytes).unwrap().trim().into()
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one publication workflow checks keyed capture and subsequent offline reads"
)]
fn shared_updates_full_folder_publication_keeps_ordinary_reads_offline_and_caches_git_by_commit() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let mut binding = binding(home.path());
    binding.profile = crate::task_sync::progress::ExportProfile {
        include_notes: true,
        origin_machine: Some("host-a".into()),
    };
    std::fs::create_dir_all(&binding.repository.main_checkout).unwrap();
    git(&binding.repository.main_checkout, &["init", "--quiet"]);
    let binding_id = store.activate_task_sync_binding(&binding).unwrap();
    let mut items = Vec::new();
    for index in 0..2 {
        let mut request = crate::storage::work::test_support::root_request(
            &binding.project.0,
            &format!("native-{index}"),
            0,
        );
        request.notes = vec![format!("Native prose {index}")];
        items.push(
            store
                .create_work(&request, &crate::DevelopmentNoopRedactor)
                .unwrap(),
        );
    }
    let exported = store
        .export_task_sync_folder(&binding.project, &binding.profile)
        .unwrap();
    crate::task_sync::directory::write_folder(
        &binding.repository.main_checkout.join(".engram"),
        &exported.tree,
        &binding.profile,
    )
    .unwrap();
    git(&binding.repository.main_checkout, &["add", "--", ".engram"]);
    git(
        &binding.repository.main_checkout,
        &["commit", "--quiet", "-m", "Publish task fixture"],
    );
    let oid = git(&binding.repository.main_checkout, &["rev-parse", "HEAD"]);
    let snapshot = crate::task_sync::git::read_task_snapshot(
        &binding.repository.git_directory,
        &binding.project,
        &oid,
    )
    .unwrap();
    let mut selected =
        super::super::super::tests::input(&store, &binding_id, &binding, exported.tree.clone());
    selected.intent.direction = Direction::Outgoing;
    selected.intent.local_planning_basis = selected.intent.candidate.planning_fingerprint.clone();
    selected.intent.commit.oid = snapshot.commit.oid;
    selected.intent.commit.tree = snapshot.commit.tree;
    selected.intent.commit.parents = snapshot.commit.parents;
    selected.intent.commit.raw_bytes_fingerprint = snapshot.commit.raw_bytes_fingerprint;
    std::fs::write(
        binding
            .repository
            .git_directory
            .join("engram-task-sync-commits")
            .join(format!("{}.json", selected.intent.operation)),
        crate::canonical::canonical_bytes(&selected.intent.commit).unwrap(),
    )
    .unwrap();
    store.prepare_task_sync_exchange(&selected, at(6)).unwrap();
    store
        .record_task_sync_exchange_progress(&selected, OutgoingProgress::LocalCommitRecorded, at(7))
        .unwrap();
    store
        .record_task_sync_exchange_progress(&selected, OutgoingProgress::PushStarted, at(8))
        .unwrap();
    store
        .record_task_sync_exchange_progress(
            &selected,
            OutgoingProgress::Observed(PublicationObservation::Confirmed {
                observed_tip: oid.clone(),
            }),
            at(9),
        )
        .unwrap();

    crate::storage::work::task_sync::origin::PUBLICATION_SNAPSHOT_READS.with(|count| count.set(0));
    assert_eq!(
        store
            .export_task_sync_folder(&binding.project, &binding.profile)
            .unwrap()
            .tree,
        exported.tree
    );
    crate::storage::work::task_sync::origin::PUBLICATION_SNAPSHOT_READS
        .with(|count| assert_eq!(count.get(), 1));
    assert!(store.verify_all().unwrap().is_healthy());
    std::fs::rename(
        &binding.repository.git_directory,
        home.path().join("offline-git"),
    )
    .unwrap();
    crate::storage::work::task_sync::origin::PUBLICATION_SNAPSHOT_READS.with(|count| count.set(0));
    for item in &items {
        let notes = store
            .work_record_index(
                &binding.project,
                item.work_id,
                crate::storage::WorkRecordKind::Notes,
            )
            .unwrap();
        assert_eq!(notes.len(), 1);
        let crate::storage::WorkRecordContent::Note(note) = store
            .work_record_content(&binding.project, item.work_id, &notes[0])
            .unwrap()
        else {
            panic!("native note expected");
        };
        assert!(note.summary.starts_with("Native prose"));
    }
    store
        .save_work_graph_snapshot(
            &binding.project,
            &actor("reader"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(10),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    crate::storage::work::task_sync::origin::PUBLICATION_SNAPSHOT_READS
        .with(|count| assert_eq!(count.get(), 0));
    let error = store
        .export_task_sync_folder(&binding.project, &binding.profile)
        .unwrap_err();
    assert_eq!(
        crate::store_error_value(&error)["error"]["code"],
        "task_sync_incremental_basis_unavailable"
    );
    assert!(error.to_string().contains(&oid));
}
