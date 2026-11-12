use super::*;

#[test]
fn admitted_delta_and_successor_history_preserves_projections_and_original_execution() {
    let (mut f, original, old_run) = ended_fixture();
    let (request, payload) = resolved(&f, &original);
    let applied = apply_planning_successor(
        &mut f.store,
        &f.input.binding_id,
        &f.input.binding,
        &request,
        &payload,
        at(70),
        &DevelopmentNoopRedactor,
    )
    .unwrap();
    let source = f
        .store
        .capture_task_sync_membership_component(
            &f.input.binding_id,
            crate::task_sync::TaskSyncMembershipView::Source,
            [f.selected],
        )
        .unwrap();
    assert_eq!(
        source.component.documents[&f.selected].title,
        "original incoming title"
    );
    assert_ne!(
        source.membership.inventory_root,
        payload.output.inventory_root
    );
    let Fixture {
        store,
        input,
        selected,
        home,
    } = f;
    let mut f = Fixture {
        store: super::super::super::historical::assert_roundtrip(store, &input.binding),
        input,
        selected,
        home,
    };
    assert_eq!(f.apply().unwrap(), original);
    assert_eq!(
        apply_planning_successor(
            &mut f.store,
            &f.input.binding_id,
            &f.input.binding,
            &request,
            &payload,
            at(99),
            &DevelopmentNoopRedactor,
        )
        .unwrap(),
        applied
    );
    assert_eq!(
        crate::storage::work::query::load_work_run(&f.store.connection, old_run.run_id,).unwrap(),
        old_run
    );
    let actual = f
        .store
        .capture_task_sync_membership_component(
            &f.input.binding_id,
            crate::task_sync::TaskSyncMembershipView::Source,
            [f.selected],
        )
        .unwrap();
    assert_eq!(actual.head, source.head);
    assert_eq!(actual.membership, source.membership);
    assert_eq!(actual.component.documents, source.component.documents);
    assert_eq!(actual.component.members, source.component.members);
    assert_eq!(actual.component.missing, source.component.missing);
}
