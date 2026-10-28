use super::*;

#[test]
fn git_tree_both_host_ingestion_routes_refuse_malformed_values_by_name() {
    let observe = execution_observe_frame();
    let checkpoint = serde_json::json!({
        "operation":"turn_checkpoint", "routing_token":"routing", "grant_id":"grant",
        "next_intent":"continue", "idempotency_key":"checkpoint",
        "observations":[{
            "observation_id":"producer", "action_fingerprint":ObjectId::from_canonical_bytes(b"check"),
            "effect":"observe", "outcome":"succeeded", "source_changed":false,
            "source_basis":{"workspace_id":"workspace", "source_revision":"content"},
            "observed_at":"2026-10-02T03:04:00Z"
        }]
    });
    for (frame, pointer) in [
        (observe, "/occurrence/source_change/sighting/source_basis"),
        (checkpoint, "/observations/0/source_basis"),
    ] {
        assert!(parse_host_control_request(&serde_json::to_vec(&frame).unwrap()).is_ok());
        for length in [40, 64] {
            let mut valid = frame.clone();
            valid.pointer_mut(pointer).unwrap()["git_tree"] = Value::String("a".repeat(length));
            assert!(parse_host_control_request(&serde_json::to_vec(&valid).unwrap()).is_ok());
        }
        for invalid in [
            Value::Null,
            Value::String("A".repeat(40)),
            Value::String("private-invalid-tree".into()),
        ] {
            let mut bad = frame.clone();
            bad.pointer_mut(pointer).unwrap()["git_tree"] = invalid;
            let response = parse_frame(Ok(serde_json::to_vec(&bad).unwrap())).unwrap_err();
            let HostControlResponse::Error { error } = response else {
                panic!("invalid tree admitted");
            };
            assert_eq!(error.code, "invalid_request");
            assert!(error.message.contains("git_tree"), "{}", error.message);
            assert!(!error.message.contains("private-invalid-tree"));
        }
    }
}

#[test]
fn git_tree_capability_tracks_the_actual_definition_on_each_successful_response() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("capability.db");
    let mut server = HostControlServer::open_with_host_path_identity(
        database.clone(),
        None,
        ProjectId("capability-project".into()),
        "host-test".into(),
        SessionId("capability-session".into()),
        None,
    )
    .unwrap();
    let bound = server
        .handle(HostControlRequest::SessionBind {
            external_ref: "capability-anchor".into(),
            title: "capability".into(),
            assurance: ControlAssurance::TurnGated,
            mediated_effects: vec![EffectClass::Observe],
            work_binding: None,
            capability_map_revision: 1,
            idempotency_key: "bind".into(),
        })
        .unwrap();
    assert_eq!(
        bound["verification_evidence_git_tree_applicability_v1"],
        true
    );
    let routing_token = bound["routing_token"].as_str().unwrap().to_owned();
    let status = server
        .handle(HostControlRequest::SessionStatus {
            routing_token: routing_token.clone(),
        })
        .unwrap();
    assert_eq!(
        status["verification_evidence_git_tree_applicability_v1"],
        true
    );
    let raw = rusqlite::Connection::open(&database).unwrap();
    raw.execute_batch("DROP INDEX objects_verification_git_tree_applicability_v1")
        .unwrap();
    let status = server
        .handle(HostControlRequest::SessionStatus {
            routing_token: routing_token.clone(),
        })
        .unwrap();
    assert!(
        status
            .get("verification_evidence_git_tree_applicability_v1")
            .is_none()
    );
    raw.execute_batch(
        "CREATE INDEX objects_verification_git_tree_applicability_v1 ON objects(created_at)",
    )
    .unwrap();
    let status = server
        .handle(HostControlRequest::SessionStatus { routing_token })
        .unwrap();
    assert!(
        status
            .get("verification_evidence_git_tree_applicability_v1")
            .is_none()
    );
}
