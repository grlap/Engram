//! A snapshot carries what a store holds, byte for byte: text the write path
//! admits is never refused by save or load, whatever controls it carries.
//! Terminal safety belongs to rendering, which escapes it on every read.

use super::*;
use crate::domain::{ReviseWorkRequest, WorkRevisionPatch};

/// Text an agent can store: a terminal escape with its SGR sequence, a bidi
/// override, a variation selector, a carriage return and a decomposed accent.
fn hostile(label: &str) -> String {
    format!("{label} \u{1b}[31mred\u{1b}[0m \u{202e}rev\u{fe0f} cr\rlf e\u{301}")
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one round trip writes hostile text through every admitted path and reads it back"
)]
fn hostile_text_the_store_admits_round_trips_byte_exact_and_renders_escaped() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let project = ProjectId("snapshot-hostile-text".into());
    let mut source = SqliteStore::open(directory.path().join("source.db")).expect("source");

    // The ordinary write paths admit every one of these.
    let mut request = root_create_request(&project, "hostile-root");
    request.title = hostile("title");
    request.outcome = hostile("outcome");
    request.acceptance = vec![hostile("criterion")];
    request.labels = vec![hostile("label")];
    request.notes = vec![hostile("initial note")];
    let item = source
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("add admits hostile text");
    let revised = source
        .revise_work(
            &ReviseWorkRequest {
                work_id: item.work_id,
                expected_revision: item.revision,
                patch: WorkRevisionPatch {
                    title: Some(hostile("revised title")),
                    ..WorkRevisionPatch::default()
                },
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner-session"),
                idempotency_key: "hostile-revise".into(),
                updated_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("update admits hostile text");
    source
        .add_work_blocker(
            &AddWorkBlockerRequest {
                work_id: item.work_id,
                expected_work_revision: revised.revision,
                kind: WorkBlockerKind::Manual,
                detail: hostile("blocker"),
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner-session"),
                idempotency_key: "hostile-blocker".into(),
                blocked_at: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("blocking admits hostile text");
    let blocked = source.get_work_item(item.work_id).expect("blocked item");
    source
        .dispose_work(
            &DisposeWorkRequest {
                work_id: item.work_id,
                expected_work_revision: blocked.revision,
                disposition: WorkDisposition::Cancelled,
                replacement_id: None,
                reason: hostile("cancel reason"),
                actor: actor("planner-session"),
                idempotency_key: "hostile-cancel".into(),
                disposed_at: at(4),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("cancelling admits hostile text");
    // A memory body keeps the whitespace its author wrote.
    let memory_body = "  a memory body keeps its spacing \n";
    insert_classified_project_memory(
        &mut source,
        &project,
        "spaced-memory",
        memory_body,
        Sensitivity::Internal,
        at(5),
    );

    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(6),
            &DevelopmentNoopRedactor,
        )
        .expect("save carries what the store holds");
    let body = &saved.document.body;
    let saved_item = &body.items[0];
    assert_eq!(saved_item.title, hostile("revised title"));
    assert_eq!(saved_item.outcome, hostile("outcome"));
    assert_eq!(saved_item.acceptance, vec![hostile("criterion")]);
    assert_eq!(saved_item.labels, vec![hostile("label")]);
    assert_eq!(
        saved_item.disposal_reason.as_deref(),
        Some(hostile("cancel reason").as_str())
    );
    let details: Vec<&str> = body
        .blockers
        .iter()
        .map(|blocker| blocker.detail.as_str())
        .collect();
    assert_eq!(details, vec![hostile("blocker").as_str()]);
    let WorkGraphSnapshotMemoryState::Active { body: memory, .. } = &body.memories[0].state else {
        panic!("the memory fixture is active");
    };
    assert_eq!(
        memory,
        &WorkGraphSnapshotText::Present {
            value: memory_body.into()
        }
    );
    // The container an operator receives is inert in a terminal: every
    // unsafe character is a JSON escape, and only layout newlines are raw.
    let bytes = saved.document.container_bytes().expect("container bytes");
    let container = std::str::from_utf8(&bytes).expect("UTF-8 container");
    let raw: Vec<char> = container
        .chars()
        .filter(|&ch| ch != '\n' && crate::domain::is_unsafe_rendered_text_char(ch))
        .collect();
    assert!(
        raw.is_empty(),
        "raw unsafe characters in the container: {raw:?}"
    );
    for escape in ["\\u001b", "\\u202e", "\\ufe0f", "\\r"] {
        assert!(container.contains(escape), "{escape} is missing");
    }

    // A dry run and a load both accept it, and the restored store holds the
    // same bytes.
    let destination_path = directory.path().join("destination.db");
    let mut destination = SqliteStore::open(&destination_path).expect("destination");
    let preview = destination
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &bytes,
            true,
            at(7),
            &DevelopmentNoopRedactor,
        )
        .expect("dry run accepts it");
    assert!(!preview.loaded);
    destination
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &bytes,
            false,
            at(8),
            &DevelopmentNoopRedactor,
        )
        .expect("load accepts it");
    let restored = destination
        .get_work_item(item.work_id)
        .expect("restored item");
    assert_eq!(restored.title, hostile("revised title"));
    assert_eq!(restored.outcome, hostile("outcome"));
    assert_eq!(restored.acceptance, vec![hostile("criterion")]);
    assert!(destination.verify_all().expect("verify").is_healthy());

    // Saving the restored store reproduces the same items, history and
    // memories, and that save loads again.
    let resaved = destination
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(9),
            &DevelopmentNoopRedactor,
        )
        .expect("re-save carries the restored text");
    assert_eq!(resaved.document.body.items, saved.document.body.items);
    assert_eq!(resaved.document.body.memories, saved.document.body.memories);
    let mut again = SqliteStore::open(directory.path().join("again.db")).expect("again");
    again
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &snapshot_bytes(&resaved.document),
            false,
            at(10),
            &DevelopmentNoopRedactor,
        )
        .expect("the re-saved snapshot loads");
    assert_eq!(
        again.get_work_item(item.work_id).expect("item").title,
        hostile("revised title")
    );
    drop((source, destination, again));

    // Reading the restored item renders every control escaped, never raw.
    let reader = crate::verbs::AgentVerbs::new(
        destination_path,
        project,
        "reader".into(),
        crate::SessionId("reader".into()),
        None,
    );
    let shown = reader
        .show(&item.short_ref, at(11))
        .expect("show the restored item");
    let text = shown.text();
    for raw in ['\u{1b}', '\u{202e}', '\u{fe0f}', '\r'] {
        assert!(!text.contains(raw), "raw {raw:?} rendered: {text}");
    }
    assert!(text.contains("\\u{1b}"), "{text}");
}

