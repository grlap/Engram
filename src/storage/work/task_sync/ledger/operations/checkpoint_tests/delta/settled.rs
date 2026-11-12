//! Legacy incoming preparation reads a settled delta outcome through the common
//! settlement contract. The legacy plan then applies its own native-state rule.

use super::*;

#[test]
fn legacy_incoming_preparation_treats_a_delta_outcome_as_settled() {
    let mut f = Fixture::new();
    let original = f.apply().unwrap();
    assert!(matches!(
        original.event.transition,
        Transition::DeltaImportApplied { .. }
    ));
    let binding = &f.input.binding;
    let next = super::super::super::tests::input_with_cut(
        &f.store,
        &f.input.binding_id,
        binding,
        f.store.export_task_tree(&binding.project).unwrap(),
        producing_origin::cut_on(&f.store.connection, &binding.project).unwrap(),
    );
    let before = state(&f.store);
    let error = f
        .store
        .prepare_task_sync_exchange(&next, at(60))
        .unwrap_err()
        .to_string();
    // The delta outcome is no unresolved operation. The legacy plan still
    // refuses the native planning the delta wrote, for its own reason.
    assert!(
        !error.contains("new exchange bypasses unresolved operation"),
        "{error}"
    );
    assert!(
        error.contains("import plan refuses native execution state"),
        "{error}"
    );
    assert_eq!(state(&f.store), before);
}
