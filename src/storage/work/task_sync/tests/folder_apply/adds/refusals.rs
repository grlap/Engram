//! Named refusals of additions, and the bounded work admission does.

use super::super::super::super::folder_admission::{
    ELIGIBILITY_READS, ORIGIN_VALIDATIONS, REFERENCE_READS, RETAINED_VALIDATIONS,
};
use super::*;
use crate::task_sync::activity::{ActivityNotice, ActivityState};

/// The profile of a destination that shares notes and activity.
fn notes_profile() -> ExportProfile {
    ExportProfile {
        include_notes: true,
        origin_machine: Some("host-a".into()),
    }
}

fn notes_exchange(
    store: &mut SqliteStore,
    tree: &TaskTree,
    key: &str,
    second: i64,
) -> Result<TaskSyncImportResult, StoreError> {
    notes_exchange_in(store, &project(), tree, key, second)
}

fn notes_exchange_in(
    store: &mut SqliteStore,
    project: &ProjectId,
    tree: &TaskTree,
    key: &str,
    second: i64,
) -> Result<TaskSyncImportResult, StoreError> {
    let profile = notes_profile();
    let basis = store
        .export_task_sync_folder(project, &profile)
        .unwrap()
        .export_basis;
    store.apply_task_sync_folder(
        TaskSyncFolderApply {
            project,
            actor: &actor("folder-importer"),
            tree,
            export_basis: &basis,
            profile: &profile,
            complete: true,
            now: at(second),
        },
        key,
    )
}

fn notes_exported(store: &SqliteStore) -> TaskTree {
    store
        .export_task_sync_folder(&project(), &notes_profile())
        .unwrap()
        .tree
}

/// The fixture as a notes-sharing destination, enrolled.
fn notes_enrolled(edit: impl FnOnce(&mut Fixture)) -> (SqliteStore, Fixture) {
    let mut fixture = fixture();
    fixture.tree.project.notes_included = true;
    edit(&mut fixture);
    let mut store = SqliteStore::open_in_memory().unwrap();
    assert!(
        notes_exchange(&mut store, &fixture.tree, "enrol", 5)
            .unwrap()
            .changed
    );
    (store, fixture)
}

/// A started activity: its event is the start, so both ids are one.
fn notice(task: WorkId, machine: &str) -> ActivityNotice {
    let id = ObjectId::mint();
    ActivityNotice {
        activity_id: id.clone(),
        task_id: task,
        origin_machine: machine.into(),
        actor: SharedActor::from_actor(&actor("remote-worker")),
        state: ActivityState::Started,
        state_revision: 1,
        event_id: id,
    }
}

/// Enrols another project of the same store with one task, edited first.
/// Its import origin then holds whatever that task carries, outside any
/// folder of this project.
fn enrol_other_project(store: &mut SqliteStore, edit: impl FnOnce(&mut TaskDocument)) {
    let other = ProjectId("folder-apply-other".into());
    let mut task = document("held by another project", None);
    edit(&mut task);
    let tree = TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: other.clone(),
            notes_included: true,
        },
        tasks: vec![task],
    };
    assert!(
        notes_exchange_in(store, &other, &tree, "enrol-other", 4)
            .unwrap()
            .changed
    );
}

/// The same activity identity, reported for another task.
fn moved(notice: &ActivityNotice, task: WorkId) -> TaskDocument {
    let mut document = document("reuses an activity", None);
    document.id = task;
    let mut notice = notice.clone();
    notice.task_id = task;
    document.activity = std::iter::once((
        notice.origin_machine.clone(),
        std::iter::once((notice.activity_id.clone(), notice)).collect(),
    ))
    .collect();
    document
}

fn with_short_ref_of(id: WorkId, title: &str) -> TaskDocument {
    let mut hex = id.0.simple().to_string();
    let first = if hex.starts_with('0') { "1" } else { "0" };
    hex.replace_range(0..1, first);
    let mut task = document(title, None);
    task.id = WorkId(uuid::Uuid::parse_str(&hex).unwrap());
    task
}

