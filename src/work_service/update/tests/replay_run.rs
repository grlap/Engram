//! The run a replayed receipt's obligation page belongs to: a claim's
//! receipt names its run in its binding; otherwise the stored basis counts
//! only when it focused the receipt's own item, and a basis the current
//! shape cannot read never fails the replay.

use super::*;
use crate::domain::ControlWorkBinding;

fn receipt(work_id: WorkId, control_binding: Option<ControlWorkBinding>) -> WorkMutationReceipt {
    WorkMutationReceipt {
        work_id,
        work_ref: "w-000000000000".into(),
        revision: 1,
        control_binding,
        result: serde_json::json!({}),
    }
}

fn binding(work_id: WorkId, run_id: WorkRunId) -> ControlWorkBinding {
    ControlWorkBinding {
        root_execution_id: crate::RootExecutionId::new(),
        work_id,
        run_id,
        work_revision: 1,
        claim_id: crate::WorkClaimId::new(),
        claim_fence: 1,
    }
}

fn basis(focused: WorkId, active_run: Option<WorkRunId>) -> serde_json::Value {
    serde_json::json!({
        "focused_work": { "work_id": focused, "active_run_id": active_run, "extra": 1 },
        "claim": null,
        "handoffs": [],
    })
}

#[test]
fn a_replayed_page_resolves_the_run_it_was_built_on() {
    let parent = WorkId::new();
    let child = WorkId::new();
    let parent_run = WorkRunId::new();
    let child_run = WorkRunId::new();
    let replayed = |basis: Option<&serde_json::Value>, receipt: &WorkMutationReceipt| {
        super::super::replayed_run_id(basis, receipt)
    };

    // A claim of the parent's next ready child: the basis names the parent,
    // the receipt and its binding the child, whose run the page belongs to.
    let from_parent = basis(parent, Some(parent_run));
    assert_eq!(
        replayed(
            Some(&from_parent),
            &receipt(child, Some(binding(child, child_run)))
        ),
        Some(child_run)
    );
    // Without a binding, a basis that focused another item says nothing: the
    // page's own rows decide, and a page without rows is returned as
    // recorded.
    assert_eq!(replayed(Some(&from_parent), &receipt(child, None)), None);
    // A binding for another item is not the page's run either.
    assert_eq!(
        replayed(
            Some(&from_parent),
            &receipt(child, Some(binding(parent, parent_run)))
        ),
        None
    );
    // A basis that focused the receipt's own item names its active run.
    assert_eq!(
        replayed(Some(&basis(child, Some(child_run))), &receipt(child, None)),
        Some(child_run)
    );
    // A basis the current shape cannot read, or none at all, never fails.
    let unreadable = serde_json::json!({ "focused_work": "not an item" });
    assert_eq!(replayed(Some(&unreadable), &receipt(child, None)), None);
    assert_eq!(replayed(None, &receipt(child, None)), None);
}
