use std::io::Cursor;

use super::*;

#[test]
fn turn_grant_read_round_trips_on_the_host_channel() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let session_id = SessionId("grant-read-host".into());
    let connection_token = store
        .resume_control_connection(&session_id, Utc::now())
        .unwrap();
    let mut server = HostControlServer {
        store,
        project_id: ProjectId("grant-read-project".into()),
        actor_id: "host-test".into(),
        session_id,
        connection_token,
        source_skill: None,
        actor_context: None,
        actor_context_normalized: false,
    };
    let bound = server
        .handle(HostControlRequest::SessionBind {
            external_ref: "grant-read-anchor".into(),
            title: "grant read".into(),
            assurance: ControlAssurance::TurnGated,
            mediated_effects: vec![EffectClass::Observe],
            work_binding: None,
            capability_map_revision: 1,
            idempotency_key: "bind".into(),
        })
        .unwrap();
    let routing = bound["routing_token"].as_str().unwrap();
    let decision = server
        .handle(HostControlRequest::TurnEvaluate {
            routing_token: routing.into(),
            idempotency_key: "evaluate".into(),
            intent_fingerprint: ObjectId::from_canonical_bytes(b"evaluate").as_str().into(),
            purpose: None,
            requested_effects: vec![EffectClass::Observe],
            resource_intents: vec![],
        })
        .unwrap();
    let grant_id = decision["grant"]["grant_id"].as_str().unwrap();
    let request = |id: &str| serde_json::json!({"operation":"turn_grant_read","routing_token":routing,"grant_id":id});
    let frames = [
        request(grant_id),
        request("missing"),
        request(" "),
        serde_json::json!({"operation":"turn_grant_read","routing_token":routing,"grant_id":7}),
        serde_json::json!({"operation":"turn_grant_read","routing_token":routing,"grant_id":"missing","idempotency_key":"extra"}),
    ];
    let mut input = Vec::new();
    for frame in &frames {
        serde_json::to_writer(&mut input, frame).unwrap();
        input.push(b'\n');
    }
    let mut output = Vec::new();
    server.serve(Cursor::new(input), &mut output).unwrap();
    let replies = output
        .split(|b| *b == b'\n')
        .filter(|b| !b.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(replies[0]["status"], "ok");
    assert_eq!(
        replies[0]["result"],
        serde_json::json!({"control_schema_version":crate::CONTROL_SCHEMA_VERSION,"session_id":server.session_id,"grant_id":grant_id,"status":"found","state":"issued","begun_at":null,"completed_at":null})
    );
    assert_eq!(
        replies[1]["result"],
        serde_json::json!({"control_schema_version":crate::CONTROL_SCHEMA_VERSION,"session_id":server.session_id,"grant_id":"missing","status":"not_found"})
    );
    assert_eq!(replies[2]["error"]["code"], "invalid_turn_grant_id");
    assert_eq!(replies[3]["error"]["code"], "invalid_request");
    assert_eq!(replies[4]["error"]["code"], "invalid_request");
    assert_eq!(
        store_error_code(&StoreError::ControlTurnGrantSessionMismatch),
        "turn_grant_session_mismatch"
    );
    assert!(matches!(
        serde_json::from_value::<HostControlRequest>(
            serde_json::json!({"operation":"session_status","routing_token":routing})
        )
        .unwrap(),
        HostControlRequest::SessionStatus { .. }
    ));
}

#[test]
fn different_build_refusal_preserves_host_wire_code() {
    let error = StoreError::DifferentBuildSchema;
    assert_eq!(store_error_code(&error), "control_projection_invalid");
    assert!(!error.to_string().contains("invalid data"));
}

#[test]
fn moved_evaluation_basis_preserves_both_host_wire_codes() {
    for (moved, expected) in [
        (
            crate::EvaluationBasisMove::CheckRecorded,
            "acceptance_evaluation_resubmit",
        ),
        (
            crate::EvaluationBasisMove::SourceChanged,
            "acceptance_evaluation_void",
        ),
    ] {
        let error = StoreError::AcceptanceEvaluationBasisMoved {
            work: crate::WorkId::new(),
            moved,
            reason: "evaluation basis moved".into(),
            observation: None,
        };
        let response = HostControlResponse::Error {
            error: HostControlErrorBody {
                code: store_error_code(&error),
                message: error.to_string(),
                details: store_error_details(&error),
            },
        };
        let wire = serde_json::to_value(response).expect("host response");
        assert_eq!(wire["error"]["code"], expected);
    }
}

