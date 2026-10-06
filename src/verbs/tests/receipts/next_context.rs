use super::*;
use crate::work_service::{WorkCurrentStatus, WorkDiscoverySummary, WorkDiscoveryView};

fn capture() -> crate::storage::WorkRecordAddress {
    crate::storage::WorkRecordAddress {
        hash: crate::ObjectId::mint(),
        member: None,
    }
}

#[test]
fn recovery_selects_structured_subjects_preserves_order_and_counts_overflow() {
    let mut compact = context_receipt();
    let reference = compact.held[0].work_ref.clone();
    compact.peek = Some(crate::work_service::WorkNextPeek {
        delivery_advanced: false,
        more_changes_available: false,
        memory_listing_due: false,
    });
    let capture = capture();
    compact.changes = vec![crate::verbs::next_context::CompactChange {
        line: format!("{reference} quoted in unrelated prose"),
        subject: Some("unrelated-subject".into()),
        attribution: "peer noted".into(),
        note: None,
    }];
    for (index, kind) in [
        "noted",
        "claimed",
        "handoff_offered",
        "released",
        "noted",
        "claimed",
    ]
    .iter()
    .enumerate()
    {
        compact
            .changes
            .push(crate::verbs::next_context::CompactChange {
                line: format!("{kind} event {index}"),
                subject: Some(reference.clone()),
                attribution: format!("peer {kind}"),
                note: (index == 0).then(|| (reference.clone(), capture.clone())),
            });
    }
    crate::verbs::next_recovery::prepare(&mut compact);
    assert_eq!(
        compact
            .changes
            .iter()
            .map(|row| row.line.as_str())
            .collect::<Vec<_>>(),
        [
            "noted event 0",
            "claimed event 1",
            "handoff_offered event 2",
            "released event 3"
        ]
    );
    assert_eq!(
        compact
            .omissions
            .iter()
            .filter(|row| row.section == "changes")
            .map(|row| row.omitted_count)
            .sum::<usize>(),
        3
    );
    let value = compact_next_value(&compact);
    assert!(value["changes"][0].as_str().unwrap().contains(&format!(
        "engram work show {reference} --note {}",
        capture.locator(usize::MAX)
    )));
    assert_eq!(value["peek"]["more_changes_available"], true);
    assert_eq!(value["details"], "engram work next --peek --verbose");
}

#[test]
fn typed_capture_members_and_record_ids_keep_identical_bodies_distinct() {
    let mut compact = context_receipt();
    compact.held.clear();
    compact.discovery.assigned[0].current_status = None;
    compact.discovery.participated[0].current_status = None;
    compact.discovery.participated[0].note_identity = Some(capture());
    let value = compact_next_value(&compact);
    assert_eq!(value["participated"][0]["note"], "Different ordinary note");
    assert!(value["participated"][0].get("context_ref").is_none());
    let id = crate::ObjectId::mint();
    for (row, index) in compact
        .discovery
        .assigned
        .iter_mut()
        .chain(&mut compact.discovery.participated)
        .zip(1..)
    {
        row.note_identity = Some(crate::storage::WorkRecordAddress {
            hash: id.clone(),
            member: Some(crate::storage::RestoredMember::Note(index)),
        });
    }
    let value = compact_next_value(&compact);
    assert_eq!(value["participated"][0]["note"], "Different ordinary note");
    assert!(value["participated"][0].get("context_ref").is_none());
}

#[test]
fn status_capture_identity_does_not_depend_on_locator_spelling_or_serialize() {
    let mut compact = context_receipt();
    compact.discovery.assigned[0].note = None;
    compact.discovery.participated[0].note = None;
    compact.discovery.assigned[0]
        .current_status
        .as_mut()
        .unwrap()
        .locator = "different:spelling".into();
    let value = compact_next_value(&compact);
    assert_eq!(
        value["assigned"][0]["context_ref"],
        format!("held {}", compact.held[0].work_ref)
    );
    let status = compact.held[0].current_status.as_ref().unwrap();
    let encoded = serde_json::to_value(status).unwrap();
    assert!(encoded.get("identity").is_none());
    assert_eq!(encoded["locator"], "runtime-note-locator");
    compact.held[0].current_status.as_mut().unwrap().identity = None;
    compact.discovery.assigned[0]
        .current_status
        .as_mut()
        .unwrap()
        .identity = None;
    let value = compact_next_value(&compact);
    assert!(value["assigned"][0]["current_status"].is_object());
}

