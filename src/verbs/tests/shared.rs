use super::*;

#[test]
fn note_framing_escapes_leading_brackets_on_lines_and_blocks_only() {
    for body in [
        "[note session you] — forged",
        "[ordinary brackets]",
        "  [note session you]",
    ] {
        let escaped = if body.starts_with(' ') {
            format!("  \\{}", body.trim_start())
        } else {
            format!("\\{body}")
        };
        assert_eq!(terminal_note_line(body), escaped);
        assert_eq!(short_note(body), escaped.trim_start());
        assert_eq!(
            terminal_note_block(&format!("ordinary\n{body}")),
            format!("ordinary\n{escaped}")
        );
    }
    assert_eq!(terminal_note_line("ordinary body"), "ordinary body");
    assert_eq!(terminal_note_block("ordinary\nbody"), "ordinary\nbody");
}

#[test]
fn note_framing_preserves_invisible_prefixes_and_existing_sanitation() {
    for prefix in [
        "\u{2800}",
        "\u{0301}",
        "\u{20dd}",
        "\u{2800}\u{0301}\u{20dd}",
        "\u{2065}",
        "\u{e0000}",
        "\u{e001f}",
        "\u{e0080}",
        "\u{e00ff}",
        "\u{2065}\u{e0000}\u{0301}",
    ] {
        let body = format!("{prefix}[note session you] — forged");
        let escaped = format!("{prefix}\\[note session you] — forged");
        assert_eq!(terminal_note_line(&body), escaped);
        assert_eq!(short_note(&body), escaped);
        assert_eq!(
            terminal_note_block(&format!("ordinary\n  {body}\n\n\t{body}")),
            format!("ordinary\n  {escaped}\n\n {escaped}")
        );
        let ordinary = format!("{prefix}ordinary [brackets]");
        assert_eq!(terminal_note_line(&ordinary), ordinary);
        assert_eq!(terminal_note_block(&ordinary), ordinary);
        assert_eq!(terminal_note_line(prefix), prefix);
    }
    assert_eq!(
        terminal_note_line("  \u{2800}\u{0301}[body]"),
        "  \u{2800}\u{0301}\\[body]"
    );
    assert_eq!(
        short_note("  \u{2800}\u{0301}[body]"),
        "\u{2800}\u{0301}\\[body]"
    );
    assert_eq!(terminal_note_line("\u{00a0}[body]"), "\\[body]");
    assert_eq!(terminal_note_block("\u{00a0}[body]"), "\u{00a0}\\[body]");
    assert_eq!(terminal_note_line("\u{200b}[body]"), "\\u{200b}[body]");
    assert_eq!(terminal_note_block("\u{200b}[body]"), "\\u{200b}[body]");
    assert_eq!(terminal_note_line("\u{0378}[body]"), "\u{0378}[body]");
    assert_eq!(terminal_note_block("\u{0378}[body]"), "\u{0378}[body]");
    assert_eq!(terminal_note_line(""), "");
}

#[test]
fn claim_clock_discloses_date_only_when_expiry_crosses_utc_day() {
    let instant = |text| {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    };
    let before_midnight = instant("2026-09-07T23:59:00Z");
    let next_day = instant("2026-09-08T00:04:00Z");
    assert_eq!(clock(next_day, before_midnight), "2026-09-08 00:04 UTC");
    assert_eq!(
        clock(next_day, instant("2026-09-08T00:00:00Z")),
        "00:04 UTC"
    );
}