// A historical actor is carried as the store holds it: the work write path
// binds only a non-blank actor and session, so the stricter shape required of
// the operator who saves or loads never applies to recorded history.
#[test]
fn a_historical_actor_the_write_path_admits_round_trips_unchanged() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let project = ProjectId("snapshot-historical-actor".into());
    let mut source = SqliteStore::open(directory.path().join("source.db")).expect("source");
    let mut author = actor("historical-session");
    author.actor_id = " agent \u{1b}[31mred".into();
    author.actor_kind = String::new();
    author.reason = "line one\nline two\r".into();
    author.source_tool = Some("tool\u{202e}".into());
    let mut request = root_create_request(&project, "historical-actor");
    request.actor = author.clone();
    source
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("the write path admits the actor");

    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(2),
            &DevelopmentNoopRedactor,
        )
        .expect("save carries the historical actor");
    let carried = saved.document.body.records.iter().any(|record| {
        matches!(
            &record.payload,
            WorkGraphSnapshotRecordPayload::Native { history }
                if history.events.iter().any(|event| event.actor == author)
        )
    });
    assert!(carried, "the saved history keeps the actor byte exact");

    let mut destination =
        SqliteStore::open(directory.path().join("destination.db")).expect("destination");
    destination
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &snapshot_bytes(&saved.document),
            false,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("load accepts the historical actor");
    let resaved = destination
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect("re-save carries the restored actor");
    let records = serde_json::to_string(&resaved.document.body.records).expect("records json");
    for value in [&author.actor_id, &author.reason] {
        let encoded = serde_json::to_string(value).expect("value json");
        assert!(
            records.contains(&encoded[1..encoded.len() - 1]),
            "restored history keeps {value:?}"
        );
    }
}

