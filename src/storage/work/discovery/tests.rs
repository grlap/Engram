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
        action_fingerprint: ObjectId::from_canonical_bytes(b"read workspace"),
        effect: EffectClass::Observe,
        outcome: ExecutionOutcome::Succeeded,
        source_changed: false,
        reported_source_change: None,
        obligation_rule_set: active_rule_set_id(&store.connection),
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

/// The same actor id, recorded as the agent words record it, in `session`.
fn as_agent(session: &str) -> crate::ActorContext {
    let mut by = actor(session);
    by.actor_id = "coordinator".into();
    by.actor_kind = crate::work_service::WORD_ACTOR_KIND.into();
    by
}

#[test]
fn continuity_treats_typed_host_evidence_as_no_participation_of_its_own() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let project = ProjectId("continuity-typed-evidence".into());
    let replacement = SessionId("replacement".into());
    let mut request = root_request(&project.0, "earlier-note", 0);
    request.actor = as_agent("earlier");
    request.notes = vec!["Earlier session's note".into()];
    let item = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .unwrap();
    // The replacement session records only typed host evidence under a claim
    // that then lapses: no note, observation or gate of its own. As in the
    // status tests, the real typed evidence persistence helper writes the
    // rows this read selects from, without driving a host control turn.
    let claim = claim(&mut store, &item, &replacement.0, "claim-new", 1, 5);
    let run = crate::storage::work::query::load_work_run(&store.connection, claim.run_id).unwrap();
    let evidence = crate::EnvironmentEvidence {
        schema_version: crate::domain::SCHEMA_VERSION,
        project_id: project.clone(),
        binding: crate::ControlWorkBinding {
            root_execution_id: run.root_execution_id,
            work_id: item.work_id,
            run_id: run.run_id,
            work_revision: claim.accepted_work_revision,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
        },
        session_id: replacement.clone(),
        source_basis: crate::ExecutionSourceBasis {
            workspace_id: "workspace".into(),
            source_revision: "revision-1".into(),
            source_root_generation: None,
            source_root_state: None,
        },
        environment_fingerprint: ObjectId::from_canonical_bytes(b"environment"),
        components: None,
        observed_at: at(2),
        actor: as_agent(&replacement.0),
        recorded_at: at(2),
    };
    let transaction = store.connection.transaction().unwrap();
    crate::storage::work::completion::append_control_environment_evidence_on(
        &transaction,
        &evidence,
    )
    .unwrap();
    transaction.commit().unwrap();
    let own = store
        .work_discovery(&project, &replacement, "coordinator", false, at(10))
        .unwrap();
    assert_eq!(own.items.len(), 0, "typed evidence is not an own row");
    // So it does not hide the earlier session's participation either.
    let continuity = store
        .work_discovery_same_actor(&project, &replacement, "coordinator", at(10))
        .unwrap();
    assert_eq!(continuity.items.len(), 1);
    assert_eq!(continuity.items[0].work.work_id, item.work_id);
    assert_eq!(
        continuity.items[0].note.as_deref(),
        Some("Earlier session's note")
    );
}

#[test]
fn continuity_sql_names_every_shell_default_actor_marker() {
    for literal in [
        crate::domain::DEFAULTED_OS_USER_ACTOR_SOURCE,
        crate::domain::DEFAULTED_PROCESS_ACTOR_SOURCE,
        crate::domain::DEFAULTED_ACTOR_REFERENCE,
    ] {
        assert!(
            ACTOR_CONTINUITY_SQL.contains(&format!("'{literal}'")),
            "the SQL default-marker predicate must match actor_defaulted: {literal}"
        );
    }
}

#[test]
fn continuity_sql_work_does_not_grow_with_unrelated_closed_history() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let project = ProjectId("continuity-project".into());
    let replacement = SessionId("replacement".into());
    for index in 0..2 {
        let mut request = root_request(&project.0, &format!("candidate-{index}"), index);
        request.actor = as_agent("earlier");
        request.notes = vec![format!("Earlier note {index}")];
        store
            .create_work(&request, &DevelopmentNoopRedactor)
            .unwrap();
    }
    let measure = |store: &SqliteStore| {
        let page = store
            .work_discovery_same_actor(&project, &replacement, "coordinator", at(3))
            .unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.omitted, 0);
        page.vm_steps
    };
    let mut before = None;
    for index in 0..=32 {
        let mut request = root_request(&project.0, &format!("closed-{index}"), 0);
        // Closed history by the very same actor, which must not be read.
        request.actor = as_agent("earlier");
        request.notes = (0..16)
            .map(|n| format!("Closed note {index}/{n}"))
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
            // As below: seed a trailing key before measuring.
            before = Some(measure(&store));
        }
    }
    let (before, after) = (before.unwrap(), measure(&store));
    assert!(
        after <= before,
        "closed history increased SQLite VM work: {before} -> {after}"
    );
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
