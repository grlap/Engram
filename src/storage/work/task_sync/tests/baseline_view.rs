//! The per-task baseline view: rebuilt from the records alone, updated for
//! exactly the changed tasks, and chained to one predecessor.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use super::feed_changes::{PROJECT, document, exchange, profile, project, tree};
use super::*;
use crate::storage::work::task_sync::baseline;

type Rows = (
    Vec<(
        String,
        String,
        String,
        i64,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
    )>,
    Vec<(String, String, i64, String, String, i64)>,
);

fn view(store: &SqliteStore) -> Rows {
    let rows = store
        .connection
        .prepare(
            "SELECT project_id, work_id, record_id, position, document, leaf
             FROM work_task_sync_baseline ORDER BY project_id, work_id",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let heads = store
        .connection
        .prepare(
            "SELECT project_id, record_id, position, inventory_root, export_basis, include_notes
             FROM work_task_sync_baseline_heads ORDER BY project_id",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    (rows, heads)
}

fn head(store: &SqliteStore) -> baseline::BaselineHead {
    baseline::head(&store.connection, &project())
        .unwrap()
        .unwrap()
}

/// Enrols `count` tasks, then edits two, adds one and removes one.
fn exchanged(count: usize) -> (SqliteStore, TaskTree) {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let enrolled = tree((0..count).map(document).collect());
    exchange(&mut store, &enrolled, "enrol", 5);
    let mut next = enrolled.clone();
    for task in next.tasks.iter_mut().take(2) {
        task.title.push_str(" edited");
    }
    next.tasks.pop();
    next.tasks.push(document(count));
    let next = tree(next.tasks);
    exchange(&mut store, &next, "edit", 6);
    (store, next)
}

#[test]
fn repair_rebuilds_the_baseline_view_and_its_root_from_the_records_alone() {
    let (store, current) = exchanged(40);
    let written = view(&store);
    // The incrementally maintained root equals the root of the whole
    // membership computed from scratch.
    assert_eq!(
        head(&store).inventory_root,
        baseline::tree_root(&current).unwrap()
    );
    assert_eq!(
        written.0.len(),
        41,
        "40 enrolled, one added; the removed one stays retired"
    );
    assert_eq!(written.0.iter().filter(|row| row.4.is_none()).count(), 1);

    store
        .connection
        .execute_batch(
            "DROP TABLE work_task_sync_baseline;
             DROP TABLE work_task_sync_baseline_heads;",
        )
        .unwrap();
    let findings =
        crate::storage::work::schema::repair_rebuildable_schema_on(&store.connection).unwrap();
    assert!(findings.is_empty(), "{findings:?}");
    assert_eq!(view(&store), written);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn the_doctor_names_a_baseline_row_the_records_do_not_derive() {
    let (store, current) = exchanged(3);
    store
        .connection
        .execute(
            "UPDATE work_task_sync_baseline SET document = NULL, leaf = NULL
             WHERE project_id = ?1 AND work_id = ?2",
            rusqlite::params![PROJECT, current.tasks[0].id.0.to_string()],
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE work_task_sync_baseline_heads SET inventory_root = ?1",
            ["0".repeat(64)],
        )
        .unwrap();
    let report = format!("{:?}", store.verify_all().unwrap());
    assert!(
        report.contains(&format!(
            "work_task_sync_baseline:{PROJECT}:{}",
            current.tasks[0].id.0
        )),
        "{report}"
    );
    assert!(
        report.contains(&format!("work_task_sync_baseline_heads:{PROJECT}")),
        "{report}"
    );
}

thread_local! {
    static STATEMENTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Records the text of every statement as it starts, triggers included.
#[allow(
    clippy::needless_pass_by_value,
    reason = "rusqlite's trace callback type passes the event by value"
)]
fn traced(event: rusqlite::trace::TraceEvent<'_>) {
    if let rusqlite::trace::TraceEvent::Stmt(_, sql) = event {
        STATEMENTS.with(|statements| statements.borrow_mut().push(sql.to_owned()));
    }
}

fn written_rows(store: &SqliteStore) -> i64 {
    store
        .connection
        .query_row("SELECT count FROM temp.baseline_writes", [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[test]
fn only_the_bootstrap_writes_every_member_and_a_change_of_k_touches_k_rows() {
    const MEMBERS: usize = 2000;
    let mut store = SqliteStore::open_in_memory().unwrap();
    store
        .connection
        .execute_batch(
            "CREATE TEMP TABLE baseline_writes (count INTEGER NOT NULL);
             INSERT INTO temp.baseline_writes VALUES (0);
             CREATE TEMP TRIGGER baseline_insert AFTER INSERT ON main.work_task_sync_baseline
             BEGIN UPDATE temp.baseline_writes SET count = count + 1; END;
             CREATE TEMP TRIGGER baseline_update AFTER UPDATE ON main.work_task_sync_baseline
             BEGIN UPDATE temp.baseline_writes SET count = count + 1; END;",
        )
        .unwrap();
    let enrolled = tree((0..MEMBERS).map(document).collect());
    exchange(&mut store, &enrolled, "enrol", 5);
    assert_eq!(written_rows(&store), i64::try_from(MEMBERS).unwrap());

    // Three edits, one addition and one departure, appended as one record
    // against the head, with no complete tree in hand.
    let mut changes = BTreeMap::new();
    for task in enrolled.tasks.iter().take(3) {
        let mut edited = task.clone();
        edited.title.push_str(" edited");
        changes.insert(task.id, Some(edited));
    }
    let added = document(MEMBERS);
    changes.insert(added.id, Some(added));
    changes.insert(enrolled.tasks[MEMBERS - 1].id, None);
    let k = changes.len();

    let previous = head(&store).record;
    let previous_entries =
        baseline::entries(&store.connection, &project(), changes.keys().copied()).unwrap();
    STATEMENTS.with(|statements| statements.borrow_mut().clear());
    store.connection.trace_v2(
        rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
        Some(traced),
    );
    let transaction = store.begin_work_mutation().unwrap();
    baseline::append(
        &transaction,
        &project(),
        Some(previous),
        changes.clone(),
        Some("basis after five changes"),
        &profile(),
        at(6),
    )
    .unwrap();
    // Measure only baseline::append's own keyed operations. The enclosing
    // import writer also advances the selected existing tasks' durable heads.
    transaction.trace_v2(rusqlite::trace::TraceEventCodes::empty(), None);
    super::super::reconciliation::advance_on(
        &transaction,
        &project(),
        changes
            .keys()
            .copied()
            .filter(|id| enrolled.tasks.iter().any(|task| task.id == *id)),
        &previous_entries,
    )
    .unwrap();
    transaction.commit().unwrap();

    assert_eq!(written_rows(&store), i64::try_from(MEMBERS + k).unwrap());
    let touching = STATEMENTS.with(|statements| {
        statements
            .borrow()
            .iter()
            .filter(|sql| {
                sql.contains("work_task_sync_baseline ")
                    || sql.contains("work_task_sync_baseline\n")
            })
            .cloned()
            .collect::<Vec<_>>()
    });
    // One read for the new root and one read and one write in the replay,
    // for each changed task: never a statement per member, and each one
    // keyed by a single task.
    assert_eq!(touching.len(), 3 * k, "{touching:#?}");
    for sql in &touching {
        assert!(
            sql.contains("AND work_id = ") || sql.contains("ON CONFLICT(project_id, work_id)"),
            "{sql}"
        );
    }
    let plan = store
        .connection
        .prepare(
            "EXPLAIN QUERY PLAN
             SELECT record_id, position, document, leaf FROM work_task_sync_baseline
             WHERE project_id = ?1 AND work_id = ?2",
        )
        .unwrap()
        .query_map(
            rusqlite::params![PROJECT, changes.keys().next().unwrap().0.to_string()],
            |row| row.get::<_, String>(3),
        )
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    // A text primary key is served by SQLite's automatic index: one keyed
    // search on both columns, never a scan of the project's rows.
    assert!(
        !plan.is_empty()
            && plan.iter().all(|step| {
                step.starts_with("SEARCH work_task_sync_baseline USING ")
                    && step.ends_with("(project_id=? AND work_id=?)")
            }),
        "{plan:?}"
    );

    // The updated root is the root of the new membership from scratch.
    let mut members = enrolled
        .tasks
        .iter()
        .map(|task| (task.id, task.clone()))
        .collect::<BTreeMap<_, _>>();
    for (id, change) in &changes {
        match change {
            Some(document) => members.insert(*id, document.clone()),
            None => members.remove(id),
        };
    }
    assert_eq!(
        head(&store).inventory_root,
        baseline::tree_root(&tree(members.into_values().collect())).unwrap()
    );
    let report = store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
}

#[test]
fn a_baseline_planned_against_an_older_head_refuses_by_name() {
    let (mut store, current) = exchanged(3);
    // The enrolment record, which the later exchange already continued.
    let stale = store
        .connection
        .query_row(
            "SELECT object_id FROM work_feed_entries
             WHERE feed_kind = 'project' AND feed_id = ?1
               AND object_kind = 'task_sync_import_planning'
             ORDER BY position LIMIT 1",
            [PROJECT],
            |row| row.get::<_, String>(0),
        )
        .map(|id| ObjectId::from_stored(id).unwrap())
        .unwrap();
    assert_ne!(stale, head(&store).record);
    let before = state(&store);
    let mut edited = current.tasks[1].clone();
    edited.title.push_str(" again");
    let transaction = store.begin_work_mutation().unwrap();
    let error = baseline::append(
        &transaction,
        &project(),
        Some(stale),
        BTreeMap::from([(edited.id, Some(edited))]),
        Some("stale basis"),
        &profile(),
        at(7),
    )
    .unwrap_err();
    drop(transaction);
    assert_eq!(
        refusal_code(&error),
        "task_sync_baseline_head_moved",
        "{error:#}"
    );
    // A fork from no predecessor refuses the same way once a head exists.
    let transaction = store.begin_work_mutation().unwrap();
    let error = baseline::append(
        &transaction,
        &project(),
        None,
        BTreeMap::new(),
        Some("unchained basis"),
        &profile(),
        at(7),
    )
    .unwrap_err();
    drop(transaction);
    assert_eq!(refusal_code(&error), "task_sync_baseline_head_moved");
    assert_eq!(state(&store), before);
}

#[test]
fn the_keyed_reader_returns_exactly_the_requested_entries_and_refuses_a_mismatch() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let enrolled = tree((0..30).map(document).collect());
    exchange(&mut store, &enrolled, "enrol", 5);
    let mut next = enrolled.clone();
    next.tasks[0].title.push_str(" edited");
    let edited = next.tasks[0].clone();
    let removed = next.tasks.pop().unwrap().id;
    let untouched = next.tasks[1].id;
    exchange(&mut store, &tree(next.tasks), "edit", 6);
    let never_shared = WorkId(uuid::Uuid::new_v4());
    let enrolment = head_record_at(&store, 0);
    let latest = head(&store);

    let found = baseline::entries(
        &store.connection,
        &project(),
        [edited.id, removed, never_shared, untouched],
    )
    .unwrap();
    // Only the requested ids are answered; one never shared has no entry.
    assert_eq!(
        found.keys().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([edited.id, removed, untouched])
    );
    // A changed member and a departure carry the record that set them.
    assert_eq!(
        found[&edited.id],
        baseline::BaselineEntry {
            record: latest.record.clone(),
            position: latest.position,
            document: Some(edited.clone()),
        }
    );
    assert_eq!(found[&removed].record, latest.record);
    assert_eq!(found[&removed].document, None);
    // An unchanged member still carries the enrolment record.
    assert_eq!(found[&untouched].record, enrolment);
    assert_eq!(
        found[&untouched].document.as_ref().map(|task| task.id),
        Some(untouched)
    );

    // A row whose document no longer matches its leaf refuses, and so does
    // a row naming a record the feed does not hold at its position.
    store
        .connection
        .execute(
            "UPDATE work_task_sync_baseline SET document = ?1
             WHERE project_id = ?2 AND work_id = ?3",
            rusqlite::params![
                serde_json::to_vec(&found[&untouched].document).unwrap(),
                PROJECT,
                edited.id.0.to_string()
            ],
        )
        .unwrap();
    let error = baseline::entries(&store.connection, &project(), [edited.id]).unwrap_err();
    assert!(
        format!("{error:#}").contains("does not match its task"),
        "{error:#}"
    );
    store
        .connection
        .execute(
            "UPDATE work_task_sync_baseline SET position = position + 1
             WHERE project_id = ?1 AND work_id = ?2",
            rusqlite::params![PROJECT, removed.0.to_string()],
        )
        .unwrap();
    let error = baseline::entries(&store.connection, &project(), [removed]).unwrap_err();
    assert!(
        format!("{error:#}").contains("does not hold at its position"),
        "{error:#}"
    );
}

/// The planning record at `index` in the project feed's planning entries.
fn head_record_at(store: &SqliteStore, index: i64) -> ObjectId {
    store
        .connection
        .query_row(
            "SELECT object_id FROM work_feed_entries
             WHERE feed_kind = 'project' AND feed_id = ?1
               AND object_kind = 'task_sync_import_planning'
             ORDER BY position LIMIT 1 OFFSET ?2",
            rusqlite::params![PROJECT, index],
            |row| row.get::<_, String>(0),
        )
        .map(|id| ObjectId::from_stored(id).unwrap())
        .unwrap()
}

/// Appends a hand-built baseline record through the ordinary feed append,
/// which applies it to the view in the same transaction.
fn append_raw(
    store: &mut SqliteStore,
    planning: &baseline::TaskSyncImportPlanning,
) -> Result<(), StoreError> {
    let kind = crate::storage::work::task_sync::folder::IMPORT_PLANNING_KIND;
    let transaction = store.begin_work_mutation()?;
    let previous = baseline::entries(&transaction, &project(), planning.changes.keys().copied())?;
    let object = CanonicalObject::mint(planning)?;
    SqliteStore::insert_object(&transaction, kind, &object)?;
    crate::storage::work::feeds::append_to_project_feed(&transaction, &project(), kind, &object)?;
    let mut existing = Vec::new();
    for id in planning.changes.keys() {
        if transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_items WHERE project_id=?1 AND work_id=?2)",
            rusqlite::params![project().0, id.0.to_string()],
            |row| row.get::<_, bool>(0),
        )? {
            existing.push(*id);
        }
    }
    super::super::reconciliation::advance_on(&transaction, &project(), existing, &previous)?;
    transaction.commit()?;
    Ok(())
}

fn record(
    store: &SqliteStore,
    changes: BTreeMap<WorkId, Option<TaskDocument>>,
    inventory_root: String,
    include_notes: bool,
) -> baseline::TaskSyncImportPlanning {
    baseline::TaskSyncImportPlanning {
        project_id: project(),
        previous: Some(head(store).record),
        changes,
        inventory_root,
        export_basis: Some("hand-built basis".into()),
        profile: crate::task_sync::progress::ExportProfile {
            include_notes,
            // Shared notes need an origin alias to be a valid profile.
            origin_machine: include_notes.then(|| "hand-built-host".into()),
        },
        created_at: at(9),
    }
}

#[test]
fn every_refused_baseline_record_names_its_reason_and_leaves_the_view_unchanged() {
    let (mut store, current) = exchanged(4);
    let retired = baseline::retired(&store.connection, &project()).unwrap();
    let retired_id = *retired.iter().next().unwrap();
    let member = current.tasks[0].clone();
    let mut edited = member.clone();
    edited.title.push_str(" again");
    let stranger = WorkId(uuid::Uuid::new_v4());
    let root = head(&store).inventory_root;
    let refused = [
        (
            record(
                &store,
                BTreeMap::from([(member.id, Some(edited))]),
                "0".repeat(64),
                false,
            ),
            "fingerprint does not match its changes",
        ),
        (
            record(
                &store,
                BTreeMap::from([(stranger, None)]),
                root.clone(),
                false,
            ),
            "which is not a member",
        ),
        (
            record(
                &store,
                BTreeMap::from([(retired_id, None)]),
                root.clone(),
                false,
            ),
            "which is not a member",
        ),
        (
            record(
                &store,
                BTreeMap::from([(stranger, Some(member.clone()))]),
                root.clone(),
                false,
            ),
            "record is inconsistent",
        ),
        (
            record(&store, BTreeMap::new(), root.clone(), true),
            "note selection differs from its predecessor",
        ),
    ];
    for (planning, reason) in refused {
        let before = view(&store);
        let error = append_raw(&mut store, &planning).unwrap_err();
        assert!(format!("{error:#}").contains(reason), "{reason}: {error:#}");
        assert_eq!(view(&store), before, "{reason}");
    }
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn a_retired_task_can_rejoin_and_the_root_follows_it() {
    let (mut store, _) = exchanged(4);
    let retired_id = *baseline::retired(&store.connection, &project())
        .unwrap()
        .iter()
        .next()
        .unwrap();
    let mut returning = document(90);
    returning.id = retired_id;
    let changes = BTreeMap::from([(retired_id, Some(returning))]);
    let root = baseline::next_root(&store.connection, &project(), &changes).unwrap();
    let planning = record(&store, changes, root, false);
    append_raw(&mut store, &planning).unwrap();
    assert_eq!(
        baseline::retired(&store.connection, &project()).unwrap(),
        BTreeSet::new()
    );
    let loaded = baseline::load(&store.connection, &project())
        .unwrap()
        .unwrap();
    assert!(loaded.tree.tasks.iter().any(|task| task.id == retired_id));
    assert_eq!(
        loaded.head.inventory_root,
        baseline::tree_root(&loaded.tree).unwrap()
    );
    let report = store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
}

#[test]
fn load_refuses_a_view_whose_members_do_not_match_the_head() {
    let (store, current) = exchanged(3);
    store
        .connection
        .execute(
            "UPDATE work_task_sync_baseline_heads SET inventory_root = ?1",
            ["0".repeat(64)],
        )
        .unwrap();
    let error = baseline::load(&store.connection, &project()).err().unwrap();
    assert!(
        format!("{error:#}").contains("do not sum to the head fingerprint"),
        "{error:#}"
    );
    store
        .connection
        .execute(
            "UPDATE work_task_sync_baseline SET leaf = zeroblob(32)
             WHERE project_id = ?1 AND work_id = ?2",
            rusqlite::params![PROJECT, current.tasks[0].id.0.to_string()],
        )
        .unwrap();
    let error = baseline::load(&store.connection, &project()).err().unwrap();
    assert!(
        format!("{error:#}").contains("does not match its task"),
        "{error:#}"
    );
}

/// Stores `object` and appends it to the project feed without applying it,
/// as only a damaged store or an imported old record would hold it.
fn feed_directly(store: &SqliteStore, object: &CanonicalObject) {
    let kind = crate::storage::work::task_sync::folder::IMPORT_PLANNING_KIND;
    SqliteStore::insert_object(&store.connection, kind, object).unwrap();
    let position: i64 = store
        .connection
        .query_row(
            "SELECT position FROM work_feed_heads
             WHERE feed_kind = 'project' AND feed_id = ?1",
            [PROJECT],
            |row| row.get(0),
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO work_feed_entries
                 (feed_kind, feed_id, position, object_kind, object_id, work_id)
             VALUES ('project', ?1, ?2, ?3, ?4, NULL)",
            rusqlite::params![PROJECT, position + 1, kind, object.key().as_str()],
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE work_feed_heads SET position = ?2
             WHERE feed_kind = 'project' AND feed_id = ?1",
            rusqlite::params![PROJECT, position + 1],
        )
        .unwrap();
}

#[test]
fn a_record_refused_mid_replay_leaves_no_partial_rows_for_the_doctor() {
    let (store, current) = exchanged(3);
    // A valid edit followed, in the same record, by an invalid departure.
    // The replay refuses the record; staging means none of its rows land.
    let mut edited = current.tasks[0].clone();
    edited.title.push_str(" in a refused record");
    // The greatest id sorts last, so the edit is always staged before the
    // departure is refused.
    let stranger = WorkId(uuid::Uuid::from_u128(u128::MAX));
    assert!(edited.id < stranger);
    let changes = BTreeMap::from([(edited.id, Some(edited)), (stranger, None)]);
    let root = baseline::next_root(&store.connection, &project(), &changes).unwrap();
    let object = CanonicalObject::mint(&record(&store, changes, root, false)).unwrap();
    feed_directly(&store, &object);
    let report = format!("{:?}", store.verify_all().unwrap());
    assert!(
        report.contains("work_task_sync_baseline:underivable:"),
        "{report}"
    );
    assert!(
        !report.contains(&format!("work_task_sync_baseline:{PROJECT}:")),
        "a refused record left partial rows: {report}"
    );
}

#[test]
fn repair_names_a_planning_record_of_the_retired_complete_shape() {
    let (store, _) = exchanged(2);
    // A complete-tree record, which no live store holds, fed directly so
    // that the append path does not refuse it first.
    let object = CanonicalObject::mint(&serde_json::json!({
        "project_id": PROJECT,
        "tree": {
            "project": {"format": "engram-task-planning-v1", "project_id": PROJECT},
            "tasks": []
        },
        "inventory_fingerprint": "0".repeat(64),
        "export_basis": "basis",
        "profile": {"include_notes": false},
        "created_at": "2026-01-01T00:00:00Z"
    }))
    .unwrap();
    feed_directly(&store, &object);
    let findings =
        crate::storage::work::schema::repair_rebuildable_schema_on(&store.connection).unwrap();
    let id = object.key().as_str();
    assert!(
        findings.iter().any(|finding| {
            finding.starts_with("work_task_sync_baseline:underivable:") && finding.contains(id)
        }),
        "{findings:?}"
    );
    assert!(
        findings
            .iter()
            .any(|finding| finding.starts_with(&format!("work_project_feed_tasks:{id}"))),
        "{findings:?}"
    );
}

#[test]
fn current_imported_head_reads_one_keyed_document_without_decoding_its_whole_delta() {
    let (store, current) = exchanged(40);
    let chosen = current
        .tasks
        .iter()
        .find(|task| task.title.ends_with(" edited"))
        .unwrap();
    STATEMENTS.with(|statements| statements.borrow_mut().clear());
    store.connection.trace_v2(
        rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
        Some(traced),
    );
    assert_eq!(
        store
            .task_sync_imported_baseline(&project(), chosen.id)
            .unwrap(),
        Some(chosen.clone())
    );
    store
        .connection
        .trace_v2(rusqlite::trace::TraceEventCodes::empty(), None);
    STATEMENTS.with(|statements| {
        let statements = statements.borrow();
        assert!(
            statements.iter().all(|sql| {
                !sql.contains("canonical_json")
                    || (sql.contains("object.object_kind = 'work_event'")
                        && sql.contains("entry.work_id = ?1")
                        && sql.contains("ORDER BY entry.position DESC LIMIT 1"))
            }),
            "current baseline read must not decode a whole planning record: {statements:#?}"
        );
        for sql in statements
            .iter()
            .filter(|sql| sql.contains("FROM work_task_sync_baseline"))
        {
            assert!(
                sql.contains("AND work_id = "),
                "baseline reads must stay keyed: {sql}"
            );
        }
    });
    let report = store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
}
