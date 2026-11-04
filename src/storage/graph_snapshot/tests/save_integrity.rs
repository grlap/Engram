use super::*;

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario proves snapshot determinism, identity, audit order, and feed isolation"
)]
fn consecutive_idle_saves_keep_body_cut_digest_and_order() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let mut store = SqliteStore::open(directory.path().join("engram.db")).expect("store");
    let project = ProjectId("snapshot/project with spaces".into());
    let root = create_root(&mut store, &project, "Snapshot root", "snapshot-root");
    store
        .remember_project_memory(
            &RememberProjectMemoryRequest {
                revise: false,
                expected_revision: None,
                retiring_target: crate::domain::ProjectMemoryRetiringTargetChange::Keep,
                project_id: project.clone(),
                session_id: crate::SessionId("memory-session".into()),
                key: Some("snapshot-contract".into()),
                body: "Preserve the graph deterministically".into(),
                actor: actor("memory-session"),
                created_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("remember project memory");
    let work_head = store
        .work_feed_head(&crate::FeedId::Project(project.clone()))
        .expect("work feed head");

    let first = store
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::DefaultFile,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("first save");
    let second = store
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::DefaultFile,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect("second save");
    let repeated_timestamp = store
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::DefaultFile,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect("same-timestamp save");

    assert_eq!(first.document.body, second.document.body);
    assert_eq!(first.body_sha256, second.body_sha256);
    assert_eq!(
        first.document.manifest.summary,
        second.document.manifest.summary
    );
    assert_ne!(
        first.document.manifest.exported_at,
        second.document.manifest.exported_at
    );
    assert_eq!(
        first.document.manifest.exporting_build,
        work_graph_snapshot_exporting_build()
    );
    assert_eq!(first.document.body.summary.widening_reason, None);
    assert_eq!(second.document, repeated_timestamp.document);
    assert_eq!(first.document.body.summary.as_of.work_feed, work_head);
    assert_eq!(first.document.body.summary.as_of.project_memory, 1);
    assert_eq!(first.document.body.summary.section_counts.items, 1);
    assert_eq!(first.document.body.summary.section_counts.records, 1);
    assert_eq!(first.document.body.summary.section_counts.memories, 1);
    assert_eq!(first.document.body.items[0].work_id, root.work_id);
    assert_eq!(first.document.body.records[0].work_id, root.work_id);
    assert_eq!(
        CanonicalObject::freeze(&first.document.body)
            .expect("canonical body")
            .key(),
        &first.body_sha256
    );
    assert_eq!(first.document.manifest.body_sha256, first.body_sha256);
    let body_json = serde_json::to_string(&first.document.body).expect("render snapshot body");
    assert!(!body_json.contains("exporting_build"));
    let rendered = serde_json::to_string(&first.document).expect("render snapshot");
    assert!(!rendered.contains("active_run_id"));
    assert!(!rendered.contains("claim_fence"));
    assert!(!rendered.contains("completion_seal"));

    let audits = store
        .work_graph_snapshot_save_audits(&project)
        .expect("ordered save audits");
    assert_eq!(audits.len(), 3);
    assert_ne!(audits[0].attempt_id, audits[1].attempt_id);
    assert_ne!(audits[1].attempt_id, audits[2].attempt_id);
    assert_eq!(audits[0].body_sha256, first.body_sha256);
    assert_eq!(audits[0].widening_reason, None);
    assert_eq!(audits[1].body_sha256, second.body_sha256);
    assert_eq!(audits[2].body_sha256, repeated_timestamp.body_sha256);
    let (audit_total, recent_audits) = store
        .recent_work_graph_snapshot_save_audits(&project, 2)
        .expect("bounded recent save audits");
    assert_eq!(audit_total, 3);
    assert_eq!(recent_audits, audits[1..]);
    let integrity = store.verify_all().expect("integrity after save audits");
    assert_eq!(integrity.checked_graph_snapshot_audits, 3);
    assert!(
        integrity.invalid_graph_snapshot_audits.is_empty(),
        "{:?}",
        integrity.invalid_graph_snapshot_audits
    );
    assert_eq!(
        store
            .work_feed_head(&crate::FeedId::Project(project))
            .expect("work head after audit"),
        work_head,
        "save audit must not enter the project work feed"
    );
}

#[test]
fn unkeyed_project_scope_memory_does_not_enter_the_keyed_snapshot_section() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let mut store = SqliteStore::open(directory.path().join("engram.db")).expect("store");
    let project = ProjectId("snapshot-unkeyed-project-memory".into());
    create_root(&mut store, &project, "Snapshot root", "snapshot-root");
    store
        .insert_historical_memory_fixture(
            &NoteRequest {
                project_id: project.clone(),
                task_id: None,
                work_id: None,
                prose: "shared unkeyed project observation".into(),
                visibility: NoteVisibility::Shared,
                kind: Some(MemoryKind::Episode),
                authority: Some(Authority::Soft),
                sensitivity: Some(Sensitivity::Internal),
                title: Some("Unkeyed observation".into()),
                tags: Vec::new(),
                evidence: Vec::new(),
                refs: Vec::new(),
                actor: actor("memory-session"),
                idempotency_key: "unkeyed-project-observation".into(),
                created_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("capture unkeyed project memory");

    let snapshot = store
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("save ignores non-keyed project memory");
    assert!(
        snapshot.document.body.memories.is_empty(),
        "{:?}",
        snapshot.document.body.memories
    );
    assert_eq!(snapshot.document.body.summary.section_counts.memories, 0);
    assert_eq!(snapshot.document.body.summary.as_of.project_memory, 0);
}

#[test]
fn save_refuses_work_projection_that_disagrees_with_canonical_feed_history() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let mut store = SqliteStore::open(directory.path().join("engram.db")).expect("store");
    let project = ProjectId("snapshot-work-integrity".into());
    let root = create_root(&mut store, &project, "Snapshot root", "snapshot-root");
    let projected_bytes: Vec<u8> = store
        .connection
        .query_row(
            "SELECT item_json FROM work_items WHERE work_id = ?1",
            [root.work_id.0.to_string()],
            |row| row.get(0),
        )
        .expect("work projection");
    let mut projected: serde_json::Value =
        serde_json::from_slice(&projected_bytes).expect("decode work projection");
    projected["title"] = serde_json::json!("projection drift");
    store
        .connection
        .execute(
            "UPDATE work_items SET item_json = ?2 WHERE work_id = ?1",
            rusqlite::params![
                root.work_id.0.to_string(),
                serde_json::to_vec(&projected).expect("serialize corrupt projection"),
            ],
        )
        .expect("corrupt work projection");

    assert!(matches!(
        store.save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidWorkProjection(message))
            if message.contains("work integrity verification")
    ));
    let observed = store
        .work_graph_snapshot_save_audits(&project)
        .expect("save audit query");
    assert!(observed.is_empty(), "{observed:?}");
}