#[test]
fn an_activity_identity_an_import_origin_holds_collides() {
    // The identity is held by another project's import origin, outside this
    // folder, whose own graph rules would refuse a duplicate within it.
    let (mut store, fixture) = notes_enrolled(|_| {});
    let held = std::cell::RefCell::new(None);
    enrol_other_project(&mut store, |task| {
        let notice = notice(task.id, "laptop");
        task.activity = std::iter::once((
            "laptop".to_owned(),
            std::iter::once((notice.activity_id.clone(), notice.clone())).collect(),
        ))
        .collect();
        *held.borrow_mut() = Some(notice);
    });
    let held = held.into_inner().unwrap();
    let reuse = moved(&held, WorkId(uuid::Uuid::new_v4()));
    let reuse_id = reuse.id;
    let resolved = with(&fixture.tree, vec![reuse]);
    let before = state(&store);
    let message = conflict(&notes_exchange(&mut store, &resolved, "add-1", 6).unwrap_err());
    assert!(
        message.contains(&format!(
            "{}: id_collision (activity laptop/{} is already held)",
            reuse_id.0,
            held.activity_id.as_str()
        )),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn an_activity_identity_of_a_host_local_task_collides() {
    // A native task claimed here reports activity under this host's alias;
    // the folder omits it, so it is held only by the current planning.
    let (mut store, fixture) = notes_enrolled(|_| {});
    let native = native_root(&mut store, PROJECT, "native-active", 6);
    claim_restored(&mut store, native.work_id, "worker", 7);
    let current = notes_exported(&store);
    let held = current
        .tasks
        .iter()
        .find(|task| task.id == native.work_id)
        .and_then(|task| task.activity.get("host-a"))
        .and_then(|entries| entries.values().next())
        .expect("the claim reports activity")
        .clone();
    let reuse = moved(&held, WorkId(uuid::Uuid::new_v4()));
    let reuse_id = reuse.id;
    let resolved = with(&fixture.tree, vec![reuse]);
    let before = state(&store);
    let message = conflict(&notes_exchange(&mut store, &resolved, "add-1", 8).unwrap_err());
    assert!(
        message.contains(&format!(
            "{}: id_collision (activity host-a/{} is already held)",
            reuse_id.0,
            held.activity_id.as_str()
        )),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

/// Claims a native root of another project on this host and returns the
/// activity that project's export reports for it under `alias`. Neither the
/// importing project's planning nor any import origin holds it; its id is
/// the id of the event that recorded the claim, a record of this store.
fn native_activity_elsewhere(store: &mut SqliteStore, alias: &str) -> ActivityNotice {
    let other = ProjectId("folder-apply-native-elsewhere".into());
    let native = native_root(store, &other.0, "native-elsewhere", 6);
    claim_restored(store, native.work_id, "worker", 7);
    let profile = ExportProfile {
        include_notes: true,
        origin_machine: Some(alias.into()),
    };
    store
        .export_task_sync_folder(&other, &profile)
        .unwrap()
        .tree
        .tasks
        .iter()
        .find(|task| task.id == native.work_id)
        .and_then(|task| task.activity.get(alias))
        .and_then(|entries| entries.values().next())
        .expect("the claim reports activity")
        .clone()
}

fn assert_activity_collides(message: &str, reuse: WorkId, held: &ActivityNotice) {
    assert!(
        message.contains(&format!(
            "{}: id_collision (activity {}/{} is already held)",
            reuse.0,
            held.origin_machine,
            held.activity_id.as_str()
        )),
        "{message}"
    );
}

#[test]
fn an_activity_identity_of_a_native_task_in_another_project_collides() {
    let (mut store, fixture) = notes_enrolled(|_| {});
    let held = native_activity_elsewhere(&mut store, "host-a");
    let reuse = moved(&held, WorkId(uuid::Uuid::new_v4()));
    let reuse_id = reuse.id;
    let resolved = with(&fixture.tree, vec![reuse]);
    let before = state(&store);
    let message = conflict(&notes_exchange(&mut store, &resolved, "add-1", 8).unwrap_err());
    assert_activity_collides(&message, reuse_id, &held);
    assert_eq!(state(&store), before);
}

#[test]
fn a_native_activity_reported_under_another_alias_collides() {
    // The other project exported under "laptop"; this destination imports
    // under "host-a", so neither alias names the other.
    let (mut store, fixture) = notes_enrolled(|_| {});
    let held = native_activity_elsewhere(&mut store, "laptop");
    let reuse = moved(&held, WorkId(uuid::Uuid::new_v4()));
    let reuse_id = reuse.id;
    let resolved = with(&fixture.tree, vec![reuse]);
    let before = state(&store);
    let message = conflict(&notes_exchange(&mut store, &resolved, "add-1", 8).unwrap_err());
    assert_activity_collides(&message, reuse_id, &held);
    assert_eq!(state(&store), before);
}

#[test]
fn a_native_activity_collides_on_a_destination_without_an_alias() {
    // This destination shares neither notes nor an alias.
    let (mut store, fixture) = enrolled();
    let held = native_activity_elsewhere(&mut store, "laptop");
    let reuse = moved(&held, WorkId(uuid::Uuid::new_v4()));
    let reuse_id = reuse.id;
    let resolved = with(&fixture.tree, vec![reuse]);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 8).unwrap_err());
    assert_activity_collides(&message, reuse_id, &held);
    assert_eq!(state(&store), before);
}

#[test]
fn a_note_id_an_import_origin_holds_collides() {
    let note = SharedNote {
        id: ObjectId::mint(),
        kind: SharedNoteKind::Generic,
        body: "Found the cause".into(),
        author: SharedActor::from_actor(&actor("remote-author")),
        recorded_at: at(2),
        origin_machine: "laptop".into(),
    };
    let held = note.clone();
    let (mut store, fixture) = notes_enrolled(|_| {});
    // Held by another project's import origin, outside this folder.
    enrol_other_project(&mut store, |task| task.notes = vec![note]);
    let mut reuse = document("reuses a note", None);
    reuse.notes = vec![held.clone()];
    let reuse_id = reuse.id;
    let resolved = with(&fixture.tree, vec![reuse]);
    let before = state(&store);
    let message = conflict(&notes_exchange(&mut store, &resolved, "add-1", 6).unwrap_err());
    assert!(
        message.contains(&format!(
            "{}: id_collision (note {} is already held)",
            reuse_id.0,
            held.id.as_str()
        )),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn a_short_ref_this_project_holds_collides() {
    // A host-local task the folder omits holds the short ref; a duplicate
    // within the folder itself is refused earlier, as an invalid graph.
    let (mut store, fixture) = enrolled();
    let local = native_root(&mut store, PROJECT, "host-local", 6);
    let twin = with_short_ref_of(local.work_id, "same short ref here");
    let twin_id = twin.id;
    let resolved = with(&fixture.tree, vec![twin]);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 7).unwrap_err());
    assert!(
        message.contains(&format!(
            "{}: id_collision (short ref {} is already held in this project)",
            twin_id.0, local.short_ref
        )),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn a_claimed_grandparent_refuses_an_addition() {
    let (mut store, fixture) = enrolled();
    let claim = claim_restored(&mut store, fixture.parent, "worker", 6);
    let grandchild = document("added grandchild", Some(fixture.second));
    let grandchild_id = grandchild.id;
    let resolved = with(&fixture.tree, vec![grandchild]);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 7).unwrap_err());
    assert!(
        message.contains(&format!(
            "{}: ancestor_binding (ancestor {}: active_run (run {}))",
            grandchild_id.0, fixture.parent.0, claim.run_id.0
        )),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn a_held_parent_with_planning_history_refuses_an_addition() {
    let (mut store, fixture) = enrolled();
    let redactor = crate::DevelopmentNoopRedactor;
    for (step, add) in [(6, true), (7, false)] {
        let held = item(&store, fixture.second);
        let request = crate::domain::ChangeWorkPrerequisiteRequest {
            work_id: fixture.second,
            prerequisite_id: fixture.first,
            expected_revision: held.revision,
            authority: WorkPlanningAuthority::Project,
            actor: actor("planner"),
            idempotency_key: format!("edge-{step}"),
            changed_at: at(step),
        };
        if add {
            store.add_work_prerequisite(&request, &redactor).unwrap();
        } else {
            store.remove_work_prerequisite(&request, &redactor).unwrap();
        }
    }
    let child = document("added child", Some(fixture.second));
    let child_id = child.id;
    let resolved = with(&exported(&store), vec![child]);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 8).unwrap_err());
    assert!(
        message.contains(&format!(
            "{}: ancestor_binding (ancestor {}: planning_history (event ",
            child_id.0, fixture.second.0
        )),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn an_added_ancestor_that_is_not_open_refuses_its_unfinished_child() {
    let (mut store, fixture) = enrolled();
    let parent = completed_document("added completed parent", None);
    let child = document("open under it", Some(parent.id));
    let (parent_id, child_id) = (parent.id, child.id);
    let resolved = with(&fixture.tree, vec![parent, child]);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 6).unwrap_err());
    assert!(
        message.contains(&format!(
            "{}: ancestor_binding (ancestor {} is Completed)",
            child_id.0, parent_id.0
        )),
        "{message}"
    );
    assert!(!message.contains(&format!("{}:", parent_id.0)), "{message}");
    assert_eq!(state(&store), before);
}

#[test]
fn additions_beneath_a_bound_held_ancestor_refuse_together() {
    let (mut store, fixture) = enrolled();
    let claim = claim_restored(&mut store, fixture.parent, "worker", 6);
    let middle = document("added middle", Some(fixture.second));
    let leaf = document("added leaf", Some(middle.id));
    let (middle_id, leaf_id) = (middle.id, leaf.id);
    let resolved = with(&fixture.tree, vec![middle, leaf]);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 7).unwrap_err());
    for id in [middle_id, leaf_id] {
        assert!(
            message.contains(&format!(
                "{}: ancestor_binding (ancestor {}: active_run (run {}))",
                id.0, fixture.parent.0, claim.run_id.0
            )),
            "{message}"
        );
    }
    assert_eq!(state(&store), before);
}

