//! Derived setup sizes and preservation at public recovery boundaries.

use super::*;

fn response(fixture: &Fixture) -> serde_json::Value {
    serde_json::from_slice(
        &fs::read(fixture.owned.path().join("process-finish-response.json")).unwrap(),
    )
    .unwrap()
}

#[test]
fn task_materialize_maximal_setup_inputs_complete_and_recover_after_opt_out() {
    // No final LF exercises the additional separator byte as well as the block.
    let bytes = vec![b'#'; 64 * 1024];
    for boundary in [None, Some("setup-linked-1")] {
        let fixture = Fixture::with_setup_bytes(false, &tree(1), Some([&bytes, &bytes]));
        let setup = intent::read(
            &Directory::open(&fixture.admin).unwrap(),
            fixture.selection.setup_operation,
        )
        .unwrap();
        assert!(
            setup
                .basis
                .edits
                .iter()
                .all(|edit| edit.after.len() > bytes.len())
        );
        let operation = if let Some(boundary) = boundary {
            assert_eq!(
                child(&fixture, Some(boundary), true).status.code(),
                Some(77)
            );
            let journal: serde_json::Value =
                serde_json::from_slice(&fs::read(fixture.private().join("journal.json")).unwrap())
                    .unwrap();
            fixture.disable();
            Some(journal["operation"].clone())
        } else {
            None
        };
        let linked = boundary.map(|_| {
            [".gitignore", ".gitattributes"].map(|name| {
                file_identity(&fs::File::open(fixture.root.join(name)).unwrap()).unwrap()
            })
        });
        assert!(child(&fixture, None, boundary.is_none()).status.success());
        let result = response(&fixture);
        assert!(result.get("error").is_none(), "{result}");
        if let Some(operation) = operation {
            assert_eq!(result["receipt"]["operation"], operation);
        }
        for (index, edit) in setup.basis.edits.iter().enumerate() {
            assert_eq!(fs::read(fixture.root.join(&edit.path)).unwrap(), edit.after);
            if let Some(ids) = linked {
                assert_eq!(
                    file_identity(&fs::File::open(fixture.root.join(&edit.path)).unwrap()).unwrap(),
                    ids[index]
                );
            }
        }
        fixture.assert_output();
        let before = snapshot(fixture.owned.path());
        assert!(fixture.run().repeated);
        assert_eq!(snapshot(fixture.owned.path()), before);
    }
}

#[test]
fn task_materialize_occupied_root_after_move_preserves_both_paths_across_restart() {
    for index in 0..2 {
        let fixture = Fixture::new(false, 1);
        assert_eq!(
            child(&fixture, Some(&format!("moved-{index}")), true)
                .status
                .code(),
            Some(77)
        );
        let name = [".gitignore", ".gitattributes"][index];
        let public = fixture.root.join(name);
        let retained = fixture.private().join(format!("replace-{index}/displaced"));
        fs::write(&public, b"independent occupied public bytes\n").unwrap();
        fixture.disable();
        let before_public = snapshot(&public);
        let before_private = snapshot(&fixture.private());
        assert!(child(&fixture, None, false).status.success());
        let result = response(&fixture);
        let error = result["error"].as_str().unwrap();
        assert!(
            error.contains("public path") && error.contains("is occupied"),
            "{error}"
        );
        assert!(error.contains(&public.display().to_string()), "{error}");
        assert!(
            error
                .replace('\\', "/")
                .contains(&retained.display().to_string().replace('\\', "/")),
            "{error}"
        );
        assert_eq!(snapshot(&public), before_public);
        assert_eq!(snapshot(&fixture.private()), before_private);
    }
}

#[test]
fn task_materialize_owned_fresh_shared_tree_preserves_private_sibling_on_recovery_and_repeat() {
    let fixture = Fixture::new(false, 1);
    assert_eq!(
        child(&fixture, Some("engram-renamed"), true).status.code(),
        Some(77)
    );
    let public = fixture.root.join(".engram");
    fs::write(public.join("private.db"), b"unrelated private sibling\n").unwrap();
    let before = snapshot(&public);
    fixture.disable();
    assert!(child(&fixture, None, false).status.success());
    let result = response(&fixture);
    assert!(result.get("error").is_none(), "{result}");
    assert_eq!(snapshot(&public), before);
    let completed = snapshot(fixture.owned.path());
    assert!(fixture.run().repeated);
    assert_eq!(snapshot(fixture.owned.path()), completed);
}

#[test]
fn task_materialize_reserves_directory_capacity_before_publication_and_repeat() {
    let ignore = &b"/.engram/\n/target/\n"[..];
    let attributes = &b"*.md text eol=lf\n"[..];
    // Each reserved root name is independently missing. All-absent alone
    // cannot distinguish a duplicate reservation from the intended names.
    for (existing, setup) in [
        (false, [Some(ignore), Some(attributes)]),
        (true, [None, Some(attributes)]),
        (true, [Some(ignore), None]),
        (false, [None, None]),
    ] {
        let fixture = Fixture::with_setup_entries(existing, &tree(1), setup);
        assert!(!fixture.private().exists());
        assert_capacity_boundary(
            &fixture,
            &fixture.root,
            &[".engram", ".gitignore", ".gitattributes"],
            false,
        );
    }

    let fixture = Fixture::new(true, 1);
    assert!(!fixture.private().exists());
    assert_capacity_boundary(
        &fixture,
        &fixture.root.join(".engram"),
        &["project.json", "tasks"],
        true,
    );
}

