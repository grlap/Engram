use super::super::feeds::reserve_feed_position;
use super::super::test_support::*;
use super::super::*;
use super::prerequisites::classify_prerequisite_state;
use super::*;
use crate::storage::concurrent_commit::{ITEM_FEED_HEAD, read_across_a_concurrent_commit};

mod concurrency;

fn latest_event_id(store: &SqliteStore, work_id: WorkId) -> String {
    store
        .connection
        .query_row(
            "SELECT latest_event_id FROM work_items WHERE work_id = ?1",
            [work_id.0.to_string()],
            |row| row.get(0),
        )
        .expect("the item's latest event")
}

/// A read in autocommit makes several statements. Another connection's
/// commit landing between them must not look like corruption: the read sees
/// one commit state. Here a new event for the item commits just before the
/// reader reads the item's feed head, so the item's stored latest event and
/// the feed head would otherwise be read across that commit. The event is
/// read directly, as completion and child resolution read it, not through
/// the item read that already holds one snapshot around it.
#[test]
fn a_latest_event_read_in_autocommit_sees_one_commit_state_across_a_concurrent_commit() {
    let directory = crate::test_support::temp_home().expect("directory");
    let path = directory.path().join("engram.sqlite3");
    let mut writer = SqliteStore::open(&path).expect("writer store");
    let work = writer
        .create_work(
            &root_request("race-project", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let held = claim(&mut writer, &work, "holder", "claim", 1, 3_600);
    let work = writer.get_work_item(work.work_id).expect("claimed item");
    let event_before = latest_event_id(&writer, work.work_id);
    let reader = SqliteStore::open(&path).expect("reader store");

    let evidence_for = work.clone();
    let read = read_across_a_concurrent_commit(
        &reader,
        |reader| latest_canonical_work_event_for_item_optional(&reader.connection, work.work_id),
        ITEM_FEED_HEAD,
        move || {
            writer
                .record_work_evidence(
                    &RecordWorkEvidenceRequest {
                        work_id: evidence_for.work_id,
                        run_id: held.run_id,
                        expected_work_revision: evidence_for.revision,
                        holder: held.holder.clone(),
                        claim_id: held.claim_id,
                        claim_fence: held.fence,
                        summary: "committed between the reader's statements".into(),
                        refs: vec!["test:race".into()],
                        actor: actor("holder"),
                        idempotency_key: "concurrent-evidence".into(),
                        recorded_at: at(50),
                    },
                    &DevelopmentNoopRedactor,
                )
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
    );
    let event = read
        .expect("one commit state, no false corruption")
        .expect("the item has an event");
    assert_eq!(
        event.work, work,
        "the read returns the event that was latest when it started"
    );
    // A fresh read sees the concurrent event, and reads it consistently.
    assert_ne!(latest_event_id(&reader, work.work_id), event_before);
    latest_canonical_work_event_for_item_optional(&reader.connection, work.work_id)
        .expect("read after the commit")
        .expect("the item has an event");
    let after = reader
        .get_work_item(work.work_id)
        .expect("read after the commit");
    assert_eq!(after.work_id, work.work_id);
}

/// The same read across a commit that revises the item itself, landing
/// after the reader has read the item's row but before it reads the item's
/// latest event. The row and the event must still come from one commit, or
/// the old row would be compared with the new revision's event.
#[test]
fn a_work_item_read_in_autocommit_compares_its_row_and_event_from_one_commit() {
    let directory = crate::test_support::temp_home().expect("directory");
    let path = directory.path().join("engram.sqlite3");
    let mut writer = SqliteStore::open(&path).expect("writer store");
    let work = writer
        .create_work(
            &root_request("race-revision-project", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let reader = SqliteStore::open(&path).expect("reader store");

    let revised_from = work.clone();
    let read = read_across_a_concurrent_commit(
        &reader,
        |reader| reader.get_work_item(work.work_id),
        &["SELECT latest_event_id FROM work_items WHERE work_id = ?1"],
        move || {
            writer
                .revise_work(
                    &ReviseWorkRequest {
                        work_id: revised_from.work_id,
                        expected_revision: revised_from.revision,
                        patch: WorkRevisionPatch {
                            title: Some("revised between the reader's statements".into()),
                            ..WorkRevisionPatch::default()
                        },
                        authority: delegated(&revised_from.project_id.0, "planner"),
                        actor: actor("planner"),
                        idempotency_key: "concurrent-revision".into(),
                        updated_at: at(2),
                    },
                    &DevelopmentNoopRedactor,
                )
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
    );
    assert_eq!(
        read.expect("one commit state, no false corruption"),
        work,
        "the read reports the item as it was when it started"
    );
    // A fresh read sees the revision.
    let after = reader
        .get_work_item(work.work_id)
        .expect("read after the commit");
    assert_eq!(after.revision, work.revision + 1);
    assert_eq!(after.title, "revised between the reader's statements");
}

/// The claim-epoch lookup bind's history check uses reads the item's event
/// index newest first without a sort or full scan, and stops at the newest
/// event, so its steps and decodes stay the same as the claim's history
/// grows.
#[test]
fn claim_epoch_lookup_stops_at_the_newest_event_as_history_grows() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let work = store
        .create_work(
            &root_request("epoch-project", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let held = claim(&mut store, &work, "holder", "claim", 1, 3_600);
    let work = store.get_work_item(work.work_id).expect("claimed item");
    let lookup = |store: &SqliteStore| {
        reset_work_event_decode_count();
        let found = any_canonical_claim_epoch_event(
            &store.connection,
            work.work_id,
            held.accepted_work_revision,
            held.claim_id,
            held.fence,
            |event| {
                event
                    .claim
                    .as_ref()
                    .is_some_and(|recorded| recorded.claim_id == held.claim_id)
            },
        )
        .expect("claim-epoch lookup");
        let decodes = work_event_decode_count();
        let mut statement = store
            .connection
            .prepare(CLAIM_EPOCH_EVENTS_SQL)
            .expect("prepare the lookup");
        let mut rows = statement
            .query(rusqlite::params![
                work.work_id.0.to_string(),
                held.accepted_work_revision,
                held.claim_id.0.to_string(),
                held.fence
            ])
            .expect("run the lookup");
        rows.next()
            .expect("step the lookup")
            .expect("the newest event records the claim");
        drop(rows);
        let status = |kind| statement.get_status(kind);
        (
            found,
            decodes,
            status(rusqlite::StatementStatus::VmStep),
            status(rusqlite::StatementStatus::Sort),
            status(rusqlite::StatementStatus::FullscanStep),
        )
    };
    let (found, decodes, steps, sorts, full_scan_steps) = lookup(&store);
    assert!(found);
    assert_eq!((decodes, sorts, full_scan_steps), (1, 0, 0));

    for index in 0..64 {
        evidence(
            &mut store,
            &work,
            &held,
            "holder",
            &format!("evidence-{index}"),
            2 + index,
        );
    }
    let grown = lookup(&store);
    assert_eq!(
        grown,
        (true, 1, steps, 0, 0),
        "{grown:?} after {steps} steps"
    );
}

#[test]
fn resolver_sql_bounds_collisions_and_recovers_an_omitted_target_by_full_id() {
    let mut store = SqliteStore::open_in_memory().expect("collision fixture");
    let mut items = (0..9)
        .map(|index| {
            store
                .create_work(
                    &root_request("ambiguous-project", &format!("candidate-{index}"), index),
                    &DevelopmentNoopRedactor,
                )
                .expect("candidate")
        })
        .collect::<Vec<_>>();

    // Production enforces uniqueness. This deliberately corrupt-shaped
    // fixture replaces the constrained table with an unconstrained copy so
    // the real resolver SQL and canonical projection loader exercise the
    // defensive ambiguity path.
    store
        .connection
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE work_items_collision AS SELECT * FROM work_items;
             DROP TABLE work_items;
             ALTER TABLE work_items_collision RENAME TO work_items;",
        )
        .expect("remove short-ref uniqueness for collision fixture");
    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("collision transaction");
    for (index, item) in items.iter_mut().enumerate() {
        let mut event = latest_canonical_work_event_for_item(&transaction, item.work_id)
            .expect("latest candidate event");
        item.short_ref = "w-collision".into();
        item.title = format!("Collision candidate {index}");
        item.revision += 1;
        item.updated_at = at(10 + i64::try_from(index).expect("small index"));
        transaction
            .execute(
                "UPDATE work_items SET short_ref = ?2 WHERE work_id = ?1",
                params![item.work_id.0.to_string(), item.short_ref],
            )
            .expect("persist colliding short ref");
        persist_work_item(&transaction, item).expect("persist colliding candidate");
        event.revision = item.revision;
        event.work.clone_from(item);
        event.created_at = item.updated_at;
        let root = event.root_execution.as_ref().map(|address| {
            super::super::root_state::resolve(&transaction, address).expect("fixture root state")
        });
        append_work_event(&transaction, &WorkEventDraft::with_root_state(&event, root))
            .expect("append colliding candidate event");
    }
    transaction.commit().expect("commit collision fixture");
    store
        .connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .expect("restore foreign-key enforcement after collision fixture");

    let error = store
        .resolve_work_ref(&ProjectId("ambiguous-project".into()), "w-collision")
        .expect_err("nine matching rows must be ambiguous");
    let StoreError::WorkReferenceAmbiguous {
        reference,
        candidates,
        more,
    } = error
    else {
        panic!("expected typed ambiguous-reference error, got {error:?}");
    };
    assert_eq!(reference, "w-collision");
    assert_eq!(candidates.len(), MAX_AMBIGUOUS_WORK_CANDIDATES);
    assert_eq!(more, 1);
    let mut expected = items.iter().map(|item| item.work_id).collect::<Vec<_>>();
    expected.sort_by_key(|work_id| work_id.0);
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.work_id)
            .collect::<Vec<_>>(),
        expected[..MAX_AMBIGUOUS_WORK_CANDIDATES]
    );
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.title.as_str())
            .collect::<Vec<_>>(),
        expected[..MAX_AMBIGUOUS_WORK_CANDIDATES]
            .iter()
            .map(|work_id| {
                items
                    .iter()
                    .find(|item| item.work_id == *work_id)
                    .expect("expected item")
                    .title
                    .as_str()
            })
            .collect::<Vec<_>>()
    );
    let omitted_id = expected[MAX_AMBIGUOUS_WORK_CANDIDATES];
    let omitted = items
        .iter()
        .find(|item| item.work_id == omitted_id)
        .expect("omitted target");
    let omitted_candidate = WorkReferenceCandidate {
        work_id: omitted.work_id,
        short_ref: omitted.short_ref.clone(),
        title: omitted.title.clone(),
        lifecycle: omitted.lifecycle,
    };
    assert_eq!(
        command_work_ref_on(
            &store.connection,
            &ProjectId("ambiguous-project".into()),
            &omitted_candidate,
        )
        .expect("known omitted target uses its full id"),
        omitted_id.0.to_string()
    );
}

#[test]
fn prerequisite_state_uses_one_hop_satisfaction_and_dead_edge_rules() {
    let prerequisite_id = WorkId::new();
    assert_eq!(
        classify_prerequisite_state(WorkLifecycle::Completed, None, prerequisite_id)
            .expect("completed state"),
        WorkPrerequisiteState::Satisfied
    );
    assert_eq!(
        classify_prerequisite_state(WorkLifecycle::Open, None, prerequisite_id)
            .expect("open state"),
        WorkPrerequisiteState::Pending
    );
    assert_eq!(
        classify_prerequisite_state(WorkLifecycle::Cancelled, None, prerequisite_id)
            .expect("cancelled state"),
        WorkPrerequisiteState::Dead
    );
    assert_eq!(
        classify_prerequisite_state(
            WorkLifecycle::Superseded,
            Some(WorkLifecycle::Completed),
            prerequisite_id,
        )
        .expect("completed replacement"),
        WorkPrerequisiteState::Satisfied
    );
    assert_eq!(
        classify_prerequisite_state(
            WorkLifecycle::Superseded,
            Some(WorkLifecycle::Open),
            prerequisite_id,
        )
        .expect("live replacement"),
        WorkPrerequisiteState::Pending
    );
    assert_eq!(
        classify_prerequisite_state(
            WorkLifecycle::Superseded,
            Some(WorkLifecycle::Cancelled),
            prerequisite_id,
        )
        .expect("cancelled replacement"),
        WorkPrerequisiteState::Dead
    );
}

#[test]
fn prerequisite_page_bounds_real_edges_and_counts_each_omitted_class() {
    const PAGE_LIMIT: usize = 8;

    let project = "bounded-prerequisite-page";
    let mut store = SqliteStore::open_in_memory().expect("prerequisite page fixture");
    let mut dependent = store
        .create_work(
            &root_request(project, "dependent", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("dependent work");
    let prerequisites = (0..14)
        .map(|index| {
            store
                .create_work(
                    &root_request(project, &format!("prerequisite-{index}"), index + 1),
                    &DevelopmentNoopRedactor,
                )
                .expect("prerequisite work")
        })
        .collect::<Vec<_>>();

    for (index, prerequisite) in prerequisites.iter().enumerate() {
        dependent = store
            .add_work_prerequisite(
                &ChangeWorkPrerequisiteRequest {
                    work_id: dependent.work_id,
                    prerequisite_id: prerequisite.work_id,
                    expected_revision: dependent.revision,
                    authority: delegated(project, "planner"),
                    actor: actor("planner"),
                    idempotency_key: format!("add-prerequisite-{index}"),
                    changed_at: at(100 + i64::try_from(index).expect("small index")),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("add prerequisite edge");
    }

    for (index, prerequisite) in prerequisites[..11].iter().enumerate() {
        store
            .dispose_work(
                &DisposeWorkRequest {
                    work_id: prerequisite.work_id,
                    expected_work_revision: prerequisite.revision,
                    disposition: WorkDisposition::Cancelled,
                    replacement_id: None,
                    reason: "exercise dead prerequisite paging".into(),
                    actor: actor("planner"),
                    idempotency_key: format!("cancel-prerequisite-{index}"),
                    disposed_at: at(200 + i64::try_from(index).expect("small index")),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("cancel prerequisite");
    }

    let completed = prerequisites.last().expect("completed prerequisite");
    let completed_claim = claim(
        &mut store,
        completed,
        "planner",
        "completed-prerequisite-claim",
        300,
        300,
    );
    let completed_evidence = evidence(
        &mut store,
        completed,
        &completed_claim,
        "planner",
        "completed-prerequisite-evidence",
        301,
    );
    checkpoint(
        &mut store,
        completed,
        &completed_claim,
        "planner",
        "completed-prerequisite-checkpoint",
        302,
        std::slice::from_ref(&completed_evidence),
    );
    complete(
        &mut store,
        completed,
        &completed_claim,
        "planner",
        &completed_evidence,
        "completed-prerequisite-complete",
        303,
    )
    .expect("complete prerequisite");

    let page = store
        .work_prerequisites_with_state(dependent.work_id, PAGE_LIMIT)
        .expect("bounded prerequisite page");
    assert_eq!(page.items.len(), PAGE_LIMIT);
    assert!(
        page.items
            .iter()
            .all(|(_, state)| *state == WorkPrerequisiteState::Dead)
    );
    assert_eq!(page.omitted_by_state, [3, 2, 1]);

    let mixed_page = store
        .work_prerequisites_with_state(dependent.work_id, 13)
        .expect("mixed-state prerequisite page");
    let mixed_states = mixed_page
        .items
        .iter()
        .map(|(_, state)| *state)
        .collect::<Vec<_>>();
    assert_eq!(
        mixed_states,
        [
            vec![WorkPrerequisiteState::Dead; 11],
            vec![WorkPrerequisiteState::Pending; 2],
        ]
        .concat()
    );
    assert_eq!(mixed_page.omitted_by_state, [0, 0, 1]);

    let mut expected_refs = prerequisites[..13]
        .iter()
        .map(|prerequisite| prerequisite.short_ref.clone())
        .collect::<Vec<_>>();
    expected_refs[..11].sort();
    expected_refs[11..].sort();
    assert_eq!(
        mixed_page
            .items
            .iter()
            .map(|(prerequisite, _)| prerequisite.short_ref.clone())
            .collect::<Vec<_>>(),
        expected_refs
    );
}

#[test]
fn work_feed_arithmetic_refuses_overflow() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let feed = FeedId::Project(ProjectId("project-feed-overflow".into()));

    let interval = store.work_feed_between(&feed, i64::MIN, i64::MAX);
    assert!(matches!(
        interval,
        Err(StoreError::InvalidWorkProjection(reason)) if reason.contains("overflowed")
    ));

    let (feed_kind, feed_id) = feed_parts(&feed);
    store
        .connection
        .execute(
            "INSERT INTO work_feed_heads (feed_kind, feed_id, position)
             VALUES (?1, ?2, ?3)",
            params![feed_kind, feed_id, i64::MAX],
        )
        .expect("install saturated feed head");
    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("overflow transaction");
    let allocation = reserve_feed_position(&transaction, &feed);
    assert!(matches!(
        allocation,
        Err(StoreError::InvalidWorkProjection(reason)) if reason.contains("position overflowed")
    ));
    transaction.rollback().expect("discard saturated feed head");

    assert!(matches!(
        checkpoint_feed_end(i64::MAX),
        Err(StoreError::InvalidWorkProjection(reason))
            if reason.contains("checkpoint run-feed position arithmetic overflowed")
    ));
}

#[test]
fn focused_work_memory_is_shared_once_while_private_scratch_stays_actor_local() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-memory", "root-memory", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("create root");
    let owner_session = SessionId("planner".into());
    let work_claim = claim(&mut store, &root, "planner", "work-memory-claim", 1, 300);
    store
        .focus_work_session(&root.project_id, &owner_session, root.work_id, at(1))
        .expect("focus work before capture");
    let shared = store
        .capture_note(
            &NoteRequest {
                project_id: root.project_id.clone(),
                task_id: None,
                work_id: Some(root.work_id),
                prose: "Constraint: never bypass the focused work safety contract".into(),
                visibility: NoteVisibility::Shared,
                kind: None,
                authority: None,
                sensitivity: Some(Sensitivity::Internal),
                title: None,
                tags: Vec::new(),
                evidence: Vec::new(),
                refs: Vec::new(),
                actor: actor("planner"),
                idempotency_key: "shared-work-memory".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("capture shared work memory");
    assert_eq!(
        shared.scope,
        Scope::Work {
            project: root.project_id.clone(),
            work: root.work_id,
        }
    );
    assert_eq!(shared.cursor, None);
    assert_eq!(shared.work_positions.len(), 9);

    let private = store
        .capture_note(
            &NoteRequest {
                project_id: root.project_id.clone(),
                task_id: None,
                work_id: Some(root.work_id),
                prose: "scratch: private focused hypothesis".into(),
                visibility: NoteVisibility::Private,
                kind: None,
                authority: None,
                sensitivity: Some(Sensitivity::Internal),
                title: None,
                tags: Vec::new(),
                evidence: Vec::new(),
                refs: Vec::new(),
                actor: actor("planner"),
                idempotency_key: "private-work-memory".into(),
                created_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("capture private work memory");
    assert!(matches!(private.scope, Scope::Agent { work: Some(work), .. } if work == root.work_id));
    assert!(private.work_positions.is_empty());

    store
        .show_memory(
            &private.version,
            &root.project_id,
            Some(crate::TaskId::new()),
            Some(root.work_id),
            &owner_session,
            "planner",
        )
        .expect("unrelated task binding does not hide owned work scratch");

    let restricted = store
        .capture_note(
            &NoteRequest {
                project_id: root.project_id.clone(),
                task_id: None,
                work_id: Some(root.work_id),
                prose: "restricted: must never appear in focus or search".into(),
                visibility: NoteVisibility::Shared,
                kind: None,
                authority: None,
                sensitivity: Some(Sensitivity::Restricted),
                title: None,
                tags: Vec::new(),
                evidence: Vec::new(),
                refs: Vec::new(),
                actor: actor("planner"),
                idempotency_key: "restricted-work-memory".into(),
                created_at: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("capture restricted work memory");

    let peer_session = SessionId("peer".into());
    store
        .focus_work_session(&root.project_id, &peer_session, root.work_id, at(3))
        .expect("focus peer on root work");
    let owner = store
        .search_work_memories(
            &root.project_id,
            root.work_id,
            &owner_session,
            "planner",
            None,
            Some(20),
        )
        .expect("owner work memories");
    assert_eq!(owner.len(), 2);
    let peer = store
        .search_work_memories(
            &root.project_id,
            root.work_id,
            &peer_session,
            "peer",
            None,
            Some(20),
        )
        .expect("peer work memories");
    assert_eq!(peer.len(), 1);
    assert_eq!(peer[0].version, shared.version);
    store
        .show_memory(
            &shared.version,
            &root.project_id,
            None,
            Some(root.work_id),
            &SessionId("peer".into()),
            "peer",
        )
        .expect("peer can inspect shared work memory");
    assert!(matches!(
        store.show_memory(
            &private.version,
            &root.project_id,
            None,
            Some(root.work_id),
            &SessionId("peer".into()),
            "peer",
        ),
        Err(StoreError::MemoryAccessDenied(_))
    ));
    assert!(matches!(
        store.show_memory(
            &restricted.version,
            &root.project_id,
            None,
            Some(root.work_id),
            &SessionId("planner".into()),
            "planner",
        ),
        Err(StoreError::MemoryAccessDenied(_))
    ));

    let project_feed = store
        .work_feed_after(&FeedId::Project(root.project_id.clone()), 0, 100)
        .expect("project feed");
    assert!(
        project_feed
            .iter()
            .any(|entry| entry.object_id == shared.version)
    );
    assert!(
        project_feed
            .iter()
            .all(|entry| entry.object_id != private.version)
    );

    let decomposition = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![
                    child("memory-child", ChildRequirement::Optional, "Memory child"),
                    child(
                        "memory-sibling",
                        ChildRequirement::Optional,
                        "Memory sibling",
                    ),
                ],
                prerequisites: Vec::new(),
                authority: WorkPlanningAuthority::Claim {
                    run_id: work_claim.run_id,
                    holder: work_claim.holder.clone(),
                    claim_id: work_claim.claim_id,
                    claim_fence: work_claim.fence,
                },
                actor: actor("planner"),
                idempotency_key: "memory-child-decompose".into(),
                created_at: at(4),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("create child focus");
    let child_work = &decomposition.children[0];
    store
        .focus_work_session(&root.project_id, &peer_session, child_work.work_id, at(5))
        .expect("move peer focus to child work");
    claim(
        &mut store,
        child_work,
        "peer",
        "child-work-memory-claim",
        5,
        300,
    );
    let child_view = store
        .search_work_memories(
            &root.project_id,
            child_work.work_id,
            &peer_session,
            "peer",
            None,
            Some(20),
        )
        .expect("root-shared memory is applicable from child focus");
    assert!(
        child_view
            .iter()
            .any(|memory| memory.version == shared.version)
    );
    store
        .show_memory(
            &shared.version,
            &root.project_id,
            None,
            Some(child_work.work_id),
            &SessionId("peer".into()),
            "peer",
        )
        .expect("child focus can inspect root-shared memory");
    assert!(matches!(
        store.show_memory(
            &private.version,
            &root.project_id,
            None,
            Some(child_work.work_id),
            &owner_session,
            "planner",
        ),
        Err(StoreError::MemoryAccessDenied(_))
    ));

    assert!(matches!(
        store.search_work_memories(
            &root.project_id,
            root.work_id,
            &peer_session,
            "peer",
            None,
            Some(20),
        ),
        Err(StoreError::InvalidWork(_))
    ));
}
