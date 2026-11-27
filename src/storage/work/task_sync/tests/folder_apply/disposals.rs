//! A peer's cancellation or supersession of a task this store holds Open.

use super::*;
use crate::storage::TaskSyncTaskStatus;

fn cancelled(tree: &TaskTree, id: WorkId, reason: &str) -> TaskTree {
    edited(tree, id, |task| {
        task.lifecycle = WorkLifecycle::Cancelled;
        task.disposal_reason = Some(reason.into());
    })
}

fn superseded(tree: &TaskTree, id: WorkId, by: WorkId, reason: &str) -> TaskTree {
    edited(tree, id, |task| {
        task.lifecycle = WorkLifecycle::Superseded;
        task.superseded_by = Some(by);
        task.disposal_reason = Some(reason.into());
    })
}

fn status(result: &TaskSyncImportResult, id: WorkId) -> (TaskSyncTaskStatus, Option<String>) {
    let task = result
        .tasks
        .iter()
        .find(|task| task.task == id)
        .unwrap_or_else(|| panic!("{id:?} is in the receipt: {result:?}"));
    (task.status, task.reason.clone())
}

fn native_dispose(
    store: &mut SqliteStore,
    id: WorkId,
    replacement: Option<WorkId>,
    reason: &str,
    second: i64,
) {
    let item = query::load_work_item(&store.connection, id).unwrap();
    store
        .dispose_work(
            &crate::domain::DisposeWorkRequest {
                work_id: id,
                expected_work_revision: item.revision,
                disposition: if replacement.is_some() {
                    crate::WorkDisposition::Superseded
                } else {
                    crate::WorkDisposition::Cancelled
                },
                replacement_id: replacement,
                reason: reason.into(),
                actor: actor("planner"),
                idempotency_key: format!("dispose-{second}"),
                disposed_at: at(second),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
}

#[test]
fn a_refused_move_does_not_reorder_a_parents_disposal_before_its_held_descendant() {
    // Held R -> A -> P -> M -> C, with M completed and C optional and open.
    // The folder moves M under R, which this store refuses, and cancels P
    // and C. By the folder's hierarchy P and C are both two levels deep and
    // P's id sorts first; by the hierarchy this store keeps, C is two levels
    // below P and must end first.
    let id = |n: u128| WorkId(uuid::Uuid::from_u128(n));
    let (r_id, a_id, p_id, m_id, c_id) = (id(1), id(2), id(3), id(4), id(5));
    let make = |task_id: WorkId, title: &str, parent: Option<WorkId>| {
        let mut task = document(title, parent);
        task.id = task_id;
        task
    };
    let mut m = make(m_id, "m", Some(p_id));
    m.lifecycle = WorkLifecycle::Completed;
    m.completion = Some(SharedCompletion {
        summary: "done elsewhere".into(),
        completed_at: at(2),
        actor: SharedActor::from_actor(&actor("reported-author")),
    });
    let mut c = make(c_id, "c", Some(m_id));
    c.child_requirement = ChildRequirement::Optional;
    let mut tasks = vec![
        make(r_id, "r", None),
        make(a_id, "a", Some(r_id)),
        make(p_id, "p", Some(a_id)),
        m,
        c,
    ];
    tasks.sort_by_key(|task| task.id.0);
    let tree = TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: project(),
            notes_included: false,
        },
        tasks,
    };
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &tree, "enrol", 5).unwrap();

    let resolved = cancelled(
        &cancelled(
            &edited(&tree, m_id, |task| task.parent_id = Some(r_id)),
            p_id,
            "Dropped elsewhere",
        ),
        c_id,
        "Dropped elsewhere",
    );
    let result = exchange(&mut store, &resolved, "refused-move", 6).unwrap();
    assert_eq!(
        status(&result, m_id),
        (
            TaskSyncTaskStatus::Refused,
            Some("unsupported_fields".into())
        )
    );
    for task in [c_id, p_id] {
        assert_eq!(status(&result, task), (TaskSyncTaskStatus::Applied, None));
        assert_eq!(
            query::load_work_item(&store.connection, task)
                .unwrap()
                .lifecycle,
            WorkLifecycle::Cancelled
        );
    }
    assert_eq!(
        query::load_work_item(&store.connection, m_id)
            .unwrap()
            .parent_id,
        Some(p_id)
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn a_supersession_onto_a_replacement_this_store_already_ended_applies() {
    let (mut store, fixture) = enrolled();
    // This store cancels the second child before the peer's supersession of
    // the first by it arrives; the merged folder carries both.
    native_dispose(&mut store, fixture.second, None, "Ended here", 6);
    let resolved = superseded(
        &exported(&store),
        fixture.first,
        fixture.second,
        "Replaced elsewhere",
    );
    let result = exchange(&mut store, &resolved, "onto-ended", 7).unwrap();
    assert_eq!(
        status(&result, fixture.first),
        (TaskSyncTaskStatus::Applied, None)
    );
    let first = query::load_work_item(&store.connection, fixture.first).unwrap();
    assert_eq!(first.lifecycle, WorkLifecycle::Superseded);
    assert_eq!(first.superseded_by, Some(fixture.second));
    assert_eq!(exported(&store), resolved);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn a_peer_cancellation_and_supersession_apply_as_disposals_and_an_exact_repeat_changes_nothing() {
    let (mut store, fixture) = enrolled();
    let origins = durable(&store);
    let replacement = document("replacement", None);
    let replacement_id = replacement.id;
    let mut resolved = superseded(
        &cancelled(&fixture.tree, fixture.first, "Dropped elsewhere"),
        fixture.second,
        replacement_id,
        "Replaced elsewhere",
    );
    resolved = edited(&resolved, fixture.second, |task| {
        task.title = "Renamed before its supersession".into();
    });
    resolved.tasks.push(replacement);
    resolved.tasks.sort_by_key(|task| task.id.0);

    let result = exchange(&mut store, &resolved, "dispose-1", 6).unwrap();
    assert!(result.changed);
    for id in [fixture.first, fixture.second, replacement_id] {
        assert_eq!(status(&result, id), (TaskSyncTaskStatus::Applied, None));
    }
    for (id, lifecycle, replacement, reason) in [
        (
            fixture.first,
            WorkLifecycle::Cancelled,
            None,
            "Dropped elsewhere",
        ),
        (
            fixture.second,
            WorkLifecycle::Superseded,
            Some(replacement_id),
            "Replaced elsewhere",
        ),
    ] {
        let item = query::load_work_item(&store.connection, id).unwrap();
        assert_eq!(item.lifecycle, lifecycle);
        assert_eq!(item.superseded_by, replacement);
        assert_eq!(item.active_run_id, None);
        let history = events(&store, id);
        let last = history.last().unwrap();
        assert_eq!(
            last.transition,
            WorkTransition::Disposed {
                lifecycle,
                replacement_id: replacement,
                reason: reason.into(),
            }
        );
        // No execution state is fabricated for a reported outcome.
        assert!(last.run_id.is_none() && last.run.is_none() && last.claim.is_none());
        assert_eq!(last.actor.actor_id, "folder-importer");
    }
    // The planning edit lands before the disposal ends the task.
    assert!(matches!(
        events(&store, fixture.second)[0].transition,
        WorkTransition::Revised { .. }
    ));
    assert_eq!(exported(&store), resolved);
    let records = durable(&store);
    assert!(origins.iter().all(|origin| records.contains(origin)));
    assert!(store.verify_all().unwrap().is_healthy());

    // The same folder again changes nothing.
    let histories = [fixture.first, fixture.second].map(|id| events(&store, id));
    let repeat = exchange(&mut store, &resolved, "dispose-2", 8).unwrap();
    assert!(!repeat.changed && repeat.tasks.is_empty(), "{repeat:?}");
    assert_eq!(
        [fixture.first, fixture.second].map(|id| events(&store, id)),
        histories
    );
    assert_eq!(durable(&store), records);
    assert_eq!(exported(&store), resolved);
}

#[test]
fn a_supersession_round_trips_between_two_stores_with_its_replacement_and_reason() {
    let fixture = fixture();
    let mut author = SqliteStore::open_in_memory().unwrap();
    let mut peer = SqliteStore::open_in_memory().unwrap();
    for store in [&mut author, &mut peer] {
        exchange(store, &fixture.tree, "enrol", 5).unwrap();
    }
    // The author supersedes an open task natively and publishes its export.
    native_dispose(
        &mut author,
        fixture.first,
        Some(fixture.second),
        "Folded into the second child",
        6,
    );
    let published = exported(&author);
    let result = exchange(&mut peer, &published, "import-1", 9).unwrap();
    assert_eq!(
        result
            .tasks
            .iter()
            .map(|task| task.task)
            .collect::<Vec<_>>(),
        vec![fixture.first]
    );
    assert_eq!(
        status(&result, fixture.first),
        (TaskSyncTaskStatus::Applied, None)
    );
    assert_eq!(exported(&peer), published);

    let first = query::load_work_item(&peer.connection, fixture.first).unwrap();
    assert_eq!(first.lifecycle, WorkLifecycle::Superseded);
    assert_eq!(first.superseded_by, Some(fixture.second));
    let document = |tree: &TaskTree| {
        tree.tasks
            .iter()
            .find(|task| task.id == fixture.first)
            .cloned()
            .unwrap()
    };
    let carried = document(&exported(&peer));
    assert_eq!(carried, document(&published));
    assert_eq!(carried.lifecycle, WorkLifecycle::Superseded);
    assert_eq!(carried.superseded_by, Some(fixture.second));
    assert_eq!(
        carried.disposal_reason.as_deref(),
        Some("Folded into the second child")
    );
    assert!(peer.verify_all().unwrap().is_healthy());

    // The peer's export carries the same lifecycle, replacement and reason
    // back, and the author finds nothing to change in it.
    let returned = exported(&peer);
    let back = exchange(&mut author, &returned, "return-1", 10).unwrap();
    assert!(back.tasks.is_empty(), "{back:?}");
    assert_eq!(exported(&author), returned);
    assert!(author.verify_all().unwrap().is_healthy());
}

#[test]
fn live_local_execution_holds_a_disposal_and_an_expired_claim_ends_with_it() {
    let (mut store, fixture) = enrolled();
    let claim = claim_restored(&mut store, fixture.first, "worker", 6);
    let resolved = cancelled(&fixture.tree, fixture.first, "Dropped elsewhere");
    let before = state(&store);
    let held = exchange(&mut store, &resolved, "dispose-live", 7).unwrap();
    assert_eq!(
        status(&held, fixture.first),
        (TaskSyncTaskStatus::Held, Some("active_run".into()))
    );
    assert!(refusals(&held).contains(&format!("active_run (run {})", claim.run_id.0)));
    assert_eq!(state(&store), before);

    // Once the claim has lapsed the same folder applies, ending the dormant
    // run and claim without crediting them.
    let lapsed = 6 + 3600 + 1;
    let result = exchange(&mut store, &resolved, "dispose-lapsed", lapsed).unwrap();
    assert_eq!(
        status(&result, fixture.first),
        (TaskSyncTaskStatus::Applied, None)
    );
    let item = query::load_work_item(&store.connection, fixture.first).unwrap();
    assert_eq!(item.lifecycle, WorkLifecycle::Cancelled);
    assert_eq!(item.active_run_id, None);
    let last = events(&store, fixture.first).pop().unwrap();
    assert_eq!(last.run_id, Some(claim.run_id));
    assert_eq!(last.run.unwrap().state, crate::WorkRunState::Cancelled);
    assert_eq!(last.claim.unwrap().state, crate::WorkClaimState::Released);
    // As a local disposal does, the departed holder is accounted for, so the
    // open root's own completion does not wait on its contribution.
    let execution = query::active_root_execution_optional(&store.connection, fixture.parent)
        .unwrap()
        .unwrap();
    assert!(
        execution
            .waivers
            .iter()
            .any(|waiver| waiver.participant.0 == "worker"
                && waiver.waived_by == "folder-importer"
                && waiver.reason == "Dropped elsewhere"),
        "{execution:?}"
    );
    assert_eq!(
        last.root_execution
            .as_ref()
            .map(|bound| bound.root_execution_id),
        Some(execution.root_execution_id)
    );
    assert_eq!(exported(&store), resolved);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn a_lapsed_handoff_offer_expires_with_its_audit_when_a_disposal_ends_the_run() {
    let (mut store, fixture) = enrolled();
    let claim = claim_restored(&mut store, fixture.first, "worker", 6);
    let item = query::load_work_item(&store.connection, fixture.first).unwrap();
    let offer = store
        .offer_work_handoff(
            &crate::domain::OfferWorkHandoffRequest {
                work_id: fixture.first,
                run_id: claim.run_id,
                expected_work_revision: item.revision,
                from: claim.holder.clone(),
                to: SessionId("successor".into()),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                ttl_seconds: 300,
                checkpoint_summary: "handing the run on".into(),
                actor: actor("worker"),
                idempotency_key: "offer-to-successor".into(),
                offered_at: at(7),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let resolved = cancelled(&fixture.tree, fixture.first, "Dropped elsewhere");
    // Both the claim and the offer have lapsed.
    let result = exchange(&mut store, &resolved, "dispose-offered", 6 + 3600 + 1).unwrap();
    assert_eq!(
        status(&result, fixture.first),
        (TaskSyncTaskStatus::Applied, None)
    );
    let state: String = store
        .connection
        .query_row(
            "SELECT state FROM work_handoff_offers WHERE offer_id = ?1",
            [offer.offer_id.0.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "expired");
    let history = events(&store, fixture.first);
    let expired = history
        .iter()
        .position(|event| {
            matches!(&event.transition, WorkTransition::HandoffExpired { offer_id, .. }
                if *offer_id == offer.offer_id)
        })
        .expect("an audited expiry");
    assert!(matches!(
        history.last().unwrap().transition,
        WorkTransition::Disposed { .. }
    ));
    assert!(expired < history.len() - 1);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn a_lapsed_handoff_offer_expires_with_its_audit_when_a_reported_completion_ends_the_run() {
    let (mut store, fixture) = enrolled();
    let claim = claim_restored(&mut store, fixture.first, "worker", 6);
    let item = query::load_work_item(&store.connection, fixture.first).unwrap();
    let offer = store
        .offer_work_handoff(
            &crate::domain::OfferWorkHandoffRequest {
                work_id: fixture.first,
                run_id: claim.run_id,
                expected_work_revision: item.revision,
                from: claim.holder.clone(),
                to: SessionId("successor".into()),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                ttl_seconds: 300,
                checkpoint_summary: "handing the run on".into(),
                actor: actor("worker"),
                idempotency_key: "offer-to-successor".into(),
                offered_at: at(7),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let resolved = edited(&fixture.tree, fixture.first, |task| {
        task.lifecycle = WorkLifecycle::Completed;
        task.completion = Some(SharedCompletion {
            summary: "done elsewhere".into(),
            completed_at: at(3),
            actor: SharedActor::from_actor(&actor("reported-author")),
        });
    });
    let result = exchange(&mut store, &resolved, "complete-offered", 6 + 3600 + 1).unwrap();
    assert_eq!(
        status(&result, fixture.first),
        (TaskSyncTaskStatus::Applied, None)
    );
    let state: String = store
        .connection
        .query_row(
            "SELECT state FROM work_handoff_offers WHERE offer_id = ?1",
            [offer.offer_id.0.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "expired");
    let history = events(&store, fixture.first);
    let expired = history
        .iter()
        .position(|event| {
            matches!(&event.transition, WorkTransition::HandoffExpired { offer_id, .. }
                if *offer_id == offer.offer_id)
        })
        .expect("an audited expiry");
    assert!(matches!(
        history.last().unwrap().transition,
        WorkTransition::CompletionReported { .. }
    ));
    assert!(expired < history.len() - 1);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn an_addition_the_graph_refuses_does_not_hold_its_parents_disposal_as_open_descendants() {
    // The destination holds open P, and a native edge T -> P whose removal
    // the folder brings but this store holds, since T's root A is executing.
    // The folder cancels P and adds C, a required child of P depending on T:
    // P -> C -> T -> P closes in the mixed graph, so C is refused there and
    // never inserted. P is a member of that cycle too, so it stays refused,
    // for the cycle, not for C as an open descendant.
    let a = document("a", None);
    let t = document("t", Some(a.id));
    let p = document("p", None);
    let (a_id, t_id, p_id) = (a.id, t.id, p.id);
    let mut tasks = vec![a, t, p];
    tasks.sort_by_key(|task| task.id.0);
    let tree = TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: project(),
            notes_included: false,
        },
        tasks,
    };
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &tree, "enrol", 5).unwrap();
    let held = query::load_work_item(&store.connection, t_id).unwrap();
    store
        .add_work_prerequisite(
            &crate::domain::ChangeWorkPrerequisiteRequest {
                work_id: t_id,
                prerequisite_id: p_id,
                expected_revision: held.revision,
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "t-after-p".into(),
                changed_at: at(6),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let current = exported(&store);
    enrol_current(&mut store, 7);
    claim_restored(&mut store, a_id, "worker", 8);
    let mut c = document("c", Some(p_id));
    c.prerequisites = vec![t_id];
    let c_id = c.id;
    let mut resolved = cancelled(
        &edited(&current, t_id, |task| task.prerequisites.clear()),
        p_id,
        "Dropped elsewhere",
    );
    resolved.tasks.push(c);
    resolved.tasks.sort_by_key(|task| task.id.0);
    let result = exchange(&mut store, &resolved, "merged", 9).unwrap();
    assert_ne!(status(&result, t_id).0, TaskSyncTaskStatus::Applied);
    assert_ne!(status(&result, c_id).0, TaskSyncTaskStatus::Applied);
    let (p_status, p_reason) = status(&result, p_id);
    assert_eq!(p_status, TaskSyncTaskStatus::Refused, "{result:?}");
    assert_ne!(p_reason.as_deref(), Some("open_descendants"), "{result:?}");
    assert!(query::load_work_item(&store.connection, c_id).is_err());
    assert_eq!(
        query::load_work_item(&store.connection, p_id)
            .unwrap()
            .lifecycle,
        WorkLifecycle::Open
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn a_supersession_closing_a_cycle_through_retired_work_is_refused_for_that_task_alone() {
    // R, a required child of X, depends on Y. Retiring R keeps its durable
    // edges, so Y superseded by X would close Y -> X -> R -> Y.
    let x = document("x", None);
    let mut r = document("r", Some(x.id));
    let y = document("y", None);
    r.prerequisites = vec![y.id];
    let independent = document("independent", None);
    let (x_id, r_id, y_id, independent_id) = (x.id, r.id, y.id, independent.id);
    let mut tasks = vec![x, r, y, independent];
    tasks.sort_by_key(|task| task.id.0);
    let tree = TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: project(),
            notes_included: false,
        },
        tasks,
    };
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &tree, "enrol", 5).unwrap();
    let retired = without(&tree, &[r_id]);
    let result = exchange(&mut store, &retired, "retire", 6).unwrap();
    assert_eq!(status(&result, r_id), (TaskSyncTaskStatus::Applied, None));

    let resolved = edited(
        &superseded(&retired, y_id, x_id, "Folded into x"),
        independent_id,
        |task| task.title = "Renamed beside the refusal".into(),
    );
    let result = exchange(&mut store, &resolved, "cycle", 7).unwrap();
    assert_eq!(
        status(&result, y_id),
        (TaskSyncTaskStatus::Refused, Some("durable_graph".into()))
    );
    assert_eq!(
        status(&result, independent_id),
        (TaskSyncTaskStatus::Applied, None)
    );
    assert_eq!(
        query::load_work_item(&store.connection, y_id)
            .unwrap()
            .lifecycle,
        WorkLifecycle::Open
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn a_removal_beside_its_parents_cancellation_ends_first() {
    // Another host retired the first child and then cancelled the parent; a
    // host behind both reads them in one import.
    let (mut store, fixture) = enrolled();
    let resolved = cancelled(
        &cancelled(
            &without(&fixture.tree, &[fixture.first]),
            fixture.second,
            "Whole effort dropped",
        ),
        fixture.parent,
        "Whole effort dropped",
    );
    let result = exchange(&mut store, &resolved, "retire-and-cancel", 6).unwrap();
    for id in [fixture.first, fixture.second, fixture.parent] {
        assert_eq!(status(&result, id), (TaskSyncTaskStatus::Applied, None));
    }
    for id in [fixture.first, fixture.parent] {
        assert_eq!(
            query::load_work_item(&store.connection, id)
                .unwrap()
                .lifecycle,
            WorkLifecycle::Cancelled
        );
    }
    assert_eq!(exported(&store), resolved);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn an_open_descendant_below_an_ended_child_holds_the_ancestors_disposal() {
    let root = document("root", None);
    let ended = document("ended child", Some(root.id));
    let mut grandchild = document("optional grandchild", Some(ended.id));
    grandchild.child_requirement = ChildRequirement::Optional;
    let (root_id, ended_id, grandchild_id) = (root.id, ended.id, grandchild.id);
    let mut tasks = vec![root, ended, grandchild];
    tasks.sort_by_key(|task| task.id.0);
    let tree = TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: project(),
            notes_included: false,
        },
        tasks,
    };
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &tree, "enrol", 5).unwrap();
    // The child is reported complete and the root cancelled, while the
    // optional grandchild stays open.
    let resolved = cancelled(
        &edited(&tree, ended_id, |task| {
            task.lifecycle = WorkLifecycle::Completed;
            task.completion = Some(SharedCompletion {
                summary: "done elsewhere".into(),
                completed_at: at(3),
                actor: SharedActor::from_actor(&actor("reported-author")),
            });
        }),
        root_id,
        "Dropped elsewhere",
    );
    let result = exchange(&mut store, &resolved, "deep", 6).unwrap();
    assert_eq!(
        status(&result, ended_id),
        (TaskSyncTaskStatus::Applied, None)
    );
    assert!(
        refusals(&result).contains(&format!(
            "{}: open_descendants (descendant {} remains unfinished)",
            root_id.0, grandchild_id.0
        )),
        "{result:?}"
    );
    assert_eq!(
        query::load_work_item(&store.connection, root_id)
            .unwrap()
            .lifecycle,
        WorkLifecycle::Open
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn a_replacement_added_at_the_open_descendant_limit_uses_the_capacity_its_supersession_frees() {
    // A native root holds the limit of open descendants, all shared.
    let limit = crate::storage::work::MAX_OPEN_WORK_DESCENDANTS;
    let redactor = crate::DevelopmentNoopRedactor;
    let mut store = SqliteStore::open_in_memory().unwrap();
    let root_id = store
        .create_work(&root_request(PROJECT, "full-root", 5), &redactor)
        .unwrap()
        .work_id;
    let limit = usize::try_from(limit).unwrap();
    let batch_size = crate::storage::work::MAX_CHILDREN_PER_DECOMPOSITION;
    let mut replaced = None;
    for (batch, start) in (0..limit).step_by(batch_size).enumerate() {
        let parent = query::load_work_item(&store.connection, root_id).unwrap();
        let decomposition = store
            .decompose_work(
                &crate::domain::DecomposeWorkRequest {
                    parent_id: root_id,
                    expected_parent_revision: parent.revision,
                    children: (start..limit.min(start + batch_size))
                        .map(|index| {
                            crate::storage::work::test_support::child(
                                &format!("child-{index}"),
                                ChildRequirement::Required,
                                &format!("child {index}"),
                            )
                        })
                        .collect(),
                    prerequisites: Vec::new(),
                    authority: WorkPlanningAuthority::Project,
                    actor: actor("planner"),
                    idempotency_key: format!("fill-{batch}"),
                    created_at: at(5),
                },
                &redactor,
            )
            .unwrap();
        replaced.get_or_insert(decomposition.children[0].work_id);
    }
    let replaced = replaced.unwrap();
    let tree = exported(&store);
    exchange(&mut store, &tree, "enrol", 6).unwrap();
    let replacement = document("replacement", Some(root_id));
    let replacement_id = replacement.id;
    let mut resolved = superseded(&tree, replaced, replacement_id, "Replaced elsewhere");
    resolved.tasks.push(replacement);
    resolved.tasks.sort_by_key(|task| task.id.0);
    let result = exchange(&mut store, &resolved, "replace-at-limit", 7).unwrap();
    for id in [replaced, replacement_id] {
        assert_eq!(status(&result, id), (TaskSyncTaskStatus::Applied, None));
    }
    assert_eq!(exported(&store), resolved);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn a_parent_disposal_waits_for_open_children_and_applies_after_them() {
    let (mut store, fixture) = enrolled();
    let before = state(&store);
    let alone = cancelled(&fixture.tree, fixture.parent, "Whole effort dropped");
    let result = exchange(&mut store, &alone, "parent-alone", 6).unwrap();
    let (status_alone, reason) = status(&result, fixture.parent);
    assert_eq!(status_alone, TaskSyncTaskStatus::Refused);
    assert_eq!(reason.as_deref(), Some("open_descendants"));
    assert_eq!(state(&store), before);

    // A child held by live execution keeps its parent with it.
    claim_restored(&mut store, fixture.second, "worker", 7);
    let whole = cancelled(
        &cancelled(&alone, fixture.first, "Whole effort dropped"),
        fixture.second,
        "Whole effort dropped",
    );
    let result = exchange(&mut store, &whole, "whole-held", 8).unwrap();
    for id in [fixture.second, fixture.parent] {
        assert_eq!(
            status(&result, id),
            (TaskSyncTaskStatus::Held, Some("active_run".into()))
        );
    }
    assert_eq!(
        status(&result, fixture.first).0,
        TaskSyncTaskStatus::Applied
    );

    // After the claim lapses, the rest of the subtree ends, children first.
    let result = exchange(&mut store, &whole, "whole", 7 + 3600 + 1).unwrap();
    for id in [fixture.parent, fixture.second] {
        assert_eq!(status(&result, id), (TaskSyncTaskStatus::Applied, None));
    }
    let order = store
        .connection
        .prepare(
            "SELECT work_id FROM work_feed_entries WHERE feed_kind = 'project'
             AND object_kind = 'work_event' ORDER BY position DESC LIMIT 2",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        order,
        vec![fixture.parent.0.to_string(), fixture.second.0.to_string()]
    );
    assert_eq!(exported(&store), whole);
    assert!(store.verify_all().unwrap().is_healthy());
}
