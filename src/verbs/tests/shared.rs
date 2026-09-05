use super::*;

#[test]
fn checkpoint_before_completion_collapses_by_work_identity() {
    let change = |position, kind: &str, summary: &str| WorkChange {
        from_current_session: false,
        entry: crate::domain::WorkFeedEntry {
            position: crate::domain::FeedPosition {
                feed: crate::domain::FeedId::Project(ProjectId("collapse-project".into())),
                position,
            },
            object_kind: "work_event".into(),
            object_hash: hash(if position == 1 { 'a' } else { 'b' }),
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
        collapse_changes(&changes),
        vec!["w-000000000001 completed by peer (model=peer;reasoning=high): \"Delivered title\""]
    );
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
