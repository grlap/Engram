use super::*;

fn assert_corrupt_without_writes(project: &ProjectId, bytes: &[u8], message: &str) {
    let directory = tempdir().expect("refusal directory");
    let mut destination =
        SqliteStore::open(directory.path().join("destination.db")).expect("destination");
    let before = [
        "objects",
        "work_items",
        "work_restored_records",
        "memory_heads",
    ]
    .map(|table| {
        let count: i64 = destination
            .connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("initial destination count");
        (table, count)
    });
    for dry_run in [true, false] {
        let error = destination
            .load_work_graph_snapshot(
                project,
                &actor("load-session"),
                bytes,
                dry_run,
                at(10),
                &DevelopmentNoopRedactor,
            )
            .expect_err("corrupt snapshot must refuse both preview and load");
        assert!(
            matches!(error, StoreError::InvalidGraphSnapshot(ref detail) if detail.contains(message)),
            "{error:?}"
        );
        for (table, initial_count) in before {
            let count: i64 = destination
                .connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .expect("destination count");
            assert_eq!(count, initial_count, "refusal wrote {table}");
        }
    }
}

#[test]
fn duplicate_json_members_refuse_before_canonical_hashing() {
    let directory = tempdir().expect("source directory");
    let project = ProjectId("snapshot-duplicate-members".into());
    let mut source = SqliteStore::open(directory.path().join("source.db")).expect("source");
    create_imported_root(&mut source, &project);
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("save imported source");
    let bytes = serde_json::to_string(&saved.document).expect("snapshot JSON");
    let original: Value = serde_json::from_str(&bytes).expect("original value");
    for duplicate in [
        bytes.replacen('{', "{\"body\":null,", 1),
        bytes.replacen(
            "\"canonical_json\":{",
            "\"canonical_json\":{\"schema_version\":null,",
            1,
        ),
    ] {
        assert_ne!(duplicate, bytes, "fixture must introduce a duplicate");
        // A last-value-wins decoder silently discards the poisoned first value,
        // leaving a document whose body and carried source hashes still match.
        assert_eq!(
            serde_json::from_str::<Value>(&duplicate).expect("last-value-wins JSON"),
            original
        );
        assert_corrupt_without_writes(&project, duplicate.as_bytes(), "duplicate JSON member");
        assert!(matches!(
            crate::parse_work_graph_snapshot_document(duplicate.as_bytes()),
            Err(StoreError::InvalidGraphSnapshot(message)) if message.contains("duplicate JSON member")
        ));
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "both terminal states and native/inherited layers share the same proof and round-trip contract"
)]
fn terminal_snapshot_layers_bind_their_latest_disposal_event() {
    let directory = tempdir().expect("source directory");
    let project = ProjectId("snapshot-terminal-proof".into());
    let mut source = SqliteStore::open(directory.path().join("source.db")).expect("source");
    let cancelled = create_root(&mut source, &project, "Cancelled", "cancelled");
    let superseded = create_root(&mut source, &project, "Superseded", "superseded");
    let replacement = create_root(&mut source, &project, "Replacement", "replacement");
    for (item, disposition, replacement_id) in [
        (&cancelled, WorkDisposition::Cancelled, None),
        (
            &superseded,
            WorkDisposition::Superseded,
            Some(replacement.work_id),
        ),
        // Supersession checks its target at admission, not forever: after
        // A -> B, native B may itself be cancelled. Preserve that history.
        (&replacement, WorkDisposition::Cancelled, None),
    ] {
        source
            .dispose_work(
                &DisposeWorkRequest {
                    work_id: item.work_id,
                    expected_work_revision: item.revision,
                    disposition,
                    replacement_id,
                    reason: format!("dispose {} deliberately", item.title),
                    actor: actor("planner-session"),
                    idempotency_key: format!("dispose-{}", item.short_ref),
                    disposed_at: at(2),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("dispose source item");
    }
    let mut document = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("save terminal items")
        .document;
    let original_items = document.body.items.clone();
    for generation in 0..2 {
        for work_id in [cancelled.work_id, superseded.work_id] {
            for defect in [
                "missing",
                "reason",
                "lifecycle",
                "successor",
                "latest",
                "revised",
                "reopened",
            ] {
                let mut corrupt = document.clone();
                let record = corrupt
                    .body
                    .records
                    .iter_mut()
                    .find(|record| record.work_id == work_id)
                    .expect("item history");
                let mut history = match &record.payload {
                    WorkGraphSnapshotRecordPayload::Native { history } => (**history).clone(),
                    WorkGraphSnapshotRecordPayload::Restored { canonical_json, .. } => {
                        serde_json::from_value::<RestoredRecord>(canonical_json.clone())
                            .expect("inherited record")
                            .history
                    }
                };
                let event = history
                    .events
                    .iter_mut()
                    .rev()
                    .find(|event| event.kind == "disposed")
                    .expect("disposal event");
                match defect {
                    "missing" => history.events.retain(|event| event.kind != "disposed"),
                    "reason" => event.reason = Some("different justification".into()),
                    "lifecycle" => {
                        let cancelled = event.lifecycle == Some(WorkLifecycle::Cancelled);
                        event.lifecycle = Some(if cancelled {
                            WorkLifecycle::Superseded
                        } else {
                            WorkLifecycle::Cancelled
                        });
                        event.related_work_id = cancelled.then_some(replacement.work_id);
                    }
                    "successor" => {
                        if event.lifecycle == Some(WorkLifecycle::Cancelled) {
                            // A well-shaped supersession event cannot prove cancellation.
                            event.lifecycle = Some(WorkLifecycle::Superseded);
                        }
                        event.related_work_id = Some(cancelled.work_id);
                    }
                    "latest" => {
                        let mut later = event.clone();
                        later.reason = Some("newest disposal disagrees".into());
                        history.events.push(later);
                    }
                    "revised" | "reopened" => {
                        let mut later = event.clone();
                        later.kind = defect.into();
                        later.reason = (defect == "reopened").then(|| "resume work".into());
                        later.lifecycle = None;
                        later.related_work_id = None;
                        history.events.push(later);
                    }
                    _ => unreachable!("fixed defect list"),
                }
                match &mut record.payload {
                    WorkGraphSnapshotRecordPayload::Native { history: target } => {
                        **target = history;
                    }
                    WorkGraphSnapshotRecordPayload::Restored {
                        canonical_json,
                        object_hash,
                    } => {
                        let mut restored: RestoredRecord =
                            serde_json::from_value(canonical_json.clone())
                                .expect("restored record");
                        restored.history = history;
                        *canonical_json =
                            serde_json::to_value(&restored).expect("changed inherited record");
                        *object_hash = CanonicalObject::freeze(&restored)
                            .expect("rehash inherited record")
                            .hash()
                            .clone();
                    }
                }
                rebind_snapshot_body(&mut corrupt);
                assert_corrupt_without_writes(&project, &snapshot_bytes(&corrupt), "disposal");
            }
        }
        let mut destination =
            SqliteStore::open(directory.path().join(format!("roundtrip-{generation}.db")))
                .expect("round-trip store");
        destination
            .load_work_graph_snapshot(
                &project,
                &actor("load-session"),
                &snapshot_bytes(&document),
                false,
                at(4),
                &DevelopmentNoopRedactor,
            )
            .expect("load terminal items");
        let saved = destination
            .save_work_graph_snapshot(
                &project,
                &actor("save-session"),
                None,
                WorkGraphSnapshotDestinationKind::Stdout,
                at(5),
                &DevelopmentNoopRedactor,
            )
            .expect("resave terminal items");
        assert_eq!(saved.document.body.items, original_items);
        if generation > 0 {
            assert_eq!(
                saved.document.body.records, document.body.records,
                "inherited records remain verbatim"
            );
        }
        document = saved.document;
    }
    // An older proof does not rescue a new, terminal layer with no disposal.
    let mut corrupt = document;
    let index = corrupt
        .body
        .records
        .iter()
        .position(|record| record.work_id == cancelled.work_id)
        .expect("cancelled record");
    corrupt.body.records.insert(
        index + 1,
        crate::WorkGraphSnapshotRecord {
            work_id: cancelled.work_id,
            generation_index: 1,
            payload: WorkGraphSnapshotRecordPayload::Native {
                history: Box::new(WorkGraphSnapshotHistory {
                    notes: Vec::new(),
                    events: Vec::new(),
                    completion: None,
                }),
            },
        },
    );
    corrupt.body.summary.section_counts.records += 1;
    rebind_snapshot_body(&mut corrupt);
    assert_corrupt_without_writes(&project, &snapshot_bytes(&corrupt), "disposal");
}

#[test]
fn nonterminal_snapshot_layers_refuse_a_latest_disposal_event() {
    let directory = tempdir().expect("source directory");
    let project = ProjectId("snapshot-nonterminal-proof".into());
    let mut source = SqliteStore::open(directory.path().join("source.db")).expect("source");
    create_root(&mut source, &project, "Open work", "open");
    let document = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("save open item")
        .document;
    let mut destination =
        SqliteStore::open(directory.path().join("restored.db")).expect("restored destination");
    destination
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &snapshot_bytes(&document),
            false,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect("load open item");
    let inherited = destination
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(5),
            &DevelopmentNoopRedactor,
        )
        .expect("save inherited open item")
        .document;
    for original in [document, inherited] {
        for lifecycle in [
            WorkLifecycle::Proposed,
            WorkLifecycle::Open,
            WorkLifecycle::Completed,
        ] {
            for after_disposal in [false, true] {
                let mut corrupt = original.clone();
                corrupt.body.items[0].lifecycle = lifecycle;
                let change_history = |history: &mut WorkGraphSnapshotHistory| {
                    history.completion = (lifecycle == WorkLifecycle::Completed).then(|| {
                        crate::WorkGraphSnapshotCompletion {
                            summary: "completed work".into(),
                            completed_at: at(2),
                            actor: actor("planner-session"),
                        }
                    });
                    let mut disposed = history.events[0].clone();
                    disposed.kind = "disposed".into();
                    disposed.lifecycle = Some(WorkLifecycle::Cancelled);
                    disposed.reason = Some("cancel deliberately".into());
                    history.events.push(disposed);
                    if after_disposal {
                        let mut later = history.events[0].clone();
                        later.kind = "revised".into();
                        history.events.push(later);
                    }
                };
                match &mut corrupt.body.records[0].payload {
                    WorkGraphSnapshotRecordPayload::Native { history } => change_history(history),
                    WorkGraphSnapshotRecordPayload::Restored {
                        object_hash,
                        canonical_json,
                    } => {
                        let mut restored: RestoredRecord =
                            serde_json::from_value(canonical_json.clone())
                                .expect("inherited record");
                        restored.item.lifecycle = lifecycle;
                        change_history(&mut restored.history);
                        *canonical_json =
                            serde_json::to_value(&restored).expect("changed inherited record");
                        *object_hash = CanonicalObject::freeze(&restored)
                            .expect("rehash record")
                            .hash()
                            .clone();
                    }
                }
                rebind_snapshot_body(&mut corrupt);
                assert_corrupt_without_writes(&project, &snapshot_bytes(&corrupt), "disposal");
            }
        }
    }
}
