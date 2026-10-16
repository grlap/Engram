//! Admission and compensation interruption boundaries.

use super::*;

#[test]
fn task_materialize_process_recovers_initially_absent_setup_links() {
    for index in 0..2 {
        for step in ["before-setup-link", "setup-linked"] {
            let fixture = Fixture::with_setup(false, &tree(1), false);
            let boundary = format!("{step}-{index}");
            assert_eq!(
                child(&fixture, Some(&boundary), true).status.code(),
                Some(77)
            );
            fixture.disable();
            assert!(child(&fixture, None, false).status.success());
            let response: serde_json::Value = serde_json::from_slice(
                &fs::read(fixture.owned.path().join("process-finish-response.json")).unwrap(),
            )
            .unwrap();
            assert!(response.get("error").is_none(), "{boundary}: {response}");
            fixture.assert_output();
        }
    }
}

#[test]
fn task_materialize_process_preserves_unadmitted_partial_replacement_plans() {
    for index in 0..2 {
        for step in [
            "replace-created",
            "replace-plan-created",
            "replace-plan-prefix",
            "replace-plan-synced",
            "replace-plan-published",
        ] {
            let fixture = Fixture::new(false, 1);
            let setup = [".gitignore", ".gitattributes"]
                .map(|name| fs::read(fixture.root.join(name)).unwrap());
            let boundary = format!("{step}-{index}");
            assert_eq!(
                child(&fixture, Some(&boundary), true).status.code(),
                Some(77)
            );
            assert!(!fixture.private().join("journal.json").exists());
            assert!(!fixture.root.join(".engram").exists());
            for (position, name) in [".gitignore", ".gitattributes"].iter().enumerate() {
                assert_eq!(fs::read(fixture.root.join(name)).unwrap(), setup[position]);
            }
            let preserved = snapshot(&fixture.private());
            fixture.disable();
            assert!(child(&fixture, None, false).status.success());
            let response: serde_json::Value = serde_json::from_slice(
                &fs::read(fixture.owned.path().join("process-finish-response.json")).unwrap(),
            )
            .unwrap();
            assert!(
                response["error"]
                    .as_str()
                    .unwrap()
                    .contains("incomplete private materialization attempt"),
                "{boundary}: {response}"
            );
            assert_eq!(snapshot(&fixture.private()), preserved);
        }
    }
}

#[test]
fn task_materialize_process_preserves_relocates_and_freshly_admits_unowned_attempts() {
    for existing_private in [false, true] {
        for boundary in ["private-created", "replace-created-0", "replace-created-1"] {
            let fixture = Fixture::new(existing_private, 1);
            let ordinary_code = fs::read(fixture.root.join("code.txt")).unwrap();
            let retained_private = existing_private
                .then(|| fs::read(fixture.root.join(".engram/private.db")).unwrap());
            let source_store = fs::read(&fixture.store).unwrap();
            assert_eq!(
                child(&fixture, Some(boundary), true).status.code(),
                Some(77),
                "{existing_private}/{boundary}"
            );
            let attempt = fixture.private();
            let attempt_before = snapshot(&attempt);
            let public_before_refusal = snapshot(&fixture.root);

            assert!(child(&fixture, None, true).status.success());
            let refusal: serde_json::Value = serde_json::from_slice(
                &fs::read(fixture.owned.path().join("process-finish-response.json")).unwrap(),
            )
            .unwrap();
            let error = refusal["error"]
                .as_str()
                .expect("incomplete-attempt refusal");
            assert!(
                error.contains("incomplete private materialization attempt"),
                "{error}"
            );
            assert!(error.contains(&attempt.display().to_string()), "{error}");
            assert_eq!(snapshot(&attempt), attempt_before);
            assert_eq!(snapshot(&fixture.root), public_before_refusal);

            let preserved = fixture
                .owned
                .path()
                .join(format!("preserved-incomplete-{boundary}"));
            fs::rename(&attempt, &preserved).unwrap();
            let preserved_before = snapshot(&preserved);

            assert!(child(&fixture, None, true).status.success());
            let fresh: serde_json::Value = serde_json::from_slice(
                &fs::read(fixture.owned.path().join("process-finish-response.json")).unwrap(),
            )
            .unwrap();
            assert!(fresh.get("error").is_none(), "{fresh}");
            assert_eq!(fresh["receipt"]["recovered"], false, "{fresh}");
            fixture.assert_output();
            assert_eq!(
                fs::read(fixture.root.join("code.txt")).unwrap(),
                ordinary_code
            );
            assert_eq!(fs::read(&fixture.store).unwrap(), source_store);
            if let Some(retained_private) = &retained_private {
                assert_eq!(
                    fs::read(fixture.root.join(".engram/private.db")).unwrap(),
                    *retained_private
                );
            }
            assert_eq!(snapshot(&preserved), preserved_before);

            assert!(child(&fixture, None, true).status.success());
            let repeated: serde_json::Value = serde_json::from_slice(
                &fs::read(fixture.owned.path().join("process-finish-response.json")).unwrap(),
            )
            .unwrap();
            assert!(repeated.get("error").is_none(), "{repeated}");
            assert_eq!(repeated["receipt"]["repeated"], true, "{repeated}");
            assert_eq!(snapshot(&preserved), preserved_before);
        }
    }
}

