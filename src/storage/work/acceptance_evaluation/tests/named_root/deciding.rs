//! Under a named root, a sighting on another workspace decides nothing and is
//! never named; a change inside the root is.

use super::*;

fn status_observation(
    store: &SqliteStore,
    work: &WorkItem,
) -> Option<crate::storage::DecidingObservation> {
    store
        .acceptance_evaluation_status(work.work_id, None)
        .expect("status read")
        .expect("an evaluation")
        .stale_observation
}

#[test]
fn a_sighting_on_another_workspace_is_not_named_under_a_named_root() {
    let (mut fixture, work, _claim, mut host) = sighted_root("project-deciding-foreign");
    let note = fixture.evidence.clone();
    let store = &mut fixture.store;
    let read = cut(store, &work);
    record(
        store,
        &declared_pass(&work, &note, read, "R1", "declared", 40),
    )
    .expect("the declared evaluation records");
    assert_eq!(stale_reason(store, &work), None);

    // Another workspace sights another revision. (A reported change there
    // would open an obligation, a check after the cut, which is another move.)
    host.basis = workspace("workspace-C", "R9", None);
    host.report(store, &[(false, Some("R8"))], 50);
    assert_eq!(stale_reason(store, &work), None);
    assert_eq!(status_observation(store, &work), None);

    // A change inside the root is the one named.
    host.basis = workspace("workspace-B", "R2", Some(9));
    host.checkpoint(store, true, None, 60);
    let named = status_observation(store, &work).expect("the deciding observation");
    assert_eq!(named.workspace.as_deref(), Some("workspace-B"));
    assert_eq!(named.revision.as_deref(), Some("R2"));
    assert_eq!(named.root_generation, Some(9));
    assert!(named.source_changed);
    assert_eq!(named.evaluated_revision.as_deref(), Some("R1"));
    assert!(named.evaluated_revision_declared);
}

// A named-root rebinding refuses as a source move, but no observation
// decided it, so none is named.
#[test]
fn a_rebinding_after_the_cut_names_no_observation() {
    let (mut fixture, work, claim, host) = sighted_root("project-deciding-rebind");
    let note = fixture.evidence.clone();
    let store = &mut fixture.store;
    let read = cut(store, &work);
    host_binds(
        store,
        &host,
        &claim,
        "workspace-B",
        10,
        NamedRootBindingKind::Bound,
        45,
        "rename-B",
        45,
    )
    .expect("host renames B");
    let error = record(
        store,
        &declared_pass(&work, &note, read, "R1", "after-rebind", 50),
    )
    .expect_err("a rebinding after the cut refuses");
    let message = error.to_string();
    let StoreError::AcceptanceEvaluationBasisMoved { observation, .. } = error else {
        panic!("expected a moved basis, got {error:?}");
    };
    assert_eq!(observation, None);
    assert!(
        !message.contains("deciding source observation"),
        "{message}"
    );
}
