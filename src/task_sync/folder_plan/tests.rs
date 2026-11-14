use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};

use super::{
    DeltaPlan, ProjectChange, RelationChange, ScalarChange, TaskChange, TaskDelta, TaskField, plan,
};
use crate::{
    ChildRequirement, ObjectId, ProjectId, WorkBlockerKind, WorkId, WorkItemKind, WorkLifecycle,
    domain::{
        AcceptanceBinding, AcceptanceEvaluationMode, VerificationKind, VerificationRequirement,
    },
    storage::StoreError,
    task_sync::{
        FORMAT, ProjectDocument, SharedActor, SharedBlocker, SharedCompletion, TaskDocument,
        TaskSyncRefusal, TaskTree,
        notes::{SharedNote, SharedNoteKind},
    },
};

fn at() -> DateTime<Utc> {
    "2026-10-18T00:00:00Z".parse().unwrap()
}

fn actor() -> SharedActor {
    SharedActor {
        actor_id: "greg".into(),
        actor_kind: "human".into(),
    }
}

fn task(title: &str) -> TaskDocument {
    TaskDocument {
        id: WorkId::new(),
        external_ref: None,
        title: title.into(),
        outcome: "Deliver work".into(),
        acceptance: vec!["Implemented".into()],
        acceptance_bindings: Vec::new(),
        evaluation_mode: None,
        kind: WorkItemKind::Task,
        priority: 1,
        labels: Vec::new(),
        assigned_to: None,
        deferred_until: None,
        lifecycle: WorkLifecycle::Open,
        parent_id: None,
        child_requirement: ChildRequirement::Required,
        prerequisites: Vec::new(),
        blockers: Vec::new(),
        superseded_by: None,
        disposal_reason: None,
        completion: None,
        notes: Vec::new(),
        activity: BTreeMap::default(),
    }
}

#[test]
fn selected_plan_reuses_folder_field_rules_without_deleting_omitted_context() {
    let held = task("held");
    let context = task("context");
    let removed = task("removed");
    let added = task("added");
    let mut revised = held.clone();
    revised.title = "updated held".into();
    let current_tree = tree(&[held.clone(), context.clone(), removed.clone()]);
    let resolved_tree = tree(&[revised.clone(), context.clone(), added.clone()]);
    let current = current_tree
        .tasks
        .iter()
        .map(|task| (task.id, task.clone()))
        .collect();
    let changes = BTreeMap::from([
        (held.id, Some(revised)),
        (removed.id, None),
        (added.id, Some(added.clone())),
    ]);
    let membership = std::collections::BTreeSet::from([held.id, context.id, removed.id]);
    let selected =
        super::plan_selected(&current_tree.project, &current, &changes, &membership).unwrap();
    let full = plan(
        &current_tree,
        &resolved_tree,
        &membership.iter().copied().collect::<HashSet<_>>(),
    )
    .unwrap();
    assert_eq!(selected.project, full.project);
    assert_eq!(
        selected.tasks,
        full.tasks
            .into_iter()
            .filter(|task| changes.contains_key(&task.id))
            .collect::<Vec<_>>()
    );
    assert!(!selected.tasks.iter().any(|task| task.id == context.id));
    assert_eq!(
        selected
            .tasks
            .iter()
            .find(|task| task.id == removed.id)
            .unwrap()
            .delta,
        TaskDelta::Deleted
    );
    assert_eq!(
        selected
            .tasks
            .iter()
            .find(|task| task.id == added.id)
            .unwrap()
            .delta,
        TaskDelta::Added
    );
}

#[test]
fn selected_plan_refuses_unknown_removals_and_wrong_keys_and_preserves_unsupported_fields() {
    let held = task("held");
    let current_tree = tree(std::slice::from_ref(&held));
    let current = BTreeMap::from([(held.id, held.clone())]);
    let membership = std::collections::BTreeSet::from([held.id]);
    assert!(
        super::plan_selected(
            &current_tree.project,
            &current,
            &BTreeMap::from([(held.id, None)]),
            &std::collections::BTreeSet::new()
        )
        .is_err()
    );
    assert!(
        super::plan_selected(
            &current_tree.project,
            &BTreeMap::new(),
            &BTreeMap::from([(held.id, None)]),
            &membership
        )
        .is_err()
    );
    assert!(
        super::plan_selected(
            &current_tree.project,
            &current,
            &BTreeMap::from([(WorkId::new(), Some(held.clone()))]),
            &membership
        )
        .is_err()
    );
    let mut changed = held.clone();
    changed.acceptance.push("new criterion".into());
    changed.kind = WorkItemKind::Bug;
    let selected = super::plan_selected(
        &current_tree.project,
        &current,
        &BTreeMap::from([(held.id, Some(changed))]),
        &membership,
    )
    .unwrap();
    let TaskDelta::Changed(change) = &selected.tasks[0].delta else {
        panic!("changed task")
    };
    assert_eq!(change.unsupported, vec![TaskField::Kind]);
    assert_eq!(
        change
            .scalar
            .as_ref()
            .unwrap()
            .acceptance
            .as_ref()
            .unwrap()
            .last()
            .unwrap(),
        "new criterion"
    );
    assert!(!change.is_applicable());
}

