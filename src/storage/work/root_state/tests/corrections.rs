use super::*;
use crate::domain::{ChildRequirement, DecomposeWorkRequest};

#[test]
fn root_delta_transition_errors_name_the_broken_invariant_without_writes() {
    let (store, _, id) = fixture();
    let transaction = store.connection.unchecked_transaction().unwrap();
    let base = projected(&transaction, id).unwrap().0;
    for (fault, expected) in [
        ("identity", "root identity, schema or generation changed"),
        ("generation", "root identity, schema or generation changed"),
        ("created_at", "root origin created_at changed"),
        ("revision", "root revision moved backwards"),
    ] {
        let mut changed = base.clone();
        match fault {
            "identity" => changed.root_id = crate::WorkId::new(),
            "generation" => changed.generation += 1,
            "created_at" => changed.created_at += chrono::Duration::seconds(1),
            "revision" => changed.revision -= 1,
            _ => unreachable!(),
        }
        let before = test_database_shape_snapshot(&transaction).unwrap();
        let error = persist(&transaction, &changed).unwrap_err();
        assert!(error.to_string().contains(expected), "{fault}: {error}");
        assert_eq!(test_database_shape_snapshot(&transaction).unwrap(), before);
    }
}

#[test]
fn root_delta_collection_order_errors_name_each_collection() {
    let (store, root, id) = fixture();
    let transaction = store.connection.unchecked_transaction().unwrap();
    let (mut value, address) = projected(&transaction, id).unwrap();
    let hash = CanonicalObject::freeze(&"second member")
        .unwrap()
        .hash()
        .clone();
    value.run_ids = vec![WorkRunId::new(), WorkRunId::new()];
    value.required_child_seals = vec![address.head.clone(), hash.clone()];
    value.required_child_waivers = [root.work_id, crate::WorkId::new()]
        .map(|work_id| RequiredChildWaiver {
            work_id,
            work_revision: 1,
            waived_by: "test".into(),
            reason: "codec fixture".into(),
        })
        .to_vec();
    value.expected_contributors = vec![SessionId("a".into()), SessionId("b".into())];
    value.contributions = vec![
        RootContribution {
            participant: SessionId("a".into()),
            object: address.head,
        },
        RootContribution {
            participant: SessionId("a".into()),
            object: hash,
        },
    ];
    value.waivers = ["a", "b"]
        .map(|participant| CompletionWaiver {
            participant: SessionId(participant.into()),
            waived_by: "test".into(),
            reason: "codec fixture".into(),
        })
        .to_vec();
    let value = assemble(&header(&value), &members(&value).unwrap()).unwrap();
    for collection in [
        "run_ids",
        "required_child_seals",
        "required_child_waivers",
        "expected_contributors",
        "contributions",
        "waivers",
    ] {
        let mut changed = value.clone();
        match collection {
            "run_ids" => changed.run_ids.reverse(),
            "required_child_seals" => changed.required_child_seals.reverse(),
            "required_child_waivers" => changed.required_child_waivers.reverse(),
            "expected_contributors" => changed.expected_contributors.reverse(),
            "contributions" => changed.contributions.reverse(),
            "waivers" => changed.waivers.reverse(),
            _ => unreachable!(),
        }
        let before = test_database_shape_snapshot(&transaction).unwrap();
        let error = persist(&transaction, &changed).unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&format!("noncanonical collection order: {collection}")),
            "{error}"
        );
        assert_eq!(test_database_shape_snapshot(&transaction).unwrap(), before);
    }
}

