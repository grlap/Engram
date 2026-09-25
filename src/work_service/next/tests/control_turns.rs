//! A host's turn gate and the `next` word share a session id but not a
//! delivery. No control operation writes the work session's state, so the
//! page `next` staged is still pending after the session binds, is granted a
//! turn, begins it and reports it, and the next call continues from it.

use super::*;
use crate::domain::{
    ActorContext, AssuranceLevel, ControlAssurance, ControlTurnBeginDecision,
    ControlTurnCheckpointDecision, ControlTurnDecision, EffectClass, TurnIntent, TurnNextIntent,
};

const PROJECT: &str = "control-turns";
const SESSION: &str = "shared";

fn changes_only() -> WorkNextQuery {
    WorkNextQuery {
        sections: vec![WorkNextSection::Changes],
        ..WorkNextQuery::default()
    }
}

/// The whole stored work-session row, column by column.
fn work_session_row(database: &std::path::Path) -> Vec<rusqlite::types::Value> {
    let connection = rusqlite::Connection::open(database).expect("reader");
    let mut statement = connection
        .prepare("SELECT * FROM work_session_state WHERE project_id = ?1 AND session_id = ?2")
        .expect("prepare");
    let columns = statement.column_count();
    statement
        .query_row([PROJECT, SESSION], |row| {
            (0..columns).map(|index| row.get(index)).collect()
        })
        .expect("the work session row")
}

fn host_actor() -> ActorContext {
    ActorContext {
        actor_id: "agent".into(),
        actor_kind: "agent".into(),
        assurance: AssuranceLevel::Asserted,
        run_id: None,
        session_id: Some(SessionId(SESSION.into())),
        source_tool: Some("host-control:session_bind".into()),
        source_skill: None,
        provenance_chain: Vec::new(),
        reason: "bind the host control session".into(),
    }
}

#[test]
fn control_turns_leave_the_sessions_next_delivery_alone() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("engram.sqlite3");
    let service = LocalWorkService::new(
        database.clone(),
        ProjectId(PROJECT.into()),
        "agent".into(),
        SessionId(SESSION.into()),
        None,
    );
    for (title, key) in [("First", "first"), ("Second", "second"), ("Third", "third")] {
        service
            .work_propose(root_input(title, key), at(0))
            .expect("root");
    }
    let staged = service
        .work_next(1, changes_only(), at(1))
        .expect("stage a page")
        .delivered_through
        .expect("a staged page");
    let before = work_session_row(&database);

    let project = ProjectId(PROJECT.into());
    let session = SessionId(SESSION.into());
    let mut store = SqliteStore::open(&database).expect("host store");
    let connection = store
        .resume_control_connection(&session, at(2))
        .expect("host connection");
    let binding = store
        .bind_control_session(
            &project,
            "dummy:CONTROL-TURNS",
            "Turns beside next",
            &session,
            &connection,
            &host_actor(),
            ControlAssurance::TurnGated,
            &[EffectClass::Observe, EffectClass::Communicate],
            1,
            "bind-beside-next",
            at(2),
        )
        .expect("bind");
    let ControlTurnDecision::Grant { grant } = store
        .evaluate_control_turn(
            &project,
            &session,
            &connection,
            &binding.routing_token,
            &TurnIntent {
                idempotency_key: "turn-beside-next".into(),
                intent_fingerprint: crate::ObjectId::from_canonical_bytes(b"turn-beside-next"),
                purpose: None,
                requested_effects: vec![EffectClass::Observe],
                resource_intents: Vec::new(),
            },
            at(3),
        )
        .expect("evaluate")
    else {
        panic!("the turn must be granted");
    };
    assert!(matches!(
        store
            .begin_control_turn(
                &project,
                &session,
                &connection,
                &binding.routing_token,
                &grant.grant_id,
                &[],
                "begin-beside-next",
                at(3),
            )
            .expect("begin"),
        ControlTurnBeginDecision::Begin { .. }
    ));
    assert!(matches!(
        store
            .checkpoint_control_turn(
                &project,
                &session,
                &connection,
                &binding.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                "checkpoint-beside-next",
                at(4),
            )
            .expect("checkpoint"),
        ControlTurnCheckpointDecision::Checkpointed { .. }
    ));
    store
        .control_status(
            &project,
            &session,
            &connection,
            &binding.routing_token,
            at(5),
        )
        .expect("status");
    drop(store);
    assert_eq!(
        work_session_row(&database),
        before,
        "no control operation writes the work session's state"
    );

    let next = service
        .work_next(1, changes_only(), at(6))
        .expect("the next page")
        .delivered_through
        .expect("another staged page");
    assert!(next > staged);
    let confirmed = SqliteStore::open(&database)
        .expect("reader")
        .work_session_state(&project, &session, at(7))
        .expect("session state")
        .project_cursor;
    assert_eq!(
        confirmed, staged,
        "next confirmed the page it staged before"
    );
}