#[test]
fn an_addition_beneath_a_refused_addition_names_that_ancestor() {
    let (mut store, fixture) = enrolled();
    // The added parent collides with a host-local task's short ref; its
    // added child is otherwise admissible.
    let local = native_root(&mut store, PROJECT, "host-local", 6);
    let parent = with_short_ref_of(local.work_id, "colliding parent");
    let child = document("child of the colliding parent", Some(parent.id));
    let (parent_id, child_id) = (parent.id, child.id);
    let resolved = with(&fixture.tree, vec![parent, child]);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 7).unwrap_err());
    assert!(
        message.contains(&format!("{}: id_collision (short ref ", parent_id.0)),
        "{message}"
    );
    assert!(
        message.contains(&format!(
            "{}: ancestor_binding (added ancestor {} is refused)",
            child_id.0, parent_id.0
        )),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn a_blocked_held_ancestor_takes_an_addition() {
    // A blocker is planning, not execution: it does not bind the ancestor.
    let mut fixture = fixture();
    let parent = fixture.parent;
    fixture
        .tree
        .tasks
        .iter_mut()
        .find(|task| task.id == parent)
        .unwrap()
        .blockers = vec![SharedBlocker {
        blocker_id: "waiting-on-review".into(),
        kind: WorkBlockerKind::Manual,
        detail: "Waiting on review".into(),
        created_by: SharedActor::from_actor(&actor("remote-author")),
        created_at: at(2),
    }];
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &fixture.tree, "enrol", 5).unwrap();
    let child = document("added under a blocked parent", Some(parent));
    let child_id = child.id;
    let resolved = with(&fixture.tree, vec![child]);
    assert!(exchange(&mut store, &resolved, "add-1", 6).unwrap().changed);
    assert_eq!(item(&store, child_id).root_id, parent);
    assert_eq!(exported(&store), resolved);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn a_finished_addition_under_a_completion_sealed_here_refuses() {
    // The second child is claimed and completed on this host, so its seal
    // accounts for exactly the children it had.
    let (mut store, fixture) = enrolled();
    let claim = claim_restored(&mut store, fixture.second, "worker", 6);
    let work = item(&store, fixture.second);
    let proof = crate::storage::work::test_support::evidence(
        &mut store, &work, &claim, "worker", "proof", 7,
    );
    crate::storage::work::test_support::checkpoint(
        &mut store,
        &work,
        &claim,
        "worker",
        "checkpoint",
        8,
        std::slice::from_ref(&proof),
    );
    let work = item(&store, fixture.second);
    crate::storage::work::test_support::complete(
        &mut store, &work, &claim, "worker", &proof, "complete", 9,
    )
    .unwrap();
    assert_eq!(
        item(&store, fixture.second).lifecycle,
        WorkLifecycle::Completed
    );

    let history = completed_document("history under a sealed child", Some(fixture.second));
    let history_id = history.id;
    let resolved = with(&exported(&store), vec![history]);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 10).unwrap_err());
    assert!(
        message.contains(&format!(
            "{}: ancestor_binding (ancestor {} was completed on this host)",
            history_id.0, fixture.second.0
        )),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn admission_reads_a_shared_ancestor_once_and_validates_the_origin_history_once() {
    let (mut store, fixture) = enrolled();
    let children = (0..40)
        .map(|index| document(&format!("added child {index}"), Some(fixture.parent)))
        .collect::<Vec<_>>();
    let resolved = with(&fixture.tree, children);
    ELIGIBILITY_READS.with(|count| count.set(0));
    ORIGIN_VALIDATIONS.with(|count| count.set(0));
    assert!(exchange(&mut store, &resolved, "add-1", 6).unwrap().changed);
    // Forty additions share one held ancestor; each graph check validates
    // its base alone, then once with every addition.
    assert_eq!(ELIGIBILITY_READS.with(std::cell::Cell::get), 1);
    assert_eq!(ORIGIN_VALIDATIONS.with(std::cell::Cell::get), 2);
}

/// A destination whose origin history holds q -> p while its current
/// planning does not, so an added child of p that requires q closes a cycle
/// in that history.
fn diverged_origin() -> (SqliteStore, WorkId, WorkId) {
    let p = document("p", None);
    let mut q = document("q", None);
    q.prerequisites = vec![p.id];
    let (p_id, q_id) = (p.id, q.id);
    let mut tasks = vec![p, q];
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
    let held = item(&store, q_id);
    store
        .remove_work_prerequisite(
            &crate::domain::ChangeWorkPrerequisiteRequest {
                work_id: q_id,
                prerequisite_id: p_id,
                expected_revision: held.revision,
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "drop-edge".into(),
                changed_at: at(6),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    (store, p_id, q_id)
}

fn cycle_closer(p: WorkId, q: WorkId, index: usize) -> TaskDocument {
    let mut x = document(&format!("x {index}"), Some(p));
    x.prerequisites = vec![q];
    x
}

fn origin_blamed(message: &str) -> usize {
    message.matches(": origin_graph (").count()
}

fn reset_counters() {
    ORIGIN_VALIDATIONS.with(|count| count.set(0));
    RETAINED_VALIDATIONS.with(|count| count.set(0));
}

fn validations() -> (usize, usize) {
    (
        RETAINED_VALIDATIONS.with(std::cell::Cell::get),
        ORIGIN_VALIDATIONS.with(std::cell::Cell::get),
    )
}

#[test]
fn origin_history_blame_names_only_the_culprit_beside_an_innocent() {
    let (mut store, p, q) = diverged_origin();
    let culprit = cycle_closer(p, q, 0);
    let innocent = document("an unrelated root", None);
    let (culprit_id, innocent_id) = (culprit.id, innocent.id);
    let resolved = with(&exported(&store), vec![culprit, innocent]);
    reset_counters();
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 7).unwrap_err());
    assert!(
        message.contains(&format!("{}: origin_graph (", culprit_id.0)),
        "{message}"
    );
    assert!(!message.contains(&innocent_id.0.to_string()), "{message}");
    assert!(!message.contains("folder:"), "{message}");
    // The origin history alone, with every addition, then without the
    // named culprit.
    assert_eq!(validations().1, 3);
}

#[test]
fn origin_history_blame_names_two_culprits_and_spares_an_innocent() {
    let (mut store, p, q) = diverged_origin();
    let first = cycle_closer(p, q, 0);
    let second = cycle_closer(p, q, 1);
    let innocent = document("an unrelated root", None);
    let (first_id, second_id, innocent_id) = (first.id, second.id, innocent.id);
    let resolved = with(&exported(&store), vec![first, second, innocent]);
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 7).unwrap_err());
    for id in [first_id, second_id] {
        assert!(
            message.contains(&format!("{}: origin_graph (", id.0)),
            "{message}"
        );
    }
    assert!(!message.contains(&innocent_id.0.to_string()), "{message}");
    assert_eq!(origin_blamed(&message), 2, "{message}");
}

