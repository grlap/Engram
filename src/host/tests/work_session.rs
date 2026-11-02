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

fn retained(connection: &rusqlite::Connection) -> Vec<(String, Vec<u8>)> {
    connection
        .prepare("SELECT object_id, canonical_json FROM objects ORDER BY object_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

#[test]
fn removed_turn_operations_have_no_host_dispatch() {
    for operation in [
        "session_bind",
        "session_status",
        "turn_grant_read",
        "turn_evaluate",
        "turn_begin",
        "turn_checkpoint",
    ] {
        let frame = serde_json::json!({"operation":operation});
        let response = parse_frame(Ok(serde_json::to_vec(&frame).unwrap())).unwrap_err();
        let HostControlResponse::Error { error } = response else {
            panic!("removed operation admitted")
        };
        assert_eq!(error.code, "invalid_request");
    }
}