/// The bind request parses with every field named and refuses an unknown
/// one; its refusal carries the typed parts as `details`, which every
/// other error leaves out.
#[test]
fn verification_bind_request_and_refusal_wire_shape() {
    let id = crate::ObjectId::from_canonical_bytes(b"a record");
    let binding = crate::domain::ControlWorkBinding {
        root_execution_id: crate::domain::RootExecutionId::new(),
        work_id: crate::WorkId::new(),
        run_id: crate::domain::WorkRunId::new(),
        work_revision: 3,
        claim_id: crate::domain::WorkClaimId::new(),
        claim_fence: 1,
    };
    let mut request = serde_json::json!({
        "operation": "verification_bind",
        "routing_token": "routing",
        "idempotency_key": "bind-1",
        "original": id,
        "measurement": {
            "workspace_id": "root",
            "source_revision": "R1",
            "measured_at": "2026-10-05T07:00:00Z",
        },
        "targets": [{
            "binding": binding,
            "sighting": {"observation": id, "source_revision": "R1"},
            "criteria": [1],
        }],
    });
    let parsed: HostControlRequest =
        serde_json::from_value(request.clone()).expect("the request parses");
    assert_eq!(parsed.operation(), "verification_bind");
    request["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<HostControlRequest>(request).is_err());

    let refusal = crate::domain::VerificationBindRefusal {
        original: None,
        request: None,
        targets: vec![crate::domain::VerificationBindTargetRefusal {
            work_id: binding.work_id,
            work_ref: "w-target".into(),
            reason: crate::domain::VerificationBindTargetReason::SightingNotNewest,
            expected: None,
            actual: None,
            remedy: Some("obtain genuine source accounting".into()),
        }],
    };
    let error = StoreError::VerificationBindRefused(Box::new(refusal));
    let wire = |error: &StoreError| {
        serde_json::to_value(HostControlResponse::Error {
            error: HostControlErrorBody {
                code: store_error_code(error),
                message: error.to_string(),
                details: store_error_details(error),
            },
        })
        .expect("host response")
    };
    let refused = wire(&error);
    assert_eq!(refused["error"]["code"], "verification_bind_refused");
    assert_eq!(
        refused["error"]["details"]["targets"][0]["reason"],
        "sighting_not_newest"
    );
    let other = wire(&StoreError::DifferentBuildSchema);
    assert!(other["error"].get("details").is_none(), "{other}");
}

#[test]
fn oversized_control_frame_is_rejected_and_drained() {
    let mut input = vec![b'x'; MAX_HOST_CONTROL_FRAME_BYTES + 1];
    input.extend_from_slice(b"\n{}\n");
    let mut output = Vec::new();
    let mut server = HostControlServer {
        store: SqliteStore::open_in_memory().expect("store"),
        project_id: ProjectId("frame-project".into()),
        actor_id: "frame-agent".into(),
        session_id: SessionId("frame-session".into()),
        connection_token: "frame-connection".into(),
        source_skill: None,
        actor_context: None,
        actor_context_normalized: false,
    };

    server
        .serve(Cursor::new(input), &mut output)
        .expect("serve bounded frames");
    let responses = String::from_utf8(output)
        .expect("response UTF-8")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("response JSON"))
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["error"]["code"], "invalid_request");
    assert!(
        responses[0]["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("exceeds"))
    );
    assert_eq!(responses[1]["error"]["code"], "invalid_request");
}

#[test]
fn removed_host_obligation_waiver_is_refused() {
    let clean = serde_json::json!({
        "operation": "obligation_waive",
        "routing_token": "routing-token",
        "obligation_id": uuid::Uuid::nil().to_string(),
        "expected_definition": "a".repeat(64),
        "waived_by": "operator",
        "reason": "reviewed exception",
        "idempotency_key": "waive-once"
    });
    let error = parse_host_control_request(&serde_json::to_vec(&clean).expect("encode frame"))
        .expect_err("removed operation must fail closed");
    assert!(error.contains("obligation_waive"));
    assert!(error.contains("unknown variant"));
}

#[test]
fn typed_evaluation_admission_preserves_host_private_error_code() {
    let cause = serde_json::from_value(serde_json::json!({
        "kind": "eligibility",
        "mismatch": "mode_disallowed",
        "requested_mode": "same_session",
        "task_mark": null,
        "admitted_modes": ["independent_session"],
        "remedy": "request_eligible_evaluation",
    }))
    .expect("typed admission cause");
    let error = StoreError::AcceptanceEvaluationAdmissionRefused {
        work: crate::domain::WorkId::new(),
        reason: "unchanged admission reason".into(),
        cause: Box::new(cause),
    };
    assert_eq!(store_error_code(&error), "storage_error");
    // A standing blocking evaluation answers as an evaluation refusal.
    let reroll = serde_json::from_value(serde_json::json!({
        "kind": "reroll",
        "mismatch": "blocking_evaluation_stands",
        "evaluation": "0".repeat(32),
        "feed": {"kind": "run_execution", "id": uuid::Uuid::nil()},
        "after_position": 3,
        "through_position": 5,
        "criterion": 1,
        "verdict": "fail",
        "remedy": "record_new_evidence_then_evaluate",
    }))
    .expect("typed re-roll cause");
    let error = StoreError::AcceptanceEvaluationAdmissionRefused {
        work: crate::domain::WorkId::new(),
        reason: "unchanged re-roll reason".into(),
        cause: Box::new(reroll),
    };
    assert_eq!(store_error_code(&error), "acceptance_evaluation_refused");
}

