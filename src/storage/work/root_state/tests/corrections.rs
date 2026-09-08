use super::*;

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