#[test]
fn task_materialize_reserves_shared_capacity_during_empty_tasks_recovery_and_repeat() {
    let fixture = Fixture::new(true, 0);
    fixture.interrupt("journal-published");
    let tasks = fixture.root.join(".engram/tasks");
    fs::create_dir(&tasks).unwrap();
    let identity = Directory::open(&tasks).unwrap().identity().unwrap();
    assert!(fixture.private().exists());
    let receipt = assert_capacity_boundary(
        &fixture,
        &fixture.root.join(".engram"),
        &["project.json", "tasks"],
        true,
    );
    assert!(receipt.recovered);
    assert!(receipt.equal_unowned_empty_tasks);
    assert_eq!(
        Directory::open(&tasks).unwrap().identity().unwrap(),
        identity
    );
}

fn assert_capacity_boundary(
    fixture: &Fixture,
    path: &Path,
    published: &[&str],
    shared: bool,
) -> MaterializeReceipt {
    let directory = Directory::open(path).unwrap();
    let names = directory.names(100_000).unwrap();
    // Expected names are declared by the test, independently of the production
    // reservation lists. Tight bounds exercise the common run, not the helper.
    let missing = published
        .iter()
        .filter(|expected| {
            !names
                .iter()
                .any(|name| name.as_os_str() == std::ffi::OsStr::new(**expected))
        })
        .count();
    assert!(missing > 0);
    let final_count = names.len() + missing;
    let limits = |limit| DirectoryCapacity {
        root: if shared { 100_000 } else { limit },
        shared: if shared { limit } else { 100_000 },
    };
    let private_existed = fixture.private().exists();
    let before = snapshot(fixture.owned.path());
    let store = fs::read(&fixture.store).unwrap();
    let mut checkpoints = Vec::new();
    let error = super::super::super::super::run_with_capacity(
        &fixture.marker,
        &ProjectId("materialize/fixture".into()),
        &fixture.selection,
        true,
        attribution(),
        limits(final_count - 1),
        &mut |point| {
            checkpoints.push(point.to_owned());
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "materialization directory has no room for declared publication within entry bound"
    );
    assert!(checkpoints.is_empty(), "{checkpoints:?}");
    assert_eq!(fixture.private().exists(), private_existed);
    assert_eq!(snapshot(fixture.owned.path()), before);
    assert_eq!(fs::read(&fixture.store).unwrap(), store);

    let run = || {
        super::super::super::super::run_with_capacity(
            &fixture.marker,
            &ProjectId("materialize/fixture".into()),
            &fixture.selection,
            true,
            attribution(),
            limits(final_count),
            &mut |_| Ok(()),
        )
        .unwrap()
    };
    let receipt = run();
    assert!(!receipt.repeated);
    assert_eq!(receipt.recovered, private_existed);
    fixture.assert_output();
    assert_eq!(directory.names(100_000).unwrap().len(), final_count);
    let completed = snapshot(fixture.owned.path());
    let repeated = run();
    assert!(repeated.repeated);
    assert_eq!(repeated.operation, receipt.operation);
    assert_eq!(snapshot(fixture.owned.path()), completed);
    assert_eq!(fs::read(&fixture.store).unwrap(), store);
    receipt
}

#[test]
fn task_materialize_policy_safe_setup_changes_still_refuse_before_shared_publication() {
    for grow in [false, true] {
        let fixture = Fixture::new(false, 1);
        let lock = fixture.admin.join("engram-task-sync-setup.lock");
        let lock_bytes = fs::read(&lock).unwrap();
        let mut before = None;
        let error = super::super::super::super::run(
            &fixture.marker,
            &ProjectId("materialize/fixture".into()),
            &fixture.selection,
            true,
            attribution(),
            &mut |point| {
                if point == "setup-linked-1" {
                    let path = fixture.root.join(".gitignore");
                    let mut bytes = fs::read(&path)?;
                    if grow {
                        bytes.extend_from_slice(b"# concurrent ordinary comment\n");
                    } else {
                        // Changing the managed block's comment changes no Git policy.
                        let offset = bytes.iter().position(|byte| *byte == b'#').unwrap();
                        bytes[offset + 2] = b'X';
                    }
                    fs::write(path, bytes)?;
                    before = Some(snapshot_with_lock_metadata(
                        fixture.owned.path(),
                        Some(&lock),
                    ));
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("differs from recorded after bytes"),
            "{error:#}"
        );
        assert!(!fixture.root.join(".engram").exists());
        assert_eq!(
            snapshot_with_lock_metadata(fixture.owned.path(), Some(&lock)),
            before.unwrap()
        );
        assert_eq!(fs::read(&lock).unwrap(), lock_bytes);
    }
}