/// The documented `execution_observe` frame: an unadmitted turn with a
/// source change and one check, its cause asserted by the host.
fn execution_observe_frame() -> Value {
    serde_json::json!({
        "operation": "execution_observe",
        "routing_token": "routing-token",
        "idempotency_key": "termal-turn-17-observed",
        "binding": {
            "root_execution_id": uuid::Uuid::new_v4(),
            "work_id": uuid::Uuid::new_v4(),
            "run_id": uuid::Uuid::new_v4(),
            "work_revision": 3,
            "claim_id": uuid::Uuid::new_v4(),
            "claim_fence": 2
        },
        "root_basis": {
            "capture_run_cut": 12,
            "latest_event": null,
            "state": {"state": "none"}
        },
        "observed_interval": {
            "from": "2026-10-02T03:00:00Z",
            "through": "2026-10-02T03:05:00Z"
        },
        "occurrence": {
            "kind": "unadmitted_turn",
            "host_turn_ref": "termal-turn-17",
            "source_change": {
                "detection": "content_comparison",
                "workspace_id": "workspace-A",
                "baseline": {
                    "workspace_id": "workspace-A",
                    "source_revision": "rev-a",
                    "observed_at": "2026-10-02T03:00:00Z"
                },
                "sighting": {
                    "source_basis": {
                        "workspace_id": "workspace-A",
                        "source_revision": "rev-b"
                    },
                    "observed_at": "2026-10-02T03:04:00Z"
                }
            },
            "observed_checks": [{
                "host_check_id": "cargo-test",
                "check_kind": "test",
                "observed_result": "passed",
                "started_at": "2026-10-02T03:01:00Z",
                "finished_at": "2026-10-02T03:03:00Z",
                "observed_at": "2026-10-02T03:03:00Z",
                "host_evidence_ref": "termal://check-log/17"
            }]
        },
        "causality": {
            "kind": "host_assertion",
            "claimed_actor": {
                "actor_id": "greg/claude",
                "actor_kind": "agent",
                "assurance": "asserted",
                "run_id": null,
                "session_id": "session-7284",
                "source_tool": null,
                "source_skill": null,
                "provenance_chain": [],
                "reason": "the host saw the session's terminal"
            },
            "basis": "terminal ownership"
        },
        "policy_basis": {"mode": "audit_only"}
    })
}

#[test]
fn execution_observe_frame_parses_strictly() {
    let frame = execution_observe_frame();
    let parsed = parse_host_control_request(&serde_json::to_vec(&frame).expect("frame"))
        .expect("the documented frame parses");
    assert_eq!(parsed.operation(), "execution_observe");
    assert!(matches!(
        parsed,
        HostControlRequest::ExecutionObserve {
            ref occurrence,
            causality: crate::domain::ObservationCausality::HostAssertion { .. },
            ref policy_basis,
            ..
        } if matches!(**occurrence, crate::domain::ObservedOccurrence::UnadmittedTurn { .. })
            && **policy_basis == crate::domain::ObservationPolicyBasis::AuditOnly {}
    ));
    // Nothing a caller sends can claim credit, a grant or verified cause.
    for (pointer, field, value) in [
        (
            "/occurrence/observed_checks/0",
            "credit",
            serde_json::json!("credited"),
        ),
        ("", "grant_id", serde_json::json!("grant-1")),
        ("/occurrence", "turn_status", serde_json::json!("succeeded")),
    ] {
        let mut altered = frame.clone();
        altered
            .pointer_mut(pointer)
            .and_then(Value::as_object_mut)
            .expect("object")
            .insert(field.into(), value);
        let error = parse_host_control_request(&serde_json::to_vec(&altered).expect("frame"))
            .expect_err("an unknown field is refused");
        assert!(error.contains(field), "{error}");
    }
    let mut verified = frame;
    verified["causality"] = serde_json::json!({"kind": "verified"});
    assert!(parse_host_control_request(&serde_json::to_vec(&verified).expect("frame")).is_err());
}

// Fields the request types reuse are as strict as the rest: an unknown
// field inside the root state, the asserted actor or one of its
// provenance links is refused, never dropped.
#[test]
fn execution_observe_refuses_unknown_nested_fields() {
    let frame = execution_observe_frame();
    for (pointer, field) in [
        ("/root_basis/state", "generation_hint"),
        ("/causality/claimed_actor", "verified_by"),
    ] {
        let mut altered = frame.clone();
        altered
            .pointer_mut(pointer)
            .and_then(Value::as_object_mut)
            .expect("object")
            .insert(field.into(), serde_json::json!("x"));
        let error = parse_host_control_request(&serde_json::to_vec(&altered).expect("frame"))
            .expect_err("an unknown nested field is refused");
        assert!(error.contains(field), "{error}");
    }
    let mut linked = frame;
    linked["causality"]["claimed_actor"]["provenance_chain"] = serde_json::json!([
        {"relation": "asserted_by", "source": "host", "reference": null, "weight": 1}
    ]);
    let error = parse_host_control_request(&serde_json::to_vec(&linked).expect("frame"))
        .expect_err("an unknown provenance field is refused");
    assert!(error.contains("weight"), "{error}");
}