#[test]
fn save_refuses_project_memory_state_position_drift() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let mut store = SqliteStore::open(directory.path().join("engram.db")).expect("store");
    let project = ProjectId("snapshot-memory-state-integrity".into());
    store
        .remember_project_memory(
            &RememberProjectMemoryRequest {
                revise: false,
                expected_revision: None,
                retiring_target: crate::domain::ProjectMemoryRetiringTargetChange::Keep,
                project_id: project.clone(),
                session_id: crate::SessionId("memory-session".into()),
                key: Some("snapshot-contract".into()),
                body: "preserve the canonical memory cut".into(),
                actor: actor("memory-session"),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("remember project memory");
    store
        .connection
        .execute(
            "UPDATE project_memory_state SET change_position = change_position + 1
             WHERE project_id = ?1",
            [project.0.as_str()],
        )
        .expect("corrupt project-memory position");

    assert!(matches!(
        store.save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidMemoryProjection(message))
            if message.contains("canonical history")
    ));
    let observed = store
        .work_graph_snapshot_save_audits(&project)
        .expect("save audit query");
    assert!(observed.is_empty(), "{observed:?}");
}

#[test]
fn save_refuses_project_memory_head_projection_drift() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let mut store = SqliteStore::open(directory.path().join("engram.db")).expect("store");
    let project = ProjectId("snapshot-memory-head-integrity".into());
    store
        .remember_project_memory(
            &RememberProjectMemoryRequest {
                revise: false,
                expected_revision: None,
                retiring_target: crate::domain::ProjectMemoryRetiringTargetChange::Keep,
                project_id: project.clone(),
                session_id: crate::SessionId("memory-session".into()),
                key: Some("snapshot-contract".into()),
                body: "preserve the canonical memory projection".into(),
                actor: actor("memory-session"),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("remember project memory");
    store
        .connection
        .execute(
            "UPDATE memory_heads SET title = 'projection drift'
             WHERE project_id = ?1",
            [project.0.as_str()],
        )
        .expect("corrupt project-memory head");

    assert!(matches!(
        store.save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidMemoryProjection(message))
            if message.contains("canonical objects")
    ));
    let observed = store
        .work_graph_snapshot_save_audits(&project)
        .expect("save audit query");
    assert!(observed.is_empty(), "{observed:?}");
}
