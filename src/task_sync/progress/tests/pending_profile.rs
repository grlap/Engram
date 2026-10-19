use super::*;

fn selected(include_notes: bool, origin_machine: Option<&str>) -> ExportProfile {
    ExportProfile {
        include_notes,
        origin_machine: origin_machine.map(str::to_owned),
    }
}

fn assert_pending_guidance(message: &str) {
    for expected in [
        "original destination",
        "original include-notes setting",
        "origin-machine selection (including no origin-machine)",
        "without --rebaseline",
    ] {
        assert!(message.contains(expected), "{message}");
    }
    assert!(!message.contains("use --rebaseline to change it"));
}

#[test]
fn task_prepare_pending_profile_drift_guides_without_changing_retained_operation() {
    let alias = selected(false, Some("host-a"));
    let notes = selected(true, Some("host-a"));
    for (original, changed) in [
        (alias.clone(), notes.clone()),
        (notes, alias.clone()),
        (alias.clone(), selected(false, Some("host-b"))),
        (alias.clone(), selected(false, None)),
        (selected(false, None), alias),
    ] {
        for opted_out in [false, true] {
            for output_exists in [false, true] {
                let home = crate::test_support::temp_home().unwrap();
                let store = home.path().join("store.db");
                fs::write(&store, b"not SQLite; pending recovery must not open it").unwrap();
                let progress = home.path().join("progress");
                let output = home.path().join("candidate");
                let mut snapshot = tree(2);
                snapshot.project.notes_included = original.include_notes;
                let mut captured = capture(snapshot.clone());
                captured.cut = ExportCut {
                    position: 7,
                    anchor: Some(FeedAnchor {
                        object_kind: "work_event".into(),
                        object_id: ObjectId::from_canonical_bytes(b"pending profile cut"),
                    }),
                };
                prepare_with_producer(
                    &output,
                    &progress,
                    &store,
                    &snapshot.project.project_id,
                    &original,
                    false,
                    &producer(),
                    |_| Ok(captured),
                )
                .unwrap();
                let pending = journal::load(&progress).unwrap().current.unwrap();
                fs::remove_file(pending.directory.join("complete")).unwrap();
                if !output_exists {
                    assert!(
                        fs::canonicalize(&output)
                            .unwrap()
                            .starts_with(fs::canonicalize(home.path()).unwrap())
                    );
                    fs::remove_dir_all(&output).unwrap();
                }
                let before = listing(home.path());
                let retargeted = home.path().join("retargeted");
                for rebaseline in [false, true] {
                    for destination in [&output, &retargeted] {
                        let result = if opted_out {
                            resume_pending(
                                destination,
                                &progress,
                                &store,
                                &snapshot.project.project_id,
                                &changed,
                                rebaseline,
                            )
                        } else {
                            prepare_with_producer(
                                destination,
                                &progress,
                                &store,
                                &snapshot.project.project_id,
                                &changed,
                                rebaseline,
                                &producer(),
                                |_| panic!("pending profile refusal must not capture"),
                            )
                        };
                        let message = result.unwrap_err().to_string();
                        assert_pending_guidance(&message);
                        let prefix = match (opted_out, rebaseline) {
                            (false, false) => "prepared profile or origin-machine is pinned",
                            (false, true) => "pending preparation must finish before rebaseline",
                            (true, false) => "prepared project or profile differs from selection",
                            (true, true) => "task sync is not enabled; a new baseline cannot start",
                        };
                        assert!(message.contains(prefix), "{message}");
                        // Observe before the loader can clean any recognized scratch.
                        assert_eq!(listing(home.path()), before);
                    }
                }
                let retained = journal::load(&progress).unwrap().current.unwrap();
                assert!(!retained.complete);
                assert_eq!(retained.meta.operation, pending.meta.operation);
                assert_eq!(retained.meta.sequence, pending.meta.sequence);
                assert_eq!(retained.meta.cut, pending.meta.cut);
                assert_eq!(retained.meta.profile, original);
                assert_eq!(retained.meta.destination, pending.meta.destination);
                assert_eq!(retained.meta.producer, pending.meta.producer);
                assert_eq!(retained.documents, pending.documents);
                let recovered = if opted_out {
                    resume_pending(
                        &output,
                        &progress,
                        &store,
                        &snapshot.project.project_id,
                        &original,
                        false,
                    )
                } else {
                    prepare_with_producer(
                        &output,
                        &progress,
                        &store,
                        &snapshot.project.project_id,
                        &original,
                        false,
                        &producer(),
                        |_| panic!("pending profile recovery must not capture"),
                    )
                }
                .unwrap();
                assert!(recovered.recovered);
                assert_eq!(recovered.operation, pending.meta.operation);
                assert_eq!(recovered.project_position, pending.meta.cut.position);
                assert_eq!(
                    directory::read_documents(&output).unwrap(),
                    pending.documents
                );
                let completed = journal::load(&progress).unwrap().current.unwrap();
                assert!(completed.complete);
                assert_eq!(completed.meta.cut, pending.meta.cut);
            }
        }
    }
}

#[test]
fn task_prepare_completed_profile_drift_still_recommends_and_accepts_rebaseline() {
    let home = crate::test_support::temp_home().unwrap();
    let store = home.path().join("store.db");
    fs::write(&store, b"route").unwrap();
    let progress = home.path().join("progress");
    let first = home.path().join("first");
    let second = home.path().join("second");
    let snapshot = tree(1);
    let initial = prepare_with_producer(
        &first,
        &progress,
        &store,
        &snapshot.project.project_id,
        &profile(),
        false,
        &producer(),
        |_| Ok(capture(snapshot.clone())),
    )
    .unwrap();
    let changed = selected(false, Some("host-a"));
    let before = listing(home.path());
    let refused = prepare_with_producer(
        &second,
        &progress,
        &store,
        &snapshot.project.project_id,
        &changed,
        false,
        &producer(),
        |_| panic!("completed profile drift must refuse before capture"),
    )
    .unwrap_err();
    assert_eq!(
        refused.to_string(),
        "local work input is invalid: prepared profile or origin-machine is pinned; use --rebaseline to change it"
    );
    assert_eq!(listing(home.path()), before);
    let rebased = prepare_with_producer(
        &second,
        &progress,
        &store,
        &snapshot.project.project_id,
        &changed,
        true,
        &producer(),
        |_| Ok(capture(snapshot.clone())),
    )
    .unwrap();
    assert!(!rebased.recovered);
    assert_ne!(rebased.operation, initial.operation);
    assert_eq!(rebased.path, "full");
    assert_eq!(rebased.reason, "explicit rebaseline");
}
