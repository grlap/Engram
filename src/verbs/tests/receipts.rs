use super::*;

#[test]
fn compact_next_trims_every_advisory_section_instead_of_failing() {
    let row = compact_test_row(0);
    let receipt = CompactNextReceipt {
        focus: Some(row.clone()),
        held: (1..=20).map(compact_test_row).collect(),
        ready: (21..=40).map(compact_test_row).collect(),
        changes: (0..8)
            .map(|index| format!("change {index}: {}", "x".repeat(90)))
            .collect(),
        memories: Some(ProjectMemorySignal {
            count: 3,
            changed: true,
        }),
        omissions: Vec::new(),
        guidance: Guidance {
            reminders: (0..4)
                .map(|index| format!("reminder {index}: {}", "r".repeat(90)))
                .collect(),
            next: vec!["engram work show w-000000000000".into()],
        },
    };

    let fitted = fit_compact_next(receipt).expect("compact next fits");
    let complete = Receipt::assemble(
        compact_next_lines(&fitted),
        fitted.guidance.clone(),
        compact_next_value(&fitted),
        false,
    )
    .with_build_identity();
    assert!(complete.text().len() < MAX_COMPACT_NEXT_JSON_BYTES);
    assert_eq!(
        serde_json::to_string(&complete.value)
            .unwrap()
            .matches("build_fingerprint")
            .count(),
        1
    );
    assert!(
        serde_json::to_vec_pretty(&compact_next_value(&fitted))
            .expect("compact JSON")
            .len()
            < MAX_COMPACT_NEXT_JSON_BYTES
    );
    assert!(compact_omitted(&fitted, "changes") > 0);
    assert!(compact_omitted(&fitted, "ready") > 0);
    assert_eq!(compact_omitted(&fitted, "memories"), 0);
    assert!(fitted.memories.is_none());
    assert!(fitted.focus.is_some());
    assert!(!fitted.guidance.next.is_empty());
    assert!(
        fitted
            .ready
            .iter()
            .chain(&fitted.held)
            .any(|row| row.labels_omitted.is_some_and(|omitted| omitted >= 2))
    );
    let line = compact_row_line(&row);
    assert!(line.contains("[bug]"));
    assert!(line.contains(" blocked \"Readable compact title"));
    assert!(line.contains("← w-000000000000"));
    assert!(line.contains("held by session-with-a-readable-name until 01:45"));
}

#[test]
fn compact_next_sheds_labels_in_navigation_priority_order() {
    let mut focus = compact_test_row(0);
    focus.labels = vec!["focus".into()];
    let mut held = compact_test_row(1);
    held.labels = vec!["held".into()];
    let mut first_ready = compact_test_row(2);
    first_ready.labels = vec!["first".into()];
    let mut last_ready = compact_test_row(3);
    last_ready.labels = vec!["label-with-a-quoted-\"value\"".into()];
    let last_ready_title = last_ready.title.clone();
    let receipt = CompactNextReceipt {
        focus: Some(focus),
        held: vec![held],
        ready: vec![first_ready, last_ready],
        changes: Vec::new(),
        memories: None,
        omissions: Vec::new(),
        guidance: Guidance::default(),
    };
    let before = serde_json::to_vec_pretty(&compact_next_value(&receipt))
        .expect("labeled receipt")
        .len();
    let fitted = fit_compact_next_to(receipt, before).expect("labels alone fit receipt");
    let after = serde_json::to_vec_pretty(&compact_next_value(&fitted))
        .expect("shed receipt")
        .len();
    assert!(after < before);
    assert_eq!(compact_omitted(&fitted, "ready"), 0);
    assert_eq!(fitted.ready.len(), 2);
    assert_eq!(fitted.ready[0].labels, vec!["first"]);
    assert!(fitted.ready[1].labels.is_empty());
    assert_eq!(fitted.ready[1].labels_omitted, Some(1));
    assert_eq!(fitted.ready[1].title, last_ready_title);
    assert_eq!(fitted.held[0].labels, vec!["held"]);
    assert_eq!(
        fitted.focus.as_ref().expect("focus remains").labels,
        vec!["focus"]
    );
}

#[test]
fn compact_label_shed_restores_and_continues_to_a_reducing_row() {
    let mut first_ready = compact_test_row(1);
    first_ready.labels = vec!["long-escaped-\"label\"".into()];
    let mut last_ready = compact_test_row(2);
    last_ready.labels = vec!["x".into()];
    let receipt = CompactNextReceipt {
        focus: None,
        held: Vec::new(),
        ready: vec![first_ready, last_ready],
        changes: Vec::new(),
        memories: None,
        omissions: Vec::new(),
        guidance: Guidance::default(),
    };
    let mut short_candidate = receipt.clone();
    let short_row = &mut short_candidate.ready[1];
    short_row.labels.clear();
    short_row.labels_omitted = Some(1);
    let threshold = serde_json::to_vec_pretty(&compact_next_value(&short_candidate))
        .expect("short-label candidate")
        .len();
    let mut fitted = receipt;

    assert!(shed_compact_labels(&mut fitted, threshold).expect("later label shed"));
    assert!(fitted.ready[0].labels.is_empty());
    assert_eq!(fitted.ready[0].labels_omitted, Some(1));
    assert_eq!(fitted.ready[1].labels, vec!["x"]);
    assert_eq!(fitted.ready[1].labels_omitted, None);
}

