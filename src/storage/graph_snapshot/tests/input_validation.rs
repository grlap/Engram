use super::*;

fn assert_corrupt_without_writes(project: &ProjectId, bytes: &[u8], message: &str) {
    let directory = crate::test_support::temp_home().expect("refusal directory");
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
    let directory = crate::test_support::temp_home().expect("source directory");
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
    let directory = crate::test_support::temp_home().expect("source directory");
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
                    // The inherited record keeps the id it was stored
                    // under; an id never follows content.
                    WorkGraphSnapshotRecordPayload::Restored { canonical_json, .. } => {
                        let mut restored: RestoredRecord =
                            serde_json::from_value(canonical_json.clone())
                                .expect("restored record");
                        restored.history = history;
                        *canonical_json =
                            serde_json::to_value(&restored).expect("changed inherited record");
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
                    source_notices: Vec::new(),
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
    let directory = crate::test_support::temp_home().expect("source directory");
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
                    // The inherited record keeps the id it was stored
                    // under; an id never follows content.
                    WorkGraphSnapshotRecordPayload::Restored { canonical_json, .. } => {
                        let mut restored: RestoredRecord =
                            serde_json::from_value(canonical_json.clone())
                                .expect("inherited record");
                        restored.item.lifecycle = lifecycle;
                        change_history(&mut restored.history);
                        *canonical_json =
                            serde_json::to_value(&restored).expect("changed inherited record");
                    }
                }
                rebind_snapshot_body(&mut corrupt);
                assert_corrupt_without_writes(&project, &snapshot_bytes(&corrupt), "disposal");
            }
        }
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "two save/load generations establish independent scalar and historical-proof regressions"
)]
fn load_validates_exact_and_internal_shape_of_every_restored_generation() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let project = ProjectId("snapshot-restored-generation-validation".into());
    let mut source =
        SqliteStore::open(directory.path().join("generation-source.db")).expect("source store");
    let root = create_root(&mut source, &project, "Generation root", "generation-root");
    let first = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(2),
            &DevelopmentNoopRedactor,
        )
        .expect("save first generation");
    let mut middle =
        SqliteStore::open(directory.path().join("generation-middle.db")).expect("middle store");
    middle
        .load_work_graph_snapshot(
            &project,
            &actor("middle-load"),
            &snapshot_bytes(&first.document),
            false,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("load first generation");
    middle
        .claim_work(
            &ClaimWorkRequest {
                work_id: root.work_id,
                expected_work_revision: 1,
                expected_run_id: None,
                holder: crate::SessionId("runner-session".into()),
                ttl_seconds: 300,
                recovery_reason: None,
                actor: actor("runner-session"),
                idempotency_key: "generation-claim".into(),
                claimed_at: at(4),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("claim restored work");
    let second = middle
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(5),
            &DevelopmentNoopRedactor,
        )
        .expect("save second generation");
    let mut terminal =
        SqliteStore::open(directory.path().join("generation-terminal.db")).expect("terminal store");
    terminal
        .load_work_graph_snapshot(
            &project,
            &actor("terminal-load"),
            &snapshot_bytes(&second.document),
            false,
            at(6),
            &DevelopmentNoopRedactor,
        )
        .expect("load second generation");
    let carried = terminal
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(7),
            &DevelopmentNoopRedactor,
        )
        .expect("save two carried generations");
    assert_eq!(carried.document.body.records.len(), 2);
    assert!(carried.document.body.records.iter().all(|record| matches!(
        record.payload,
        crate::WorkGraphSnapshotRecordPayload::Restored { .. }
    )));

    let mut nonpreserved_scalar = carried.document.clone();
    let crate::WorkGraphSnapshotRecordPayload::Restored {
        object_id,
        canonical_json,
    } = &mut nonpreserved_scalar.body.records[0].payload
    else {
        panic!("first generation must be restored");
    };
    let created_at = canonical_json["history"]["events"][0]["occurred_at"]
        .as_str()
        .expect("history event timestamp")
        .strip_suffix('Z')
        .expect("UTC timestamp")
        .to_owned()
        + "+00:00";
    canonical_json["history"]["events"][0]["occurred_at"] = serde_json::json!(created_at);
    // Content changes under the inherited record's own stored id.
    assert_eq!(*object_id, carried_id(&carried.document, 0));
    rebind_snapshot_body(&mut nonpreserved_scalar);
    let mut destination =
        SqliteStore::open(directory.path().join("generation-unknown.db")).expect("destination");
    assert!(matches!(
        destination.load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &snapshot_bytes(&nonpreserved_scalar),
            false,
            at(8), &DevelopmentNoopRedactor,),
        Err(StoreError::InvalidGraphSnapshot(message)) if message.contains("exactly preserved")
    ));

    let mut invalid_old_lifecycle = carried.document.clone();
    let crate::WorkGraphSnapshotRecordPayload::Restored {
        object_id,
        canonical_json,
    } = &mut invalid_old_lifecycle.body.records[0].payload
    else {
        panic!("first generation must be restored");
    };
    canonical_json["item"]["lifecycle"] = serde_json::json!("completed");
    assert_eq!(*object_id, carried_id(&carried.document, 0));
    rebind_snapshot_body(&mut invalid_old_lifecycle);
    let mut destination =
        SqliteStore::open(directory.path().join("generation-lifecycle.db")).expect("destination");
    assert!(matches!(
        destination.load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &snapshot_bytes(&invalid_old_lifecycle),
            false,
            at(8), &DevelopmentNoopRedactor,),
        Err(StoreError::InvalidGraphSnapshot(message))
            if message.contains("completion proof")
    ));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one refusal matrix proves corrupt inputs never leave partial destination state"
)]
fn load_refuses_incompatible_and_corrupt_documents_without_partial_state() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let project = ProjectId("snapshot-load-refusals".into());
    let mut source =
        SqliteStore::open(directory.path().join("source-refusals.db")).expect("source store");
    let root = create_root(&mut source, &project, "Snapshot root", "snapshot-root");
    insert_classified_project_memory(
        &mut source,
        &project,
        "restricted-refusal",
        "restricted body",
        Sensitivity::Restricted,
        at(2),
    );
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

    let refuse = |name: &str, bytes: &[u8], destination: &ProjectId| {
        let mut store = SqliteStore::open(directory.path().join(format!("{name}.db")))
            .expect("open refusal destination");
        let error = store
            .load_work_graph_snapshot(
                destination,
                &actor("load-session"),
                bytes,
                false,
                at(4),
                &DevelopmentNoopRedactor,
            )
            .expect_err("load must be refused");
        let work_count = store
            .connection
            .query_row("SELECT COUNT(*) FROM work_items", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("count destination work");
        assert_eq!(work_count, 0, "{name} left partial work");
        error
    };

    let mut digest_mismatch = saved.document.clone();
    digest_mismatch.body.items[0].title = "Changed without rebinding the digest".into();
    assert!(matches!(
        refuse(
            "digest-mismatch",
            &snapshot_bytes(&digest_mismatch),
            &project
        ),
        StoreError::InvalidGraphSnapshot(message) if message.contains("digest")
    ));

    let mut dangling = saved.document.clone();
    dangling.body.items[0].prerequisites = vec![WorkId::new()];
    rebind_snapshot_body(&mut dangling);
    assert!(matches!(
        refuse("dangling", &snapshot_bytes(&dangling), &project),
        StoreError::InvalidGraphSnapshot(message) if message.contains("dangling")
    ));

    let mut false_redaction_count = saved.document.clone();
    false_redaction_count.body.summary.redacted.memories = 2;
    rebind_snapshot_body(&mut false_redaction_count);
    assert!(matches!(
        refuse(
            "redaction-count",
            &snapshot_bytes(&false_redaction_count),
            &project
        ),
        StoreError::InvalidGraphSnapshot(message) if message.contains("redacted counts")
    ));

    let mut unwidened_plaintext = saved.document.clone();
    let WorkGraphSnapshotMemoryState::Active {
        body, sensitivity, ..
    } = &mut unwidened_plaintext.body.memories[0].state
    else {
        panic!("restricted fixture must be active");
    };
    assert_eq!(*sensitivity, Sensitivity::Restricted);
    *body = WorkGraphSnapshotText::Present {
        value: "restricted body".into(),
    };
    unwidened_plaintext.body.summary.redacted.memories = 0;
    rebind_snapshot_body(&mut unwidened_plaintext);
    assert!(matches!(
        refuse(
            "unwidened-restricted-plaintext",
            &snapshot_bytes(&unwidened_plaintext),
            &project
        ),
        StoreError::InvalidGraphSnapshot(message) if message.contains("requires a widened snapshot")
    ));

    let mut invalid_gate = saved.document.clone();
    let WorkGraphSnapshotRecordPayload::Native { history } =
        &mut invalid_gate.body.records[0].payload
    else {
        panic!("source save must contain a native history layer");
    };
    // A gate that claims to pass while naming a failure is inconsistent in
    // any build; spacing alone is a stored shape the gate contract keeps.
    history.notes.push(crate::WorkGraphSnapshotNote {
        evidence_kind: crate::WorkEvidenceKind::Generic,
        summary: "inconsistent gate".into(),
        refs: Vec::new(),
        gate: Some(crate::WorkGraphSnapshotGate {
            name: "cargo-test".into(),
            passed: true,
            failed: vec!["a failure".into()],
            evidence_ref: None,
        }),
        actor: actor("gate-session"),
        recorded_at: at(2),
    });
    rebind_snapshot_body(&mut invalid_gate);
    assert!(matches!(
        refuse("invalid-gate", &snapshot_bytes(&invalid_gate), &project),
        StoreError::InvalidGraphSnapshot(message) if message.contains("gate fields")
    ));

    let mut unknown_event = saved.document.clone();
    let WorkGraphSnapshotRecordPayload::Native { history } =
        &mut unknown_event.body.records[0].payload
    else {
        panic!("source save must contain a native history layer");
    };
    history.events[0].kind = "future_transition".into();
    rebind_snapshot_body(&mut unknown_event);
    assert!(matches!(
        refuse(
            "unknown-event",
            &snapshot_bytes(&unknown_event),
            &project
        ),
        StoreError::InvalidGraphSnapshot(message) if message.contains("history event")
    ));

    let mut different_build = saved.document.clone();
    let different_fingerprint = ObjectId::from_stored("0".repeat(64)).expect("valid hash");
    different_build.body.summary.format_fingerprint = different_fingerprint.clone();
    different_build.manifest.summary.format_fingerprint = different_fingerprint;
    assert!(matches!(
        refuse(
            "different-build",
            &snapshot_bytes(&different_build),
            &project
        ),
        StoreError::GraphDifferentBuild
    ));

    let mut future_document =
        serde_json::to_value(&different_build).expect("future snapshot JSON value");
    future_document["body"]
        .as_object_mut()
        .expect("future snapshot body")
        .insert("future_member".into(), serde_json::json!(true));
    assert!(matches!(
        refuse(
            "different-build-with-future-member",
            &serde_json::to_vec(&future_document).expect("future snapshot bytes"),
            &project
        ),
        StoreError::GraphDifferentBuild
    ));

    let mut unknown_member = serde_json::to_value(&saved.document).expect("snapshot JSON value");
    unknown_member["body"]
        .as_object_mut()
        .expect("snapshot body object")
        .insert("unexpected".into(), serde_json::json!(true));
    assert!(matches!(
        refuse(
            "unknown-member",
            &serde_json::to_vec(&unknown_member).expect("unknown-member bytes"),
            &project
        ),
        StoreError::InvalidGraphSnapshot(message) if message.contains("unknown field")
    ));

    let mut unknown_actor_member =
        serde_json::to_value(&saved.document).expect("snapshot JSON value");
    unknown_actor_member["body"]["records"][0]["history"]["events"][0]["actor"]
        .as_object_mut()
        .expect("history actor object")
        .insert("unexpected".into(), serde_json::json!(true));
    let changed_body = CanonicalObject::freeze(&unknown_actor_member["body"])
        .expect("freeze body with nested unknown member")
        .key()
        .to_string();
    unknown_actor_member["manifest"]["body_sha256"] = serde_json::json!(changed_body);
    assert!(matches!(
        refuse(
            "unknown-actor-member",
            &serde_json::to_vec(&unknown_actor_member).expect("unknown actor bytes"),
            &project
        ),
        StoreError::InvalidGraphSnapshot(message) if message.contains("not preserved")
    ));

    let other_project = ProjectId("another-project".into());
    assert!(matches!(
        refuse(
            "project-mismatch",
            &snapshot_bytes(&saved.document),
            &other_project
        ),
        StoreError::GraphProjectMismatch { .. }
    ));

    let mut nonempty =
        SqliteStore::open(directory.path().join("nonempty.db")).expect("nonempty store");
    create_root(&mut nonempty, &project, "Existing root", "existing-root");
    assert!(matches!(
        nonempty.load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &snapshot_bytes(&saved.document),
            false,
            at(4),
            &DevelopmentNoopRedactor
        ),
        Err(StoreError::GraphDestinationNotEmpty)
    ));
    assert_eq!(
        nonempty
            .connection
            .query_row("SELECT COUNT(*) FROM work_items", [], |row| row
                .get::<_, i64>(0))
            .expect("count preserved work"),
        1
    );
    assert!(matches!(
        nonempty.inspect_work(root.work_id, at(5)),
        Err(StoreError::WorkNotFound(_))
    ));

    let mut memory_nonempty =
        SqliteStore::open(directory.path().join("memory-nonempty.db")).expect("memory store");
    memory_nonempty
        .capture_note(
            &NoteRequest {
                project_id: project.clone(),
                task_id: None,
                work_id: None,
                prose: "existing unkeyed project observation".into(),
                visibility: NoteVisibility::Shared,
                kind: Some(MemoryKind::Episode),
                authority: Some(Authority::Soft),
                sensitivity: Some(Sensitivity::Internal),
                title: Some("Existing observation".into()),
                tags: Vec::new(),
                evidence: Vec::new(),
                refs: Vec::new(),
                actor: actor("memory-session"),
                idempotency_key: "existing-observation".into(),
                created_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("capture existing memory");
    assert!(matches!(
        memory_nonempty.load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &snapshot_bytes(&saved.document),
            false,
            at(4),
            &DevelopmentNoopRedactor
        ),
        Err(StoreError::GraphDestinationNotEmpty)
    ));
}

