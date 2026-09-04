use super::*;

#[test]
fn inactive_process_default_sessions_are_reclaimed_atomically_without_live_authority() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("process-default-retention-project".into());
    let cleanup_second = crate::storage::PROCESS_DEFAULT_WORK_SESSION_RETENTION_SECONDS + 100;
    let expired = process_default_session_at(10, at(0));
    let recent = process_default_session_at(11, at(cleanup_second - 10));
    let legacy = SessionId(format!("local-process-12-{}", uuid::Uuid::new_v4()));
    let live = SessionId(format!("local-process-13-{}", uuid::Uuid::new_v4()));
    let handoff = process_default_session_at(14, at(0));
    let bound = process_default_session_at(15, at(0));
    let pending = process_default_session_at(16, at(0));
    let staged = process_default_session_at(17, at(0));
    let stable = SessionId("stable-host-session".into());
    let mut first = SqliteStore::open(&database).expect("first store");
    let binder = SessionId("retention-binder".into());
    first
        .start_task(
            &project,
            "retention-task",
            "Retention task",
            &binder,
            actor(&binder.0),
            at(0),
        )
        .expect("task session bind");
    first
        .join_task(&project, "retention-task", &bound, actor(&bound.0), at(0))
        .expect("explicit process session binding");
    let root = first
        .create_work(
            &root_request(&project.0, "retention-root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root work");
    let live_claim = claim(
        &mut first,
        &root,
        &live.0,
        "live-claim",
        cleanup_second - 10,
        1_000,
    );
    first
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: root.work_id,
                run_id: live_claim.run_id,
                expected_work_revision: root.revision,
                from: live.clone(),
                to: handoff.clone(),
                claim_id: live_claim.claim_id,
                claim_fence: live_claim.fence,
                ttl_seconds: 1_000,
                checkpoint_summary: "retain the open handoff".into(),
                actor: actor(&live.0),
                idempotency_key: "retention-handoff".into(),
                offered_at: at(cleanup_second - 9),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("open handoff");
    for (index, (session, seen_at)) in [
        (&expired, at(0)),
        (&recent, at(cleanup_second - 10)),
        (&legacy, at(0)),
        (&live, at(0)),
        (&handoff, at(0)),
        (&bound, at(0)),
        (&pending, at(0)),
        (&staged, at(0)),
        (&stable, at(0)),
    ]
    .into_iter()
    .enumerate()
    {
        first
            .connection
            .execute(
                "INSERT INTO work_session_state (
                     project_id, session_id, focused_work_id, project_cursor, updated_at_ms
                 ) VALUES (?1, ?2, NULL, 0, ?3)",
                params![project.0, session.0, seen_at.timestamp_millis()],
            )
            .expect("session state");
        first
            .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                project_id: &project,
                session_id: session,
                operation: "work_update:gate",
                idempotency_key: &format!("attempt-{index}"),
                intent: &serde_json::json!({"name": "retention"}),
                basis: &serde_json::json!({"revision": root.revision}),
                now: seen_at,
            })
            .expect("protocol attempt");
        if *session != pending {
            first
                .finish_work_protocol_attempt(
                    &project,
                    session,
                    "work_update:gate",
                    &format!("attempt-{index}"),
                    &serde_json::json!({"receipt": {"work_id": root.work_id}}),
                )
                .expect("completed protocol attempt");
        }
    }
    let staged_payload =
        CanonicalObject::freeze(&serde_json::json!({"staged": true})).expect("staged payload");
    first
        .connection
        .execute(
            "UPDATE work_session_state SET
                 tentative_project_cursor = 0,
                 tentative_delivery_token = 'retained-delivery-token',
                 tentative_delivery_payload_hash = ?3,
                 tentative_delivery_payload = ?4
             WHERE project_id = ?1 AND session_id = ?2",
            params![
                project.0,
                staged.0,
                staged_payload.hash().as_str(),
                staged_payload.bytes()
            ],
        )
        .expect("stage an unconfirmed delivery");
    let bulk_expired = MAX_PROCESS_DEFAULT_SESSION_RECLAIMS_PER_CREATION + 6;
    for index in 0..bulk_expired {
        let session = process_default_session_at(
            1_000 + u32::try_from(index).expect("bounded test pid"),
            at(0),
        );
        first
            .connection
            .execute(
                "INSERT INTO work_session_state (
                     project_id, session_id, focused_work_id, project_cursor, updated_at_ms
                 ) VALUES (?1, ?2, NULL, 0, ?3)",
                params![project.0, session.0, at(0).timestamp_millis()],
            )
            .expect("bulk expired session state");
    }
    let second = SqliteStore::open(&database).expect("second store");

    let first_creator = process_default_session_at(20, at(cleanup_second));
    assert!(
        first
            .initialize_process_default_work_session(&project, &first_creator, at(cleanup_second))
            .expect("session creation triggers bounded reclamation")
    );
    assert_eq!(
        second
            .connection
            .query_row(
                "SELECT COUNT(*) FROM work_session_state WHERE project_id = ?1",
                [project.0.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .expect("remaining session states"),
        i64::try_from(bulk_expired + 10 - MAX_PROCESS_DEFAULT_SESSION_RECLAIMS_PER_CREATION)
            .expect("bounded remaining count")
    );
    assert!(
        !first
            .initialize_process_default_work_session(
                &project,
                &first_creator,
                at(cleanup_second + 1)
            )
            .expect("an existing session never scans reclamation again")
    );
    let second_creator = process_default_session_at(21, at(cleanup_second + 1));
    assert!(
        first
            .initialize_process_default_work_session(
                &project,
                &second_creator,
                at(cleanup_second + 1)
            )
            .expect("second session creation drains the next bounded page")
    );
    assert_eq!(
        second
            .connection
            .query_row(
                "SELECT COUNT(*) FROM work_session_state WHERE project_id = ?1",
                [project.0.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .expect("retained session states"),
        9
    );
    assert_eq!(
        second
            .connection
            .query_row(
                "SELECT COUNT(*) FROM work_protocol_attempts WHERE project_id = ?1",
                [project.0.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .expect("retained protocol attempts"),
        7
    );
    for retained in [
        &recent,
        &live,
        &handoff,
        &bound,
        &pending,
        &staged,
        &stable,
        &first_creator,
        &second_creator,
    ] {
        let count = second
            .connection
            .query_row(
                "SELECT COUNT(*) FROM work_session_state
                 WHERE project_id = ?1 AND session_id = ?2",
                params![project.0, retained.0],
                |row| row.get::<_, i64>(0),
            )
            .expect("retained session state");
        assert_eq!(count, 1, "session {} remains", retained.0);
    }

    let explain =
        format!("EXPLAIN QUERY PLAN {PROCESS_DEFAULT_SESSION_RECLAMATION_CANDIDATES_SQL}");
    let process_default_glob = format!(
        "{}*",
        crate::storage::PROCESS_DEFAULT_WORK_SESSION_NAMESPACE
    );
    let mut statement = first
        .connection
        .prepare(&explain)
        .expect("prepare retention candidate plan");
    let plan = statement
        .query_map(
            params![
                project.0,
                at(1).timestamp_millis(),
                process_default_glob,
                first_creator.0,
                at(cleanup_second).timestamp_millis(),
                i64::try_from(MAX_PROCESS_DEFAULT_SESSION_RECLAIMS_PER_CREATION)
                    .expect("bounded reclamation limit")
            ],
            |row| row.get::<_, String>(3),
        )
        .expect("explain retention candidates")
        .collect::<Result<Vec<_>, _>>()
        .expect("retention candidate plan");
    drop(statement);
    for required_index in [
        "work_session_state_retention",
        "work_claims_holder_live",
        "work_handoff_offer_from_live",
        "work_handoff_offer_to_live",
    ] {
        assert!(
            plan.iter().any(|detail| detail.contains(required_index)),
            "retention plan does not use {required_index}: {plan:?}"
        );
    }
    assert!(
        plan.iter()
            .all(|detail| !detail.contains("USE TEMP B-TREE")),
        "retention candidate discovery sorts through a temporary B-tree: {plan:?}"
    );

    let rollback_candidate = process_default_session_at(30, at(0));
    first
        .connection
        .execute(
            "INSERT INTO work_session_state (
                 project_id, session_id, focused_work_id, project_cursor, updated_at_ms
             ) VALUES (?1, ?2, NULL, 0, ?3)",
            params![project.0, rollback_candidate.0, at(0).timestamp_millis()],
        )
        .expect("rollback candidate");
    let rollback_creator = process_default_session_at(31, at(cleanup_second + 2));
    first
        .connection
        .execute_batch(&format!(
            "CREATE TEMP TRIGGER refuse_retention_creator
             BEFORE INSERT ON work_session_state
             WHEN NEW.session_id = '{}'
             BEGIN
                 SELECT RAISE(ABORT, 'refuse creator after reclamation');
             END;",
            rollback_creator.0
        ))
        .expect("install rollback trigger");
    assert!(
        first
            .initialize_process_default_work_session(
                &project,
                &rollback_creator,
                at(cleanup_second + 2)
            )
            .is_err(),
        "a refused creator must abort the reclamation transaction"
    );
    first
        .connection
        .execute_batch("DROP TRIGGER refuse_retention_creator;")
        .expect("drop rollback trigger");
    for (session, expected) in [(&rollback_candidate, 1_i64), (&rollback_creator, 0_i64)] {
        assert_eq!(
            second
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM work_session_state
                     WHERE project_id = ?1 AND session_id = ?2",
                    params![project.0, session.0],
                    |row| row.get::<_, i64>(0),
                )
                .expect("rollback state count"),
            expected,
            "rollback preserves prior rows and refuses the creator"
        );
    }
    assert!(second.verify_all().expect("integrity report").is_healthy());
}

#[test]
fn prerelease_agent_grant_schema_is_refused_as_a_different_build() {
    let directory = tempfile::tempdir().expect("temporary schema fixture");
    let database = directory.path().join("engram.sqlite3");
    drop(SqliteStore::open(&database).expect("create current store"));
    let connection = Connection::open(&database).expect("open schema fixture");
    connection
        .execute(
            "CREATE TABLE work_authority_grants (obsolete TEXT NOT NULL)",
            [],
        )
        .expect("inject prerelease grant table");
    drop(connection);

    let Err(error) = SqliteStore::open(&database) else {
        panic!("obsolete grant schema must refuse");
    };
    assert!(
        error.to_string().contains("different Engram build"),
        "unexpected refusal: {error}"
    );
}