#[test]
fn origin_history_blame_names_nineteen_culprits_and_spares_an_innocent() {
    let (mut store, p, q) = diverged_origin();
    let mut additions = (0..19)
        .map(|index| cycle_closer(p, q, index))
        .collect::<Vec<_>>();
    let innocent = document("an unrelated root", None);
    let innocent_id = innocent.id;
    additions.push(innocent);
    let resolved = with(&exported(&store), additions);
    let before = state(&store);
    reset_counters();
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 7).unwrap_err());
    assert_eq!(origin_blamed(&message), 19, "{message}");
    assert!(!message.contains(&innocent_id.0.to_string()), "{message}");
    assert!(validations().1 <= 3, "{:?}", validations());
    assert_eq!(state(&store), before);
}

#[test]
fn origin_history_blame_finds_one_culprit_among_many_innocents() {
    let (mut store, p, q) = diverged_origin();
    let culprit = cycle_closer(p, q, 0);
    let culprit_id = culprit.id;
    let mut additions = (0..NAMED_CONFLICT_LIMIT + 5)
        .map(|index| document(&format!("innocent {index}"), None))
        .collect::<Vec<_>>();
    additions.push(culprit);
    let resolved = with(&exported(&store), additions);
    let before = state(&store);
    reset_counters();
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 7).unwrap_err());
    assert_eq!(origin_blamed(&message), 1, "{message}");
    assert!(
        message.contains(&format!("{}: origin_graph (", culprit_id.0)),
        "{message}"
    );
    assert!(validations().1 <= 3, "{:?}", validations());
    assert_eq!(state(&store), before);
}

