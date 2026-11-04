use super::*;

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one round trip proves open planning relations, notes, memories, inert history, and resumed execution"
)]
fn save_load_save_recreates_inert_work_and_preserves_restored_records() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let mut source = SqliteStore::open(directory.path().join("source.db")).expect("source store");
    let project = ProjectId("snapshot-round-trip".into());
    let root = create_root(&mut source, &project, "Snapshot root", "snapshot-root");
    let prerequisite = create_root(
        &mut source,
        &project,
        "Snapshot prerequisite",
        "snapshot-prerequisite",
    );
    source
        .add_work_prerequisite(
            &ChangeWorkPrerequisiteRequest {
                work_id: root.work_id,
                prerequisite_id: prerequisite.work_id,
                expected_revision: root.revision,
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner-session"),
                idempotency_key: "snapshot-prerequisite-edge".into(),
                changed_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("add source prerequisite");
    let claim = source
        .claim_work(
            &ClaimWorkRequest {
                work_id: prerequisite.work_id,
                expected_work_revision: prerequisite.revision,
                expected_run_id: prerequisite.active_run_id,
                holder: crate::SessionId("note-session".into()),
                ttl_seconds: 900,
                recovery_reason: None,
                actor: actor("note-session"),
                idempotency_key: "claim-prerequisite-for-note".into(),
                claimed_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("claim source prerequisite");
    source
        .record_work_evidence(
            &RecordWorkEvidenceRequest {
                work_id: prerequisite.work_id,
                run_id: claim.run_id,
                expected_work_revision: prerequisite.revision,
                holder: claim.holder.clone(),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                summary: "the open prerequisite carries its note".into(),
                refs: vec!["review:open-item".into()],
                actor: actor("note-session"),
                idempotency_key: "note-open-prerequisite".into(),
                recorded_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("record source note");
    source
        .remember_project_memory(
            &RememberProjectMemoryRequest {
                revise: false,
                expected_revision: None,
                retiring_target: crate::domain::ProjectMemoryRetiringTargetChange::Keep,
                project_id: project.clone(),
                session_id: crate::SessionId("memory-session".into()),
                key: Some("snapshot-contract".into()),
                body: "recreate the keyed project memory".into(),
                actor: actor("memory-session"),
                created_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("remember source memory");
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("save source graph");
    let exported_root = saved
        .document
        .body
        .items
        .iter()
        .find(|item| item.work_id == root.work_id)
        .unwrap();
    assert_eq!(exported_root.prerequisites, [prerequisite.work_id]);
    let note_record = saved
        .document
        .body
        .records
        .iter()
        .find(|record| record.work_id == prerequisite.work_id)
        .unwrap();
    let crate::WorkGraphSnapshotRecordPayload::Native { history } = &note_record.payload else {
        panic!("source evidence is native history");
    };
    assert_eq!(history.notes.len(), 1);
    assert_eq!(
        history.notes[0].summary,
        "the open prerequisite carries its note"
    );
    assert_eq!(history.notes[0].refs, ["review:open-item"]);
    let bytes = serde_json::to_vec_pretty(&saved.document).expect("serialize snapshot");

    let mut restored =
        SqliteStore::open(directory.path().join("restored.db")).expect("restored store");
    let preview = restored
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &bytes,
            true,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect("dry-run load");
    assert!(!preview.loaded);
    assert_eq!(preview.preview.refs.len(), 2);
    assert!(preview.preview.refs.contains(&root.short_ref));
    assert!(preview.preview.refs.contains(&prerequisite.short_ref));
    assert_eq!(preview.preview.summary.section_counts.memories, 1);
    assert!(matches!(
        restored.inspect_work(root.work_id, at(4)),
        Err(StoreError::WorkNotFound(_))
    ));
    let (audit_count, audits) = restored
        .recent_work_graph_snapshot_load_audits(&project, 8)
        .expect("dry-run audit query");
    assert_eq!(audit_count, 0);
    assert!(audits.is_empty(), "{audits:?}");

    let loaded = restored
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &bytes,
            false,
            at(5),
            &DevelopmentNoopRedactor,
        )
        .expect("committed load");
    assert!(loaded.loaded);
    let (load_audit_count, load_audits) = restored
        .recent_work_graph_snapshot_load_audits(&project, 8)
        .expect("load audit query");
    assert_eq!(load_audit_count, 1);
    assert_eq!(load_audits.len(), 1);
    assert_eq!(load_audits[0].body_sha256, loaded.preview.body_sha256);
    let attempt_id = uuid::Uuid::parse_str(&load_audits[0].attempt_id).expect("load attempt id");
    assert_eq!(attempt_id.get_version(), Some(uuid::Version::SortRand));
    let memory = restored
        .project_memory_full(
            &project,
            &crate::SessionId("reader-session".into()),
            &actor("reader-session"),
            "snapshot-contract",
            None,
        )
        .expect("read restored project memory");
    assert_eq!(memory.body, "recreate the keyed project memory");
    let ready = restored
        .inspect_work(root.work_id, at(6))
        .expect("inspect restored work");
    assert!(ready.work.restored);
    assert!(ready.work.active_run_id.is_none());
    assert_eq!(ready.availability, crate::WorkAvailability::Blocked);
    assert_eq!(ready.blocked_by, vec![prerequisite.work_id]);
    let original_item_json = serde_json::to_vec(&ready.work).expect("serialize restored item");
    let mut tampered_item = ready.work.clone();
    tampered_item.title = "projection-only tamper".into();
    restored
        .connection
        .execute(
            "UPDATE work_items SET item_json = ?2 WHERE work_id = ?1",
            rusqlite::params![
                root.work_id.0.to_string(),
                serde_json::to_vec(&tampered_item).expect("serialize tampered item")
            ],
        )
        .expect("tamper restored projection");
    assert!(matches!(
        restored.get_work_item(root.work_id),
        Err(StoreError::InvalidWorkProjection(message))
            if message.contains("canonical restored record")
    ));
    restored
        .connection
        .execute(
            "UPDATE work_items SET item_json = ?2 WHERE work_id = ?1",
            rusqlite::params![root.work_id.0.to_string(), &original_item_json],
        )
        .expect("restore projected item");
    let assert_claim_refuses_projection = |store: &mut SqliteStore, key: &str| {
        assert!(matches!(
            store.claim_work(
                &ClaimWorkRequest {
                    work_id: root.work_id,
                    expected_work_revision: 1,
                    expected_run_id: None,
                    holder: crate::SessionId("tamper-session".into()),
                    ttl_seconds: 300,
                    recovery_reason: None,
                    actor: actor("tamper-session"),
                    idempotency_key: key.into(),
                    claimed_at: at(6),
                },
                &DevelopmentNoopRedactor,
            ),
            Err(StoreError::InvalidWorkProjection(_))
        ));
    };
    let mut schema_tamper = ready.work.clone();
    schema_tamper.schema_version += 1;
    restored
        .connection
        .execute(
            "UPDATE work_items SET item_json = ?2 WHERE work_id = ?1",
            rusqlite::params![
                root.work_id.0.to_string(),
                serde_json::to_vec(&schema_tamper).expect("serialize schema tamper")
            ],
        )
        .expect("tamper restored schema");
    assert_claim_refuses_projection(&mut restored, "claim-schema-tamper");
    let mut attribution_tamper = ready.work.clone();
    attribution_tamper.created_by = actor("invented-loader");
    restored
        .connection
        .execute(
            "UPDATE work_items SET item_json = ?2 WHERE work_id = ?1",
            rusqlite::params![
                root.work_id.0.to_string(),
                serde_json::to_vec(&attribution_tamper).expect("serialize attribution tamper")
            ],
        )
        .expect("tamper restored attribution");
    assert_claim_refuses_projection(&mut restored, "claim-attribution-tamper");
    for (name, created_at, updated_at) in [
        ("created-at", at(4), ready.work.updated_at),
        ("updated-at", ready.work.created_at, at(7)),
    ] {
        let mut time_tamper = ready.work.clone();
        time_tamper.created_at = created_at;
        time_tamper.updated_at = updated_at;
        restored
            .connection
            .execute(
                "UPDATE work_items SET item_json = ?2, created_at_ms = ?3, updated_at_ms = ?4
                 WHERE work_id = ?1",
                rusqlite::params![
                    root.work_id.0.to_string(),
                    serde_json::to_vec(&time_tamper).expect("serialize timestamp tamper"),
                    created_at.timestamp_millis(),
                    updated_at.timestamp_millis()
                ],
            )
            .expect("tamper both timestamp projections consistently");
        assert_claim_refuses_projection(&mut restored, name);
    }
    restored
        .connection
        .execute(
            "UPDATE work_items SET item_json = ?2, created_at_ms = ?3, updated_at_ms = ?4
             WHERE work_id = ?1",
            rusqlite::params![
                root.work_id.0.to_string(),
                &original_item_json,
                ready.work.created_at.timestamp_millis(),
                ready.work.updated_at.timestamp_millis()
            ],
        )
        .expect("restore projected item again");
    let prerequisite_anchor = restored
        .connection
        .query_row(
            "SELECT event_id FROM work_prerequisites
             WHERE work_id = ?1 AND prerequisite_id = ?2",
            rusqlite::params![
                root.work_id.0.to_string(),
                prerequisite.work_id.0.to_string()
            ],
            |row| row.get::<_, String>(0),
        )
        .expect("read restored prerequisite anchor");
    restored
        .connection
        .execute(
            "DELETE FROM work_prerequisites WHERE work_id = ?1 AND prerequisite_id = ?2",
            rusqlite::params![
                root.work_id.0.to_string(),
                prerequisite.work_id.0.to_string()
            ],
        )
        .expect("tamper restored relation projection");
    assert_claim_refuses_projection(&mut restored, "claim-relation-tamper");
    restored
        .connection
        .execute(
            "INSERT INTO work_prerequisites (work_id, prerequisite_id, event_id)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![
                root.work_id.0.to_string(),
                prerequisite.work_id.0.to_string(),
                prerequisite_anchor
            ],
        )
        .expect("restore relation projection");
    let restored_history = restored
        .work_restored_records(prerequisite.work_id)
        .expect("read restored prerequisite history");
    assert!(restored_history.iter().any(|record| {
        record.history.notes.iter().any(|note| {
            note.summary == "the open prerequisite carries its note"
                && note.refs == ["review:open-item"]
        })
    }));
    let (_, invalid) = restored
        .verify_work_projections()
        .expect("verify restored work");
    assert!(invalid.is_empty(), "{invalid:?}");

    let saved_again = restored
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(7),
            &DevelopmentNoopRedactor,
        )
        .expect("save restored graph");
    assert!(matches!(
        saved_again.document.body.records[0].payload,
        crate::WorkGraphSnapshotRecordPayload::Restored { .. }
    ));
    let bytes_again =
        serde_json::to_vec_pretty(&saved_again.document).expect("serialize restored snapshot");
    let mut restored_again = SqliteStore::open(directory.path().join("restored-again.db"))
        .expect("second restored store");
    restored_again
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &bytes_again,
            false,
            at(8),
            &DevelopmentNoopRedactor,
        )
        .expect("load restored graph again");
    let (_, invalid) = restored_again
        .verify_work_projections()
        .expect("verify second restored work");
    assert!(invalid.is_empty(), "{invalid:?}");

    let claim = restored_again
        .claim_work(
            &ClaimWorkRequest {
                work_id: prerequisite.work_id,
                expected_work_revision: 1,
                expected_run_id: None,
                holder: crate::SessionId("runner-session".into()),
                ttl_seconds: 900,
                recovery_reason: None,
                actor: actor("runner-session"),
                idempotency_key: "claim-restored-root".into(),
                claimed_at: at(9),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("claim restored work");
    assert_eq!(claim.work_id, prerequisite.work_id);
    let (_, invalid) = restored_again
        .verify_work_projections()
        .expect("verify claimed restored work");
    assert!(invalid.is_empty(), "{invalid:?}");
    let saved_after_claim = restored_again
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(10),
            &DevelopmentNoopRedactor,
        )
        .expect("save claimed restored graph");
    assert_eq!(saved_after_claim.document.body.records.len(), 3);
    assert_eq!(
        saved_after_claim
            .document
            .body
            .records
            .iter()
            .filter(|record| matches!(
                record.payload,
                crate::WorkGraphSnapshotRecordPayload::Restored { .. }
            ))
            .count(),
        2
    );
    assert_eq!(
        saved_after_claim
            .document
            .body
            .records
            .iter()
            .filter(|record| matches!(
                record.payload,
                crate::WorkGraphSnapshotRecordPayload::Native { .. }
            ))
            .count(),
        1
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one blocked graph fixture exercises all three runless planning mutations and integrity"
)]
fn runless_restored_work_supports_blocked_planning_and_disposal() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let project = ProjectId("snapshot-runless-planning".into());
    let mut source =
        SqliteStore::open(directory.path().join("runless-source.db")).expect("source store");
    let empty = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(0),
            &DevelopmentNoopRedactor,
        )
        .expect("save empty graph");
    let cancel = create_root(&mut source, &project, "Cancel restored", "runless-cancel");
    let supersede = create_root(
        &mut source,
        &project,
        "Supersede restored",
        "runless-supersede",
    );
    let decompose = create_root(
        &mut source,
        &project,
        "Decompose restored",
        "runless-decompose",
    );
    let replacement = create_root(&mut source, &project, "Replacement", "runless-replacement");
    for (index, work) in [&cancel, &supersede, &decompose].into_iter().enumerate() {
        source
            .add_work_blocker(
                &AddWorkBlockerRequest {
                    work_id: work.work_id,
                    expected_work_revision: work.revision,
                    kind: WorkBlockerKind::Manual,
                    detail: format!("restored blocker {index}"),
                    authority: WorkPlanningAuthority::Project,
                    actor: actor("planner-session"),
                    idempotency_key: format!("runless-blocker-{index}"),
                    blocked_at: at(5 + i64::try_from(index).expect("small index")),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("block source work");
    }
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(10),
            &DevelopmentNoopRedactor,
        )
        .expect("save blocked roots");
    assert_eq!(saved.document.body.blockers.len(), 3);
    for (index, work) in [&cancel, &supersede, &decompose].into_iter().enumerate() {
        let blocker = saved
            .document
            .body
            .blockers
            .iter()
            .find(|entry| entry.work_id == work.work_id)
            .unwrap();
        assert_eq!(blocker.kind, WorkBlockerKind::Manual);
        assert_eq!(blocker.detail, format!("restored blocker {index}"));
        assert_eq!(blocker.created_by, actor("planner-session"));
    }
    let bytes = snapshot_bytes(&saved.document);

    let mut cancelled =
        SqliteStore::open(directory.path().join("runless-cancel.db")).expect("cancel store");
    cancelled
        .load_work_graph_snapshot(
            &project,
            &actor("empty-load-session"),
            &snapshot_bytes(&empty.document),
            false,
            at(20),
            &DevelopmentNoopRedactor,
        )
        .expect("empty load must not obstruct a later work load with an earlier clock");
    cancelled
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &bytes,
            false,
            at(11),
            &DevelopmentNoopRedactor,
        )
        .expect("load cancel graph");
    let cancelled_item = cancelled
        .dispose_work(
            &DisposeWorkRequest {
                work_id: cancel.work_id,
                expected_work_revision: 1,
                disposition: WorkDisposition::Cancelled,
                replacement_id: None,
                reason: "cancel restored blocked work".into(),
                actor: actor("planner-session"),
                idempotency_key: "runless-cancel-dispose".into(),
                disposed_at: at(12),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("cancel restored blocked work");
    assert_eq!(cancelled_item.lifecycle, WorkLifecycle::Cancelled);
    assert!(
        cancelled
            .verify_all()
            .expect("verify cancellation")
            .is_healthy()
    );

    let mut superseded =
        SqliteStore::open(directory.path().join("runless-supersede.db")).expect("supersede store");
    superseded
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &bytes,
            false,
            at(11),
            &DevelopmentNoopRedactor,
        )
        .expect("load supersede graph");
    let superseded_item = superseded
        .dispose_work(
            &DisposeWorkRequest {
                work_id: supersede.work_id,
                expected_work_revision: 1,
                disposition: WorkDisposition::Superseded,
                replacement_id: Some(replacement.work_id),
                reason: "replace restored blocked work".into(),
                actor: actor("planner-session"),
                idempotency_key: "runless-supersede-dispose".into(),
                disposed_at: at(12),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("supersede restored blocked work");
    assert_eq!(superseded_item.lifecycle, WorkLifecycle::Superseded);
    assert_eq!(superseded_item.superseded_by, Some(replacement.work_id));
    assert!(
        superseded
            .verify_all()
            .expect("verify supersession")
            .is_healthy()
    );

    let mut decomposed =
        SqliteStore::open(directory.path().join("runless-decompose.db")).expect("decompose store");
    decomposed
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &bytes,
            false,
            at(11),
            &DevelopmentNoopRedactor,
        )
        .expect("load decompose graph");
    let mut request = DecomposeWorkRequest {
        parent_id: decompose.work_id,
        expected_parent_revision: 1,
        children: vec![ChildWorkDraft {
            acceptance_bindings: Vec::new(),
            evaluation_mode: None,
            external_ref: None,
            notes: Vec::new(),
            local_key: "native-child".into(),
            child_requirement: ChildRequirement::Required,
            title: "Native child after load".into(),
            outcome: "the child is planned".into(),
            acceptance: vec!["child exists".into()],
            kind: WorkItemKind::Task,
            priority: 2,
            labels: Vec::new(),
            assigned_to: None,
            deferred_until: None,
        }],
        prerequisites: Vec::new(),
        authority: WorkPlanningAuthority::Project,
        actor: actor("planner-session"),
        idempotency_key: "runless-decomposition".into(),
        created_at: at(12),
    };
    request.prerequisites.push(crate::ChildWorkPrerequisite {
        work_key: "native-child".into(),
        prerequisite: crate::WorkDependencyRef::Existing(decompose.work_id),
    });
    let before = decomposed
        .get_work_item(decompose.work_id)
        .expect("parent before refusal");
    let objects_before: i64 = decomposed
        .connection
        .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
        .expect("objects before refusal");
    assert!(matches!(
        decomposed.decompose_work(&request, &DevelopmentNoopRedactor),
        Err(StoreError::WorkDependencyCycle)
    ));
    assert_eq!(
        decomposed
            .get_work_item(decompose.work_id)
            .expect("parent after refusal"),
        before
    );
    let objects_after: i64 = decomposed
        .connection
        .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
        .expect("objects after refusal");
    assert_eq!(objects_after, objects_before);
    assert!(
        decomposed
            .verify_all()
            .expect("verify refused decompose")
            .is_healthy()
    );
    request.prerequisites.clear();
    let result = decomposed
        .decompose_work(&request, &DevelopmentNoopRedactor)
        .expect("decompose restored blocked work");
    assert_eq!(result.children.len(), 1);
    assert!(result.parent.active_run_id.is_some());
    assert!(
        decomposed
            .verify_all()
            .expect("verify decomposition")
            .is_healthy()
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one round trip creates terminal fanout above the live open-descendant envelope"
)]
fn terminal_direct_children_above_the_open_envelope_round_trip() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let project = ProjectId("snapshot-terminal-fanout".into());
    let mut source =
        SqliteStore::open(directory.path().join("terminal-source.db")).expect("source store");
    let root = create_root(
        &mut source,
        &project,
        "Large terminal history",
        "terminal-root",
    );
    let replacement = create_root(
        &mut source,
        &project,
        "Terminal replacement",
        "terminal-replacement",
    );
    let mut child_count = 0_usize;
    let mut open_child_count = 0_usize;
    let final_batch = super::super::super::work::MAX_OPEN_WORK_DESCENDANTS / 16 + 1;
    for batch in 0..=final_batch {
        let parent = source.get_work_item(root.work_id).expect("current parent");
        let children = (0..16)
            .map(|index| ChildWorkDraft {
                acceptance_bindings: Vec::new(),
                evaluation_mode: None,
                external_ref: None,
                notes: Vec::new(),
                local_key: format!("child-{batch:02}-{index:02}"),
                child_requirement: ChildRequirement::Optional,
                title: format!("Terminal child {batch:02}-{index:02}"),
                outcome: "Preserve terminal planning history".into(),
                acceptance: vec!["terminal history is retained".into()],
                kind: WorkItemKind::Task,
                priority: 2,
                labels: vec!["snapshot".into()],
                assigned_to: None,
                deferred_until: None,
            })
            .collect();
        let decomposition = source
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: root.work_id,
                    expected_parent_revision: parent.revision,
                    children,
                    prerequisites: Vec::new(),
                    authority: WorkPlanningAuthority::Project,
                    actor: actor("planner-session"),
                    idempotency_key: format!("terminal-batch-{batch}"),
                    created_at: at(i64::from(batch) + 2),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("decompose terminal batch");
        for (index, child) in decomposition.children.into_iter().enumerate() {
            if batch == final_batch && index >= 12 {
                open_child_count += 1;
                continue;
            }
            let superseded = child_count == 0;
            source
                .dispose_work(
                    &DisposeWorkRequest {
                        work_id: child.work_id,
                        expected_work_revision: child.revision,
                        disposition: if superseded {
                            WorkDisposition::Superseded
                        } else {
                            WorkDisposition::Cancelled
                        },
                        replacement_id: superseded.then_some(replacement.work_id),
                        reason: "retained terminal history".into(),
                        actor: actor("planner-session"),
                        idempotency_key: format!("cancel-terminal-{}", child.short_ref),
                        disposed_at: at(i64::from(batch) + 20),
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect("cancel terminal child");
            child_count += 1;
        }
    }
    assert!(child_count > super::super::super::work::MAX_OPEN_WORK_DESCENDANTS as usize);
    assert!(open_child_count > 0);
    assert!(open_child_count <= super::super::super::work::MAX_OPEN_WORK_DESCENDANTS as usize);
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(100),
            &DevelopmentNoopRedactor,
        )
        .expect("save terminal fanout");
    let exported_children: Vec<_> = saved
        .document
        .body
        .items
        .iter()
        .filter(|item| item.parent_id == Some(root.work_id))
        .collect();
    assert_eq!(exported_children.len(), child_count + open_child_count);
    let terminal_children = exported_children
        .into_iter()
        .filter(|child| {
            matches!(
                child.lifecycle,
                WorkLifecycle::Cancelled | WorkLifecycle::Superseded
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(terminal_children.len(), child_count);
    for child in terminal_children {
        assert!(matches!(
            child.lifecycle,
            WorkLifecycle::Cancelled | WorkLifecycle::Superseded
        ));
        assert_eq!(
            child.disposal_reason.as_deref(),
            Some("retained terminal history")
        );
        let record = saved
            .document
            .body
            .records
            .iter()
            .find(|record| record.work_id == child.work_id)
            .unwrap();
        let crate::WorkGraphSnapshotRecordPayload::Native { history } = &record.payload else {
            panic!("terminal source history is native");
        };
        let final_event = history.events.last().unwrap();
        assert_eq!(final_event.lifecycle, Some(child.lifecycle));
        assert_eq!(final_event.reason, child.disposal_reason);
    }
    let bytes = snapshot_bytes(&saved.document);
    let mut destination = SqliteStore::open(directory.path().join("terminal-destination.db"))
        .expect("destination store");
    let structural_size = relation_bases::graph_validation_size(&saved.document);
    GRAPH_VALIDATION_WORK.with(|work| work.set(0));
    destination
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &bytes,
            false,
            at(101),
            &DevelopmentNoopRedactor,
        )
        .expect("load terminal fanout");
    let validation_work = GRAPH_VALIDATION_WORK.with(std::cell::Cell::get);
    let bound = structural_size.saturating_mul(20).saturating_add(64);
    assert!(
        validation_work <= bound,
        "terminal fanout validation work {validation_work} exceeds {bound} for structural size {structural_size}"
    );
    let children = destination
        .work_children(root.work_id)
        .expect("restored terminal children");
    assert_eq!(children.len(), child_count + open_child_count);
    assert_eq!(
        children
            .iter()
            .filter(|child| child.lifecycle == WorkLifecycle::Superseded)
            .count(),
        1
    );
    assert_eq!(
        children
            .iter()
            .filter(|child| child.lifecycle == WorkLifecycle::Cancelled)
            .count(),
        child_count - 1
    );
    assert_eq!(
        children
            .iter()
            .filter(|child| child.lifecycle == WorkLifecycle::Open)
            .count(),
        open_child_count
    );
    assert!(children.iter().any(|child| {
        child.lifecycle == WorkLifecycle::Superseded
            && child.superseded_by == Some(replacement.work_id)
    }));
}
