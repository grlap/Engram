use super::*;

#[test]
fn task_prepare_pending_source_changes_refuse_without_capture_or_progress_advance() {
    for binding in ["store", "project"] {
        for rebaseline in [false, true] {
            let home = crate::test_support::temp_home().unwrap();
            let store = home.path().join("store.db");
            let relocated = home.path().join("relocated.db");
            fs::write(&store, b"route").unwrap();
            fs::write(&relocated, b"route").unwrap();
            let progress = home.path().join("progress");
            let output = home.path().join("candidate");
            let snapshot = tree(2);
            prepare_with_producer(
                &output,
                &progress,
                &store,
                &snapshot.project.project_id,
                &profile(),
                false,
                &producer(),
                |_| Ok(capture(snapshot.clone())),
            )
            .unwrap();
            let pending = journal::load(&progress).unwrap().current.unwrap();
            fs::remove_file(pending.directory.join("complete")).unwrap();
            let state = fs::read(pending.directory.join("state.json")).unwrap();
            let bytes = directory::read_documents(&output).unwrap();
            assert!(
                fs::canonicalize(&output)
                    .unwrap()
                    .starts_with(fs::canonicalize(home.path()).unwrap())
            );
            fs::remove_dir_all(&output).unwrap();
            let other_project = ProjectId("another-project".into());
            let target = home.path().join("retargeted");
            let result = prepare_with_producer(
                &target,
                &progress,
                if binding == "store" {
                    &relocated
                } else {
                    &store
                },
                if binding == "project" {
                    &other_project
                } else {
                    &snapshot.project.project_id
                },
                &profile(),
                rebaseline,
                &producer(),
                |_| panic!("a pending binding refusal must not capture newer state"),
            );
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("original store and project"),
                "{binding}, rebaseline={rebaseline}"
            );
            assert!(!target.exists());
            let retained = journal::load(&progress).unwrap().current.unwrap();
            assert!(!retained.complete);
            assert_eq!(retained.meta.operation, pending.meta.operation);
            assert_eq!(retained.meta.sequence, pending.meta.sequence);
            assert_eq!(
                fs::read(retained.directory.join("state.json")).unwrap(),
                state
            );
            assert!(!output.exists());
            assert_eq!(fs::read_dir(&progress).unwrap().count(), 2);
            let recovered = prepare_with_producer(
                &output,
                &progress,
                &store,
                &snapshot.project.project_id,
                &profile(),
                false,
                &producer(),
                |_| panic!("original pending operation must resume without capture"),
            )
            .unwrap();
            assert_eq!(recovered.operation, pending.meta.operation);
            assert!(recovered.recovered);
            assert_eq!(directory::read_documents(&output).unwrap(), bytes);
        }
    }
}

#[test]
fn task_prepare_divergent_destination_refuses_before_advancing_completed_progress() {
    let home = crate::test_support::temp_home().unwrap();
    let store = home.path().join("store.db");
    fs::write(&store, b"route").unwrap();
    let progress = home.path().join("progress");
    let output = home.path().join("candidate");
    let mut snapshot = tree(2);
    let first = prepare_with_producer(
        &output,
        &progress,
        &store,
        &snapshot.project.project_id,
        &profile(),
        false,
        &producer(),
        |_| Ok(capture(snapshot.clone())),
    )
    .unwrap();
    let original = directory::read_documents(&output).unwrap();
    snapshot.tasks[0].title.push_str(" changed");
    for rebaseline in [false, true] {
        let error = prepare_with_producer(
            &output,
            &progress,
            &store,
            &snapshot.project.project_id,
            &profile(),
            rebaseline,
            &producer(),
            |_| Ok(capture(snapshot.clone())),
        )
        .unwrap_err();
        assert!(error.to_string().contains("different candidate"));
        let retained = journal::load(&progress).unwrap().current.unwrap();
        assert!(retained.complete);
        assert_eq!(retained.meta.operation, first.operation);
        assert_eq!(retained.meta.sequence, 1);
        assert_eq!(directory::read_documents(&output).unwrap(), original);
        assert_eq!(fs::read_dir(&progress).unwrap().count(), 2);
    }
    let other = home.path().join("new-output");
    let receipt = prepare_with_producer(
        &other,
        &progress,
        &store,
        &snapshot.project.project_id,
        &profile(),
        false,
        &producer(),
        |_| Ok(capture(snapshot.clone())),
    )
    .unwrap();
    assert_eq!(receipt.serialized_tasks, 1);
    assert_eq!(
        directory::read_documents(&other).unwrap(),
        snapshot.documents().unwrap()
    );
}

#[test]
fn task_prepare_failed_pending_publication_keeps_cut_and_refuses_retargeting() {
    let home = crate::test_support::temp_home().unwrap();
    let store = home.path().join("store.db");
    fs::write(&store, b"route").unwrap();
    let progress = home.path().join("progress");
    let output = home.path().join("candidate");
    let snapshot = tree(2);
    prepare_with_producer(
        &output,
        &progress,
        &store,
        &snapshot.project.project_id,
        &profile(),
        false,
        &producer(),
        |_| Ok(capture(snapshot.clone())),
    )
    .unwrap();
    let pending = journal::load(&progress).unwrap().current.unwrap();
    // Simulate exit before completion and a concurrent change to published output.
    fs::remove_file(pending.directory.join("complete")).unwrap();
    let mut divergent = snapshot.clone();
    divergent.tasks[0].title.push_str(" different");
    fs::write(
        output.join(format!("tasks/{}.json", divergent.tasks[0].id.0)),
        pretty(&divergent.tasks[0]).unwrap(),
    )
    .unwrap();
    let invoke = |destination: &Path, rebaseline| {
        prepare_with_producer(
            destination,
            &progress,
            &store,
            &snapshot.project.project_id,
            &profile(),
            rebaseline,
            &producer(),
            |_| panic!("pending retry must not capture later source changes"),
        )
    };
    assert!(
        invoke(&output, false)
            .unwrap_err()
            .to_string()
            .contains("different candidate")
    );
    assert_eq!(
        directory::read_documents(&output).unwrap(),
        divergent.documents().unwrap()
    );
    let other = home.path().join("other");
    assert!(
        invoke(&other, false)
            .unwrap_err()
            .to_string()
            .contains("must resume")
    );
    assert!(
        invoke(&output, true)
            .unwrap_err()
            .to_string()
            .contains("must finish")
    );
    assert!(!other.exists());
    assert!(
        fs::canonicalize(&output)
            .unwrap()
            .starts_with(fs::canonicalize(home.path()).unwrap())
    );
    fs::remove_dir_all(&output).unwrap();
    let recovered = invoke(&output, false).unwrap();
    assert!(recovered.recovered);
    assert_eq!(recovered.operation, pending.meta.operation);
    assert_eq!(
        directory::read_documents(&output).unwrap(),
        snapshot.documents().unwrap()
    );
    assert!(journal::load(&progress).unwrap().current.unwrap().complete);
}