// A duplicate key anywhere in the request is refused, as it is for typed
// fields, never collapsed to its last value.
#[test]
fn execution_observe_refuses_duplicate_nested_keys() {
    let mut frame = execution_observe_frame();
    frame["causality"]["claimed_actor"]["provenance_chain"] = serde_json::json!([
        {"relation": "asserted_by", "source": "host", "reference": null}
    ]);
    let text = serde_json::to_string(&frame).expect("frame");
    for (needle, doubled) in [
        (
            r#""state":{"state":"none"}"#,
            r#""state":{"state":"none","state":"none"}"#,
        ),
        (
            r#""actor_id":"greg/claude""#,
            r#""actor_id":"someone-else","actor_id":"greg/claude""#,
        ),
        (
            r#""relation":"asserted_by""#,
            r#""relation":"asserted_by","relation":"asserted_by""#,
        ),
    ] {
        assert_eq!(text.matches(needle).count(), 1, "{needle}");
        let altered = text.replacen(needle, doubled, 1);
        let error =
            parse_host_control_request(altered.as_bytes()).expect_err("a duplicate key is refused");
        assert!(error.contains("duplicate"), "{needle}: {error}");
    }
}

// A field beside a field-less tag is refused too, and the field-less
// shapes serialize exactly as their tag.
#[test]
fn execution_observe_refuses_fields_beside_a_bare_tag() {
    let frame = execution_observe_frame();
    for (pointer, bare, extra) in [
        (
            "/causality",
            serde_json::json!({"kind": "unknown"}),
            ("basis", serde_json::json!("dropped cause")),
        ),
        (
            "/policy_basis",
            serde_json::json!({"mode": "audit_only"}),
            ("project_policy_epoch", serde_json::json!(1)),
        ),
    ] {
        let mut accepted = frame.clone();
        *accepted.pointer_mut(pointer).expect("field") = bare.clone();
        parse_host_control_request(&serde_json::to_vec(&accepted).expect("frame"))
            .expect("the bare tag parses");
        let mut altered = accepted;
        altered
            .pointer_mut(pointer)
            .and_then(Value::as_object_mut)
            .expect("object")
            .insert(extra.0.into(), extra.1);
        let error = parse_host_control_request(&serde_json::to_vec(&altered).expect("frame"))
            .expect_err("a field beside a bare tag is refused");
        assert!(error.contains(extra.0), "{error}");
    }
    assert_eq!(
        serde_json::to_value(crate::domain::ObservationCausality::Unknown {}).expect("json"),
        serde_json::json!({"kind": "unknown"})
    );
    assert_eq!(
        serde_json::to_value(crate::domain::ObservationPolicyBasis::AuditOnly {}).expect("json"),
        serde_json::json!({"mode": "audit_only"})
    );
}

#[test]
fn the_frame_discriminator_reads_only_the_operation() {
    let frame = serde_json::to_vec(&execution_observe_frame()).expect("frame");
    assert_eq!(
        frame_operation(&frame).as_deref(),
        Some("execution_observe")
    );
    assert_eq!(frame_operation(b"[1, 2]"), None);
    assert_eq!(frame_operation(b"{\"routing_token\": \"r\"}"), None);
    assert_eq!(frame_operation(b"{\"operation\": "), None);
}

#[test]
fn an_oversized_execution_observe_frame_is_refused_before_decoding() {
    let mut frame = execution_observe_frame();
    frame["occurrence"]["host_turn_ref"] =
        Value::String("x".repeat(crate::domain::MAX_EXECUTION_OBSERVE_REQUEST_BYTES));
    let bytes = serde_json::to_vec(&frame).expect("frame");
    assert!(bytes.len() < MAX_HOST_CONTROL_FRAME_BYTES);
    let refused = parse_frame(Ok(bytes)).expect_err("refused");
    let HostControlResponse::Error { error } = refused else {
        panic!("an error response");
    };
    assert_eq!(error.code, "invalid_request");
    assert!(
        error.message.contains("execution_observe"),
        "{}",
        error.message
    );
    assert!(error.message.contains("65536"), "{}", error.message);
}

