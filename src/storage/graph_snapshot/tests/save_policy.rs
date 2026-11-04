use super::*;

#[test]
fn redactor_refusal_returns_before_the_save_audit() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let mut store = SqliteStore::open(directory.path().join("engram.db")).expect("store");
    let project = ProjectId("snapshot-redaction".into());
    create_root(
        &mut store,
        &project,
        "reject-me snapshot root",
        "redaction-root",
    );

    assert!(matches!(
        store.save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &SentinelRedactor,
        ),
        Err(StoreError::RedactionRefused(_))
    ));
    let audits = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM objects WHERE object_kind = 'work_graph_snapshot_saved'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("count save audits");
    assert_eq!(audits, 0);
}

#[test]
fn widened_save_records_reason_even_when_current_project_memories_are_internal() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let mut store = SqliteStore::open(directory.path().join("engram.db")).expect("store");
    let project = ProjectId("snapshot-sensitive-memory".into());
    create_root(&mut store, &project, "Snapshot root", "snapshot-root");
    store
        .remember_project_memory(
            &RememberProjectMemoryRequest {
                revise: false,
                expected_revision: None,
                retiring_target: crate::domain::ProjectMemoryRetiringTargetChange::Keep,
                project_id: project.clone(),
                session_id: crate::SessionId("memory-session".into()),
                key: Some("snapshot-contract".into()),
                body: "ordinary project memory".into(),
                actor: actor("memory-session"),
                created_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("remember project memory");

    let default = store
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("default snapshot");
    let widening_reason = "restore  restricted planning context";
    let widened = store
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            Some(widening_reason),
            WorkGraphSnapshotDestinationKind::Stdout,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect("widened snapshot");

    assert_eq!(default.document.body.summary.redacted.memories, 0);
    assert_eq!(widened.document.body.summary.redacted.memories, 0);
    assert_ne!(default.body_sha256, widened.body_sha256);
    assert!(!default.document.body.summary.widened);
    assert!(widened.document.body.summary.widened);
    assert_eq!(default.document.body.summary.widening_reason, None);
    assert_eq!(
        widened.document.body.summary.widening_reason.as_deref(),
        Some(widening_reason)
    );
    assert_eq!(
        default.document.body.summary.redactor_status,
        "development no-op; no secret or PII protection"
    );
    let audits = store
        .work_graph_snapshot_save_audits(&project)
        .expect("sensitive snapshot audits");
    assert_eq!(audits.len(), 2);
    assert_eq!(audits[0].widening_reason, None);
    assert_eq!(audits[1].widening_reason.as_deref(), Some(widening_reason));
}
