use super::*;

mod process;

#[test]
fn task_materialize_admitted_replacement_plans_are_bound_before_any_public_edit() {
    for index in 0..2 {
        for changed in [
            "bytes",
            "plan-identity",
            "pending-identity",
            "directory-identity",
        ] {
            let fixture = Fixture::new(false, 1);
            fixture.interrupt("journal-published");
            let attempt = fixture.private().join(format!("replace-{index}"));
            match changed {
                "bytes" => fs::write(attempt.join("plan"), b"changed plan").unwrap(),
                "plan-identity" | "pending-identity" => {
                    let name = if changed == "plan-identity" {
                        "plan"
                    } else {
                        ".plan.pending"
                    };
                    let bytes = fs::read(attempt.join(name)).unwrap();
                    fs::rename(
                        attempt.join(name),
                        fixture.owned.path().join("preserved-plan"),
                    )
                    .unwrap();
                    fs::write(attempt.join(name), bytes).unwrap();
                }
                "directory-identity" => {
                    let preserved = fixture.owned.path().join("preserved-attempt");
                    fs::rename(&attempt, &preserved).unwrap();
                    fs::create_dir(&attempt).unwrap();
                    for name in ["plan", ".plan.pending"] {
                        fs::hard_link(preserved.join(name), attempt.join(name)).unwrap();
                    }
                }
                _ => unreachable!(),
            }
            fixture.disable();
            let before = snapshot(fixture.owned.path());
            let error = fixture
                .service
                .materialize_task_sync_setup(&fixture.marker, &fixture.selection)
                .unwrap_err();
            assert!(
                error.to_string().contains("replacement"),
                "{index}/{changed}: {error:#}"
            );
            assert_eq!(snapshot(fixture.owned.path()), before);
        }
    }
}

#[test]
fn task_materialize_completion_repair_refuses_unknown_prefixes_and_replaced_sources() {
    for changed in [
        "pending",
        "source-prefix",
        "source-identity",
        "directory-identity",
    ] {
        let fixture = Fixture::new(true, 1);
        fixture.interrupt("public-checked");
        let private = fixture.private();
        fs::write(private.join(".complete.pending"), b"").unwrap();
        let source = private.join("completion-recovery/source");
        match changed {
            "pending" => fs::write(private.join(".complete.pending"), b"wrong prefix").unwrap(),
            "source-prefix" => fs::write(&source, b"wrong prefix").unwrap(),
            "source-identity" => {
                fs::rename(&source, fixture.owned.path().join("preserved-source")).unwrap();
                fs::write(&source, b"").unwrap();
            }
            "directory-identity" => {
                fs::rename(
                    private.join("completion-recovery"),
                    fixture.owned.path().join("preserved-recovery"),
                )
                .unwrap();
                fs::create_dir(private.join("completion-recovery")).unwrap();
                fs::write(&source, b"").unwrap();
            }
            _ => unreachable!(),
        }
        fixture.disable();
        let before = snapshot(fixture.owned.path());
        let error = fixture
            .service
            .materialize_task_sync_setup(&fixture.marker, &fixture.selection)
            .unwrap_err();
        assert!(error.to_string().contains("completion"), "{error:#}");
        assert_eq!(snapshot(fixture.owned.path()), before, "{changed}");
    }
}

#[test]
fn task_materialize_recovers_each_admitted_publication_boundary_after_opt_out() {
    for existing in [false, true] {
        let mut boundaries = vec![
            "journal-published",
            "replacement-ready-0",
            "before-move-0",
            "moved-0",
            "before-setup-link-0",
            "setup-linked-0",
            "replacement-ready-1",
            "before-move-1",
            "moved-1",
            "before-setup-link-1",
            "setup-linked-1",
            "setup-checked",
            "public-checked",
            "complete-published",
        ];
        if existing {
            boundaries.extend([
                "before-tasks-rename",
                "tasks-renamed",
                "before-project-link",
                "project-linked",
            ]);
        } else {
            boundaries.extend(["before-engram-rename", "engram-renamed"]);
        }
        for boundary in boundaries {
            let fixture = Fixture::new(existing, 2);
            fixture.interrupt(boundary);
            fixture.disable();
            let recovered = fixture.run();
            assert!(recovered.recovered, "{boundary}");
            fixture.assert_output();
            let before = snapshot(fixture.owned.path());
            assert!(fixture.run().repeated);
            assert_eq!(snapshot(fixture.owned.path()), before);
        }
    }
}

#[test]
fn task_materialize_preserves_incomplete_private_attempts_without_adopting_them() {
    for boundary in ["private-created", "replace-created-0", "replace-created-1"] {
        let fixture = Fixture::new(false, 1);
        fixture.interrupt(boundary);
        let before = snapshot(fixture.owned.path());
        let error = fixture
            .service
            .materialize_task_sync_setup(&fixture.marker, &fixture.selection)
            .unwrap_err();
        assert!(error.to_string().contains("incomplete"), "{error:#}");
        assert_eq!(snapshot(fixture.owned.path()), before);
    }
}

