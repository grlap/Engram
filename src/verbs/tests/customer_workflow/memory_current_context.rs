use super::*;

fn seed(verbs: &AgentVerbs) {
    for (index, body) in [
        "Superseded first guidance",
        "Superseded second guidance",
        "Current guidance",
    ]
    .into_iter()
    .enumerate()
    {
        verbs
            .service
            .remember_project_memory(
                body.into(),
                Some("context-rule".into()),
                index > 0,
                None,
                at(i64::try_from(index).unwrap()),
            )
            .unwrap();
    }
}

#[test]
fn current_memory_context_full_reads_do_not_offer_history() {
    for mcp in [false, true] {
        let (_home, verbs, _, _) = fixture();
        let verbs = if mcp {
            verbs.with_mcp_argument_names()
        } else {
            verbs
        };
        seed(&verbs);
        let receipt = verbs
            .memories(
                &MemoriesInput {
                    query: Some("context-rule".into()),
                    full: true,
                    ..MemoriesInput::default()
                },
                at(4),
            )
            .unwrap();
        assert_eq!(receipt.value["revision"], 3);
        assert_eq!(receipt.value["body"], "Current guidance");
        assert!(!receipt.text().contains("Superseded"));
        assert!(
            receipt
                .next
                .iter()
                .all(|command| !command.contains("--revision")),
            "{:?}",
            receipt.next
        );
        assert_eq!(receipt.value["next"], serde_json::json!(receipt.next));
    }
}

#[test]
fn current_memory_context_history_requires_an_explicit_revision() {
    for mcp in [false, true] {
        let (_home, verbs, _, _) = fixture();
        let verbs = if mcp {
            verbs.with_mcp_argument_names()
        } else {
            verbs
        };
        seed(&verbs);
        for (revision, body) in [(3, "Current guidance"), (2, "Superseded second guidance")] {
            let receipt = verbs
                .memories(
                    &MemoriesInput {
                        query: Some("context-rule".into()),
                        full: true,
                        revision: Some(revision),
                        ..MemoriesInput::default()
                    },
                    at(4),
                )
                .unwrap();
            assert_eq!(receipt.value["body"], body);
            assert!(receipt.next.contains(&format!(
                "engram work memories context-rule --full --revision {}",
                revision - 1
            )));
            if revision < 3 {
                assert!(
                    receipt
                        .next
                        .contains(&"engram work memories context-rule --full".to_owned())
                );
            }
        }
    }
}

#[test]
fn current_memory_context_revise_keeps_diff_and_only_the_current_read() {
    for (append, section, replacement, added) in [
        (false, None, "Replacement guidance", "Replacement guidance"),
        (true, None, "Additional guidance", "\n\nAdditional guidance"),
        (false, Some("policy"), "Revised policy", "Revised"),
    ] {
        let (_home, verbs, _, _) = fixture();
        let original =
            "<!-- engram-section policy -->\nOriginal policy\n<!-- /engram-section policy -->";
        verbs
            .service
            .remember_project_memory(
                original.into(),
                Some("context-rule".into()),
                false,
                None,
                at(0),
            )
            .unwrap();
        let input = RememberInput {
            text: replacement.into(),
            key: Some("context-rule".into()),
            revise: true,
            expected_revision: Some(1),
            retires_with: None,
            clear_retires_with: false,
            append,
            section: section.map(str::to_owned),
        };
        let receipt = verbs.remember(input.clone(), at(1)).unwrap();
        assert_eq!(receipt.value["replaced_revision"], 1);
        assert!(receipt.value["change"]["added_bytes"].as_u64().unwrap() > 0);
        assert_eq!(receipt.value["change"]["added"], added);
        assert!(receipt.text().contains(added.trim()));
        let expected_body = if append {
            format!("{original}\n\n{replacement}")
        } else if section.is_some() {
            original.replace("Original policy", replacement)
        } else {
            replacement.to_owned()
        };
        let current = verbs
            .memories(
                &MemoriesInput {
                    query: Some("context-rule".into()),
                    full: true,
                    ..MemoriesInput::default()
                },
                at(2),
            )
            .unwrap();
        assert_eq!(current.value["body"], expected_body);
        assert_eq!(
            receipt
                .next
                .iter()
                .filter(|command| *command == "engram work memories context-rule --full")
                .count(),
            1
        );
        assert!(
            receipt
                .next
                .iter()
                .all(|command| !command.contains("--revision")),
            "{:?}",
            receipt.next
        );
        let replay = verbs.remember(input, at(3)).unwrap();
        assert_eq!(replay.value["duplicate"], true);
        assert_eq!(replay.value["revision"], 2);
        assert_eq!(replay.value["change"], receipt.value["change"]);
        assert_eq!(replay.next, receipt.next);
    }
}

#[test]
fn current_memory_context_recovery_coalesces_revisions_after_reopen() {
    let (_home, reader, path, project) = fixture();
    reader
        .service
        .remember_project_memory(
            "Initial guidance".into(),
            Some("context-rule".into()),
            false,
            None,
            at(0),
        )
        .unwrap();
    reader
        .memories(
            &MemoriesInput {
                context_generation: Some("before".into()),
                ..MemoriesInput::default()
            },
            at(1),
        )
        .unwrap();
    let peer = AgentVerbs::new(
        path.clone(),
        project.clone(),
        "peer".into(),
        SessionId("peer".into()),
        None,
    );
    for (now, text) in [(2, "Superseded guidance"), (3, "Current guidance")] {
        peer.service
            .remember_project_memory(
                text.into(),
                Some("context-rule".into()),
                true,
                None,
                at(now),
            )
            .unwrap();
    }
    drop(peer);
    drop(reader);
    let reader = AgentVerbs::new(
        path,
        project,
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let next = reader
        .next(
            &NextInput {
                peek: true,
                context_generation: Some("after".into()),
                ..NextInput::default()
            },
            at(4),
        )
        .unwrap();
    assert_eq!(next.value["memories"]["changes"]["count"], 1);
    assert_eq!(
        next.value["memories"]["changes"]["keys"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let listing = reader
        .memories(
            &MemoriesInput {
                context_generation: Some("after".into()),
                ..MemoriesInput::default()
            },
            at(5),
        )
        .unwrap();
    assert_eq!(listing.value["memories"][0]["revision"], 3);
    assert_eq!(
        listing.value["memories"][0]["first_line"],
        "Current guidance"
    );
    assert!(!listing.text().contains("Superseded"));
    let full = reader
        .memories(
            &MemoriesInput {
                query: Some("context-rule".into()),
                full: true,
                ..MemoriesInput::default()
            },
            at(6),
        )
        .unwrap();
    assert_eq!(full.value["body"], "Current guidance");
}
