use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};

use super::{DeltaPlan, ProjectChange, ScalarChange, TaskChange, TaskDelta, TaskField, plan};
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
    TaskDelta::Changed(TaskChange {
        scalar: None,
        unsupported,
    })
}

fn scalar(change: ScalarChange) -> TaskDelta {
    TaskDelta::Changed(TaskChange {
        scalar: Some(change),
        unsupported: Vec::new(),
    })
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
        TaskDelta::Changed(TaskChange {
            scalar: Some(_),
            ..
        })
    ));
}

#[test]
fn every_other_field_change_is_unsupported_and_named() {
    type Edit = fn(&mut TaskDocument);
    let cases: [(TaskField, Edit); 11] = [
        (TaskField::ExternalRef, |task| {
            task.external_ref = Some("tracker#12".into());
        }),
        (TaskField::Acceptance, |task| {
            task.acceptance = vec!["Something else".into()];
        }),
        (TaskField::AcceptanceBindings, |task| {
            task.acceptance_bindings = vec![AcceptanceBinding {
                criterion: 1,
                requirement: VerificationRequirement {
                    check_kind: VerificationKind::Test,
                    check_fingerprint: None,
                },
            }];
        }),
        (TaskField::EvaluationMode, |task| {
            task.evaluation_mode = Some(AcceptanceEvaluationMode::SameSession);
        }),
        (TaskField::Kind, |task| task.kind = WorkItemKind::Bug),
        (TaskField::Labels, |task| task.labels = vec!["sync".into()]),
        (TaskField::AssignedTo, |task| {
            task.assigned_to = Some("greg".into());
        }),
        (TaskField::DeferredUntil, |task| {
            task.deferred_until = Some(at());
        }),
        (TaskField::Lifecycle, |task| {
            task.lifecycle = WorkLifecycle::Proposed;
        }),
        (TaskField::ChildRequirement, |task| {
            task.child_requirement = ChildRequirement::Optional;
        }),
        (TaskField::Blockers, |task| {
            task.blockers = vec![SharedBlocker {
                blocker_id: "waiting".into(),
                kind: WorkBlockerKind::Manual,
                detail: "Waiting on input".into(),
                created_by: actor(),
                created_at: at(),
            }];
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
fn relation_changes_are_unsupported() {
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
        &fields(vec![TaskField::Prerequisites])
    );
}

#[test]
fn disposal_and_completion_name_every_changed_field_in_document_order() {
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
        (
            edited(&a, |task| {
                task.lifecycle = WorkLifecycle::Completed;
                task.completion = completed("a").completion;
            }),
            vec![TaskField::Lifecycle, TaskField::Completion],
        ),
    ];
    for (changed, fields) in cases {
        let resolved = tree(&[changed, b.clone()]);
        let plan = deltas(plan(&current, &resolved, &membership));
        assert_eq!(delta(&plan, a.id), &self::fields(fields));
    }
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
        task.labels = vec!["sync".into()];
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
        &TaskDelta::Changed(TaskChange {
            scalar: Some(ScalarChange {
                title: Some("Renamed".into()),
                outcome: None,
                priority: None,
            }),
            unsupported: vec![TaskField::Labels],
        })
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
    assert!(!plan.is_applicable());
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
    assert_eq!(
        plan.scalar_changes().map(|(id, _)| id).collect::<Vec<_>>(),
        [a.id]
    );
    assert_eq!(plan.additions().collect::<Vec<_>>(), [n.id]);
    assert_eq!(delta(&plan, b.id), &fields(vec![TaskField::Labels]));
    assert_eq!(delta(&plan, d.id), &TaskDelta::Deleted);
    // The labels change and the deletion make the folder inapplicable.
    assert!(!plan.is_applicable());
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
