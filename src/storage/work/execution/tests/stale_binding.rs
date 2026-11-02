use super::*;
use crate::domain::NamedRootBindingKind;
use crate::storage::HostSessionAuthority;

#[test]
fn restarted_host_requires_the_current_holder_and_fence_for_new_root_records() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("engram.sqlite3");
    let mut store = SqliteStore::open(&database).unwrap();
    let work = store
        .create_work(
            &root_request("project-restart", "restart-work", 1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let initial = claim(&mut store, &work, "runner", "initial-claim", 2, 10);
    let session = initial.holder.clone();
    let connection = store.resume_work_connection(&session, at(3)).unwrap();
    let mut host = actor("runner");
    host.run_id = Some(initial.run_id.0.to_string());
    store
        .bind_named_root(
            &work.project_id,
            &session,
            &connection,
            HostSessionAuthority::Work,
            initial.claim_id,
            initial.fence,
            "restart-workspace",
            1,
            at(4),
            NamedRootBindingKind::Bound,
            None,
            &mut host,
            "initial-root",
            at(4),
        )
        .unwrap();
    drop(store);
    assert_eq!(initial.expires_at, at(12));
    let mut store = SqliteStore::open(&database).unwrap();
    let replacement_connection = store.resume_work_connection(&session, at(13)).unwrap();
    let before = test_database_shape_snapshot(&store.connection);
    assert!(
        store
            .bind_named_root(
                &work.project_id,
                &session,
                &replacement_connection,
                HostSessionAuthority::Work,
                initial.claim_id,
                initial.fence,
                "restart-workspace",
                2,
                at(14),
                NamedRootBindingKind::Bound,
                None,
                &mut host,
                "expired-claim",
                at(14)
            )
            .is_err()
    );
    assert_eq!(test_database_shape_snapshot(&store.connection), before);
    assert_eq!(
        store.current_work_claim(work.work_id).unwrap(),
        Some(initial.clone()),
        "reopening and refusing never renew a work claim"
    );
    let recovered = claim(&mut store, &work, "runner", "recover-claim", 17, 120);
    assert!(recovered.fence > initial.fence);
    let before = test_database_shape_snapshot(&store.connection);
    assert!(
        store
            .bind_named_root(
                &work.project_id,
                &session,
                &replacement_connection,
                HostSessionAuthority::Work,
                initial.claim_id,
                initial.fence,
                "restart-workspace",
                2,
                at(18),
                NamedRootBindingKind::Bound,
                None,
                &mut host,
                "replaced-claim",
                at(18)
            )
            .is_err()
    );
    assert_eq!(test_database_shape_snapshot(&store.connection), before);
    let fresh = store
        .bind_named_root(
            &work.project_id,
            &session,
            &replacement_connection,
            HostSessionAuthority::Work,
            recovered.claim_id,
            recovered.fence,
            "restart-workspace",
            2,
            at(19),
            NamedRootBindingKind::Bound,
            None,
            &mut host,
            "recovered-root",
            at(19),
        )
        .unwrap();
    let event: crate::domain::NamedRootBindingEvent =
        super::super::super::feeds::load_typed_work_object(
            &store.connection,
            &fresh.event,
            "named_root_binding",
        )
        .unwrap();
    assert_eq!(event.claim_id, recovered.claim_id);
    assert_eq!(event.claim_fence, recovered.fence);
    assert_eq!(
        store.current_work_claim(work.work_id).unwrap(),
        Some(recovered)
    );
    assert_eq!(store.connection.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name IN
        ('control_sessions','control_turn_results','control_turn_grants','control_turn_grant_supersessions')",
        [], |row| row.get::<_, i64>(0)).unwrap(), 0);
}