#[test]
fn task_materialize_first_private_attempt_failure_names_the_retained_recovery_path() {
    let fixture = Fixture::new(false, 1);
    let ordinary_code = fs::read(fixture.root.join("code.txt")).unwrap();
    let error = super::super::run(
        &fixture.marker,
        &ProjectId("materialize/fixture".into()),
        &fixture.selection,
        true,
        attribution(),
        &mut |point| {
            if point == "private-created" {
                fs::write(
                    fixture.root.join(".gitignore"),
                    b"concurrent ordinary public edit\n",
                )?;
            }
            Ok(())
        },
    )
    .unwrap_err();
    let message = format!("{error:#}");
    let private = fixture.private();
    assert!(
        message.contains("incomplete private materialization attempt at"),
        "{message}"
    );
    let printed_attempt = message
        .strip_prefix("incomplete private materialization attempt at ")
        .and_then(|message| message.split_once("; preserve"))
        .map(|(path, _)| Path::new(path))
        .expect("first refusal prints the retained attempt path");
    assert!(same_file::is_same_file(&private, printed_attempt).unwrap());
    assert!(message.contains("move this attempt aside within Git administration"));
    assert!(message.contains("rerun setup preparation and materialization"));
    assert_eq!(
        fs::read(fixture.root.join("code.txt")).unwrap(),
        ordinary_code
    );
    assert_eq!(
        fs::read(fixture.root.join(".gitignore")).unwrap(),
        b"concurrent ordinary public edit\n"
    );
    assert!(private.is_dir());
}

#[test]
fn task_materialize_operator_can_preserve_and_reprepare_incomplete_private_attempts() {
    for boundary in ["private-created", "replace-created-0", "replace-created-1"] {
        let fixture = Fixture::new(false, 1);
        fixture.interrupt(boundary);
        let private = fixture.private();
        let before = snapshot(&private);
        let preserved = fixture.admin.join("preserved-incomplete-materialization");
        fs::rename(&private, &preserved).unwrap();
        assert_eq!(snapshot(&preserved), before);
        let reprepare = fixture.reprepare();
        assert!(reprepare.repeated, "{boundary}");
        assert_eq!(reprepare.operation, fixture.selection.setup_operation);
        let fresh = fixture.run();
        assert!(!fresh.recovered, "{boundary}");
        fixture.assert_output();
        assert_eq!(snapshot(&preserved), before);
        let repeated = fixture.run();
        assert!(repeated.repeated, "{boundary}");
        assert_eq!(repeated.operation, fresh.operation);
        assert_eq!(snapshot(&preserved), before);
    }
}

#[test]
fn task_materialize_operator_can_restore_managed_bytes_after_policy_refusal() {
    let fixture = Fixture::new(false, 1);
    let error = super::super::run(
        &fixture.marker,
        &ProjectId("materialize/fixture".into()),
        &fixture.selection,
        true,
        attribution(),
        &mut |point| {
            if point == "setup-linked-1" {
                let ignore = fixture.root.join(".gitignore");
                let mut bytes = fs::read(&ignore)?;
                bytes.extend_from_slice(b"/.engram/tasks/\n");
                fs::write(ignore, bytes)?;
            }
            Ok(())
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("shared task path is ignored"));
    let private = Directory::open(&fixture.private()).unwrap();
    let record = super::super::journal::read(&private, &fixture.private()).unwrap();
    for edit in &record.basis.edits {
        fs::write(fixture.root.join(&edit.path), &edit.after).unwrap();
    }
    drop(private);
    fixture.run();
    fixture.assert_output();
}

#[test]
fn task_materialize_recovers_without_the_source_store_or_b1_generation() {
    let fixture = Fixture::new(true, 2);
    fixture.interrupt("journal-published");
    fixture.disable();
    fs::write(&fixture.store, b"independent store replacement").unwrap();
    // Relocate the owned source rather than deleting a tree through a test helper.
    fs::rename(
        &fixture.selection.progress_directory,
        fixture.owned.path().join("pruned-progress"),
    )
    .unwrap();
    fs::rename(
        &fixture.selection.candidate_directory,
        fixture.owned.path().join("old-candidate"),
    )
    .unwrap();
    let receipt = fixture.run();
    assert!(receipt.recovered);
    assert_eq!(
        fs::read(&fixture.store).unwrap(),
        b"independent store replacement"
    );
    assert_shared(&fixture);
}

#[test]
fn task_materialize_setup_move_mismatch_restores_or_reports_the_displaced_bytes() {
    for blocked in [false, true] {
        let fixture = Fixture::new(false, 1);
        let changed = b"concurrent ordinary public edit\n";
        let replacement = b"another concurrent root target\n";
        let result = super::super::run(
            &fixture.marker,
            &ProjectId("materialize/fixture".into()),
            &fixture.selection,
            true,
            attribution(),
            &mut |point| {
                if point == "before-move-0" {
                    fs::write(fixture.root.join(".gitignore"), changed)?;
                }
                if blocked && point == "before-restore-0" {
                    fs::write(fixture.root.join(".gitignore"), replacement)?;
                }
                Ok(())
            },
        );
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains(&format!("restore_incomplete={blocked}")),
            "{error}"
        );
        assert_eq!(
            fs::read(fixture.private().join("replace-0/displaced")).unwrap(),
            changed
        );
        assert_eq!(
            fs::read(fixture.root.join(".gitignore")).unwrap(),
            if blocked {
                replacement.as_slice()
            } else {
                changed.as_slice()
            }
        );
        assert!(!fixture.root.join(".engram/project.json").exists());
        let before = snapshot(fixture.owned.path());
        let repeated_error = fixture
            .service
            .materialize_task_sync_setup(&fixture.marker, &fixture.selection)
            .unwrap_err();
        assert!(
            repeated_error
                .to_string()
                .contains(&format!("restore_incomplete={blocked}")),
            "{repeated_error:#}"
        );
        assert_eq!(snapshot(fixture.owned.path()), before);
    }
}
