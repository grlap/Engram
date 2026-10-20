//! Graph attribution a public folder application no longer reaches: an
//! addition whose root in a valid graph differs from the folder's.

use super::*;

fn task(title: &str, parent: Option<WorkId>) -> TaskDocument {
    TaskDocument {
        id: WorkId(uuid::Uuid::new_v4()),
        external_ref: None,
        title: title.into(),
        outcome: format!("Deliver {title}"),
        acceptance: vec!["delivered".into()],
        acceptance_bindings: Vec::new(),
        evaluation_mode: None,
        kind: crate::WorkItemKind::Task,
        priority: 1,
        labels: Vec::new(),
        assigned_to: None,
        deferred_until: None,
        lifecycle: WorkLifecycle::Open,
        parent_id: parent,
        child_requirement: crate::ChildRequirement::Required,
        prerequisites: Vec::new(),
        blockers: Vec::new(),
        superseded_by: None,
        disposal_reason: None,
        completion: None,
        notes: Vec::new(),
        activity: std::collections::BTreeMap::default(),
    }
}

fn tree(mut tasks: Vec<TaskDocument>) -> TaskTree {
    tasks.sort_by_key(|task| task.id.0);
    TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: ProjectId("folder-admission-unit".into()),
            notes_included: false,
        },
        tasks,
    }
}

#[test]
fn a_differing_root_is_named_without_claiming_an_invalid_graph() {
    // The destination holds a task under the first root; the folder holds it
    // under the second and adds a task beneath it. With the addition, the
    // destination's planning is a valid task graph, but there the
    // addition's root is the first root, not the folder's second.
    let first = task("first root", None);
    let second = task("second root", None);
    let held = task("held", Some(first.id));
    let mut moved = held.clone();
    moved.parent_id = Some(second.id);
    let added = task("added", Some(held.id));
    let added_id = added.id;
    let current = tree(vec![first.clone(), second.clone(), held]);
    let resolved = tree(vec![first, second, moved, added]);
    let prospective = Prospective::new(&current, &resolved).unwrap();
    let base = current.tasks.iter().collect::<Vec<_>>();
    let findings = check_graph(
        &prospective,
        &resolved.project.project_id,
        false,
        &base,
        &[added_id],
        "retained_graph",
        "the destination's planning",
        || {},
    )
    .unwrap();
    assert!(findings.batch.is_empty(), "{:?}", findings.batch);
    assert_eq!(findings.conflicts.len(), 1);
    let named = &findings.conflicts[0];
    assert_eq!(named.id, added_id);
    assert_eq!(named.reason, "retained_graph");
    assert_eq!(
        named.detail,
        "the destination's planning with this addition is a valid task graph, but this addition's computed root there differs from the folder's"
    );
}