#[test]
fn root_delta_written_head_refuses_wrong_connection_state_and_stale_head() {
    let (store, _, id) = fixture();
    let transaction = store.connection.unchecked_transaction().unwrap();
    let written = update(&transaction, id, |value| value.revision += 1).unwrap();
    let other = Connection::open_in_memory().unwrap();
    let before = test_database_shape_snapshot(&transaction).unwrap();
    assert_eq!(
        written
            .event_ref(&transaction, Some(written.value()))
            .unwrap(),
        written.address
    );
    for (connection, value) in [(&other, Some(written.value())), (&*transaction, None)] {
        assert!(
            written
                .event_ref(connection, value)
                .unwrap_err()
                .to_string()
                .contains("another transaction or event state")
        );
    }
    let mut changed = written.value().clone();
    changed.revision += 1;
    assert!(
        written
            .event_ref(&transaction, Some(&changed))
            .unwrap_err()
            .to_string()
            .contains("another transaction or event state")
    );
    assert_eq!(test_database_shape_snapshot(&transaction).unwrap(), before);
    // Advance by the substrate writer: the old proof must refuse, not reload
    // or silently attach the newer head to the earlier event state.
    persist(&transaction, &changed).unwrap();
    let before = test_database_shape_snapshot(&transaction).unwrap();
    assert!(
        written
            .event_ref(&transaction, Some(written.value()))
            .unwrap_err()
            .to_string()
            .contains("written head is no longer current")
    );
    assert_eq!(test_database_shape_snapshot(&transaction).unwrap(), before);
}

#[test]
fn root_delta_identifier_parser_names_root_execution() {
    let id = RootExecutionId::new();
    assert_eq!(
        super::super::super::query::parse_root_execution_id(&id.0.to_string()).unwrap(),
        id
    );
    let error = super::super::super::query::parse_root_execution_id("not-a-uuid").unwrap_err();
    assert!(error.to_string().contains("invalid root execution id"));
    assert!(!error.to_string().contains("invalid work id"));
}

fn refuse_header_equality(store: &SqliteStore, id: RootExecutionId, label: &str) {
    let before = test_database_shape_snapshot(&store.connection).unwrap();
    let error = projected(&store.connection, id).unwrap_err();
    assert!(
        matches!(
            error,
            StoreError::InvalidWorkProjection(ref reason)
                if reason == "root state: header differs from canonical head"
        ),
        "{label}: {error}"
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        before,
        "{label}"
    );
}

#[test]
fn root_projection_scalar_column_drift_refuses_header_equality() {
    // root_execution_id is the lookup key, so it is not mutated here.
    // created_at_ms/updated_at_ms are compared by doctor, not this read-time guard.
    for sql in [
        "UPDATE work_root_executions SET revision = revision + 1 WHERE root_execution_id = ?1",
        "UPDATE work_root_executions SET generation = generation + 1 WHERE root_execution_id = ?1",
        "UPDATE work_root_executions SET project_id = 'drifted-project' WHERE root_execution_id = ?1",
        "UPDATE work_root_executions SET state = 'completed' WHERE root_execution_id = ?1",
    ] {
        let (store, _, id) = fixture();
        store.connection.execute(sql, [id.0.to_string()]).unwrap();
        refuse_header_equality(&store, id, sql);
    }

    // Active root_id is unique, so the FK-valid drift target is a child item
    // rather than a second root.
    let (mut store, root, id) = fixture();
    let other = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![child(
                    "drift-target",
                    ChildRequirement::Optional,
                    "Drift target",
                )],
                prerequisites: Vec::new(),
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "root-id-drift".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap()
        .children
        .into_iter()
        .next()
        .expect("child");
    store
        .connection
        .execute(
            "UPDATE work_root_executions SET root_id = ?1 WHERE root_execution_id = ?2",
            rusqlite::params![other.work_id.0.to_string(), id.0.to_string()],
        )
        .unwrap();
    refuse_header_equality(&store, id, "root_id");
}

#[test]
fn root_projection_extra_header_field_refuses_unknown_fields() {
    let (store, _, id) = fixture();
    let bytes: Vec<u8> = store
        .connection
        .query_row(
            "SELECT header_json FROM work_root_executions WHERE root_execution_id = ?1",
            [id.0.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    let mut header: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    header
        .as_object_mut()
        .expect("header object")
        .insert("unexpected".into(), serde_json::json!(true));
    store
        .connection
        .execute(
            "UPDATE work_root_executions SET header_json = ?1 WHERE root_execution_id = ?2",
            rusqlite::params![serde_json::to_vec(&header).unwrap(), id.0.to_string()],
        )
        .unwrap();
    let before = test_database_shape_snapshot(&store.connection).unwrap();
    let error = projected(&store.connection, id).unwrap_err();
    assert!(
        matches!(
            error,
            StoreError::InvalidWorkProjection(ref reason)
                if reason == "root state: unexpected header fields"
        ),
        "{error}"
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        before
    );
}