// A project-memory actor is held to the attribution remember and forget admit,
// so a dry run refuses exactly what the real load would, before any write.
#[test]
fn a_memory_actor_the_store_could_not_hold_is_refused_by_dry_run_and_load() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let project = ProjectId("snapshot-memory-actor".into());
    let mut source = SqliteStore::open(directory.path().join("source.db")).expect("source");
    insert_classified_project_memory(
        &mut source,
        &project,
        "kept",
        "kept body",
        Sensitivity::Internal,
        at(1),
    );
    insert_classified_project_memory(
        &mut source,
        &project,
        "retired",
        "retired body",
        Sensitivity::Internal,
        at(2),
    );
    source
        .forget_project_memory(
            &crate::ForgetProjectMemoryRequest {
                project_id: project.clone(),
                session_id: crate::SessionId("retirer".into()),
                key: "retired".into(),
                actor: actor("retirer"),
                created_at: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("forget");
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect("save")
        .document;
    let mut destination =
        SqliteStore::open(directory.path().join("destination.db")).expect("destination");
    let before = crate::storage::test_database_shape_snapshot(&destination.connection);
    // A blank actor kind and a blank reason both pass the work-history actor
    // rule; neither is an attribution a project-memory record can hold.
    for (key, field) in [("kept", "actor kind"), ("retired", "reason")] {
        let mut document = saved.clone();
        let memory = document
            .body
            .memories
            .iter_mut()
            .find(|memory| memory.key == key)
            .expect("fixture memory");
        match &mut memory.state {
            WorkGraphSnapshotMemoryState::Active { actor, .. } => actor.actor_kind = String::new(),
            WorkGraphSnapshotMemoryState::Tombstone { actor, .. } => actor.reason = " ".into(),
        }
        rebind_snapshot_body(&mut document);
        for dry_run in [true, false] {
            let refused = destination.load_work_graph_snapshot(
                &project,
                &actor("load-session"),
                &snapshot_bytes(&document),
                dry_run,
                at(5),
                &DevelopmentNoopRedactor,
            );
            assert!(
                matches!(&refused, Err(StoreError::InvalidGraphSnapshot(message)) if message.contains(field)),
                "{key} (dry run {dry_run}): {refused:?}"
            );
            assert_eq!(
                crate::storage::test_database_shape_snapshot(&destination.connection),
                before
            );
        }
    }
}

// Save checks its document with the loader before the disclosure audit: a
// stored record the loader would refuse is never disclosed or audited. A note
// summary corrupted below the write path still reads back, so it reaches that
// check rather than an earlier read-side refusal.
#[test]
fn save_refuses_before_its_audit_a_document_the_loader_would_reject() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("snapshot-self-check".into());
    let item = create_root(&mut store, &project, "Noted root", "noted-root");
    let claim = store
        .claim_work(
            &ClaimWorkRequest {
                work_id: item.work_id,
                expected_work_revision: item.revision,
                expected_run_id: item.active_run_id,
                holder: crate::SessionId("note-session".into()),
                ttl_seconds: 900,
                recovery_reason: None,
                actor: actor("note-session"),
                idempotency_key: "self-check-claim".into(),
                claimed_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("claim");
    store
        .record_work_evidence(
            &RecordWorkEvidenceRequest {
                work_id: item.work_id,
                run_id: claim.run_id,
                expected_work_revision: claim.accepted_work_revision,
                holder: claim.holder.clone(),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                summary: "a stored summary".into(),
                refs: Vec::new(),
                actor: actor("note-session"),
                idempotency_key: "self-check-note".into(),
                recorded_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("note");
    let corrupted = store
        .connection
        .execute(
            "UPDATE objects SET canonical_json = CASE typeof(canonical_json)
                 WHEN 'blob' THEN CAST(replace(CAST(canonical_json AS TEXT), ?1, ?2) AS BLOB)
                 ELSE replace(canonical_json, ?1, ?2) END
             WHERE instr(CAST(canonical_json AS TEXT), ?1) > 0",
            ["\"a stored summary\"", "\" a stored summary\""],
        )
        .expect("corrupt the stored note summary");
    assert_eq!(corrupted, 1);

    let refused = store.save_work_graph_snapshot(
        &project,
        &actor("save-session"),
        None,
        WorkGraphSnapshotDestinationKind::Stdout,
        at(3),
        &DevelopmentNoopRedactor,
    );
    assert!(
        matches!(&refused, Err(StoreError::InvalidGraphSnapshot(message)) if message.contains("history note")),
        "{:?}",
        refused.map(|_| ())
    );
    let (count, audits) = store
        .recent_work_graph_snapshot_save_audits(&project, 8)
        .expect("read disclosure audit");
    assert_eq!(count, 0, "nothing is disclosed, so nothing is audited");
    assert!(audits.is_empty(), "{audits:?}");
}

// A store refuses nothing ordinary writes admit, and the snapshot's text rule
// is no stricter than the writer's: text an author can store, the snapshot
// can carry.
#[test]
fn the_snapshot_accepts_every_title_the_write_path_admits() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("snapshot-admission-agreement".into());
    let titles = [
        hostile("title"),
        "Unsafe\u{202e}title".to_owned(),
        "tab\tand\nnewline".to_owned(),
        "zero\u{200b}width".to_owned(),
    ];
    for (index, title) in titles.iter().enumerate() {
        create_root(&mut store, &project, title, &format!("admitted-{index}"));
    }
    let saved = store
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("every admitted title is exported");
    let mut exported: Vec<String> = saved
        .document
        .body
        .items
        .iter()
        .map(|item| item.title.clone())
        .collect();
    exported.sort();
    let mut admitted = titles.to_vec();
    admitted.sort();
    assert_eq!(exported, admitted);
    let (count, _) = store
        .recent_work_graph_snapshot_save_audits(&project, 8)
        .expect("read disclosure audit");
    assert_eq!(count, 1, "the save is audited once it is disclosed");
}