fn completed(title: &str) -> TaskDocument {
    TaskDocument {
        lifecycle: WorkLifecycle::Completed,
        completion: Some(SharedCompletion {
            summary: "Delivered".into(),
            completed_at: at(),
            actor: actor(),
        }),
        ..task(title)
    }
}

fn tree(tasks: &[TaskDocument]) -> TaskTree {
    let mut tasks = tasks.to_vec();
    tasks.sort_by_key(|task| task.id.0);
    TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: ProjectId("folder-plan".into()),
            notes_included: false,
        },
        tasks,
    }
}

fn members(tasks: &[&TaskDocument]) -> HashSet<WorkId> {
    tasks.iter().map(|task| task.id).collect()
}

fn deltas(result: Result<DeltaPlan, StoreError>) -> DeltaPlan {
    result.unwrap()
}

/// A change of only the named fields beyond the scalar ones.
fn fields(unsupported: Vec<TaskField>) -> TaskDelta {
    TaskDelta::Changed(Box::new(TaskChange {
        scalar: None,
        unsupported,
        ..TaskChange::default()
    }))
}

fn scalar(change: ScalarChange) -> TaskDelta {
    TaskDelta::Changed(Box::new(TaskChange {
        scalar: Some(change),
        unsupported: Vec::new(),
        ..TaskChange::default()
    }))
}

fn delta(plan: &DeltaPlan, id: WorkId) -> &TaskDelta {
    &plan
        .tasks
        .iter()
        .find(|task| task.id == id)
        .unwrap_or_else(|| panic!("task {id:?} is not planned"))
        .delta
}

fn refusal(result: Result<DeltaPlan, StoreError>) -> TaskSyncRefusal {
    match result {
        Err(StoreError::TaskSyncRefused { refusal, .. }) => refusal,
        other => panic!("expected a task sync refusal, got {other:?}"),
    }
}

fn edited(held: &TaskDocument, edit: impl FnOnce(&mut TaskDocument)) -> TaskDocument {
    let mut task = held.clone();
    edit(&mut task);
    task
}

#[test]
fn identical_trees_plan_no_change() {
    let a = task("a");
    let b = completed("b");
    let current = tree(&[a.clone(), b.clone()]);
    let plan = deltas(plan(&current, &current.clone(), &members(&[&a, &b])));
    assert!(plan.is_unchanged());
    assert!(plan.is_applicable());
    assert_eq!(plan.tasks.len(), 2);
    assert_eq!(delta(&plan, a.id), &TaskDelta::Unchanged);
    // An unchanged completed task plans nothing to apply.
    assert_eq!(delta(&plan, b.id), &TaskDelta::Unchanged);
    assert_eq!(plan.scalar_changes().count(), 0);
}

#[test]
fn an_empty_enrolled_destination_plans_every_task_as_an_addition() {
    // An enrolled destination is compared task by task even when it holds
    // nothing, so the folder's tasks are additions, never a bootstrap.
    let a = task("a");
    let b = task("b");
    let resolved = tree(&[a.clone(), b.clone()]);
    let plan = deltas(plan(&tree(&[]), &resolved, &HashSet::new()));
    assert_eq!(delta(&plan, a.id), &TaskDelta::Added);
    assert_eq!(delta(&plan, b.id), &TaskDelta::Added);
    assert!(plan.is_applicable());
    let mut expected = vec![a.id, b.id];
    expected.sort_by_key(|id| id.0);
    assert_eq!(plan.additions().collect::<Vec<_>>(), expected);
}

#[test]
fn empty_destination_and_empty_folder_change_nothing() {
    let plan = deltas(plan(&tree(&[]), &tree(&[]), &HashSet::new()));
    assert_eq!(plan, DeltaPlan::default());
    assert!(plan.is_unchanged());
    assert!(plan.is_applicable());
}