#[test]
fn compact_change_omissions_keep_staged_and_byte_budget_meanings_separate() {
    let mut omissions = vec![CompactSectionOmission {
        section: "changes".into(),
        reason: WorkSectionOmissionReason::Staged,
        omitted_count: 2,
    }];
    record_compact_omission(&mut omissions, "changes", 3);
    let receipt = CompactNextReceipt {
        focus: None,
        held: Vec::new(),
        ready: Vec::new(),
        changes: vec!["one visible change".into()],
        memories: None,
        omissions,
        guidance: Guidance::default(),
    };

    assert_eq!(receipt.omissions.len(), 2);
    assert_eq!(
        compact_omitted_for_reason(&receipt, "changes", WorkSectionOmissionReason::Staged),
        2
    );
    assert_eq!(
        compact_omitted_for_reason(&receipt, "changes", WorkSectionOmissionReason::ByteBudget),
        3
    );
    let lines = compact_next_lines(&receipt);
    assert!(
        lines.contains(
            &"changes by others (1 shown, 2 more arrive with your next call):".to_owned()
        )
    );
    assert!(
        lines
            .contains(&"  (3 change entries omitted from this response by byte budget)".to_owned())
    );

    let byte_budget_only = CompactNextReceipt {
        focus: None,
        held: Vec::new(),
        ready: Vec::new(),
        changes: Vec::new(),
        memories: None,
        omissions: vec![CompactSectionOmission {
            section: "changes".into(),
            reason: WorkSectionOmissionReason::ByteBudget,
            omitted_count: 4,
        }],
        guidance: Guidance::default(),
    };
    assert_eq!(
        compact_next_lines(&byte_budget_only),
        vec![
            "focus: none",
            "held by you (0 shown):",
            "ready (0 shown):",
            "changes by others (none shown):",
            "  (4 change entries omitted from this response by byte budget)",
        ]
    );
}

#[test]
fn text_receipts_never_carry_hashes_or_keys() {
    let receipt = Receipt::assemble(
        vec!["claimed w-0123456789ab \"Baseline\" (held by you until 13:05 UTC)".into()],
        Guidance {
            reminders: vec!["you hold this item but have not noted progress yet".into()],
            next: vec!["engram work note w-0123456789ab \"…\"".into()],
        },
        json!({
            "receipt": { "control_binding": { "claim_fence": 7 } },
            "seal": "a".repeat(64),
            "idempotency_key": "k",
        }),
        false,
    );
    let text = receipt.text();
    assert!(!text.contains(&"a".repeat(64)));
    assert!(!text.contains("fence"));
    assert!(!text.contains("idempotency"));
    assert!(text.ends_with("next:\n  engram work note w-0123456789ab \"…\""));
    assert_eq!(receipt.value["reminders"].as_array().map(Vec::len), Some(1));
    assert_eq!(receipt.value["seal"], json!("a".repeat(64)));
}

#[test]
fn effective_session_id_only_enriches_success_receipts() {
    let session = SessionId("local-process-test".into());
    let success = Receipt::assemble(Vec::new(), Guidance::default(), json!({}), false)
        .with_effective_session_id(&session);
    assert_eq!(
        success.value["effective_session_id"],
        json!("local-process-test")
    );

    let owed = Receipt::assemble(Vec::new(), Guidance::default(), json!({}), true)
        .with_effective_session_id(&session);
    assert_eq!(owed.value.get("effective_session_id"), None);
}

#[test]
fn text_receipts_disclose_capped_next_commands_without_truncating_json() {
    let commands = (1..=5)
        .map(|number| format!("engram work show w-{number:012}"))
        .collect::<Vec<_>>();
    let receipt = Receipt::assemble(
        vec!["ready".into()],
        Guidance {
            reminders: Vec::new(),
            next: commands.clone(),
        },
        json!({}),
        false,
    );

    let text = receipt.text();
    for command in &commands[..MAX_TEXT_NEXT_COMMANDS] {
        assert!(text.contains(command));
    }
    assert!(!text.contains(&commands[MAX_TEXT_NEXT_COMMANDS]));
    assert!(text.contains("(+1 more)"));
    assert_eq!(receipt.value["next"], json!(commands));
}

#[test]
fn empty_changes_section_is_omitted_from_text() {
    let mut lines = vec!["ready w-0123456789ab".into()];
    append_changes_lines(&mut lines, &[], 0);
    assert_eq!(lines, vec!["ready w-0123456789ab"]);
}
