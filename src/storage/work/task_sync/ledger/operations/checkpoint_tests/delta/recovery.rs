use super::*;

#[test]
fn retained_incoming_delta_still_refuses_changed_native_readset() {
    let mut f = Fixture::new();
    let item = f.store.get_work_item(f.selected).unwrap();
    f.store
        .revise_work(
            &crate::ReviseWorkRequest {
                work_id: f.selected,
                expected_revision: item.revision,
                patch: crate::WorkRevisionPatch {
                    title: Some("Changed local readset".into()),
                    ..Default::default()
                },
                authority: crate::WorkPlanningAuthority::Project,
                actor: f.input.intent.actor.clone(),
                idempotency_key: "intervening-selected-revision".into(),
                updated_at: at(36),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let before = state(&f.store);
    let retained = retention::inventory(&f.input.binding.repository.git_directory).unwrap();
    let error = f.apply().unwrap_err();
    assert!(
        error.to_string().contains("native readset changed"),
        "{error}"
    );
    assert_eq!(state(&f.store), before);
    assert_eq!(
        retention::inventory(&f.input.binding.repository.git_directory).unwrap(),
        retained
    );
    assert!(
        f.store
            .completed_task_sync_delta_import(&f.input.binding_id, &f.input.intent)
            .unwrap()
            .is_none()
    );
}