#[test]
fn execution_observation_refusals_answer_with_distinct_codes() {
    for (error, code) in [
        (
            StoreError::ExecutionObservationInvalid("shape".into()),
            "execution_observation_invalid",
        ),
        (
            StoreError::ExecutionObservationBasisMismatch("basis".into()),
            "execution_observation_basis_mismatch",
        ),
        (
            StoreError::ExecutionObservationPolicyBasisMismatch("policy".into()),
            "execution_observation_policy_basis_mismatch",
        ),
    ] {
        assert_eq!(store_error_code(&error), code);
    }
}

#[test]
fn named_root_binding_frame_carries_claim_and_workspace_identity() {
    let frame = serde_json::json!({
        "operation": "named_root_bind",
        "routing_token": "routing-token",
        "claim_id": uuid::Uuid::new_v4(),
        "claim_fence": 4,
        "workspace_id": r"\\?\C:\source-root",
        "generation": 7,
        "named_at": "2026-09-28T00:00:00Z",
        "kind": "bound",
        "idempotency_key": "claim-generation-7-bound"
    });
    let parsed = parse_host_control_request(&serde_json::to_vec(&frame).expect("frame"))
        .expect("host accepts the claim-scoped frame");
    assert!(matches!(
        parsed,
        HostControlRequest::NamedRootBind { workspace_id, generation: 7, .. }
            if workspace_id == r"\\?\C:\source-root"
    ));
    let mut altered = frame;
    altered["source_path"] = serde_json::json!("C:\\source-root");
    let error = parse_host_control_request(&serde_json::to_vec(&altered).expect("frame"))
        .expect_err("a path alias cannot stand in for the host workspace identity");
    assert!(error.contains("source_path"));
}

#[test]
fn acceptance_binding_read_frame_takes_no_caller_cut() {
    let frame = serde_json::json!({
        "operation": "acceptance_binding_read",
        "routing_token": "routing-token",
        "work_id": uuid::Uuid::new_v4(),
        "expected_work_revision": 3,
        "run_id": uuid::Uuid::new_v4(),
    });
    let parsed = parse_host_control_request(&serde_json::to_vec(&frame).expect("frame"))
        .expect("a first page names no continuation");
    assert!(matches!(
        parsed,
        HostControlRequest::AcceptanceBindingRead {
            expected_work_revision: 3,
            after: None,
            ..
        }
    ));
    let mut continued = frame.clone();
    continued["after"] = serde_json::json!("abr1-00");
    assert!(matches!(
        parse_host_control_request(&serde_json::to_vec(&continued).expect("frame"))
            .expect("a continuation"),
        HostControlRequest::AcceptanceBindingRead { after: Some(ref token), .. }
            if token == "abr1-00"
    ));
    let mut cut = frame;
    cut["run_cut"] = serde_json::json!(12);
    let error = parse_host_control_request(&serde_json::to_vec(&cut).expect("frame"))
        .expect_err("the first page captures the cut; a caller cannot name one");
    assert!(error.contains("run_cut"), "{error}");
}

#[test]
fn named_root_sighting_read_frame_needs_no_routing_token() {
    let frame = serde_json::json!({
        "operation": "named_root_sighting_read",
        "work_ref": "w-0123456789ab",
        "run_id": uuid::Uuid::new_v4(),
    });
    let parsed = parse_host_control_request(&serde_json::to_vec(&frame).expect("frame"))
        .expect("the read takes no routing token");
    assert!(matches!(
        parsed,
        HostControlRequest::NamedRootSightingRead { ref work_ref, run_cut: None, .. }
            if work_ref == "w-0123456789ab"
    ));
    assert_eq!(parsed.operation(), "named_root_sighting_read");
    let mut at_cut = frame.clone();
    at_cut["run_cut"] = serde_json::json!(12);
    assert!(matches!(
        parse_host_control_request(&serde_json::to_vec(&at_cut).expect("frame"))
            .expect("a caller cut"),
        HostControlRequest::NamedRootSightingRead {
            run_cut: Some(12),
            ..
        }
    ));
    let mut token = frame;
    token["routing_token"] = serde_json::json!("routing-token");
    let error = parse_host_control_request(&serde_json::to_vec(&token).expect("frame"))
        .expect_err("an unknown field is refused");
    assert!(error.contains("routing_token"), "{error}");
}

#[test]
fn named_root_sighting_read_refusals_answer_with_distinct_codes() {
    use crate::domain::NamedRootSightingReadRefusal as Refusal;
    let refusals = [
        Refusal::InvalidWorkRef,
        Refusal::WrongRun,
        Refusal::InvalidCut,
        Refusal::ResponseTooLarge,
    ];
    let codes: std::collections::BTreeSet<&str> = refusals
        .iter()
        .map(|refusal| {
            store_error_code(&StoreError::NamedRootSightingReadRefused {
                refusal: *refusal,
                reason: "reason".into(),
            })
        })
        .collect();
    assert_eq!(codes.len(), refusals.len());
    assert!(
        codes
            .iter()
            .all(|code| code.starts_with("named_root_sighting_read_"))
    );
}