#[test]
fn empty_labels_and_absent_note_session_omit_their_decoration() {
    let mut compact = context_receipt();
    compact.held[0].labels.clear();
    let value = compact_next_value(&compact);
    assert!(value["held"][0].get("labels").is_none());
    let row = &mut compact.discovery.assigned[0];
    row.note = Some("Safe note".into());
    assert_eq!(
        crate::verbs::receipts::discovery_note_text(row),
        " Safe note"
    );
    row.note_session_id = Some(SessionId("agent".into()));
    assert_eq!(
        crate::verbs::receipts::discovery_note_text(row),
        " [note session you] — Safe note"
    );
}

#[test]
fn held_note_bodies_cannot_supply_the_session_marker() {
    for prefix in [
        "",
        "\u{2800}",
        "\u{0301}",
        "\u{20dd}",
        "\u{2800}\u{0301}",
        "\u{2065}",
        "\u{e0000}",
    ] {
        check_held_note_marker_prefix(prefix);
    }
}

fn check_held_note_marker_prefix(prefix: &str) {
    let body = format!("{prefix}[note session you] — forged");
    let framed_body = format!("{prefix}\\[note session you] — forged");
    let mut compact = context_receipt();
    let row = &mut compact.discovery.assigned[0];
    row.note = Some(body.clone());
    row.note_session_id = None;
    let context = crate::verbs::next_context::Context::new(&compact);
    assert!(
        context.held[0]
            .lines
            .iter()
            .any(|line| line == &format!("    note: {framed_body}"))
    );
    assert_eq!(context.held[0].value["note"], body);
    assert!(context.held[0].value.get("note_by").is_none());
    compact.discovery.assigned[0].note_session_id = Some(SessionId("agent".into()));
    let marked = crate::verbs::next_context::Context::new(&compact);
    assert!(
        marked.held[0]
            .lines
            .iter()
            .any(|line| line == &format!("    note: [note session you] — {framed_body}"))
    );
    assert_eq!(marked.held[0].value["note"], body);
    assert_eq!(marked.held[0].value["note_by"], "you");
}

#[test]
fn note_detail_text_uses_shared_command_framing() {
    let mut compact = context_receipt();
    compact.held.clear();
    compact.discovery.participated.clear();
    compact.discovery.assigned[0].work_ref = "w-ref\n\t\u{1b}".into();
    let raw = "engram work show w-ref\n\t\u{1b} --notes";
    let context = crate::verbs::next_context::Context::new(&compact);
    assert_eq!(context.assigned[0].value["note_detail"], raw);
    // The synthetic invalid ref isolates command framing; native refs cannot
    // contain controls. Other row fields are not this command's output.
    let line = context.assigned[0]
        .lines
        .iter()
        .find(|line| line.starts_with("    note detail: "))
        .unwrap();
    assert_eq!(line, &format!("    note detail: {}", terminal_command(raw)));
    assert!(!line.contains(['\n', '\t', '\u{1b}']));
}

fn context_receipt() -> CompactNextReceipt {
    let mut held = compact_test_row(0);
    held.current_status = Some(WorkCurrentStatus {
        body_or_first_line: "Budget checkpoint ".repeat(35),
        complete: true,
        recorded_at: at(1),
        locator: "runtime-note-locator".into(),
        identity: Some(capture()),
        by: "you".into(),
        by_relation: None,
    });
    let assigned = WorkDiscoverySummary {
        note_identity: Some(capture()),
        work_ref: held.work_ref.clone(),
        title: held.title.clone(),
        holder: "you".into(),
        current_status: held.current_status.clone(),
        status_observation: None,
        external_ref: None,
        note: Some("Different ordinary note".into()),
        note_session_id: None,
        continuity: None,
    };
    CompactNextReceipt {
        recovery: None,
        backup_reminder: None,
        claim_lapse_reminder: None,
        focus_evaluation: None,
        evaluation_obligations: None,
        ready_navigation: None,
        peek: None,
        read_cut: test_next_cut(),
        context_generation: None,
        discovery: WorkDiscoveryView {
            assigned: vec![assigned.clone()],
            participated: vec![assigned],
            ..WorkDiscoveryView::default()
        },
        held: vec![held],
        focus: None,
        ready: vec![],
        changes: vec![],
        memories: None,
        omissions: vec![],
        guidance: Guidance::default(),
    }
}