/// The stored id of the carried record at `position` in a saved document.
fn carried_id(document: &WorkGraphSnapshotDocument, position: usize) -> crate::ObjectId {
    match &document.body.records[position].payload {
        WorkGraphSnapshotRecordPayload::Restored { object_id, .. } => object_id.clone(),
        WorkGraphSnapshotRecordPayload::Native { .. } => panic!("not a carried record"),
    }
}

/// The gate notes of `work` across native and carried layers, as
/// (name, evidence reference, refs).
fn gate_notes(
    document: &WorkGraphSnapshotDocument,
    work: crate::WorkId,
) -> Vec<(String, Option<String>, Vec<String>)> {
    document
        .body
        .records
        .iter()
        .filter(|record| record.work_id == work)
        .flat_map(|record| match &record.payload {
            WorkGraphSnapshotRecordPayload::Native { history } => history.notes.clone(),
            WorkGraphSnapshotRecordPayload::Restored { canonical_json, .. } => {
                serde_json::from_value::<crate::RestoredRecord>(canonical_json.clone())
                    .expect("carried record")
                    .history
                    .notes
            }
        })
        .filter_map(|note| {
            note.gate
                .map(|gate| (gate.name, gate.evidence_ref, note.refs))
        })
        .collect()
}

fn gated_source(
    directory: &std::path::Path,
    project: &ProjectId,
) -> (SqliteStore, crate::WorkItem) {
    let mut source = SqliteStore::open(directory.join("source.db")).expect("source");
    let root = create_root(&mut source, project, "Gated", "gated-root");
    let claim = source
        .claim_work(
            &ClaimWorkRequest {
                work_id: root.work_id,
                expected_work_revision: root.revision,
                expected_run_id: root.active_run_id,
                holder: crate::SessionId("gate-session".into()),
                ttl_seconds: 900,
                recovery_reason: None,
                actor: actor("gate-session"),
                idempotency_key: "claim-gated-root".into(),
                claimed_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("claim gated root");
    for (name, reference, second) in [
        ("cargo-test", Some("logs/test.txt"), 3),
        ("cargo-fmt", None, 4),
    ] {
        let current = source.get_work_item(root.work_id).expect("current root");
        source
            .record_gate_evidence(
                &crate::domain::RecordGateEvidenceRequest {
                    work_id: root.work_id,
                    run_id: claim.run_id,
                    expected_work_revision: current.revision,
                    holder: claim.holder.clone(),
                    claim_id: claim.claim_id,
                    claim_fence: claim.fence,
                    name: name.into(),
                    failed: Vec::new(),
                    evidence_ref: reference.map(str::to_owned),
                    actor: actor("gate-session"),
                    recorded_at: at(second),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("record gate");
    }
    (source, root)
}

fn save_graph(
    store: &mut SqliteStore,
    project: &ProjectId,
    second: i64,
) -> WorkGraphSnapshotDocument {
    store
        .save_work_graph_snapshot(
            project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(second),
            &DevelopmentNoopRedactor,
        )
        .expect("save graph")
        .document
}

fn load_graph(
    directory: &std::path::Path,
    name: &str,
    project: &ProjectId,
    document: &WorkGraphSnapshotDocument,
    second: i64,
) -> SqliteStore {
    let mut store = SqliteStore::open(directory.join(name)).expect("destination");
    store
        .load_work_graph_snapshot(
            project,
            &actor("load-session"),
            &snapshot_bytes(document),
            false,
            at(second),
            &DevelopmentNoopRedactor,
        )
        .expect("load graph");
    store
}

// A gate note keeps the evidence reference it was recorded with through a
// save, a load and a second save, and again when that save's carried layer
// is loaded and saved once more; with a reference and without one.
#[test]
fn a_gate_note_round_trips_its_evidence_reference() {
    let directory = crate::test_support::temp_home().expect("directory");
    let project = ProjectId("snapshot-gate-refs".into());
    let (mut source, root) = gated_source(directory.path(), &project);
    let first = save_graph(&mut source, &project, 5);
    let expected = vec![
        (
            "cargo-test".to_owned(),
            Some("logs/test.txt".to_owned()),
            vec!["logs/test.txt".to_owned()],
        ),
        ("cargo-fmt".to_owned(), None, Vec::new()),
    ];
    assert_eq!(gate_notes(&first, root.work_id), expected);

    let mut second_store = load_graph(directory.path(), "second.db", &project, &first, 6);
    let second = save_graph(&mut second_store, &project, 7);
    assert_eq!(gate_notes(&second, root.work_id), expected);
    assert!(
        second
            .body
            .records
            .iter()
            .any(|record| record.work_id == root.work_id
                && matches!(
                    record.payload,
                    WorkGraphSnapshotRecordPayload::Restored { .. }
                )),
        "the second save carries the first layer"
    );

    let mut third_store = load_graph(directory.path(), "third.db", &project, &second, 8);
    let third = save_graph(&mut third_store, &project, 9);
    assert_eq!(gate_notes(&third, root.work_id), expected);
}

// A gate note whose refs are not exactly what the store holds for its
// evidence reference is a corrupt file, in a native layer or a carried one,
// for both a dry run and a load, and nothing is written.
#[test]
fn a_gate_note_whose_refs_disagree_with_its_reference_is_refused() {
    let directory = crate::test_support::temp_home().expect("directory");
    let project = ProjectId("snapshot-gate-ref-refusal".into());
    let (mut source, root) = gated_source(directory.path(), &project);
    let first = save_graph(&mut source, &project, 5);
    let referenced = Some("logs/test.txt".to_owned());
    let cases: [(&str, Option<String>, Vec<&str>); 5] = [
        ("missing", referenced.clone(), vec![]),
        (
            "extra",
            referenced.clone(),
            vec!["logs/test.txt", "logs/zz-extra.txt"],
        ),
        (
            "extra-before",
            referenced.clone(),
            vec!["logs/other.txt", "logs/test.txt"],
        ),
        ("mismatched", referenced, vec!["logs/other.txt"]),
        ("unreferenced", None, vec!["logs/test.txt"]),
    ];
    for (label, reference, refs) in cases {
        let mut changed = first.clone();
        let record = changed
            .body
            .records
            .iter_mut()
            .find(|record| record.work_id == root.work_id)
            .expect("gated record");
        let WorkGraphSnapshotRecordPayload::Native { history } = &mut record.payload else {
            panic!("the source save holds a native layer");
        };
        let note = history
            .notes
            .iter_mut()
            .find(|note| {
                note.gate
                    .as_ref()
                    .is_some_and(|gate| gate.name == "cargo-test")
            })
            .expect("referenced gate note");
        note.gate.as_mut().expect("gate").evidence_ref = reference;
        note.refs = refs.into_iter().map(str::to_owned).collect();
        rebind_snapshot_body(&mut changed);
        println!("case {label}");
        assert_corrupt_without_writes(
            &project,
            &snapshot_bytes(&changed),
            "gate note refs disagree with its evidence reference",
        );
    }

    // The same rule holds for a carried layer, which keeps its record id.
    let mut second_store = load_graph(directory.path(), "second.db", &project, &first, 6);
    let mut carried = save_graph(&mut second_store, &project, 7);
    let record = carried
        .body
        .records
        .iter_mut()
        .find(|record| {
            record.work_id == root.work_id
                && matches!(
                    record.payload,
                    WorkGraphSnapshotRecordPayload::Restored { .. }
                )
        })
        .expect("carried layer");
    let WorkGraphSnapshotRecordPayload::Restored {
        object_id,
        canonical_json,
    } = &mut record.payload
    else {
        unreachable!("selected a carried layer")
    };
    let kept_id = object_id.clone();
    let mut restored: crate::RestoredRecord =
        serde_json::from_value(canonical_json.clone()).expect("carried record");
    restored
        .history
        .notes
        .iter_mut()
        .find(|note| note.gate.is_some())
        .expect("carried gate note")
        .refs
        .push("logs/zz-extra.txt".into());
    *canonical_json = serde_json::to_value(&restored).expect("carried JSON");
    assert_eq!(*object_id, kept_id, "the carried record keeps its id");
    rebind_snapshot_body(&mut carried);
    assert_corrupt_without_writes(
        &project,
        &snapshot_bytes(&carried),
        "gate note refs disagree with its evidence reference",
    );
}