#[test]
fn origin_history_blame_names_every_culprit_in_one_validation() {
    let (mut store, p, q) = diverged_origin();
    let additions = (0..NAMED_CONFLICT_LIMIT + 5)
        .map(|index| cycle_closer(p, q, index))
        .collect::<Vec<_>>();
    let resolved = with(&exported(&store), additions);
    let before = state(&store);
    reset_counters();
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 7).unwrap_err());
    // The history alone, then once with every addition: all are on the cycle.
    assert_eq!(validations().1, 2);
    assert_eq!(origin_blamed(&message), NAMED_CONFLICT_LIMIT, "{message}");
    assert!(message.ends_with("; and 5 more refused tasks"), "{message}");
    assert_eq!(state(&store), before);
}

/// A destination holding `count` restored roots whose origin history has each
/// `(task, prerequisite)` edge, by index, while its current planning has none.
fn diverged_roots(count: usize, edges: &[(usize, usize)]) -> (SqliteStore, Vec<WorkId>) {
    let mut tasks = (0..count)
        .map(|index| document(&format!("root {index}"), None))
        .collect::<Vec<_>>();
    let ids = tasks.iter().map(|task| task.id).collect::<Vec<_>>();
    for &(task, prerequisite) in edges {
        tasks[task].prerequisites.push(ids[prerequisite]);
    }
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
    for (step, &(task, prerequisite)) in edges.iter().enumerate() {
        let held = item(&store, ids[task]);
        store
            .remove_work_prerequisite(
                &crate::domain::ChangeWorkPrerequisiteRequest {
                    work_id: ids[task],
                    prerequisite_id: ids[prerequisite],
                    expected_revision: held.revision,
                    authority: WorkPlanningAuthority::Project,
                    actor: actor("planner"),
                    idempotency_key: format!("drop-{step}"),
                    changed_at: at(6),
                },
                &crate::DevelopmentNoopRedactor,
            )
            .unwrap();
    }
    (store, ids)
}

