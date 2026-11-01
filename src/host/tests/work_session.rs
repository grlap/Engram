use super::*;

fn request(server: &mut HostControlServer, frame: Value) -> Value {
    server
        .handle(serde_json::from_value(frame).unwrap())
        .unwrap()
}

#[test]
fn work_session_capabilities_track_the_actual_git_tree_barrier() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("capability.db");
    let mut server = HostControlServer::open_work_session_with_host_path_identity(
        &database,
        None,
        ProjectId("capability-project".into()),
        "runner".into(),
        SessionId("ordinary-capability".into()),
        None,
    )
    .unwrap();
    let raw = rusqlite::Connection::open(&database).unwrap();
    for (change, available) in [
        (None, true),
        (
            Some("DROP INDEX objects_verification_git_tree_applicability_v1"),
            false,
        ),
        (
            Some(
                "CREATE INDEX objects_verification_git_tree_applicability_v1 ON objects(created_at)",
            ),
            false,
        ),
    ] {
        if let Some(change) = change {
            raw.execute_batch(change).unwrap();
        }
        let before = retained(&raw);
        let context = request(
            &mut server,
            serde_json::json!({"operation":"work_session_read"}),
        );
        assert_eq!(
            context["capabilities"],
            serde_json::json!(["host_work_v1", "verification_record_v1"])
        );
        assert_eq!(context["work_session"], true);
        if available {
            assert_eq!(
                context["verification_evidence_git_tree_applicability_v1"],
                true
            );
        } else {
            assert!(
                context
                    .get("verification_evidence_git_tree_applicability_v1")
                    .is_none()
            );
        }
        assert_eq!(retained(&raw), before, "capability reads change no history");
    }
}

fn retained(connection: &rusqlite::Connection) -> Vec<(String, Vec<String>)> {
    [
        "control_sessions",
        "control_turn_grants",
        "control_turn_results",
        "control_operation_results",
        "control_turn_grant_supersessions",
    ]
    .into_iter()
    .map(|table| {
        let mut statement = connection
            .prepare(&format!("SELECT * FROM {table}"))
            .unwrap();
        let columns = statement.column_count();
        let mut rows = statement
            .query_map([], |row| {
                (0..columns)
                    .map(|column| row.get_ref(column).map(|value| format!("{value:?}")))
                    .collect::<Result<Vec<_>, _>>()
                    .map(|row| format!("{row:?}"))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows.sort();
        (table.into(), rows)
    })
    .collect()
}

#[test]
fn work_session_startup_preserves_real_issued_begun_and_exited_control_records() {
    for state in ["issued", "begun", "exited"] {
        let directory = crate::test_support::temp_home().unwrap();
        let database = directory.path().join("host.db");
        let project = ProjectId("work-project".into());
        let session = SessionId("work-host".into());
        let mut legacy = HostControlServer::open(
            &database,
            project.clone(),
            "runner".into(),
            session.clone(),
            None,
        )
        .unwrap();
        let bound = request(
            &mut legacy,
            serde_json::json!({
                "operation":"session_bind", "external_ref":"work-host", "title":"host",
                "assurance":"turn_gated", "mediated_effects":["observe"],
                "capability_map_revision":1, "idempotency_key":"old-bind"
            }),
        );
        let routing = &bound["routing_token"];
        let decision = request(
            &mut legacy,
            serde_json::json!({
                "operation":"turn_evaluate", "routing_token":routing,
                "idempotency_key":"old-evaluate",
                "intent_fingerprint":ObjectId::from_canonical_bytes(b"old-turn").as_str(),
                "requested_effects":["observe"]
            }),
        );
        let grant = &decision["grant"]["grant_id"];
        assert!(grant.is_string(), "{decision}");
        if state != "issued" {
            request(
                &mut legacy,
                serde_json::json!({
                    "operation":"turn_begin", "routing_token":routing, "grant_id":grant,
                    "idempotency_key":"old-begin"
                }),
            );
        }
        if state == "exited" {
            request(
                &mut legacy,
                serde_json::json!({
                    "operation":"turn_checkpoint", "routing_token":routing, "grant_id":grant,
                    "next_intent":"exit", "idempotency_key":"old-checkpoint"
                }),
            );
        }
        drop(legacy);
        let observer = rusqlite::Connection::open(&database).unwrap();
        let before = retained(&observer);
        let mut ordinary = HostControlServer::open_work_session_with_host_path_identity(
            &database,
            Some(HostPathPolicy::host_default()),
            project.clone(),
            "runner".into(),
            session.clone(),
            None,
        )
        .unwrap();
        let context = request(
            &mut ordinary,
            serde_json::json!({"operation":"work_session_read"}),
        );
        assert_eq!(
            context["project_id"],
            serde_json::to_value(project).unwrap()
        );
        assert_eq!(
            context["session_id"],
            serde_json::to_value(session).unwrap()
        );
        assert_eq!(context["work_session"], true);
        let refused = ordinary.handle(
            serde_json::from_value(serde_json::json!({
                "operation":"turn_begin", "routing_token":routing, "grant_id":grant,
                "idempotency_key":"old-begin"
            }))
            .unwrap(),
        );
        assert!(
            matches!(refused, Err(StoreError::InvalidControlSession(message))
            if message.contains("turn admission is unavailable"))
        );
        assert_eq!(
            retained(&observer),
            before,
            "{state}: open/read/refusal changed history"
        );
    }
}
