use super::*;

fn source_at_limit() -> crate::source::SourceId {
    let mut source = crate::source::SourceId {
        commit: "a".repeat(40),
        tree: "b".repeat(40),
        submodules: vec![],
        changes: vec![crate::source::Change {
            path: "p".into(),
            status: " M".into(),
            origin: None,
            blob: Some("c".repeat(40)),
            mode: Some(crate::source::Mode::Regular),
        }],
    };
    let initial = serde_json::to_vec(&source).unwrap().len();
    source.changes[0].path = "p".repeat(crate::source::MAX_JSON_BYTES - initial + 1);
    assert_eq!(
        serde_json::to_vec(&source).unwrap().len(),
        crate::source::MAX_JSON_BYTES
    );
    source.validate().unwrap();
    source
}

fn native_frame(source: &crate::source::SourceId) -> Value {
    let now = Utc::now();
    serde_json::json!({
        "operation":"verification_record", "operation_key":"registration",
        "registration": {
            "host_run_id":"run", "request_identity":"request", "registered_at":now,
            "initiating_session_id":"session-test", "binding":execution_observe_frame()["binding"],
            "root":{"workspace_id":"workspace", "generation":1, "state":"named"}
        },
        "input":{"kind":"source_id", "source_id":source},
        "run":{"mode":"focused", "command":"cargo test", "started_at":now,
            "finished_at":now, "outcome":"passed"},
        "checks":[], "post_run":{"source_id":source}
    })
}

#[test]
fn native_registration_retains_two_owner_sized_objects() {
    let source = source_at_limit();
    let mut bytes = serde_json::to_vec(&native_frame(&source)).unwrap();
    assert!(bytes.len() > MAX_HOST_CONTROL_FRAME_BYTES);
    bytes.push(b'\n');
    assert!(bytes.len() <= crate::domain::MAX_VERIFICATION_RECORD_FRAME_BYTES);
    let mut reader = Cursor::new(bytes);
    let request = parse_frame(read_control_frame(&mut reader).unwrap().unwrap()).unwrap();
    let HostControlRequest::VerificationRecord {
        input, post_run, ..
    } = request
    else {
        panic!("native registration");
    };
    assert_eq!(
        input,
        crate::domain::RegisteredVerificationInput::SourceId {
            source_id: source.clone()
        }
    );
    assert_eq!(
        *post_run,
        crate::domain::RegisteredVerificationPostRun::SourceId { source_id: source }
    );
}

#[test]
fn native_frame_limit_counts_the_terminator_and_drains_overflow() {
    let limit = crate::domain::MAX_VERIFICATION_RECORD_FRAME_BYTES;
    let mut bytes = serde_json::to_vec(&native_frame(&source_at_limit())).unwrap();
    bytes.resize(limit - 1, b' ');
    bytes.push(b'\n');
    let mut at_limit = Cursor::new(bytes.clone());
    assert!(parse_frame(read_control_frame(&mut at_limit).unwrap().unwrap()).is_ok());
    bytes.insert(limit - 1, b' ');
    bytes.extend_from_slice(b"{\"operation\":\"work_session_read\"}\n");
    let mut overflow = Cursor::new(bytes);
    assert!(read_control_frame(&mut overflow).unwrap().unwrap().is_err());
    assert!(matches!(
        parse_frame(read_control_frame(&mut overflow).unwrap().unwrap()),
        Ok(HostControlRequest::WorkSessionRead {})
    ));
}

#[test]
fn native_allowance_does_not_expand_remote_or_ordinary_operations() {
    for operation in ["remote_verification_record", "named_root_read"] {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "operation":operation, "caller_payload":"secret".repeat(MAX_HOST_CONTROL_FRAME_BYTES / 6)
        })).unwrap();
        let HostControlResponse::Error { error } = parse_frame(Ok(bytes)).unwrap_err() else {
            panic!("bounded refusal");
        };
        assert!(error.message.contains("ordinary operation exceeds"));
        assert!(!error.message.contains("secret"));
        assert!(error.message.len() < MAX_HOST_CONTROL_ERROR_DETAIL_BYTES);
    }
}