const ALONE_ON_A_CYCLE: &str =
    "with this addition would not be a valid task graph: it closes a dependency cycle";
const TOGETHER_ON_A_CYCLE: &str = "with this and the other named additions would not be a valid task graph: together they close a dependency cycle";

fn named_for(message: &str, id: WorkId, reason: &str, detail: &str) -> bool {
    message.contains(&format!(
        "{}: {reason} (the import-origin history {detail})",
        id.0
    ))
}

#[test]
fn origin_history_blame_names_a_pair_that_cycles_only_together_beside_a_lone_culprit() {
    // Two independent historical edges, each dropped from current planning:
    // second requires first, and fourth requires third. The lone culprit
    // closes second -> first -> lone -> second on its own. The two halves
    // close fourth -> third -> half -> fifth -> other half -> fourth only
    // together, through the held fifth root, and neither references the
    // other. Unrelated roots pad the folder.
    let (mut store, roots) = diverged_roots(5, &[(1, 0), (3, 2)]);
    let lone = cycle_closer(roots[0], roots[1], 0);
    let mut first_half = document("first half", Some(roots[2]));
    first_half.prerequisites = vec![roots[4]];
    let mut second_half = document("second half", Some(roots[4]));
    second_half.prerequisites = vec![roots[3]];
    let (lone_id, halves) = (lone.id, [first_half.id, second_half.id]);
    let mut additions = vec![lone, first_half, second_half];
    let padding = (0..NAMED_CONFLICT_LIMIT)
        .map(|index| document(&format!("innocent {index}"), None))
        .collect::<Vec<_>>();
    let innocents = padding.iter().map(|task| task.id).collect::<Vec<_>>();
    additions.extend(padding);
    let resolved = with(&exported(&store), additions);
    let before = state(&store);
    reset_counters();
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 7).unwrap_err());
    assert!(
        named_for(&message, lone_id, "origin_graph", ALONE_ON_A_CYCLE),
        "{message}"
    );
    for id in halves {
        assert!(
            named_for(&message, id, "origin_graph", TOGETHER_ON_A_CYCLE),
            "{message}"
        );
    }
    for id in innocents {
        assert!(!message.contains(&id.0.to_string()), "{message}");
    }
    assert!(validations().1 <= 3, "{:?}", validations());
    assert_eq!(state(&store), before);
}

#[test]
fn origin_history_blame_spares_an_addition_that_only_bridges_two_cycles() {
    // The origin history holds second -> first and fourth -> third, both
    // dropped from current planning. One addition under the first root
    // closes first -> it -> second -> first, another under the third closes
    // third -> it -> fourth -> third, and a bridge under the first root
    // requires the third: first -> bridge -> third lies between the cycles,
    // on neither, and the history with the bridge alone is valid.
    let (mut store, roots) = diverged_roots(4, &[(1, 0), (3, 2)]);
    let near = cycle_closer(roots[0], roots[1], 0);
    let far = cycle_closer(roots[2], roots[3], 1);
    let mut bridge = document("bridge", Some(roots[0]));
    bridge.prerequisites = vec![roots[2]];
    let (near_id, far_id, bridge_id) = (near.id, far.id, bridge.id);
    let resolved = with(&exported(&store), vec![near, far, bridge]);
    let before = state(&store);
    reset_counters();
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 7).unwrap_err());
    for id in [near_id, far_id] {
        assert!(
            named_for(&message, id, "origin_graph", ALONE_ON_A_CYCLE),
            "{message}"
        );
    }
    assert!(!message.contains(&bridge_id.0.to_string()), "{message}");
    assert!(!message.contains("folder:"), "{message}");
    // The history alone, with every addition, then with the bridge alone.
    assert_eq!(validations().1, 3);
    assert_eq!(state(&store), before);
}

#[test]
fn origin_history_blame_names_many_culprits_and_spares_an_innocent() {
    let (mut store, p, q) = diverged_origin();
    // Many culprits under one root, still within its open-descendant limit.
    let culprits = 200;
    let mut additions = (0..culprits)
        .map(|index| cycle_closer(p, q, index))
        .collect::<Vec<_>>();
    let innocent = document("an unrelated root", None);
    let innocent_id = innocent.id;
    additions.push(innocent);
    let resolved = with(&exported(&store), additions);
    let before = state(&store);
    reset_counters();
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 7).unwrap_err());
    assert_eq!(origin_blamed(&message), NAMED_CONFLICT_LIMIT, "{message}");
    assert!(
        message.ends_with(&format!(
            "; and {} more refused tasks",
            culprits - NAMED_CONFLICT_LIMIT
        )),
        "{message}"
    );
    assert!(!message.contains(&innocent_id.0.to_string()), "{message}");
    // The history alone, with every addition, then with the innocent alone.
    assert_eq!(validations().1, 3);
    assert_eq!(state(&store), before);
}

