use super::*;
use crate::storage::{WorkRecordContent, WorkRecordKind};
use crate::task_sync::{
    activity::{ActivityNotice, ActivityState},
    notes::{SharedNote, SharedNoteKind},
};

mod disclosure;
mod reads;

fn shared_profile(alias: Option<&str>, notes: bool) -> ExportProfile {
    ExportProfile {
        include_notes: notes,
        origin_machine: alias
            .map(str::to_owned)
            .or_else(|| notes.then(|| "local-host".into())),
    }
}

fn capture(store: &SqliteStore, alias: Option<&str>, notes: bool) -> TaskTree {
    store
        .export_task_sync_folder(&project(), &shared_profile(alias, notes))
        .unwrap()
        .tree
}

fn apply(
    store: &mut SqliteStore,
    tree: &TaskTree,
    alias: Option<&str>,
    key: &str,
    second: i64,
) -> TaskSyncImportResult {
    let profile = shared_profile(alias, tree.project.notes_included);
    let basis = store
        .export_task_sync_folder(&project(), &profile)
        .unwrap()
        .export_basis;
    store
        .apply_task_sync_folder(
            TaskSyncFolderApply {
                project: &project(),
                actor: &actor("receiver"),
                tree,
                export_basis: &basis,
                profile: &profile,
                complete: true,
                now: at(second),
            },
            key,
        )
        .unwrap()
}

fn assert_applied(result: &TaskSyncImportResult) {
    assert!(
        result
            .tasks
            .iter()
            .all(|task| task.status == crate::storage::TaskSyncTaskStatus::Applied),
        "{result:?}"
    );
}

fn note(body: &str, alias: &str) -> SharedNote {
    SharedNote {
        id: CanonicalObject::mint(&body).unwrap().key().clone(),
        kind: SharedNoteKind::Status,
        body: body.into(),
        author: SharedActor::from_actor(&actor("reported-author")),
        recorded_at: at(1),
        origin_machine: alias.into(),
    }
}

fn episode(id: WorkId, alias: &str) -> ActivityNotice {
    let start = CanonicalObject::mint(&id).unwrap().key().clone();
    ActivityNotice {
        activity_id: start.clone(),
        task_id: id,
        origin_machine: alias.into(),
        actor: SharedActor::from_actor(&actor("reported-executor")),
        state: ActivityState::Started,
        state_revision: 1,
        event_id: start,
    }
}

fn tree_with_shared() -> TaskTree {
    let mut tree = fixture().tree;
    tree.project.notes_included = true;
    tree.tasks = vec![
        document("Shared task", None),
        document("Independent task", None),
    ];
    tree.tasks.sort_by_key(|task| task.id);
    let task = &mut tree.tasks[0];
    task.notes.push(note("First note", "host-A"));
    let notice = episode(task.id, "host-A");
    task.activity
        .entry("host-A".into())
        .or_default()
        .insert(notice.activity_id.clone(), notice);
    tree
}

