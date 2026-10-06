use super::*;
use crate::ReopenWorkRequest;
use crate::storage::work::test_support::*;

fn fixture() -> (SqliteStore, crate::WorkItem, RootExecutionId) {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let work = store
        .create_work(
            &root_request("root-read", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let id = store
        .get_work_run(work.active_run_id.unwrap())
        .unwrap()
        .root_execution_id;
    (store, work, id)
}

#[test]
fn root_read_scope_shares_materialization_but_rechecks_selection() {
    let (store, work, id) = fixture();
    store
        .work_root_read_snapshot(|scope| {
            let active = scope.active(work.work_id)?;
            let current = scope.current(id)?;
            let retained = scope.retained(id)?;
            assert!(Rc::ptr_eq(&active, &current));
            assert!(Rc::ptr_eq(&current, &retained));
            assert_eq!(scope.roots.borrow().len(), 1);
            Ok(())
        })
        .unwrap();
}

#[test]
fn root_read_scope_never_joins_or_survives_a_writer() {
    let (store, work, id) = fixture();
    let transaction = store.connection.unchecked_transaction().unwrap();
    transaction
        .execute(
            "UPDATE work_root_executions SET revision = revision WHERE root_execution_id = ?1",
            [id.0.to_string()],
        )
        .unwrap();
    let refused = store.work_root_read_snapshot(|scope| scope.active(work.work_id).map(|_| ()));
    assert!(
        refused
            .unwrap_err()
            .to_string()
            .contains("read transaction")
    );
    transaction.rollback().unwrap();
    store
        .work_root_read_snapshot(|scope| {
            scope.current(id)?;
            // Deliberate test-only violation of the read-only call graph.
            store.connection.execute(
                "UPDATE work_root_executions SET revision = revision WHERE root_execution_id = ?1",
                [id.0.to_string()],
            )?;
            assert!(
                scope
                    .current(id)
                    .unwrap_err()
                    .to_string()
                    .contains("read transaction")
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn root_read_scope_revalidates_corruption_under_an_unchanged_head() {
    let (store, _, id) = fixture();
    store
        .work_root_read_snapshot(|scope| scope.current(id).map(|_| ()))
        .unwrap();
    let head: String = store
        .connection
        .query_row(
            "SELECT head_id FROM work_root_executions WHERE root_execution_id = ?1",
            [id.0.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    store.connection.execute("UPDATE work_root_members SET member_json = CAST(json_set(member_json, '$.unexpected', 1) AS BLOB) WHERE root_execution_id = ?1", [id.0.to_string()]).unwrap();
    let after: String = store
        .connection
        .query_row(
            "SELECT head_id FROM work_root_executions WHERE root_execution_id = ?1",
            [id.0.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(after, head);
    let error = store
        .work_root_read_snapshot(|scope| scope.current(id).map(|_| ()))
        .unwrap_err();
    assert!(
        error.to_string().contains("unexpected member fields"),
        "{error}"
    );
}

#[test]
fn root_read_scope_retained_and_active_generations_are_not_interchangeable() {
    let (mut store, work, old_id) = fixture();
    let runner = claim(&mut store, &work, "holder", "claim", 1, 3600);
    let proof = evidence(&mut store, &work, &runner, "holder", "evidence", 2);
    checkpoint(
        &mut store,
        &work,
        &runner,
        "holder",
        "checkpoint",
        3,
        std::slice::from_ref(&proof),
    );
    complete(&mut store, &work, &runner, "holder", &proof, "complete", 4).unwrap();
    let closed = store.get_work_item(work.work_id).unwrap();
    let new_run = store
        .reopen_work(
            &ReopenWorkRequest {
                work_id: work.work_id,
                expected_work_revision: closed.revision,
                reason: "new generation".into(),
                actor: actor("planner"),
                idempotency_key: "reopen".into(),
                reopened_at: at(5),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    store
        .work_root_read_snapshot(|scope| {
            let old = scope.retained(old_id)?;
            assert_eq!(old.root_execution_id, old_id);
            assert!(
                scope.current(old_id).is_err(),
                "retained selection cannot prove exact-current selection"
            );
            let active = scope.active(work.work_id)?;
            assert_eq!(active.root_execution_id, new_run.root_execution_id);
            assert_ne!(active.root_execution_id, old_id);
            assert_eq!(scope.roots.borrow().len(), 2);
            Ok(())
        })
        .unwrap();
}

#[test]
fn root_read_scope_uses_one_cut_across_a_second_connections_commit() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("root-read.sqlite3");
    let mut writer = SqliteStore::open(&database).unwrap();
    let work = writer
        .create_work(
            &root_request("root-read-barrier", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let id = writer
        .get_work_run(work.active_run_id.unwrap())
        .unwrap()
        .root_execution_id;
    let reader = SqliteStore::open(&database).unwrap();
    let work_id = work.work_id;
    crate::storage::concurrent_commit::read_across_a_concurrent_commit(
        &reader,
        |reader| {
            reader.work_root_read_snapshot(|scope| {
                let first = scope.active(work_id)?;
                let second = scope.current(id)?;
                assert!(Rc::ptr_eq(&first, &second));
                assert!(
                    !second
                        .expected_contributors
                        .contains(&crate::SessionId("later".into()))
                );
                Ok(())
            })
        },
        &["FROM work_root_members"],
        move || {
            writer
                .add_expected_root_contributor_fixture(
                    work_id,
                    &crate::SessionId("later".into()),
                    at(1),
                )
                .map_err(|error| error.to_string())
        },
    )
    .unwrap();
    reader
        .work_root_read_snapshot(|scope| {
            assert!(
                scope
                    .current(id)?
                    .expected_contributors
                    .contains(&crate::SessionId("later".into()))
            );
            Ok(())
        })
        .unwrap();
}