#[test]
fn peek_disclosures_survive_even_an_impossible_budget() {
    let mut compact = context_receipt();
    compact.peek = Some(crate::work_service::WorkNextPeek {
        delivery_advanced: false,
        more_changes_available: true,
        memory_listing_due: false,
    });
    compact.memories = Some(ProjectMemorySignal {
        count: 17,
        changed: true,
    });
    compact.guidance.next = vec!["engram work memories".into(), "engram work next".into()];
    compact
        .changes
        .push(crate::verbs::next_context::CompactChange {
            line: "A change that cannot fit".into(),
            subject: None,
            attribution: "peer noted".into(),
            note: None,
        });
    let fitted = fit_compact_next_to(compact, 1).unwrap();
    let value = compact_next_value(&fitted);
    assert_eq!(value["peek"]["delivery_advanced"], false);
    assert_eq!(value["memories"], json!({"count": 17, "changed": true}));
    assert_eq!(value["memories_detail"], "engram work memories");
    assert_eq!(fitted.guidance.next, ["engram work memories"]);
    let text = compact_next_lines(&fitted).join("\n");
    assert!(fitted.changes.is_empty());
    assert_eq!(text.matches("changes by others (").count(), 1);
    assert!(text.contains("0 shown since last confirmed delivery"));
    assert!(text.contains("delivery: not advanced"));
    assert!(text.contains("memory detail: engram work memories"));
    assert!(fitted.held.is_empty());
    assert!(fitted.discovery.assigned.is_empty());
}

fn recovering_receipt() -> CompactNextReceipt {
    let mut compact = context_receipt();
    compact.peek = Some(crate::work_service::WorkNextPeek {
        delivery_advanced: false,
        more_changes_available: false,
        memory_listing_due: true,
    });
    compact.context_generation = Some("termal-7".into());
    compact.guidance.next = vec![
        "engram work memories --context-generation termal-7".into(),
        "engram work next".into(),
    ];
    compact
}

fn memory_recovery_direction(compact: &CompactNextReceipt) -> String {
    crate::verbs::memory_recovery::reminder(compact.peek.as_ref()).expect("direction")
}

#[test]
fn the_memory_recovery_direction_survives_an_impossible_budget() {
    let mut compact = recovering_receipt();
    compact.guidance.reminders = vec!["one".into(), "two".into()];
    let direction = memory_recovery_direction(&compact);
    let fitted = fit_compact_next_to(compact, 1).unwrap();
    assert_eq!(fitted.guidance.reminders, std::slice::from_ref(&direction));
    assert_eq!(
        fitted.guidance.next,
        ["engram work memories --context-generation termal-7"]
    );
    let value = compact_next_value(&fitted);
    assert_eq!(value["reminders"], json!([direction]));
    assert_eq!(value["peek"]["memory_listing_due"], true);
    assert_eq!(
        value["memories_detail"],
        "engram work memories --context-generation termal-7"
    );
    let lines = compact_next_lines(&fitted);
    assert_eq!(lines[0], direction);
    assert_eq!(
        lines[1],
        "  engram work memories --context-generation termal-7"
    );
    assert!(
        fitted
            .omissions
            .iter()
            .any(|omission| omission.section == "reminders" && omission.omitted_count == 2)
    );
}

