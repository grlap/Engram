use super::*;

#[test]
fn status_resume_assignment_preserves_wait_after_release_or_expiry_without_renewal() {
    for release in [false, true] {
        let (_dir, owner, path, project) = fixture();
        let unassigned = add(&owner, "Held only", None, false, 0);
        let assigned = assigned(&owner, "Durable duty", "agent", 1);
        for (reference, body) in [
            (&unassigned, "Ephemeral holder wait"),
            (&assigned, "Durable assigned wait"),
        ] {
            owner
                .claim(
                    ClaimInput {
                        work_ref: reference.clone(),
                        ttl_seconds: Some(300),
                        recover: None,
                    },
                    at(2),
                )
                .unwrap();
            capture_status(&owner, reference, body, 3);
            assert_eq!(
                owner.show(reference, at(4)).unwrap().value["current_status"]["body_or_first_line"],
                body
            );
            if release {
                owner
                    .update(
                        UpdateInput {
                            work_ref: Some(reference.clone()),
                            action: UpdateAction::Release {
                                reason: Some("Wait without execution authority".into()),
                            },
                        },
                        at(5),
                    )
                    .unwrap();
            }
        }
        // Holder capture renews the live claim; advance beyond that actual
        // default TTL too, without renewing either claim in the test.
        let now = if release {
            6
        } else {
            3 + crate::DEFAULT_WORK_CLAIM_TTL_SECONDS + 1
        };
        let resumed = AgentVerbs::new(
            path.clone(),
            project,
            "agent".into(),
            SessionId("fresh-session".into()),
            None,
        );
        let no_owner = resumed.show(&unassigned, at(now)).unwrap();
        assert!(no_owner.value["current_status"].is_null());
        let duty = resumed.show(&assigned, at(now)).unwrap();
        assert_eq!(
            duty.value["current_status"]["body_or_first_line"],
            "Durable assigned wait"
        );
        for reference in [&unassigned, &assigned] {
            let history = resumed
                .show_records(
                    reference,
                    &ShowInput {
                        notes: true,
                        ..ShowInput::default()
                    },
                    at(now),
                )
                .unwrap();
            assert!(
                history.value["notes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|note| note["kind"] == "status")
            );
        }
        for verbose in [false, true] {
            let next = resumed
                .next(
                    &NextInput {
                        verbose,
                        ..NextInput::default()
                    },
                    at(now),
                )
                .unwrap();
            assert!(next.value["held"].as_array().unwrap().is_empty());
            let row = next.value["assigned"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["ref"] == assigned)
                .unwrap();
            assert_eq!(
                row["current_status"]["body_or_first_line"],
                "Durable assigned wait"
            );
            assert!(next.text().contains("Durable assigned wait"));
            // Exact historical changes may contain the old body. The resume
            // contract excludes it from current-status lines, not history.
            assert!(
                !next
                    .text()
                    .lines()
                    .any(|line| line.trim_start().starts_with("status:")
                        && line.contains("Ephemeral holder wait"))
            );
            assert!(
                next.value["assigned"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|row| row["ref"] != unassigned)
            );
        }
        assert!(
            SqliteStore::open(path)
                .unwrap()
                .verify_all()
                .unwrap()
                .is_healthy()
        );
    }
}