#[test]
fn task_materialize_process_preserves_compensation_across_restart() {
    for boundary in ["before-restore-0", "restore-checked-0"] {
        let fixture = Fixture::new(false, 1);
        assert_eq!(
            child_with_race(&fixture, Some(boundary), true, true)
                .status
                .code(),
            Some(77)
        );
        let retained = fixture.private().join("replace-0/displaced");
        assert_eq!(fs::read(&retained).unwrap(), b"concurrent ordinary edit\n");
        fixture.disable();
        assert!(child(&fixture, None, false).status.success());
        let response: serde_json::Value = serde_json::from_slice(
            &fs::read(fixture.owned.path().join("process-finish-response.json")).unwrap(),
        )
        .unwrap();
        assert!(
            response["error"]
                .as_str()
                .unwrap()
                .contains("restore_incomplete=false"),
            "{boundary}: {response}"
        );
        assert_eq!(fs::read(&retained).unwrap(), b"concurrent ordinary edit\n");
        assert_eq!(
            fs::read(fixture.root.join(".gitignore")).unwrap(),
            b"concurrent ordinary edit\n"
        );
        assert_eq!(
            file_identity(&fs::File::open(&retained).unwrap()).unwrap(),
            file_identity(&fs::File::open(fixture.root.join(".gitignore")).unwrap()).unwrap()
        );
        let before = snapshot(&fixture.private());
        assert!(child(&fixture, None, false).status.success());
        assert_eq!(snapshot(&fixture.private()), before);
    }
}

#[test]
fn task_materialize_process_preserves_unadmitted_partial_journal() {
    for boundary in [
        "journal-pending-created",
        "journal-pending-prefix",
        "journal-pending-synced",
    ] {
        let fixture = Fixture::new(false, 1);
        let public = snapshot(&fixture.root);
        assert_eq!(
            child(&fixture, Some(boundary), true).status.code(),
            Some(77)
        );
        assert!(!fixture.private().join("journal.json").exists());
        for name in [".gitignore", ".gitattributes"] {
            assert_eq!(
                fs::read(fixture.root.join(name)).unwrap(),
                public[Path::new(name)].clone().unwrap()
            );
        }
        let preserved = snapshot(&fixture.private());
        fixture.disable();
        assert!(child(&fixture, None, false).status.success());
        let response: serde_json::Value = serde_json::from_slice(
            &fs::read(fixture.owned.path().join("process-finish-response.json")).unwrap(),
        )
        .unwrap();
        assert!(
            response["error"]
                .as_str()
                .unwrap()
                .contains("incomplete private materialization attempt"),
            "{boundary}: {response}"
        );
        assert_eq!(snapshot(&fixture.private()), preserved);
    }
}