#[test]
fn the_memory_recovery_direction_precedes_the_clipped_status_reminder() {
    let compact = recovering_receipt();
    let direction = memory_recovery_direction(&compact);
    let render = |receipt: &CompactNextReceipt| {
        Receipt::assemble(
            compact_next_lines(receipt),
            receipt.guidance.clone(),
            compact_next_value(receipt),
            false,
        )
        .with_build_identity(&receipt.read_cut, receipt.context_generation.as_deref())
    };
    // Derive the budget from this recovery envelope with shortened status,
    // including both reminders and navigation. An advancing envelope omits
    // recovery fields and can force the held row itself out of the packet.
    let mut clipped = compact.clone();
    while clipped.discovery.shorten_status_previews() {}
    let row = &mut clipped.held[0];
    while crate::work_service::shorten_status_previews(
        &mut row.current_status,
        &mut row.status_observation,
    ) {}
    crate::verbs::next_context::refresh_guidance(&mut clipped);
    let candidate = render(&clipped);
    let budget = crate::verbs::receipts::compact_receipt_json_bytes(&candidate.value)
        .unwrap()
        .max(agent_receipt_terminal_bytes(&candidate.text()))
        + 1;
    assert!(!agent_receipt_fits(&render(&compact), budget).unwrap());
    let reference = compact.held[0].work_ref.clone();
    let locator = compact.held[0]
        .current_status
        .as_ref()
        .unwrap()
        .locator
        .clone();
    let fitted = fit_compact_next_to(compact, budget).unwrap();
    assert_eq!(fitted.held.len(), 1);
    assert_eq!(fitted.held[0].work_ref, reference);
    assert_eq!(
        fitted.held[0].current_status.as_ref().unwrap().locator,
        locator
    );
    assert!(!fitted.held[0].current_status.as_ref().unwrap().complete);
    assert!(agent_receipt_fits(&render(&fitted), budget).unwrap());
    assert_eq!(
        fitted.guidance.reminders[..2],
        [
            direction,
            crate::verbs::next_context::CLIPPED_STATUS_REMINDER.to_owned()
        ]
    );
}

#[test]
fn the_memory_recovery_direction_is_kept_by_the_reminder_count_limit() {
    let mut compact = recovering_receipt();
    compact.discovery = WorkDiscoveryView::default();
    compact.guidance.reminders = (0..MAX_COMPACT_REMINDER_ITEMS)
        .map(|index| format!("reminder {index}"))
        .collect();
    let direction = memory_recovery_direction(&compact);
    for _ in 0..2 {
        crate::verbs::next_context::refresh_guidance(&mut compact);
        assert_eq!(compact.guidance.reminders.len(), MAX_COMPACT_REMINDER_ITEMS);
        assert_eq!(compact.guidance.reminders[0], direction);
        assert_eq!(compact.guidance.reminders[1], "reminder 0");
        assert_eq!(
            compact
                .omissions
                .iter()
                .filter(|omission| omission.section == "reminders")
                .map(|omission| omission.omitted_count)
                .collect::<Vec<_>>(),
            [1],
            "the displaced reminder is counted once"
        );
    }
}

#[test]
fn pilot_budget_clipping_adds_read_first_guidance_inside_the_budget() {
    let original = context_receipt();
    assert!(original.held[0].current_status.as_ref().unwrap().complete);
    let before = compact_next_value(&original);
    let budget = serde_json::to_vec(&before).unwrap().len();
    let fitted = fit_compact_next_to(original, budget).unwrap();
    assert_eq!(fitted.held.len(), 1);
    assert!(!fitted.held[0].current_status.as_ref().unwrap().complete);
    assert!(
        fitted
            .guidance
            .reminders
            .iter()
            .any(|reminder| reminder == crate::verbs::next_context::CLIPPED_STATUS_REMINDER)
    );
    let receipt = Receipt::assemble(
        compact_next_lines(&fitted),
        fitted.guidance.clone(),
        compact_next_value(&fitted),
        false,
    )
    .with_build_identity(&fitted.read_cut, None);
    assert!(receipt.text().len() < budget);
    assert!(serde_json::to_vec(&receipt.value).unwrap().len() < budget);
    assert!(receipt.text().contains("status body omitted"));
    assert_eq!(
        receipt.value["held"][0]["current_status"]["locator"],
        "runtime-note-locator"
    );
    assert_eq!(receipt.text().matches("Different ordinary note").count(), 1);
}

