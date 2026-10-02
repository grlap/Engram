use super::*;
use crate::work_service::{WorkCurrentStatus, WorkDiscoverySummary, WorkDiscoveryView};

fn context_receipt() -> CompactNextReceipt {
    let mut held = compact_test_row(0);
    held.current_status = Some(WorkCurrentStatus {
        body_or_first_line: "Budget checkpoint ".repeat(35),
        complete: true,
        recorded_at: at(1),
        locator: "runtime-note-locator".into(),
        by: "you".into(),
    });
    let assigned = WorkDiscoverySummary {
        note_identity: Some("ordinary-note-capture".into()),
        work_ref: held.work_ref.clone(),
        title: held.title.clone(),
        holder: "you".into(),
        current_status: held.current_status.clone(),
        status_observation: None,
        external_ref: None,
        note: Some("Different ordinary note".into()),
        note_session_id: None,
    };
    CompactNextReceipt {
        backup_reminder: None,
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
    let budget = serde_json::to_vec(&compact_next_value(&context_receipt()))
        .unwrap()
        .len();
    let fitted = fit_compact_next_to(compact, budget).unwrap();
    assert!(!fitted.held[0].current_status.as_ref().unwrap().complete);
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
    for _ in 0..2 {
        compact
            .changes
            .push(crate::verbs::next_context::CompactChange {
                line: format!("{reference} noted: A different delivered note"),
                attribution: format!("{reference} noted"),
                note: Some((reference.clone(), "delivered-note-capture".into())),
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