#[test]
fn a_dependent_of_a_graph_culprit_is_named_for_its_reference() {
    let (mut store, p, q) = diverged_origin();
    let culprit = cycle_closer(p, q, 0);
    let mut dependent = document("needs the culprit", None);
    dependent.prerequisites = vec![culprit.id];
    let (culprit_id, dependent_id) = (culprit.id, dependent.id);
    let resolved = with(&exported(&store), vec![culprit, dependent]);
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 7).unwrap_err());
    assert!(
        message.contains(&format!("{}: origin_graph (", culprit_id.0)),
        "{message}"
    );
    assert!(
        message.contains(&format!(
            "{}: refused_reference (it references added task {}, which is refused)",
            dependent_id.0, culprit_id.0
        )),
        "{message}"
    );
}

#[test]
fn a_chain_of_dependents_of_a_graph_culprit_is_named_for_its_references() {
    // culprit <- first <- second <- third, each requiring the one before.
    let (mut store, p, q) = diverged_origin();
    let culprit = cycle_closer(p, q, 0);
    let mut chain = vec![culprit];
    for index in 0..3 {
        let mut dependent = document(&format!("dependent {index}"), None);
        dependent.prerequisites = vec![chain[index].id];
        chain.push(dependent);
    }
    let ids = chain.iter().map(|task| task.id).collect::<Vec<_>>();
    let resolved = with(&exported(&store), chain);
    let before = state(&store);
    reset_counters();
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 7).unwrap_err());
    assert!(
        named_for(&message, ids[0], "origin_graph", ALONE_ON_A_CYCLE),
        "{message}"
    );
    for pair in ids.windows(2) {
        assert!(
            message.contains(&format!(
                "{}: refused_reference (it references added task {}, which is refused)",
                pair[1].0, pair[0].0
            )),
            "{message}"
        );
    }
    assert!(!message.contains("folder:"), "{message}");
    // The history alone, then once with every addition.
    assert_eq!(validations().1, 2);
    assert_eq!(state(&store), before);
}

#[test]
fn a_required_child_of_a_graph_culprit_is_named_for_its_ancestor() {
    let (mut store, p, q) = diverged_origin();
    let culprit = cycle_closer(p, q, 0);
    let child = document("child of the culprit", Some(culprit.id));
    let (culprit_id, child_id) = (culprit.id, child.id);
    let resolved = with(&exported(&store), vec![culprit, child]);
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 7).unwrap_err());
    assert!(
        named_for(&message, culprit_id, "origin_graph", ALONE_ON_A_CYCLE),
        "{message}"
    );
    assert!(
        message.contains(&format!(
            "{}: ancestor_binding (added ancestor {} is refused)",
            child_id.0, culprit_id.0
        )),
        "{message}"
    );
    assert!(!message.contains("folder:"), "{message}");
}

#[test]
fn a_dependent_of_additions_over_the_open_descendant_limit_is_named_for_its_reference() {
    // The held parent has two open children. The folder shows the second
    // cancelled, a change refused on its own, so the planning the import
    // would leave keeps it open. The folder's additions bring the root to the
    // limit in the folder; with the open second child, the planning the
    // import would leave is over it, so the added children take it over
    // together. Another root requires one of them, and its id sorts first so
    // that the refusal names it.
    let (mut store, fixture) = enrolled();
    let limit = crate::storage::work::MAX_OPEN_WORK_DESCENDANTS as usize;
    let children = (0..limit - 1)
        .map(|index| document(&format!("added child {index}"), Some(fixture.parent)))
        .collect::<Vec<_>>();
    let mut dependent = document("needs an added child", None);
    dependent.id = WorkId(uuid::Uuid::from_u128(1));
    dependent.prerequisites = vec![children[0].id];
    let (dependent_id, required) = (dependent.id, children[0].id);
    // Every addition and the cancelled child.
    let refused = children.len() + 2;
    let mut additions = children;
    additions.push(dependent);
    let cancelled = edited(&fixture.tree, fixture.second, |task| {
        task.lifecycle = WorkLifecycle::Cancelled;
        task.disposal_reason = Some("dropped elsewhere".into());
    });
    let resolved = with(&cancelled, additions);
    let before = state(&store);
    reset_counters();
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 7).unwrap_err());
    assert!(
        message.contains(&format!(
            "{}: refused_reference (it references added task {}, which is refused)",
            dependent_id.0, required.0
        )),
        "{message}"
    );
    assert!(
        message.contains(
            ": retained_graph (the destination's planning with this and the other named additions would not be a valid task graph: together they take their root over the open-descendant limit)"
        ),
        "{message}"
    );
    assert!(
        message.ends_with(&format!(
            "; and {} more refused tasks",
            refused - NAMED_CONFLICT_LIMIT
        )),
        "{message}"
    );
    assert!(!message.contains("folder:"), "{message}");
    // The planning alone, then once with every addition.
    assert_eq!(validations().0, 2);
    assert_eq!(state(&store), before);
}