#[test]
fn acceptance_binding_read_refusals_answer_with_distinct_codes() {
    use crate::domain::AcceptanceBindingReadRefusal as Refusal;
    let refusals = [
        Refusal::UnknownWork,
        Refusal::WrongProject,
        Refusal::WrongRevision,
        Refusal::WrongRun,
        Refusal::StaleCut,
        Refusal::InvalidCursor,
        Refusal::CursorBasisMismatch,
        Refusal::PageTooLarge,
    ];
    let codes: std::collections::BTreeSet<&str> = refusals
        .iter()
        .map(|refusal| {
            store_error_code(&StoreError::AcceptanceBindingReadRefused {
                refusal: *refusal,
                reason: "reason".into(),
            })
        })
        .collect();
    assert_eq!(codes.len(), refusals.len());
    assert!(
        codes
            .iter()
            .all(|code| code.starts_with("acceptance_binding_read_"))
    );
}

#[test]
fn acceptance_verification_read_frame_names_its_cut_and_criterion_strictly() {
    let frame = serde_json::json!({
        "operation": "acceptance_verification_read",
        "routing_token": "routing-token",
        "work_id": uuid::Uuid::new_v4(),
        "expected_work_revision": 3,
        "run_id": uuid::Uuid::new_v4(),
        "run_cut": 12,
        "criterion": 2,
    });
    let parsed = parse_host_control_request(&serde_json::to_vec(&frame).expect("frame"))
        .expect("a first page names no continuation");
    assert_eq!(parsed.operation(), "acceptance_verification_read");
    assert!(matches!(
        parsed,
        HostControlRequest::AcceptanceVerificationRead {
            expected_work_revision: 3,
            run_cut: 12,
            criterion: 2,
            after: None,
            ..
        }
    ));
    let mut continued = frame.clone();
    continued["after"] = serde_json::json!("avr1-00");
    assert!(matches!(
        parse_host_control_request(&serde_json::to_vec(&continued).expect("frame"))
            .expect("a continuation"),
        HostControlRequest::AcceptanceVerificationRead { after: Some(ref token), .. }
            if token == "avr1-00"
    ));
    // The cut and criterion are required; a negative criterion, a
    // caller-chosen kind and a caller-chosen project are wire errors.
    let mut negative = frame.clone();
    negative["criterion"] = serde_json::json!(-1);
    let error = parse_host_control_request(&serde_json::to_vec(&negative).expect("frame"))
        .expect_err("a negative criterion");
    assert!(error.contains("expected usize"), "{error}");
    for field in ["check_kind", "project_id"] {
        let mut altered = frame.clone();
        altered[field] = serde_json::json!("caller-chosen");
        let error = parse_host_control_request(&serde_json::to_vec(&altered).expect("frame"))
            .expect_err("an unknown field");
        assert!(error.contains(field), "{field}: {error}");
    }
    for missing in ["run_cut", "criterion"] {
        let mut altered = frame.clone();
        altered
            .as_object_mut()
            .expect("frame object")
            .remove(missing);
        let error = parse_host_control_request(&serde_json::to_vec(&altered).expect("frame"))
            .expect_err("a frame without its cut or criterion");
        assert!(error.contains(missing), "{missing}: {error}");
    }
}

#[test]
fn acceptance_verification_read_refusals_answer_with_distinct_codes() {
    use crate::domain::AcceptanceVerificationReadRefusal as Refusal;
    let refusals = [
        Refusal::UnknownWork,
        Refusal::WrongProject,
        Refusal::WrongRevision,
        Refusal::WrongRun,
        Refusal::StaleCut,
        Refusal::InvalidCriterion,
        Refusal::InvalidCursor,
        Refusal::CursorBasisMismatch,
        Refusal::PageTooLarge,
    ];
    let codes: std::collections::BTreeSet<&str> = refusals
        .iter()
        .map(|refusal| {
            store_error_code(&StoreError::AcceptanceVerificationReadRefused {
                refusal: *refusal,
                reason: "reason".into(),
            })
        })
        .collect();
    assert_eq!(codes.len(), refusals.len());
    assert!(
        codes
            .iter()
            .all(|code| code.starts_with("acceptance_verification_read_"))
    );
}

#[test]
fn host_control_frames_reject_duplicate_fields_without_unbounded_diagnostics() {
    let duplicate_top_level = br#"{
            "operation":"session_status",
            "operation":"session_status",
            "routing_token":"routing-token"
        }"#;
    let error = parse_host_control_request(duplicate_top_level)
        .expect_err("duplicate top-level field must fail closed");
    assert!(error.contains("duplicate field"));

    let duplicate_nested = br#"{
            "operation":"turn_evaluate",
            "routing_token":"routing-token",
            "purpose":"ordinary",
            "intent_fingerprint":"unused",
            "requested_effects":["observe"],
            "resource_intents":[{
                "kind":"logical",
                "namespace":"workspace",
                "namespace":"other-workspace",
                "segments":["src"],
                "coverage":"exact"
            }],
            "idempotency_key":"turn-once"
        }"#;
    let error = parse_host_control_request(duplicate_nested)
        .expect_err("duplicate nested field must fail closed");
    assert!(error.contains("duplicate field"));

    let oversized_operation = serde_json::json!({
        "operation": "x".repeat(MAX_HOST_CONTROL_OPERATION_BYTES + 1),
    });
    let error = parse_host_control_request(
        &serde_json::to_vec(&oversized_operation).expect("encode oversized operation"),
    )
    .expect_err("unknown oversized operation must fail closed");
    assert!(error.contains("<invalid>"));
    assert!(error.len() < 512);
}