#[test]
fn pilot_context_references_follow_retained_rows_and_distinct_change_notes() {
    let mut compact = context_receipt();
    let reference = compact.held[0].work_ref.clone();
    let before = compact_next_value(&compact);
    assert_eq!(
        before["assigned"][0]["context_ref"],
        format!("held {reference}")
    );
    compact.held.clear();
    let after = compact_next_value(&compact);
    assert!(after["assigned"][0]["current_status"].is_object());
    assert_eq!(
        after["participated"][0]["context_ref"],
        format!("assigned {reference}")
    );
    let delivered = capture();
    for _ in 0..2 {
        compact
            .changes
            .push(crate::verbs::next_context::CompactChange {
                line: format!("{reference} noted: A different delivered note"),
                subject: Some(reference.clone()),
                attribution: format!("{reference} noted"),
                note: Some((reference.clone(), delivered.clone())),
            });
    }
    let changed = compact_next_value(&compact);
    assert!(
        changed["changes"][0]
            .as_str()
            .unwrap()
            .contains("A different delivered note")
    );
    assert_eq!(
        changed["changes"][1],
        format!("{reference} noted — see changes entry 1 ({reference})")
    );
    compact.discovery.assigned.clear();
    compact.discovery.participated.clear();
    let standalone = compact_next_value(&compact);
    assert_eq!(standalone["changes"], changed["changes"]);
}

#[test]
fn pilot_correction_missing_identity_keeps_body_and_complete_controls_guidance() {
    let mut compact = context_receipt();
    for row in compact
        .discovery
        .assigned
        .iter_mut()
        .chain(&mut compact.discovery.participated)
    {
        row.note_identity = None;
    }
    let value = compact_next_value(&compact);
    for section in ["held", "assigned", "participated"] {
        assert_eq!(value[section][0]["note"], "Different ordinary note");
        assert!(value[section][0].get("context_ref").is_none());
        assert!(value[section][0]["note_detail"].is_string());
    }
    compact.discovery = WorkDiscoveryView::default();
    for (body, complete) in [
        ("Literal...", true),
        ("Literal…", true),
        ("No punctuation", false),
    ] {
        let status = compact.held[0].current_status.as_mut().unwrap();
        status.body_or_first_line = body.into();
        status.complete = complete;
        crate::verbs::next_context::refresh_guidance(&mut compact);
        assert_eq!(
            compact
                .guidance
                .reminders
                .iter()
                .any(|line| line == crate::verbs::next_context::CLIPPED_STATUS_REMINDER),
            !complete
        );
    }
}

/// A backup reminder longer than the compact reminder width, which stays
/// whole.
fn long_backup_reminder() -> String {
    format!(
        "backup: mode local ({}); see engram backup status",
        ["store: backup_confirmation_expired"; 4].join(", ")
    )
}

#[test]
fn the_backup_reminder_follows_the_direction_and_survives_an_impossible_budget() {
    let backup = long_backup_reminder();
    for recovering in [true, false] {
        let mut compact = if recovering {
            recovering_receipt()
        } else {
            context_receipt()
        };
        compact.backup_reminder = Some(backup.clone());
        compact.guidance.reminders = vec!["one".into(), "two".into()];
        let expected = if recovering {
            vec![memory_recovery_direction(&compact), backup.clone()]
        } else {
            vec![backup.clone()]
        };
        let fitted = fit_compact_next_to(compact, 1).unwrap();
        assert_eq!(
            fitted.guidance.reminders, expected,
            "recovering {recovering}"
        );
        let value = compact_next_value(&fitted);
        assert_eq!(value["reminders"], json!(expected));
    }
}

#[test]
fn the_backup_reminder_is_kept_by_the_reminder_count_limit() {
    let mut compact = recovering_receipt();
    compact.discovery = WorkDiscoveryView::default();
    compact.backup_reminder = Some(long_backup_reminder());
    compact.guidance.reminders = (0..MAX_COMPACT_REMINDER_ITEMS)
        .map(|index| format!("reminder {index}"))
        .collect();
    let direction = memory_recovery_direction(&compact);
    for _ in 0..2 {
        crate::verbs::next_context::refresh_guidance(&mut compact);
        assert_eq!(compact.guidance.reminders.len(), MAX_COMPACT_REMINDER_ITEMS);
        assert_eq!(compact.guidance.reminders[0], direction);
        assert_eq!(compact.guidance.reminders[1], long_backup_reminder());
        assert_eq!(compact.guidance.reminders[2], "reminder 0");
    }
}
