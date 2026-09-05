use super::*;
use crate::storage::work::test_support::*;
use crate::{DisposeWorkRequest, WorkDisposition};

#[test]
fn discovery_orders_nested_binding_observations_by_run_head() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let project = ProjectId("context-discovery".into());
    let session = SessionId("coordinator".into());
    let mut items = Vec::new();
    let mut claims = Vec::new();
    for index in 0..2 {
        let mut request = root_request(&project.0, &format!("context-{index}"), index);
        request.actor = actor(&session.0);
        request.notes = vec![format!("Own note {index}")];
        request.assigned_to = Some(session.0.clone());
        let item = store
            .create_work(&request, &DevelopmentNoopRedactor)
            .unwrap();
        claims.push(claim(
            &mut store,
            &item,
            "runner",
            &format!("claim-{index}"),
            index + 2,
            300,
        ));
        items.push(item);
    }
    assert_eq!(
        store
            .work_discovery(&project, &session, &session.0, false, at(4))
            .unwrap()
            .items[0]
            .work
            .work_id,
        items[1].work_id
    );
    let work = &items[0];
    let claim = &claims[0];
    let run = crate::storage::work::query::load_work_run(&store.connection, claim.run_id).unwrap();
    let event_position = |connection: &Connection| {
        connection.query_row(
        "SELECT MAX(position) FROM work_feed_entries WHERE feed_kind = 'project' AND work_id = ?1 AND object_kind = 'work_event'",
        [work.work_id.0.to_string()], |row| row.get::<_, i64>(0),
    ).unwrap()
    };
    let before = event_position(&store.connection);
    let mut observer = actor("runner");
    observer.run_id = Some(run.run_id.0.to_string());
    let observation = crate::ExecutionObservation {
        schema_version: crate::domain::SCHEMA_VERSION,
        project_id: project.clone(),
        binding: crate::ControlWorkBinding {
            root_execution_id: run.root_execution_id,
            work_id: work.work_id,
            run_id: run.run_id,
            work_revision: claim.accepted_work_revision,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
        },
        session_id: SessionId("runner".into()),
        grant_id: "context-read".into(),
        observation_id: "standalone-read".into(),
        action_fingerprint: ObjectHash::from_canonical_bytes(b"read workspace"),
        effect: EffectClass::Observe,
        outcome: ExecutionOutcome::Succeeded,
        source_changed: false,
        obligation_rule_set: builtin_rule_set_hash(),
        source_basis: None,
        observed_at: Some(at(0)),
        actor: observer,
        recorded_at: at(0),
    };
    let transaction = store.connection.transaction().unwrap();
    crate::storage::work::completion::append_control_execution_observation_on(
        &transaction,
        &observation,
    )
    .unwrap();
    transaction.commit().unwrap();
    // This real observation has binding.work_id, no top-level work_id and no
    // companion work event. Its asserted timestamp is deliberately older.
    assert_eq!(event_position(&store.connection), before);
    for assigned in [true, false] {
        let page = store
            .work_discovery(&project, &session, &session.0, assigned, at(4))
            .unwrap();
        assert_eq!(page.items[0].work.work_id, work.work_id);
        assert_eq!(page.items[0].note.as_deref(), Some("Own note 0"));
    }
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn discovery_sql_work_does_not_grow_with_unrelated_closed_history() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let project = ProjectId("discovery-project".into());
    let session = SessionId("coordinator".into());
    for index in 0..2 {
        let mut request = root_request(&project.0, &format!("candidate-{index}"), index);
        request.actor = actor(&session.0);
        request.notes = vec![format!("Own note {index}")];
        request.assigned_to = Some(session.0.clone());
        store
            .create_work(&request, &DevelopmentNoopRedactor)
            .unwrap();
    }
    let measure = |store: &SqliteStore| {
        [true, false].map(|assigned| {
            let page = store
                .work_discovery(&project, &session, &session.0, assigned, at(3))
                .unwrap();
            assert_eq!(page.items.len(), 2);
            assert_eq!(page.omitted, 0);
            page.vm_steps
        })
    };
    let mut before = None;
    for index in 0..=32 {
        let mut request = root_request(&project.0, &format!("closed-{index}"), 0);
        request.notes = (0..16)
            .map(|n| format!("Unrelated closed note {index}/{n}"))
            .collect();
        let item = store
            .create_work(&request, &DevelopmentNoopRedactor)
            .unwrap();
        store
            .dispose_work(
                &DisposeWorkRequest {
                    work_id: item.work_id,
                    expected_work_revision: item.revision,
                    replacement_id: None,
                    disposition: WorkDisposition::Cancelled,
                    reason: "Unrelated retained history".into(),
                    actor: actor("planner"),
                    idempotency_key: format!("close-{index}"),
                    disposed_at: at(1),
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        if index == 0 {
            // Seed a nonmatching trailing key before measuring: an indexed
            // end-of-range probe takes two more VM steps than end-of-table.
            // The assertion then isolates history growth, not that boundary.
            before = Some(measure(&store));
        }
    }
    let after = measure(&store);
    let mut plan = store
        .connection
        .prepare(&format!("EXPLAIN QUERY PLAN {}", discovery_sql(true)))
        .unwrap();
    let lines = plan
        .query_map(
            rusqlite::params![project.0, session.0, session.0, DISCOVERY_LIMIT],
            |row| row.get::<_, String>(3),
        )
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("work_items_assigned"))
    );
    for (before, after) in before.unwrap().into_iter().zip(after) {
        assert!(
            after <= before,
            "unrelated history increased SQLite VM work: {before} -> {after}"
        );
    }
    assert!(store.verify_all().unwrap().is_healthy());
}