#[test]
fn host_control_frames_reject_unknown_nested_fields() {
    let misspelled_components = serde_json::json!({
        "operation": "turn_checkpoint",
        "routing_token": "routing-token",
        "grant_id": "grant-id",
        "next_intent": "continue",
        "observations": [],
        "verification_evidence": [],
        "environment_evidence": [{
            "source_basis": {
                "workspace_id": "workspace",
                "source_revision": "revision"
            },
            "environment_fingerprint": "a".repeat(64),
            "componentz": {
                "toolchain": "stable",
                "workspace_id": "workspace",
                "capability_map_revision": 1
            },
            "observed_at": "2026-09-02T00:00:00Z"
        }],
        "idempotency_key": "checkpoint-once"
    });
    let error = parse_host_control_request(
        &serde_json::to_vec(&misspelled_components).expect("encode nested typo"),
    )
    .expect_err("misspelled optional components field must fail closed");
    assert!(error.contains("turn_checkpoint"));
    assert!(error.contains("componentz"));

    let extra_resource_field = serde_json::json!({
        "operation": "turn_evaluate",
        "routing_token": "routing-token",
        "purpose": "ordinary",
        "intent_fingerprint": "a".repeat(64),
        "requested_effects": ["observe"],
        "resource_intents": [{
            "kind": "logical",
            "namespace": "workspace",
            "segments": ["src"],
            "coverage": "exact",
            "unexpected": true
        }],
        "idempotency_key": "turn-once"
    });
    let error = parse_host_control_request(
        &serde_json::to_vec(&extra_resource_field).expect("encode extra resource field"),
    )
    .expect_err("unknown resource-subject field must fail closed");
    assert!(error.contains("turn_evaluate"));
    assert!(error.contains("unexpected"));
}

fn turn_evaluate_frame(purpose: Option<&str>, key: &str) -> Value {
    let mut frame = serde_json::json!({
        "operation": "turn_evaluate",
        "routing_token": "routing-token",
        "idempotency_key": key,
        "intent_fingerprint": ObjectId::from_canonical_bytes(key.as_bytes()).as_str(),
        "requested_effects": ["observe"],
    });
    if let Some(purpose) = purpose {
        frame["purpose"] = purpose.into();
    }
    frame
}

// While hosts move off the removed fields, a turn request may still name
// the only purpose; a recovery turn no longer parses.
#[test]
fn turn_evaluate_takes_an_ordinary_or_absent_purpose_and_refuses_recovery() {
    for purpose in [None, Some("ordinary")] {
        let frame = turn_evaluate_frame(purpose, "turn-once");
        assert!(
            matches!(
                parse_host_control_request(&serde_json::to_vec(&frame).expect("encode")),
                Ok(HostControlRequest::TurnEvaluate { .. })
            ),
            "purpose {purpose:?} must parse"
        );
    }
    let frame = turn_evaluate_frame(Some("recovery"), "turn-once");
    let error = parse_host_control_request(&serde_json::to_vec(&frame).expect("encode"))
        .expect_err("a recovery turn must be refused");
    assert!(error.contains("turn_evaluate"));
    assert!(error.contains("recovery"));
}