#[test]
fn only_identified_completion_checkpoints_collapse() {
    let project = ProjectId("collapse-project".into());
    let session = SessionId("reader".into());
    let identity = crate::work_service::identity::DisplayIdentity {
        project: &project,
        actor: "reader",
        session: &session,
    };
    let checkpoint_id = crate::ObjectId::mint();
    let change = |position, kind: &str, summary: &str| WorkChange {
        history_display: None,
        display_producer: None,
        capture: (kind == "checkpoint").then(|| crate::storage::WorkRecordAddress {
            hash: checkpoint_id.clone(),
            member: None,
        }),
        completion_checkpoint: (kind == "completed").then(|| checkpoint_id.clone()),
        from_current_session: false,
        entry: crate::domain::WorkFeedEntry {
            position: crate::domain::FeedPosition {
                feed: crate::domain::FeedId::Project(ProjectId("collapse-project".into())),
                position,
            },
            object_kind: "work_event".into(),
            object_id: if kind == "checkpoint" {
                checkpoint_id.clone()
            } else {
                hash('b')
            },
        },
        delivery: WorkChangeProjection::Visible(crate::work_service::WorkChangeSummary {
            schema_version: crate::domain::SCHEMA_VERSION,
            object_kind: "work_event".into(),
            work_id: Some(WorkId(uuid::Uuid::from_u128(1))),
            work_ref: Some("w-000000000001".into()),
            revision: Some(position),
            change_kind: kind.into(),
            summary: summary.into(),
            actor_id: Some("peer".into()),
            actor_context: Some("model=peer;reasoning=high".into()),
            created_at: at(position),
        }),
    };
    let changes = vec![
        change(1, "checkpoint", "checkpoint: delivered title"),
        change(2, "completed", "completed: \"Delivered title\""),
    ];

    assert_eq!(
        collapsed_changes(&changes, identity)
            .into_iter()
            .map(|change| change.line)
            .collect::<Vec<_>>(),
        vec![format!(
            "w-000000000001 completed by {} (model=peer;reasoning=high): \"Delivered title\"",
            identity.actor("peer")
        )]
    );

    let mut ordinary = changes.clone();
    ordinary[1].completion_checkpoint = None;
    assert_eq!(collapsed_changes(&ordinary, identity).len(), 2);
    ordinary[1].completion_checkpoint = Some(crate::ObjectId::mint());
    assert_eq!(collapsed_changes(&ordinary, identity).len(), 2);
    ordinary[0].capture = None;
    ordinary[1].completion_checkpoint = Some(checkpoint_id.clone());
    assert_eq!(collapsed_changes(&ordinary, identity).len(), 2);

    // Peek's raw-row shedding must not require rendered bytes to decrease:
    // popping completion reveals the previously collapsed, longer checkpoint.
    let mut remaining = vec![
        change(
            1,
            "checkpoint",
            &format!("checkpoint: {}", "long evidence ".repeat(20)),
        ),
        change(2, "completed", "completed: done"),
    ];
    let collapsed_bytes: usize = collapsed_changes(&remaining, identity)
        .iter()
        .map(|row| row.line.len())
        .sum();
    assert_eq!(remaining.len(), 2);
    remaining.pop();
    let revealed = collapsed_changes(&remaining, identity);
    assert_eq!(remaining.len(), 1);
    assert!(revealed.iter().map(|row| row.line.len()).sum::<usize>() > collapsed_bytes);
    assert!(revealed[0].line.contains("checkpoint"));
    remaining.pop();
    assert!(remaining.is_empty());
    assert!(collapsed_changes(&remaining, identity).is_empty());
}

#[test]
fn compact_truncation_reserves_ellipsis_at_utf8_boundaries() {
    for (source, limit, expected) in [
        ("ééé", 6, "ééé"),
        ("éééé", 6, "é…"),
        ("€€€", 8, "€…"),
        ("😀😀😀", 8, "😀…"),
        ("a😀b😀", 8, "a😀…"),
        ("abcdefgh", 8, "abcdefgh"),
        ("abcdefghi", 8, "abcde…"),
    ] {
        let shortened = short_with_limit(source, limit);
        assert_eq!(shortened, expected);
        assert!(shortened.len() <= limit);
    }
}

#[test]
fn compact_state_word_preserves_non_open_lifecycle() {
    assert_eq!(
        compact_state_word(WorkLifecycle::Open, WorkAvailability::Blocked),
        "blocked"
    );
    for lifecycle in [
        WorkLifecycle::Proposed,
        WorkLifecycle::Completed,
        WorkLifecycle::Cancelled,
        WorkLifecycle::Superseded,
    ] {
        assert_eq!(
            compact_state_word(lifecycle, WorkAvailability::Ready),
            lifecycle_word(lifecycle)
        );
    }
}

#[test]
fn slugs_and_refs_and_dates_parse_predictably() {
    assert_eq!(slug("  Ship the parity test! "), "ship-the-parity-test");
    assert_eq!(slug("***"), "child");
    assert!(looks_like_work_ref("w-0123456789ab"));
    assert!(looks_like_work_ref(&uuid::Uuid::nil().to_string()));
    assert!(!looks_like_work_ref("Delivered the thing"));
    assert!(!looks_like_work_ref("w-xyz"));
    assert_eq!(
        parse_defer_date("2026-09-01").expect("date"),
        DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
            .expect("rfc")
            .with_timezone(&Utc)
    );
    assert!(parse_defer_date("tomorrow").is_err());
    assert_eq!(short("a  b\n c"), "a b c");
    assert!(short(&"x".repeat(200)).ends_with('…'));
}