#[test]
fn title_change_is_a_scalar_edit() {
    let a = task("a");
    let resolved = tree(&[edited(&a, |task| task.title = "Renamed".into())]);
    let plan = deltas(plan(
        &tree(std::slice::from_ref(&a)),
        &resolved,
        &members(&[&a]),
    ));
    let expected = ScalarChange {
        title: Some("Renamed".into()),
        outcome: None,
        priority: None,
        ..Default::default()
    };
    assert_eq!(delta(&plan, a.id), &scalar(expected.clone()));
    assert!(plan.is_applicable());
    assert!(!plan.is_unchanged());
    assert_eq!(
        plan.scalar_changes().collect::<Vec<_>>(),
        [(a.id, &expected)]
    );
    let patch = expected.patch();
    assert_eq!(patch.title.as_deref(), Some("Renamed"));
    assert_eq!((patch.outcome.as_deref(), patch.priority), (None, None));
    assert_eq!(
        patch,
        crate::domain::WorkRevisionPatch {
            title: Some("Renamed".into()),
            ..crate::domain::WorkRevisionPatch::default()
        }
    );
}

#[test]
fn outcome_and_priority_change_together() {
    let a = task("a");
    let resolved = tree(&[edited(&a, |task| {
        task.outcome = "Deliver more".into();
        task.priority = 3;
    })]);
    let plan = deltas(plan(
        &tree(std::slice::from_ref(&a)),
        &resolved,
        &members(&[&a]),
    ));
    assert_eq!(
        delta(&plan, a.id),
        &scalar(ScalarChange {
            title: None,
            outcome: Some("Deliver more".into()),
            priority: Some(3),
            ..Default::default()
        })
    );
}

#[test]
fn a_scalar_edit_of_a_completed_task_is_left_to_admission() {
    // The planner reports what is asked; refusing a completed task is
    // admission's decision, made with the store's own records.
    let b = completed("b");
    let resolved = tree(&[edited(&b, |task| task.title = "Renamed".into())]);
    let plan = deltas(plan(
        &tree(std::slice::from_ref(&b)),
        &resolved,
        &members(&[&b]),
    ));
    assert!(matches!(
        delta(&plan, b.id),
        TaskDelta::Changed(change) if change.scalar.is_some()
    ));
}

#[test]
fn planning_edits_keep_acceptance_bindings_labels_and_assignment() {
    let original = task("shared work");
    let mut incoming = original.clone();
    incoming.acceptance = vec!["Checked on the implementing host".into()];
    incoming.acceptance_bindings = vec![AcceptanceBinding {
        criterion: 1,
        requirement: VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: None,
        },
    }];
    incoming.labels = vec!["ready".into()];
    incoming.assigned_to = Some("implementer".into());
    let classified = deltas(plan(
        &tree(std::slice::from_ref(&original)),
        &tree(std::slice::from_ref(&incoming)),
        &members(&[&original]),
    ));
    assert!(classified.is_applicable());
    let (_, change) = classified.scalar_changes().next().unwrap();
    let patch = change.patch();
    assert_eq!(patch.acceptance.as_ref(), Some(&incoming.acceptance));
    assert_eq!(
        patch.acceptance_bindings.as_ref(),
        Some(&incoming.acceptance_bindings)
    );
    assert_eq!(patch.labels.as_ref(), Some(&incoming.labels));
    assert_eq!(patch.assigned_to, incoming.assigned_to);
    assert!(!patch.clear_assignment);

    let mut cleared = incoming.clone();
    cleared.assigned_to = None;
    let classified = deltas(plan(
        &tree(std::slice::from_ref(&incoming)),
        &tree(&[cleared]),
        &members(&[&incoming]),
    ));
    let (_, change) = classified.scalar_changes().next().unwrap();
    let patch = change.patch();
    assert!(patch.clear_assignment);
    assert_eq!(patch.assigned_to, None);
    assert_eq!(patch.acceptance, None);
}