// Grants carry no delivery page. A begin may omit the tokens or send
// none; a begin naming a token is refused as outside the grant.
#[test]
fn turn_begin_takes_no_delivery_tokens_and_refuses_any() {
    let directory = crate::test_support::temp_home().expect("temp");
    let mut server = HostControlServer::open_with_host_path_identity(
        directory.path().join("control.sqlite3"),
        None,
        ProjectId("wire-transition".into()),
        "agent".into(),
        SessionId("wire-session".into()),
        None,
    )
    .expect("open the control connection");
    let mut call = |frame: Value| {
        let request = parse_host_control_request(&serde_json::to_vec(&frame).expect("encode"))
            .expect("a valid frame");
        server.handle(request).expect("handled")
    };
    let bound = call(serde_json::json!({
        "operation": "session_bind",
        "external_ref": "dummy:WIRE",
        "title": "Wire transition",
        "assurance": "turn_gated",
        "mediated_effects": ["observe", "communicate"],
        "capability_map_revision": 1,
        "idempotency_key": "bind-wire",
    }));
    assert_eq!(bound["status"]["phase"], "ready");
    assert!(bound["status"].get("confirmed_cursor").is_none());
    let routing_token = bound["routing_token"].as_str().expect("token").to_owned();
    let grant_for = |key: &str, call: &mut dyn FnMut(Value) -> Value| {
        let mut frame = turn_evaluate_frame(None, key);
        frame["routing_token"] = routing_token.clone().into();
        let decision = call(frame);
        assert_eq!(decision["decision"], "grant", "{decision}");
        assert!(decision["grant"].get("delivery").is_none());
        decision["grant"]["grant_id"]
            .as_str()
            .expect("grant id")
            .to_owned()
    };
    let begin = |grant_id: &str, tokens: Option<Value>, key: &str| {
        let mut frame = serde_json::json!({
            "operation": "turn_begin",
            "routing_token": routing_token.clone(),
            "grant_id": grant_id,
            "idempotency_key": key,
        });
        if let Some(tokens) = tokens {
            frame["delivery_tokens"] = tokens;
        }
        frame
    };

    let first = grant_for("turn-echoes-a-token", &mut call);
    let refused = call(begin(
        &first,
        Some(serde_json::json!(["token-from-an-old-page"])),
        "begin-with-a-token",
    ));
    assert_eq!(refused["decision"], "refuse");
    assert_eq!(refused["code"], "grant_scope_mismatch");

    for (key, tokens) in [("absent", None), ("empty", Some(serde_json::json!([])))] {
        let grant = grant_for(&format!("turn-{key}-tokens"), &mut call);
        let started = call(begin(&grant, tokens, &format!("begin-{key}-tokens")));
        assert_eq!(started["decision"], "begin", "{started}");
        let reported = call(serde_json::json!({
            "operation": "turn_checkpoint",
            "routing_token": routing_token.clone(),
            "grant_id": grant,
            "next_intent": "continue",
            "idempotency_key": format!("checkpoint-{key}-tokens"),
        }));
        assert_eq!(reported["decision"], "checkpointed", "{reported}");
        assert_eq!(
            reported["receipt"]["confirmed_cursor"],
            reported["receipt"]["cursor"]
        );
    }
}

#[test]
fn host_control_open_refuses_an_oversized_session_before_store_open() {
    let directory = crate::test_support::temp_home().expect("temp");
    let path = directory.path().join("control.sqlite3");
    let giant = SessionId("h".repeat(65));
    let Err(error) = HostControlServer::open_with_host_path_identity(
        &path,
        None,
        ProjectId("control-admission".into()),
        "actor".into(),
        giant.clone(),
        None,
    ) else {
        panic!("oversized control session must be refused")
    };
    assert!(matches!(
        error,
        StoreError::InvalidWork(ref reason)
            if reason == crate::SessionIdAdmissionError::TooLong.as_str()
    ));
    assert!(!error.to_string().contains(&giant.0));
    assert!(!path.exists());
}

// The host passes one execution context to every channel of a session. The
// control connection attributes its records to it exactly as the work
// words do, and an unsafe value is normalized rather than refused.
#[test]
fn host_control_attributes_its_records_to_the_supplied_actor_context() {
    let directory = crate::test_support::temp_home().expect("temp");
    let open = |name: &str| {
        HostControlServer::open_with_host_path_identity(
            directory.path().join(name),
            None,
            ProjectId("control-context".into()),
            "greg/claude".into(),
            SessionId("session-context".into()),
            None,
        )
        .expect("open the control connection")
    };
    let context_of = |actor: &ActorContext| {
        actor
            .provenance_chain
            .iter()
            .find(|link| {
                link.reference.as_deref() == Some(crate::domain::ACTOR_CONTEXT_PROVENANCE_REFERENCE)
            })
            .map(|link| link.source.clone())
    };

    let plain = open("plain.sqlite3").actor("turn_begin", "begin");
    assert_eq!(context_of(&plain), None);

    let attributed = open("attributed.sqlite3")
        .with_actor_context(Some("agent=claude;model=fable;reasoning=high".into()))
        .actor("turn_begin", "begin");
    assert_eq!(
        context_of(&attributed).as_deref(),
        Some("agent=claude;model=fable;reasoning=high")
    );
    assert_eq!(attributed.actor_id, "greg/claude");
    attributed
        .validate_attribution_context()
        .expect("a valid attribution");

    let normalized = open("normalized.sqlite3")
        .with_actor_context(Some("agent=claude\u{7};model=fable".into()))
        .actor("turn_begin", "begin");
    assert_eq!(
        context_of(&normalized).as_deref(),
        Some("agent=claude ;model=fable")
    );
    assert!(normalized.provenance_chain.iter().any(|link| {
        link.reference.as_deref() == Some(crate::domain::ACTOR_CONTEXT_NORMALIZED_REFERENCE)
    }));
    normalized
        .validate_attribution_context()
        .expect("a normalized attribution stays valid");
}