#[test]
fn a_deletion_beside_a_host_local_child_does_not_blame_an_innocent_addition() {
    // The folder omits the second child, a member, whose host-local child
    // stays: the deletion refuses on its own, and the graph the import would
    // leave still holds both, so an unrelated addition is not blamed.
    let (mut store, fixture) = enrolled();
    let second = item(&store, fixture.second);
    store
        .decompose_work(
            &crate::domain::DecomposeWorkRequest {
                parent_id: fixture.second,
                expected_parent_revision: second.revision,
                children: vec![crate::storage::work::test_support::child(
                    "h",
                    ChildRequirement::Required,
                    "Host-local child",
                )],
                prerequisites: Vec::new(),
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "decompose-second".into(),
                created_at: at(6),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let innocent = document("an unrelated root", None);
    let innocent_id = innocent.id;
    let mut resolved = with(&fixture.tree, vec![innocent]);
    resolved.tasks.retain(|task| task.id != fixture.second);
    let before = state(&store);
    reset_counters();
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 7).unwrap_err());
    assert!(
        message.contains(&format!("{}: task_deleted", fixture.second.0)),
        "{message}"
    );
    assert!(!message.contains(&innocent_id.0.to_string()), "{message}");
    assert!(!message.contains("folder:"), "{message}");
    let (retained, origins) = validations();
    assert!(retained <= 2 && origins <= 2, "{:?}", validations());
    assert_eq!(state(&store), before);
}

#[test]
fn a_host_local_task_the_folder_omits_cannot_close_a_cycle_with_an_addition() {
    // Restored roots p and q. On this host p gains a native required child
    // h that requires q, outside the exchange. The folder omits h and adds a
    // required child x of q that requires p: the folder and the origin
    // history are acyclic, but the destination's planning would hold
    // p -> h -> q -> x -> p.
    let p = document("p", None);
    let q = document("q", None);
    let (p_id, q_id) = (p.id, q.id);
    let mut tasks = vec![p, q];
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
    let redactor = crate::DevelopmentNoopRedactor;
    let parent = item(&store, p_id);
    let decomposition = store
        .decompose_work(
            &crate::domain::DecomposeWorkRequest {
                parent_id: p_id,
                expected_parent_revision: parent.revision,
                children: vec![crate::storage::work::test_support::child(
                    "h",
                    ChildRequirement::Required,
                    "Host-local child",
                )],
                prerequisites: Vec::new(),
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "decompose-p".into(),
                created_at: at(6),
            },
            &redactor,
        )
        .unwrap();
    let h = decomposition.children[0].work_id;
    let held = item(&store, h);
    store
        .add_work_prerequisite(
            &crate::domain::ChangeWorkPrerequisiteRequest {
                work_id: h,
                prerequisite_id: q_id,
                expected_revision: held.revision,
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "h-needs-q".into(),
                changed_at: at(7),
            },
            &redactor,
        )
        .unwrap();
    let mut x = document("x", Some(q_id));
    x.prerequisites = vec![p_id];
    let x_id = x.id;
    let mut current = exported(&store);
    current.tasks.retain(|task| task.id != h);
    let resolved = with(&current, vec![x]);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 8).unwrap_err());
    assert!(
        message.contains(&format!("{}: retained_graph (", x_id.0)),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn admission_reads_a_shared_reference_once() {
    let (mut store, fixture) = enrolled();
    let roots = (0..40)
        .map(|index| {
            let mut root = document(&format!("needs first {index}"), None);
            root.prerequisites = vec![fixture.first];
            root
        })
        .collect::<Vec<_>>();
    let resolved = with(&fixture.tree, roots);
    REFERENCE_READS.with(|count| count.set(0));
    assert!(exchange(&mut store, &resolved, "add-1", 6).unwrap().changed);
    // Forty additions reference one held task; its origin is read once.
    assert_eq!(REFERENCE_READS.with(std::cell::Cell::get), 1);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn an_addition_requiring_a_refused_addition_is_named_for_it() {
    let (mut store, fixture) = enrolled();
    let local = native_root(&mut store, PROJECT, "host-local", 6);
    let refused = with_short_ref_of(local.work_id, "colliding root");
    let mut dependent = document("needs the colliding root", None);
    dependent.prerequisites = vec![refused.id];
    let (refused_id, dependent_id) = (refused.id, dependent.id);
    let resolved = with(&fixture.tree, vec![refused, dependent]);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 7).unwrap_err());
    assert!(
        message.contains(&format!(
            "{}: refused_reference (it references added task {}, which is refused)",
            dependent_id.0, refused_id.0
        )),
        "{message}"
    );
    assert_eq!(state(&store), before);
}