#[test]
fn every_other_field_change_is_unsupported_and_named() {
    type Edit = fn(&mut TaskDocument);
    let cases: [(TaskField, Edit); 6] = [
        (TaskField::ExternalRef, |task| {
            task.external_ref = Some("tracker#12".into());
        }),
        (TaskField::EvaluationMode, |task| {
            task.evaluation_mode = Some(AcceptanceEvaluationMode::SameSession);
        }),
        (TaskField::Kind, |task| task.kind = WorkItemKind::Bug),
        (TaskField::DeferredUntil, |task| {
            task.deferred_until = Some(at());
        }),
        (TaskField::Lifecycle, |task| {
            task.lifecycle = WorkLifecycle::Proposed;
        }),
        (TaskField::ChildRequirement, |task| {
            task.child_requirement = ChildRequirement::Optional;
        }),
    ];
    for (field, edit) in cases {
        let a = task("a");
        let resolved = tree(&[edited(&a, edit)]);
        let plan = deltas(plan(
            &tree(std::slice::from_ref(&a)),
            &resolved,
            &members(&[&a]),
        ));
        assert_eq!(
            delta(&plan, a.id),
            &fields(vec![field]),
            "{}",
            field.as_str()
        );
        assert!(!plan.is_applicable(), "{}", field.as_str());
    }
}

#[test]
fn reparenting_stays_unsupported_while_prerequisites_are_planned() {
    let a = task("a");
    let b = task("b");
    let current = tree(&[a.clone(), b.clone()]);
    let membership = members(&[&a, &b]);
    let reparented = tree(&[a.clone(), edited(&b, |task| task.parent_id = Some(a.id))]);
    let plan_reparent = deltas(plan(&current, &reparented, &membership));
    assert_eq!(
        delta(&plan_reparent, b.id),
        &fields(vec![TaskField::ParentId])
    );
    assert_eq!(delta(&plan_reparent, a.id), &TaskDelta::Unchanged);
    let ordered = tree(&[
        a.clone(),
        edited(&b, |task| task.prerequisites = vec![a.id]),
    ]);
    let plan_ordered = deltas(plan(&current, &ordered, &membership));
    assert_eq!(
        delta(&plan_ordered, b.id),
        &TaskDelta::Changed(Box::new(TaskChange {
            relations: RelationChange {
                prerequisite_additions: vec![a.id],
                ..RelationChange::default()
            },
            ..TaskChange::default()
        }))
    );
}

#[test]
fn disposal_names_changed_fields_and_completion_is_a_reported_outcome() {
    let a = task("a");
    let b = task("b");
    let current = tree(&[a.clone(), b.clone()]);
    let membership = members(&[&a, &b]);
    let cases = [
        (
            edited(&a, |task| {
                task.lifecycle = WorkLifecycle::Cancelled;
                task.disposal_reason = Some("Dropped".into());
            }),
            vec![TaskField::Lifecycle, TaskField::DisposalReason],
        ),
        (
            edited(&a, |task| {
                task.lifecycle = WorkLifecycle::Superseded;
                task.superseded_by = Some(b.id);
                task.disposal_reason = Some("Replaced".into());
            }),
            vec![
                TaskField::Lifecycle,
                TaskField::SupersededBy,
                TaskField::DisposalReason,
            ],
        ),
    ];
    for (changed, fields) in cases {
        let resolved = tree(&[changed, b.clone()]);
        let plan = deltas(plan(&current, &resolved, &membership));
        assert_eq!(delta(&plan, a.id), &self::fields(fields));
    }
    let resolved = tree(&[
        edited(&a, |task| {
            task.lifecycle = WorkLifecycle::Completed;
            task.completion = completed("a").completion;
        }),
        b,
    ]);
    let reported = deltas(plan(&current, &resolved, &membership));
    assert_eq!(
        delta(&reported, a.id),
        &TaskDelta::Changed(Box::new(TaskChange {
            reported_completion: true,
            ..TaskChange::default()
        }))
    );
    assert!(reported.is_applicable());
}

#[test]
fn a_note_change_is_unsupported() {
    let a = task("a");
    let mut current = tree(std::slice::from_ref(&a));
    current.project.notes_included = true;
    let mut resolved = tree(&[edited(&a, |task| {
        task.notes = vec![SharedNote {
            id: ObjectId::mint(),
            kind: SharedNoteKind::Generic,
            body: "Found the cause".into(),
            author: actor(),
            recorded_at: at(),
            origin_machine: "laptop".into(),
        }];
    })]);
    resolved.project.notes_included = true;
    let plan = deltas(plan(&current, &resolved, &members(&[&a])));
    assert_eq!(plan.project, Vec::new());
    assert_eq!(delta(&plan, a.id), &fields(vec![TaskField::Notes]));
}