#[test]
fn shared_updates_relay_three_hosts_keep_note_identity_and_monotonic_activity() {
    let initial = tree_with_shared();
    let id = initial.tasks[0].id;
    let activity = initial.tasks[0].activity["host-A"]
        .keys()
        .next()
        .unwrap()
        .clone();
    let mut a = SqliteStore::open_in_memory().unwrap();
    let mut b = SqliteStore::open_in_memory().unwrap();
    let mut c = SqliteStore::open_in_memory().unwrap();
    apply(&mut a, &initial, None, "seed-a", 2);
    apply(&mut b, &initial, None, "seed-b", 2);
    apply(&mut c, &initial, None, "seed-c", 2);
    let originals = durable(&b);
    let mut next = initial.clone();
    let task = &mut next.tasks[0];
    task.notes
        .extend([note("Same body Ω", "host-A"), note("Same body Ω", "host-B")]);
    task.notes
        .sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
    let notice = task
        .activity
        .get_mut("host-A")
        .unwrap()
        .get_mut(&activity)
        .unwrap();
    notice.state = ActivityState::Released;
    notice.state_revision = 2;
    notice.event_id = CanonicalObject::mint(&"released").unwrap().key().clone();
    let before_item = b.get_work_item(id).unwrap();
    assert_applied(&apply(&mut b, &next, None, "advance-b", 3));
    assert_eq!(b.get_work_item(id).unwrap(), before_item);
    assert_eq!(durable(&b), originals);
    let from_b = capture(&b, None, true);
    assert_eq!(from_b, next);
    apply(&mut c, &from_b, None, "relay-c", 4);
    apply(&mut a, &capture(&c, None, true), None, "return-a", 5);
    for store in [&a, &b, &c] {
        assert_eq!(capture(store, None, true), next);
        let health = store.verify_all().unwrap();
        assert!(health.is_healthy(), "{health:?}");
        let rows = store
            .work_record_index(&project(), id, WorkRecordKind::Notes)
            .unwrap();
        let notes = rows
            .iter()
            .map(|row| {
                let WorkRecordContent::Note(note) =
                    store.work_record_content(&project(), id, row).unwrap()
                else {
                    panic!("note expected");
                };
                assert!(note.gate.is_none() && note.verification.is_none());
                assert!(crate::domain::status_note_role(&note.actor).is_none());
                (row.locator.clone(), note.summary)
            })
            .collect::<Vec<_>>();
        assert_eq!(notes.len(), 3);
        assert_eq!(
            notes
                .iter()
                .filter(|(_, body)| body == "Same body Ω")
                .count(),
            2
        );
        assert_eq!(
            notes
                .iter()
                .filter(|(locator, _)| locator.contains(":shared-note-"))
                .count(),
            2
        );
    }
    let stable = b
        .work_record_index(&project(), id, WorkRecordKind::Notes)
        .unwrap()
        .into_iter()
        .map(|row| row.locator)
        .collect::<Vec<_>>();
    assert!(!apply(&mut b, &next, None, "fresh-repeat", 6).changed);
    assert_eq!(
        stable,
        b.work_record_index(&project(), id, WorkRecordKind::Notes)
            .unwrap()
            .into_iter()
            .map(|row| row.locator)
            .collect::<Vec<_>>()
    );
    let saved = b
        .save_work_graph_snapshot(
            &project(),
            &actor("saver"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(7),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let text = serde_json::to_string(&saved.document).unwrap();
    assert_eq!(text.matches("Same body Ω").count(), 2);
}

#[test]
fn shared_updates_refuse_removal_rollback_and_conflicts_per_task_atomically() {
    let initial = tree_with_shared();
    let id = initial.tasks[0].id;
    let mut store = SqliteStore::open_in_memory().unwrap();
    apply(&mut store, &initial, None, "seed", 2);
    let mut advanced = initial.clone();
    let entry = advanced.tasks[0]
        .activity
        .get_mut("host-A")
        .unwrap()
        .values_mut()
        .next()
        .unwrap();
    entry.state = ActivityState::Released;
    entry.state_revision = 2;
    entry.event_id = CanonicalObject::mint(&"released").unwrap().key().clone();
    apply(&mut store, &advanced, None, "advance", 3);
    for (number, expected) in [
        (0, "note_removal"),
        (1, "activity_removal"),
        (2, "activity_rollback"),
        (3, "activity_conflict"),
        (4, "shared_identity"),
    ] {
        let mut bad = advanced.clone();
        bad.tasks[0].title = "This must not apply with its invalid shared member".into();
        match number {
            0 => bad.tasks[0].notes.clear(),
            1 => bad.tasks[0].activity.clear(),
            2 => bad.tasks[0].activity = initial.tasks[0].activity.clone(),
            3 => {
                bad.tasks[0]
                    .activity
                    .get_mut("host-A")
                    .unwrap()
                    .values_mut()
                    .next()
                    .unwrap()
                    .state = ActivityState::Abandoned;
            }
            _ => bad.tasks[0].notes[0].body.push_str(" rewritten"),
        }
        bad.tasks[1].priority = (number + 2) % 5;
        let result = apply(
            &mut store,
            &bad,
            None,
            &format!("refuse-{number}"),
            4 + i64::from(number),
        );
        assert!(refusals(&result).contains(expected), "{result:?}");
        assert_eq!(
            store.get_work_item(id).unwrap().title,
            advanced.tasks[0].title
        );
        assert_eq!(
            store.get_work_item(bad.tasks[1].id).unwrap().priority,
            bad.tasks[1].priority
        );
        assert_eq!(capture(&store, None, true).tasks[0], advanced.tasks[0]);
    }
}

#[test]
fn shared_updates_do_not_adopt_authority_on_live_or_terminal_tasks() {
    let initial = tree_with_shared();
    let id = initial.tasks[0].id;
    let mut store = SqliteStore::open_in_memory().unwrap();
    apply(&mut store, &initial, None, "seed", 2);
    let live = claim_restored(&mut store, id, "local-executor", 3);
    let before = store.get_work_item(id).unwrap();
    let mut next = initial.clone();
    next.tasks[0]
        .notes
        .push(note("Progress on the other host", "host-C"));
    next.tasks[0]
        .notes
        .sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    assert_applied(&apply(&mut store, &next, None, "live-note", 4));
    assert_eq!(store.get_work_item(id).unwrap(), before);
    assert_eq!(
        query::load_work_claim_optional(&store.connection, live.run_id)
            .unwrap()
            .unwrap(),
        live
    );
    let mut terminal = next.clone();
    terminal.tasks[0].lifecycle = WorkLifecycle::Completed;
    terminal.tasks[0].completion = Some(SharedCompletion {
        summary: "Reported done".into(),
        completed_at: at(5),
        actor: SharedActor::from_actor(&actor("remote")),
    });
    // A governing completion stays held by execution, with its note atomic.
    terminal.tasks[0]
        .notes
        .push(note("Completion note", "host-C"));
    terminal.tasks[0]
        .notes
        .sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    assert!(
        refusals(&apply(&mut store, &terminal, None, "held-completion", 5)).contains("active_run")
    );
    assert_eq!(
        capture(&store, None, true).tasks[0].notes,
        next.tasks[0].notes
    );
}

#[test]
fn shared_updates_planning_only_omission_is_visible_on_receiver_and_alias_return_recovers() {
    let mut source = SqliteStore::open_in_memory().unwrap();
    let work = source
        .create_work(
            &root_request(PROJECT, "native", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let live = claim(&mut source, &work, "executor", "claim", 1, 100);
    assert!(capture(&source, None, false).tasks[0].activity.is_empty());
    let aliased = capture(&source, Some("host-A"), false);
    let mut receiver = SqliteStore::open_in_memory().unwrap();
    apply(&mut receiver, &aliased, None, "receive-alias", 2);
    let planning = capture(&source, None, false);
    assert!(
        refusals(&apply(&mut receiver, &planning, None, "missing-episode", 3))
            .contains("activity_removal")
    );
    let item = source.get_work_item(work.work_id).unwrap();
    source
        .release_work(
            &ReleaseWorkRequest {
                work_id: work.work_id,
                run_id: live.run_id,
                expected_work_revision: item.revision,
                holder: live.holder.clone(),
                claim_id: live.claim_id,
                claim_fence: live.fence,
                reason: "Released".into(),
                waiver_reason: Some("No contribution".into()),
                actor: actor("executor"),
                idempotency_key: "release".into(),
                released_at: at(4),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let recovered = capture(&source, Some("host-A"), false);
    assert_applied(&apply(&mut receiver, &recovered, None, "recover", 5));
    // An echo agrees with native records, and enrolls the original alias.
    assert_applied(&apply(
        &mut source,
        &capture(&receiver, None, false),
        Some("host-A"),
        "return",
        6,
    ));
    assert_eq!(capture(&source, None, false), recovered);
    assert_eq!(capture(&receiver, None, false), recovered);
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one two-host workflow follows the same episode through handoff, note, completion and return"
)]
fn shared_updates_native_handoff_notes_and_returned_completion_round_trip() {
    use crate::storage::work::test_support::{checkpoint, complete, evidence};
    let mut a = SqliteStore::open_in_memory().unwrap();
    let mut request = root_request(PROJECT, "native shared", 0);
    request.notes = vec!["Initial native note".into()];
    let work = a
        .create_work(&request, &crate::DevelopmentNoopRedactor)
        .unwrap();
    let first = claim(&mut a, &work, "first", "first-claim", 1, 100);
    let initial = capture(&a, Some("host-A"), true);
    let mut b = SqliteStore::open_in_memory().unwrap();
    apply(&mut b, &initial, Some("host-B"), "initial", 2);
    let restored = b.get_work_item(work.work_id).unwrap();
    let local = b
        .claim_work(
            &ClaimWorkRequest {
                work_id: work.work_id,
                expected_work_revision: restored.revision,
                expected_run_id: restored.active_run_id,
                holder: SessionId("parallel-local".into()),
                ttl_seconds: 1,
                recovery_reason: None,
                actor: actor("parallel-local"),
                idempotency_key: "parallel-local".into(),
                claimed_at: at(3),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let item = a.get_work_item(work.work_id).unwrap();
    let offer = a
        .offer_work_handoff(
            &crate::OfferWorkHandoffRequest {
                work_id: work.work_id,
                run_id: first.run_id,
                expected_work_revision: item.revision,
                from: first.holder.clone(),
                to: SessionId("second".into()),
                claim_id: first.claim_id,
                claim_fence: first.fence,
                ttl_seconds: 100,
                checkpoint_summary: "Handoff".into(),
                actor: actor("first"),
                idempotency_key: "offer".into(),
                offered_at: at(3),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let second = a
        .accept_work_handoff(
            &crate::AcceptWorkHandoffRequest {
                work_id: work.work_id,
                offer_id: offer.offer_id,
                to: SessionId("second".into()),
                actor: actor("second"),
                idempotency_key: "accept".into(),
                accepted_at: at(4),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let transferred = capture(&a, Some("host-A"), true);
    let old = transferred.tasks[0].activity["host-A"]
        .values()
        .find(|notice| notice.actor.actor_id == "first")
        .unwrap();
    let next = transferred.tasks[0].activity["host-A"]
        .values()
        .find(|notice| notice.actor.actor_id == "second")
        .unwrap();
    assert_eq!(old.event_id, next.activity_id);
    let before = b.get_work_item(work.work_id).unwrap();
    assert_applied(&apply(
        &mut b,
        &transferred,
        Some("host-B"),
        "handoff-update",
        4,
    ));
    assert_eq!(b.get_work_item(work.work_id).unwrap(), before);
    assert_eq!(
        query::load_work_claim_optional(&b.connection, local.run_id)
            .unwrap()
            .unwrap(),
        local
    );
    let item = a.get_work_item(work.work_id).unwrap();
    a.record_work_note(
        &crate::domain::RecordWorkNoteRequest {
            status: true,
            work_id: work.work_id,
            run_id: second.run_id,
            expected_work_revision: item.revision,
            holder: second.holder.clone(),
            claim_id: second.claim_id,
            claim_fence: second.fence,
            summary: "Native progress after handoff".into(),
            refs: Vec::new(),
            actor: actor("second"),
            idempotency_key: "progress-note".into(),
            recorded_at: at(5),
        },
        &crate::DevelopmentNoopRedactor,
    )
    .unwrap();
    let item = a.get_work_item(work.work_id).unwrap();
    let proof = evidence(&mut a, &item, &second, "second", "proof", 6);
    checkpoint(
        &mut a,
        &item,
        &second,
        "second",
        "checkpoint",
        7,
        std::slice::from_ref(&proof),
    );
    complete(&mut a, &item, &second, "second", &proof, "complete", 8).unwrap();
    let completed = capture(&a, Some("host-A"), true);
    assert_applied(&apply(
        &mut b,
        &completed,
        Some("host-B"),
        "completion-and-note",
        9,
    ));
    assert_eq!(
        b.get_work_item(work.work_id).unwrap().lifecycle,
        WorkLifecycle::Completed
    );
    let outgoing = capture(&b, Some("host-B"), true);
    assert!(
        outgoing.tasks[0].activity["host-B"]
            .values()
            .all(|notice| notice.state == ActivityState::Abandoned)
    );
    assert_applied(&apply(
        &mut a,
        &outgoing,
        Some("host-A"),
        "completion-return",
        10,
    ));
    assert_eq!(capture(&a, Some("host-A"), true), outgoing);
    for store in [&a, &b] {
        let health = store.verify_all().unwrap();
        assert!(health.is_healthy(), "{health:?}");
    }
}

#[test]
fn shared_updates_dry_run_terminal_notes_and_projection_repair_keep_baseline_and_locators() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("shared-updates.db");
    let initial = tree_with_shared();
    let id = initial.tasks[0].id;
    let mut store = SqliteStore::open(&database).unwrap();
    apply(&mut store, &initial, None, "initial", 2);
    let mut next = initial.clone();
    next.tasks[0]
        .notes
        .push(note("Arrived after the first exchange", "host-C"));
    next.tasks[0]
        .notes
        .sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    let profile = shared_profile(None, true);
    let basis = store
        .export_task_sync_folder(&project(), &profile)
        .unwrap()
        .export_basis;
    let before = state(&store);
    let preview = store
        .dry_run_task_sync_folder(
            TaskSyncFolderApply {
                project: &project(),
                actor: &actor("receiver"),
                tree: &next,
                export_basis: &basis,
                profile: &profile,
                complete: true,
                now: at(3),
            },
            "preview-and-apply",
        )
        .unwrap();
    assert!(preview.changed && preview.dry_run);
    assert_eq!(state(&store), before);
    apply(&mut store, &next, None, "preview-and-apply", 3);
    let mut terminal = next.clone();
    terminal.tasks[0].lifecycle = WorkLifecycle::Completed;
    terminal.tasks[0].completion = Some(SharedCompletion {
        summary: "Reported completion".into(),
        completed_at: at(4),
        actor: SharedActor::from_actor(&actor("remote")),
    });
    apply(&mut store, &terminal, None, "complete", 4);
    let ended = store.get_work_item(id).unwrap();
    terminal.tasks[0]
        .notes
        .push(note("Late history after completion", "host-C"));
    terminal.tasks[0]
        .notes
        .sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    assert_applied(&apply(&mut store, &terminal, None, "terminal-note", 5));
    assert_eq!(store.get_work_item(id).unwrap(), ended);
    let locators = store
        .work_record_index(&project(), id, WorkRecordKind::Notes)
        .unwrap()
        .into_iter()
        .map(|row| row.locator)
        .collect::<Vec<_>>();
    assert_eq!(locators.len(), 3);
    let documents = capture(&store, None, true).documents().unwrap();
    let immutable = durable(&store);
    drop(store);
    let health = SqliteStore::repair_rebuildable_projections(&database).unwrap();
    assert!(health.is_healthy(), "{health:?}");
    let store = SqliteStore::open(&database).unwrap();
    assert_eq!(durable(&store), immutable);
    assert_eq!(capture(&store, None, true).documents().unwrap(), documents);
    assert_eq!(
        store
            .work_record_index(&project(), id, WorkRecordKind::Notes)
            .unwrap()
            .into_iter()
            .map(|row| row.locator)
            .collect::<Vec<_>>(),
        locators
    );
}

#[test]
fn shared_updates_note_windows_keep_feed_order_detail_and_continuations() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("shared-note-windows.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let mut request = root_request(PROJECT, "notes-window", 0);
    request.notes = vec!["Native before import".into()];
    let work = store
        .create_work(&request, &crate::DevelopmentNoopRedactor)
        .unwrap();
    let mut incoming = capture(&store, Some("local"), true);
    incoming.tasks[0]
        .notes
        .extend((0..70).map(|index| note(&format!("Shared note {index}"), "remote")));
    incoming.tasks[0]
        .notes
        .sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    let shared = incoming.tasks[0]
        .notes
        .iter()
        .filter(|note| note.origin_machine == "remote")
        .map(|note| note.body.clone())
        .collect::<Vec<_>>();
    apply(&mut store, &incoming, Some("local"), "many-shared-notes", 2);
    let item = store.get_work_item(work.work_id).unwrap();
    store
        .record_work_observation(
            &crate::domain::RecordWorkObservationRequest {
                status: false,
                project_id: project(),
                work_id: work.work_id,
                expected_work_revision: item.revision,
                session_id: SessionId("peer".into()),
                summary: "Native after import".into(),
                refs: Vec::new(),
                actor: crate::domain::non_holder_note_actor(actor("peer")),
                idempotency_key: "native-after".into(),
                recorded_at: at(3),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    drop(store);
    let service = crate::work_service::LocalWorkService::new(
        database,
        project(),
        "reader".into(),
        SessionId("reader".into()),
        Some("note-window-test".into()),
    );
    super::super::super::origin::PLANNING_DECODES.with(|count| count.set(0));
    let (_, first) = service
        .work_record_window(&work.short_ref, WorkRecordKind::Notes, None, at(4))
        .unwrap();
    super::super::super::origin::PLANNING_DECODES.with(|count| assert_eq!(count.get(), 1));
    assert_eq!(first.total, 72);
    assert_eq!(first.rows[0].summary, "Native after import");
    assert_eq!(first.rows.len(), 64);
    let cursor = first.continuation(first.rows.len()).unwrap().unwrap();
    let locator = first
        .rows
        .iter()
        .find(|row| row.locator.contains(":shared-note-"))
        .unwrap()
        .locator
        .clone();
    let (_, detail, assessment) = service
        .work_note_detail(&work.short_ref, &locator, None, at(4))
        .unwrap();
    assert!(detail.gate.is_none() && detail.verification.is_none() && assessment.is_none());
    assert!(crate::domain::status_note_role(&detail.actor).is_none());
    let (_, second) = service
        .work_record_window(&work.short_ref, WorkRecordKind::Notes, Some(&cursor), at(4))
        .unwrap();
    assert_eq!(second.total, 72);
    assert_eq!(second.newer, 64);
    assert_eq!(second.rows.last().unwrap().summary, "Native before import");
    assert!(second.continuation(second.rows.len()).unwrap().is_none());
    let mut expected = vec!["Native after import".to_owned()];
    expected.extend(shared.into_iter().rev());
    expected.push("Native before import".into());
    let actual = first
        .rows
        .iter()
        .chain(&second.rows)
        .map(|row| row.summary.clone())
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
}