#[test]
fn scalar_and_other_changes_report_only_the_unsupported_fields() {
    let a = task("a");
    let resolved = tree(&[edited(&a, |task| {
        task.title = "Renamed".into();
        task.kind = WorkItemKind::Bug;
    })]);
    let plan = deltas(plan(
        &tree(std::slice::from_ref(&a)),
        &resolved,
        &members(&[&a]),
    ));
    // Both parts are kept on the one change; the unsupported part makes it
    // inapplicable, so no scalar edit is offered.
    assert_eq!(
        delta(&plan, a.id),
        &TaskDelta::Changed(Box::new(TaskChange {
            scalar: Some(ScalarChange {
                title: Some("Renamed".into()),
                outcome: None,
                priority: None,
                ..Default::default()
            }),
            unsupported: vec![TaskField::Kind],
            ..TaskChange::default()
        }))
    );
    assert_eq!(plan.scalar_changes().count(), 0);
    assert!(!plan.is_applicable());
}

#[test]
fn an_omitted_host_local_task_is_retained_not_deleted() {
    let a = task("a");
    let local = task("local");
    let current = tree(&[a.clone(), local.clone()]);
    let plan = deltas(plan(
        &current,
        &tree(std::slice::from_ref(&a)),
        &members(&[&a]),
    ));
    assert_eq!(delta(&plan, local.id), &TaskDelta::RetainedLocal);
    assert_eq!(delta(&plan, a.id), &TaskDelta::Unchanged);
    assert!(plan.is_unchanged());
    assert!(plan.is_applicable());
}

#[test]
fn a_scalar_edit_beside_a_retained_host_local_task_applies() {
    let a = task("a");
    let local = task("local");
    let current = tree(&[a.clone(), local.clone()]);
    let resolved = tree(&[edited(&a, |task| task.priority = 0)]);
    let plan = deltas(plan(&current, &resolved, &members(&[&a])));
    assert!(plan.is_applicable());
    assert_eq!(delta(&plan, local.id), &TaskDelta::RetainedLocal);
    assert_eq!(
        plan.scalar_changes().map(|(id, _)| id).collect::<Vec<_>>(),
        [a.id]
    );
}

#[test]
fn an_omitted_exchanged_task_is_a_deletion() {
    let a = task("a");
    let d = task("d");
    let current = tree(&[a.clone(), d.clone()]);
    let plan = deltas(plan(
        &current,
        &tree(std::slice::from_ref(&a)),
        &members(&[&a, &d]),
    ));
    assert_eq!(delta(&plan, d.id), &TaskDelta::Deleted);
    assert!(plan.is_applicable());
}

#[test]
fn an_added_task_is_applicable_whatever_the_membership() {
    let a = task("a");
    let n = task("n");
    let resolved = tree(&[a.clone(), n.clone()]);
    for membership in [members(&[&a]), members(&[&a, &n])] {
        let plan = deltas(plan(
            &tree(std::slice::from_ref(&a)),
            &resolved,
            &membership,
        ));
        // Membership decides only deletions; whether the store may take
        // the task is admission's question.
        assert_eq!(delta(&plan, n.id), &TaskDelta::Added);
        assert!(plan.is_applicable());
        assert_eq!(plan.additions().collect::<Vec<_>>(), [n.id]);
    }
}

#[test]
fn a_mixed_folder_lists_every_delta_in_task_id_order() {
    let a = task("a");
    let b = task("b");
    let d = task("d");
    let n = task("n");
    let current = tree(&[a.clone(), b.clone(), d.clone()]);
    let resolved = tree(&[
        edited(&a, |task| task.title = "Renamed".into()),
        edited(&b, |task| task.labels = vec!["sync".into()]),
        n.clone(),
    ]);
    let plan = deltas(plan(&current, &resolved, &members(&[&a, &b, &d])));
    let ids = plan.tasks.iter().map(|task| task.id.0).collect::<Vec<_>>();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted);
    assert_eq!(ids.len(), 4);
    let mut scalar_ids = vec![a.id, b.id];
    scalar_ids.sort_by_key(|id| id.0);
    assert_eq!(
        plan.scalar_changes().map(|(id, _)| id).collect::<Vec<_>>(),
        scalar_ids
    );
    assert_eq!(plan.additions().collect::<Vec<_>>(), [n.id]);
    assert_eq!(
        delta(&plan, b.id),
        &TaskDelta::Changed(Box::new(TaskChange {
            scalar: Some(ScalarChange {
                labels: Some(vec!["sync".into()]),
                ..ScalarChange::default()
            }),
            ..TaskChange::default()
        }))
    );
    assert_eq!(delta(&plan, d.id), &TaskDelta::Deleted);
    // Labels are ordinary planning; deletion still needs storage admission.
    assert!(plan.is_applicable());
}

#[test]
fn a_notes_profile_change_is_an_unsupported_project_change() {
    let a = task("a");
    let current = tree(std::slice::from_ref(&a));
    let mut resolved = current.clone();
    resolved.project.notes_included = true;
    let plan = deltas(plan(&current, &resolved, &members(&[&a])));
    assert_eq!(plan.project, [ProjectChange::NotesProfile]);
    assert_eq!(ProjectChange::NotesProfile.code(), "notes_profile");
    assert!(!plan.is_unchanged());
    assert!(!plan.is_applicable());
}

#[test]
fn a_folder_for_another_project_refuses() {
    let a = task("a");
    let current = tree(std::slice::from_ref(&a));
    let mut resolved = current.clone();
    resolved.project.project_id = ProjectId("elsewhere".into());
    assert_eq!(
        refusal(plan(&current, &resolved, &members(&[&a]))),
        TaskSyncRefusal::OriginMismatch
    );
}

#[test]
fn an_invalid_resolved_tree_refuses_as_an_invalid_graph() {
    let a = task("a");
    let current = tree(std::slice::from_ref(&a));
    let edits: [fn(&mut TaskDocument); 3] = [
        |task| task.title = " Padded".into(),
        |task| task.outcome = String::new(),
        |task| task.priority = 7,
    ];
    for edit in edits {
        let resolved = tree(&[edited(&a, edit)]);
        assert_eq!(
            refusal(plan(&current, &resolved, &members(&[&a]))),
            TaskSyncRefusal::InvalidGraph
        );
    }
}

#[test]
fn every_task_field_has_its_document_name() {
    let names = [
        TaskField::ExternalRef,
        TaskField::Acceptance,
        TaskField::AcceptanceBindings,
        TaskField::EvaluationMode,
        TaskField::Kind,
        TaskField::Labels,
        TaskField::AssignedTo,
        TaskField::DeferredUntil,
        TaskField::Lifecycle,
        TaskField::ParentId,
        TaskField::ChildRequirement,
        TaskField::Prerequisites,
        TaskField::Blockers,
        TaskField::SupersededBy,
        TaskField::DisposalReason,
        TaskField::Completion,
        TaskField::Notes,
        TaskField::Activity,
    ]
    .map(TaskField::as_str);
    let document = serde_json::to_value(TaskDocument {
        notes: vec![SharedNote {
            id: ObjectId::mint(),
            kind: SharedNoteKind::Generic,
            body: "Kept".into(),
            author: actor(),
            recorded_at: at(),
            origin_machine: "laptop".into(),
        }],
        ..task("a")
    })
    .unwrap();
    let keys = document
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .filter(|key| !["id", "title", "outcome", "priority", "activity"].contains(key))
        .collect::<HashSet<_>>();
    // Activity is omitted from a document when empty; every other name is a key.
    let mut expected = names.into_iter().collect::<HashSet<_>>();
    assert!(expected.remove("activity"));
    assert_eq!(keys, expected);
}

#[test]
fn scalar_and_relation_differences_are_one_composed_change() {
    let mut a = task("a");
    let b = task("b");
    let c = task("c");
    a.prerequisites = vec![b.id];
    let old = SharedBlocker {
        blocker_id: "old".into(),
        kind: WorkBlockerKind::Manual,
        detail: "original".into(),
        created_by: actor(),
        created_at: at(),
    };
    a.blockers = vec![old.clone()];
    let new = SharedBlocker {
        blocker_id: "new".into(),
        ..old.clone()
    };
    let incoming = edited(&a, |task| {
        task.title = "new title".into();
        task.prerequisites = vec![c.id];
        task.blockers = vec![new.clone()];
    });
    let planned = deltas(plan(
        &tree(&[a.clone(), b.clone(), c.clone()]),
        &tree(&[incoming, b.clone(), c.clone()]),
        &members(&[&a, &b, &c]),
    ));
    assert!(planned.is_applicable());
    let TaskDelta::Changed(change) = delta(&planned, a.id) else {
        panic!("composed change")
    };
    assert_eq!(
        change.scalar.as_ref().unwrap().title.as_deref(),
        Some("new title")
    );
    assert_eq!(change.relations.prerequisite_removals, vec![b.id]);
    assert_eq!(change.relations.prerequisite_additions, vec![c.id]);
    assert_eq!(change.relations.blocker_removals, vec!["old"]);
    assert_eq!(change.relations.blocker_additions, vec![new]);
    assert!(planned.is_applicable());
}
