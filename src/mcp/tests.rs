//! MCP response and routing regressions.

use super::*;
use super::{
    arguments::{
        AddArgs, DoneArgs, EvaluateArgs, GateArgs, HandoffActionArg, HandoffArgs, NoteArgs,
        ShowArgs, WorkClaimArgs,
    },
    parameters::Parameters,
};
use crate::{
    AddInput, NextInput, StoreError, argument_names::ArgumentNames,
    storage::PROCESS_DEFAULT_WORK_SESSION_REUSE_REFUSAL, store_error_value,
    work_service::COMPLETED_WORK_LATE_FINDING_REFUSAL,
};
use chrono::TimeZone;
use chrono::Utc;
use serde_json::{Value, json};

mod agent_error_refs;

/// The MCP `show` tool pages a verification record's obligation
/// assessment: `note` gives the first eight, and `note` with `after`
/// the rest.
#[test]
fn show_pages_a_verification_records_assessment_over_mcp() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let (work_ref, records) =
        crate::storage::assessed_verification_fixture(&database, "mcp-assessment", "runner", 9, 1);
    let record = &records[0];
    let server = McpServer::new_with_actor_context(
        database,
        ProjectId("mcp-assessment".into()),
        "runner".into(),
        SessionId("runner".into()),
        None,
        None,
    );
    let args = |after: Option<String>| ShowArgs {
        work_ref: work_ref.clone(),
        notes: None,
        gates: None,
        history: None,
        after,
        note: Some(record.as_str().to_owned()),
        full: None,
        evaluations: None,
        evaluation: None,
        observations: None,
    };
    let detail = server
        .show(Parameters(args(None)))
        .structured_content
        .expect("structured detail");
    let block = &detail["note"]["assessment"];
    assert_eq!(
        (block["total"].as_u64(), block["shown"].as_u64()),
        (Some(10), Some(8))
    );
    let token = block["continuation"]
        .as_str()
        .and_then(|command| command.split_once(" --after "))
        .map(|(_, token)| token.to_owned())
        .expect("continuation");
    let rest = server
        .show(Parameters(args(Some(token))))
        .structured_content
        .expect("structured continuation");
    assert_eq!(rest["assessment"]["shown"], 2, "{rest}");
    assert_eq!(rest["assessment"]["earlier"], 8);
}

/// The MCP `show` tool gives a native verification record's typed facts,
/// in the notes window and in the record's detail, beside its summary.
#[test]
fn show_gives_a_verification_records_typed_facts_over_mcp() {
    use crate::domain::{ExecutionOutcome, VerificationKind, VerificationResult};
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let (work_ref, record, _) = crate::storage::verification_note_fixture(
        &database,
        "mcp-verification",
        "runner",
        crate::storage::HostCheck {
            key: "mcp-unknown-outcome",
            kind: VerificationKind::Test,
            outcome: ExecutionOutcome::Unknown,
            result: VerificationResult::Indeterminate,
            summary: "all tests passed",
        },
    );
    let server = McpServer::new_with_actor_context(
        database,
        ProjectId("mcp-verification".into()),
        "runner".into(),
        SessionId("runner".into()),
        None,
        None,
    );
    let args = |notes: Option<bool>, note: Option<String>| ShowArgs {
        work_ref: work_ref.clone(),
        notes,
        gates: None,
        history: None,
        after: None,
        note,
        full: None,
        evaluations: None,
        evaluation: None,
        observations: None,
    };
    let window = server
        .show(Parameters(args(Some(true), None)))
        .structured_content
        .expect("structured window");
    let row = window["notes"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["locator"].as_str() == Some(record.as_str()))
        .expect("the verification row")
        .clone();
    assert_eq!(row["verification"]["result"], "indeterminate");
    assert_eq!(row["verification"]["check_kind"], "test");
    assert_eq!(row["verification"]["source_revision"], "A3");
    assert_eq!(row["verification"]["producer_outcome"], "unknown");
    assert!(
        row["verification"]["meaning"]
            .as_str()
            .expect("plain words")
            .contains("cannot satisfy a passing-check requirement")
    );
    assert_eq!(row["summary"], "all tests passed");
    let detail = server
        .show(Parameters(args(None, Some(record.as_str().to_owned()))))
        .structured_content
        .expect("structured detail");
    assert_eq!(detail["note"]["verification"], row["verification"]);
}

#[test]
fn record_id_descriptions_preserve_mcp_argument_names() {
    let show = serde_json::to_value(schemars::schema_for!(ShowArgs)).unwrap();
    let note = show["properties"]["note"]["description"].as_str().unwrap();
    assert!(note.contains("record id") && note.contains("RECORD_ID:INDEX"));
    assert!(!note.contains("HASH"));

    let evaluate = serde_json::to_value(schemars::schema_for!(EvaluateArgs)).unwrap();
    let verdicts = evaluate["properties"]["verdicts"]["description"]
        .as_str()
        .unwrap();
    assert!(verdicts.contains("full record ids"));
    assert!(!verdicts.contains("full hashes"));
    let verdict_definition = evaluate["properties"]["verdicts"]["items"]["$ref"]
        .as_str()
        .unwrap()
        .strip_prefix('#')
        .unwrap();
    let evidence = &evaluate.pointer(verdict_definition).unwrap()["properties"]["evidence"];
    let description = evidence["description"].as_str().unwrap();
    assert!(description.contains("full record ids"));
    assert!(!description.contains("full hashes"));
    assert_eq!(evidence["type"], "array");
    assert_eq!(evidence["items"]["type"], "string");
    assert!(evaluate["properties"].get("source_fingerprint").is_some());
}

#[test]
fn different_build_refusal_preserves_mcp_wire_code_and_neutral_message() {
    let value = store_error_value(&StoreError::DifferentBuildSchema);
    assert_eq!(value["error"]["code"], "engram_store_error");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(message.contains("use the Engram build that owns this store"));
    assert!(!message.contains("invalid data"));
}

// done's refusal for a stale evaluation names the source observation that
// decided it beside the unchanged cause; a stale refusal for another
// reason names none. The receipt keeps its code and words.
#[test]
fn done_names_the_deciding_observation_beside_the_unchanged_stale_cause() {
    for decided in [true, false] {
        let directory = crate::test_support::temp_home().expect("temporary MCP home");
        let database = directory.path().join("stale-deciding.sqlite3");
        let second = Utc::now().timestamp()
            - chrono::Utc
                .with_ymd_and_hms(2026, 8, 27, 1, 0, 0)
                .single()
                .expect("epoch")
                .timestamp()
            - 20;
        let fixture = crate::storage::stale_deciding_refusal_fixture(
            &database,
            "mcp-stale-deciding",
            "runner",
            "revision-judged",
            "C:/work/other tree",
            "revision-moved",
            decided,
            second,
        );
        let server = McpServer::new_with_actor_context(
            database,
            ProjectId("mcp-stale-deciding".into()),
            "runner".into(),
            SessionId("runner".into()),
            None,
            None,
        );
        let response = server.done(Parameters(DoneArgs {
            work_ref: Some(fixture.work.short_ref.clone()),
            summary: Some("delivered".into()),
            note: None,
            links: None,
            link_basis: None,
            source_fingerprint: None,
            landing: None,
        }));
        assert_ne!(
            response.is_error,
            Some(true),
            "an owed receipt, not an error"
        );
        let value = response.structured_content.expect("structured receipt");
        assert_eq!(value["code"], "acceptance_evaluation_stale", "{value}");
        let reason = if decided { "mutation" } else { "policy" };
        assert_eq!(
            value["recovery"]["cause"],
            json!(
                crate::WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
                    reason: if decided {
                        crate::AcceptanceStaleReason::Mutation
                    } else {
                        crate::AcceptanceStaleReason::Policy
                    }
                }
            ),
            "{value}"
        );
        assert_eq!(value["recovery"]["cause"]["reason"], reason);
        let reminders = value["reminders"].as_array().expect("reminders");
        let named = reminders.iter().any(|reminder| {
            reminder
                .as_str()
                .is_some_and(|text| text.contains("The deciding source observation"))
        });
        let observation = &value["recovery"]["deciding_observation"];
        if decided {
            assert_eq!(observation["position"], fixture.position, "{value}");
            assert_eq!(observation["workspace"], "C:/work/other tree");
            assert_eq!(observation["revision"], "revision-moved");
            assert_eq!(observation["source_changed"], true);
            // Labelled as show labels sessions: the caller's own is "you".
            assert_eq!(observation["reporting_session"], "you");
            assert_eq!(observation["evaluated_revision"], "revision-judged");
            assert_eq!(observation["evaluated_revision_declared"], false);
            assert!(named, "{value}");
        } else {
            assert!(observation.is_null(), "{value}");
            assert!(!named, "{value}");
        }
    }
}

// B21/B22/B77/B78: storage's deciding source context selects both the
// service remedy and word guidance, without changing the owed status. The
// word and plain show bound host-recorded strings; service and raw error
// details keep them whole.
#[test]
fn source_recovery_causes_select_the_same_service_and_mcp_guidance() {
    for case in [
        "unconfirmed",
        "missing",
        "mismatch",
        "no_basis",
        "long_unconfirmed",
        "long_mismatch",
    ] {
        let fixture: crate::storage::SourceRecoveryTransportFixture =
            crate::storage::source_recovery_transport_fixture(case, Utc::now());
        let service = crate::LocalWorkService::new(
            fixture.database.clone(),
            fixture.work.project_id.clone(),
            "runner".into(),
            SessionId("runner".into()),
            None,
        );
        let result = service
            .work_complete_on(
                Some(&fixture.work.short_ref),
                crate::WorkCompleteInput {
                    links: vec![],
                    link_basis: None,
                    capture: None,
                    evidence: vec![],
                    acceptance: None,
                    note: None,
                    source_fingerprint: fixture.presented.clone(),
                    landing: None,
                    idempotency_key: String::new(),
                },
                Utc::now(),
            )
            .expect("structured service recovery");
        let crate::WorkCompleteResult::Refused(refusal) = result else {
            panic!("{case}: completion must remain owed")
        };
        assert_eq!(refusal.code, "acceptance_evaluation_stale");
        assert_eq!(
            refusal.recovery.cause,
            crate::WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
                reason: crate::AcceptanceStaleReason::Source,
            }
        );
        let source = refusal.recovery.source.as_ref().expect("source context");
        assert_eq!(source.mismatch, fixture.mismatch);
        assert_eq!(source.evaluation, fixture.evaluation);
        assert_eq!(source.run_id, fixture.work.active_run_id.unwrap());
        assert_eq!(
            refusal.remedy,
            crate::work_service::source_recovery_remedy(source)
        );
        let server = McpServer::new_with_actor_context(
            fixture.database.clone(),
            fixture.work.project_id.clone(),
            "runner".into(),
            SessionId("runner".into()),
            None,
            None,
        );
        let response = server.done(Parameters(DoneArgs {
            work_ref: Some(fixture.work.short_ref.clone()),
            summary: Some("delivered".into()),
            note: None,
            links: None,
            link_basis: None,
            source_fingerprint: fixture.presented.clone(),
            landing: None,
        }));
        assert_ne!(response.is_error, Some(true), "{case}: an owed receipt");
        let value = response
            .structured_content
            .expect("structured word receipt");
        assert_eq!(value["code"], refusal.code);
        assert_eq!(value["recovery"]["cause"], json!(refusal.recovery.cause));
        let shown = crate::work_service::shown_source_recovery(source);
        assert_eq!(value["recovery"]["source"], json!(shown));
        let long_declared = crate::storage::long_declared_source();
        let bounded_declared = format!("{}… (129 bytes stored)", "源".repeat(42));
        if case.starts_with("long_") {
            // The service keeps the declaration whole; the word bounds it.
            assert_eq!(source.declared_revision.as_deref(), Some(&*long_declared));
            assert_eq!(
                value["recovery"]["source"]["declared_revision"], bounded_declared,
                "{case}"
            );
        }
        if case == "long_mismatch" {
            let long_presented = crate::storage::long_presented_source();
            assert_eq!(
                source.expected_fingerprint.as_deref(),
                Some(&*long_declared)
            );
            assert_eq!(
                source.presented_fingerprint.as_deref(),
                Some(&*long_presented)
            );
            assert_eq!(
                value["recovery"]["source"]["expected_fingerprint"],
                bounded_declared
            );
            assert_eq!(
                value["recovery"]["source"]["presented_fingerprint"],
                format!("{}… (129 bytes stored)", "m".repeat(128))
            );
        }
        // Plain show names the same remedy on its own line, from the
        // bounded projection of the source context it reads.
        let verbs = crate::verbs::AgentVerbs::new(
            fixture.database.clone(),
            fixture.work.project_id.clone(),
            "runner".into(),
            SessionId("runner".into()),
            None,
        );
        let read = verbs
            .show(&fixture.work.short_ref, Utc::now())
            .expect("show the item");
        let status = crate::SqliteStore::open(&fixture.database)
            .expect("open the store")
            .acceptance_evaluation_status(fixture.work.work_id, None)
            .expect("status read")
            .expect("newest evaluation");
        let shown_in_read = read.value["acceptance_evaluation"]["source_recovery"].clone();
        match &status.source_recovery {
            Some(at_read) => {
                let line = format!("  {}", crate::work_service::source_recovery_remedy(at_read));
                assert!(
                    read.text().lines().any(|shown| shown == line),
                    "{case}: {}",
                    read.text()
                );
                assert_eq!(
                    shown_in_read,
                    json!(crate::work_service::shown_source_recovery(at_read)),
                    "{case}"
                );
                if case == "long_unconfirmed" {
                    assert_eq!(at_read.declared_revision.as_deref(), Some(&*long_declared));
                    assert_eq!(shown_in_read["declared_revision"], bounded_declared);
                }
            }
            None => assert!(shown_in_read.is_null(), "{case}: {shown_in_read}"),
        }
        assert_eq!(
            status.source_recovery.is_some(),
            matches!(case, "unconfirmed" | "no_basis" | "long_unconfirmed"),
            "{case}"
        );
        // Over MCP the remedy names the field the caller passes.
        let spelled = mcp_spelled(&refusal.remedy);
        assert!(
            value["reminders"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| { line.as_str().is_some_and(|text| text.contains(&spelled)) })
        );
        assert!(
            value["next"]
                .as_array()
                .unwrap()
                .iter()
                .any(|command| command.as_str().unwrap().contains(&fixture.work.short_ref))
        );
        let raw = StoreError::WorkCompletionRecoveryRequired {
            work: fixture.work.work_id,
            cause: refusal.recovery.cause,
            context: Box::new(crate::storage::StaleRecoveryContext {
                source: Some(source.clone()),
                ..Default::default()
            }),
        };
        let legacy = StoreError::WorkCompletionRecoveryRequired {
            work: fixture.work.work_id,
            cause: crate::WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
                reason: crate::AcceptanceStaleReason::Source,
            },
            context: Box::default(),
        };
        assert_eq!(raw.to_string(), legacy.to_string());
        let raw_json = store_error_value(&raw);
        assert_eq!(
            raw_json["error"]["code"],
            "work_completion_recovery_required"
        );
        assert_eq!(raw_json["error"]["details"]["source"], json!(source));
        if case.starts_with("long_") {
            assert_eq!(
                raw_json["error"]["details"]["source"]["declared_revision"],
                long_declared
            );
        }
    }
}

/// The MCP spelling of a text that may end with a sentence naming an
/// argument.
fn mcp_spelled(text: &str) -> String {
    crate::verbs::respell(ArgumentNames::Mcp, text).into_owned()
}

/// The MCP agent projection of a shared refusal that concerns `work`:
/// the message and details name the item by short reference, and the
/// arguments their sentences name by MCP field.
fn agent_work_projection(shared: &Value, work: &crate::WorkItem) -> (Value, Value) {
    let message = mcp_spelled(
        &shared["error"]["message"]
            .as_str()
            .expect("message")
            .replacen(&format!("{:?}", work.work_id), &work.short_ref, 1),
    );
    let mut details = shared["error"]["details"].clone();
    let Value::Object(fields) = &mut details else {
        panic!("details object: {details}");
    };
    assert_eq!(fields.remove("work_id"), Some(json!(work.work_id)));
    fields.insert("work_ref".into(), json!(work.short_ref));
    for key in ["reason", "remedy"] {
        if let Some(Value::String(text)) = fields.get_mut(key) {
            *text = mcp_spelled(text);
        }
    }
    (json!(message), details)
}

#[test]
fn evaluation_admission_errors_keep_status_and_deciding_causes_across_service_and_mcp() {
    for case in [
        "eligibility",
        "source_root",
        "wrong_run",
        "beyond_cut",
        "wrong_source",
        "wrong_basis",
    ] {
        let fixture: crate::storage::AdmissionTransportFixture =
            crate::storage::admission_transport_fixture(case, Utc::now());
        let service = crate::LocalWorkService::new(
            fixture.database.clone(),
            fixture.work.project_id.clone(),
            "runner".into(),
            SessionId("runner".into()),
            None,
        );
        let error = service
            .work_evaluate_on(&fixture.input, Utc::now())
            .expect_err(case);
        let shared = store_error_value(&error);
        assert_eq!(
            shared["error"]["details"]["cause"]["kind"], fixture.family,
            "{case}: {shared}"
        );
        assert_eq!(
            shared["error"]["details"]["cause"]["mismatch"], fixture.mismatch,
            "{case}: {shared}"
        );
        let server = McpServer::new_with_actor_context(
            fixture.database.clone(),
            fixture.work.project_id.clone(),
            "runner".into(),
            SessionId("runner".into()),
            None,
            None,
        );
        let input = &fixture.input;
        let response = server.evaluate(Parameters(EvaluateArgs {
            work_ref: input.work_ref.clone(),
            mode: input.mode.clone(),
            acceptance_basis: input.acceptance_basis,
            evidence_basis: input.evidence_basis,
            verdicts: input.verdicts.clone(),
            attempt: input.attempt.clone(),
            source_fingerprint: input.source_fingerprint.clone(),
            model: input.model.clone(),
            execution_identity: input.execution_identity.clone(),
            parent_session: input.parent_session.clone(),
            supersedes: input.supersedes.clone(),
        }));
        assert_eq!(response.is_error, Some(true), "{case}");
        let value = response
            .structured_content
            .expect("structured admission error");
        let error = &value["error"];
        assert_eq!(error["code"], "acceptance_evaluation_refused");
        // The agent envelope names the item by short reference; every
        // other detail, the cause included, is the shared one.
        let (message, details) = agent_work_projection(&shared, &fixture.work);
        assert_eq!(error["message"], message);
        assert_eq!(error["details"], details);
        let cause: crate::AcceptanceEvaluationAdmissionCause =
            serde_json::from_value(error["details"]["cause"].clone()).unwrap();
        let remedy = mcp_spelled(&crate::work_service::evaluation_admission_remedy(&cause));
        assert_eq!(error["details"]["remedy"], remedy);
        if case == "wrong_basis" {
            // The basis is the fault: the valid check it cites is not
            // named, and the remedy states the admissible pass.
            assert_eq!(error["details"]["cause"]["citation"], "", "{case}");
            assert!(
                remedy.contains("uses basis observed, and every citation of it is a passed host-minted verification"),
                "{remedy}"
            );
        }
        assert!(
            error["reminders"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry == &json!(remedy))
        );
        assert!(
            error["next"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry.as_str().unwrap().contains(&fixture.work.short_ref))
        );
        let store = crate::SqliteStore::open(&fixture.database).unwrap();
        assert!(
            store
                .acceptance_evaluation_status(fixture.work.work_id, None)
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn done_preserves_bound_check_error_status_and_exposes_typed_guidance() {
    for result in [
        crate::VerificationResult::Passed,
        crate::VerificationResult::Failed,
        crate::VerificationResult::Indeterminate,
    ] {
        let directory = crate::test_support::temp_home().expect("temporary MCP home");
        let database = directory.path().join("bound-refusal.sqlite3");
        let second = Utc::now().timestamp()
            - chrono::Utc
                .with_ymd_and_hms(2026, 8, 27, 1, 0, 0)
                .single()
                .expect("epoch")
                .timestamp()
            - 10;
        let fixture = crate::storage::bound_verification_refusal_fixture(
            &database,
            "mcp-bound-refusal",
            "runner",
            result,
            second,
        );
        let server = McpServer::new_with_actor_context(
            database,
            ProjectId("mcp-bound-refusal".into()),
            "runner".into(),
            SessionId("runner".into()),
            None,
            None,
        );
        let response = server.done(Parameters(DoneArgs {
            work_ref: Some(fixture.work.short_ref.clone()),
            summary: Some("delivered".into()),
            note: None,
            links: None,
            link_basis: None,
            source_fingerprint: None,
            landing: None,
        }));
        assert_eq!(response.is_error, Some(true));
        let value = response.structured_content.expect("structured refusal");
        let error = &value["error"];
        assert_eq!(error["code"], "work_completion_refused");
        let details = &error["details"];
        let cause: crate::WorkBoundVerificationCause =
            serde_json::from_value(details["cause"].clone()).expect("typed cause");
        assert_eq!(cause.criterion, 1);
        assert_eq!(cause.requirement.check_kind, crate::VerificationKind::Build);
        assert_eq!(cause.verification, fixture.verification);
        assert_eq!(cause.satisfied_by, fixture.satisfied_by);
        assert_eq!(cause.result, result);
        let (mismatch, remedy) = if result == crate::VerificationResult::Passed {
            (
                crate::VerificationEvidenceMismatch::StaleSourceRevision,
                crate::BoundVerificationRemedy::RunCurrentCheck,
            )
        } else {
            (
                crate::VerificationEvidenceMismatch::ResultNotPassed,
                crate::BoundVerificationRemedy::RunPassingCheckAfter,
            )
        };
        assert_eq!(cause.mismatch, mismatch);
        assert_eq!(cause.remedy, remedy);
        // A stale check names the change it must follow, in the typed
        // cause and as one sentence after the reason; no other does.
        let reason = details["reason"].as_str().expect("reason");
        if result == crate::VerificationResult::Passed {
            let source = cause.stale_source.as_ref().expect("the deciding record");
            assert_eq!(
                source.decider,
                crate::domain::StaleSourceDecider::LatestChange
            );
            assert_eq!(source.source_changed, Some(true));
            assert_eq!(details["cause"]["stale_source"]["decider"], "latest_change");
            assert!(
                error["reminders"]
                    .as_array()
                    .expect("word reminders")
                    .iter()
                    .any(|entry| entry == &json!(source.sentence())),
                "{error}"
            );
        } else {
            assert_eq!(cause.stale_source, None);
            assert!(details["cause"].get("stale_source").is_none());
            assert!(!reason.contains("deciding source record"), "{reason}");
        }
        let legacy = StoreError::WorkCompletionRefused {
            work: fixture.work.work_id,
            reason: details["reason"].as_str().expect("reason").into(),
        };
        assert_eq!(
            error["message"],
            legacy.to_string().replacen(
                &format!("{:?}", fixture.work.work_id),
                &fixture.work.short_ref,
                1
            )
        );
        let guidance = crate::work_service::bound_verification_remedy(&cause);
        assert_eq!(details["remedy"], guidance);
        assert!(
            error["reminders"]
                .as_array()
                .expect("word reminders")
                .iter()
                .any(|entry| entry == &json!(guidance))
        );
        // Native CLI JSON uses this same formatter. No adapter extracts a
        // cause from the human message, whose bytes remain unchanged.
        let typed = StoreError::WorkBoundVerificationRefused {
            work: fixture.work.work_id,
            reason: details["reason"].as_str().unwrap().into(),
            cause: Box::new(cause),
        };
        let shared = store_error_value(&typed);
        assert_eq!(shared["error"]["code"], error["code"]);
        // The agent envelope names the item by short reference; the
        // raw core one keeps the work id, and every other field agrees.
        let (message, projected) = agent_work_projection(&shared, &fixture.work);
        assert_eq!(message, error["message"]);
        assert_eq!(projected, error["details"]);
    }
}

#[test]
fn ambiguous_work_reference_has_a_stable_mcp_error_code() {
    let work_id = crate::WorkId::new();
    let error = StoreError::WorkReferenceAmbiguous {
        reference: "w-collision".into(),
        candidates: vec![crate::WorkReferenceCandidate {
            work_id,
            short_ref: "w-collision".into(),
            title: "Collision candidate".into(),
            lifecycle: crate::WorkLifecycle::Open,
        }],
        more: 2,
    };
    let value = store_error_value(&error);
    assert_eq!(value["error"]["code"], "work_reference_ambiguous");
    let details = &value["error"]["details"];
    assert_eq!(details["reference"], "w-collision");
    assert_eq!(details["candidates"][0]["work_id"], work_id.0.to_string());
    assert_eq!(details["candidates"][0]["ref"], "w-collision");
    assert_eq!(details["candidates"][0]["title"], "Collision candidate");
    assert_eq!(details["candidates"][0]["state"], "open");
    assert_eq!(details["more"], 2);
}

#[test]
fn implicit_target_refusal_has_a_stable_code_and_names_both_items() {
    let error =
        StoreError::WorkImplicitTargetConflict(Box::new(crate::storage::ImplicitTargetConflict {
            operation: "note".into(),
            focus: "w-added".into(),
            focus_state: crate::storage::ImplicitFocusState::Unclaimed,
            focus_lifecycle: crate::domain::WorkLifecycle::Open,
            held: vec!["w-held".into()],
            more: 2,
        }));
    let value = store_error_value(&error);
    assert_eq!(value["error"]["code"], "work_implicit_target_conflict");
    let details = &value["error"]["details"];
    assert_eq!(details["operation"], "note");
    assert_eq!(details["focused_ref"], "w-added");
    assert_eq!(details["focus_state"], "unclaimed");
    assert_eq!(details["held_refs"], json!(["w-held"]));
    assert_eq!(details["more"], 2);
    let message = value["error"]["message"].as_str().expect("message");
    assert!(
        message.contains("w-added") && message.contains("w-held and 2 more"),
        "{message}"
    );
    assert!(message.contains("nothing was recorded"), "{message}");
}

#[test]
fn satisfied_prerequisite_refusal_has_actionable_structured_details() {
    let work_id = crate::WorkId::new();
    let error = StoreError::WorkPrerequisiteAlreadySatisfied(work_id);
    let value = store_error_value(&error);
    assert_eq!(
        value["error"]["code"],
        "work_prerequisite_already_satisfied"
    );
    let details = &value["error"]["details"];
    assert_eq!(details["work_id"], work_id.0.to_string());
    assert_eq!(
        details["remedy"],
        "no edge is needed; run show for the prerequisite before choosing another action"
    );
}

#[test]
fn invalid_context_generation_has_a_specific_mcp_remedy() {
    let error = StoreError::InvalidProjectMemory(
        "context_generation must be 1 to 256 ASCII letters, digits, dots, underscores or dashes, and must not start with a dash".into(),
    );
    let value = store_error_value(&error);
    assert_eq!(
        value["error"]["details"]["remedy"],
        "omit context_generation or use 1 to 256 ASCII letters, digits, dots, underscores or dashes, not starting with a dash"
    );
}

#[test]
fn process_default_reuse_refusal_has_a_non_looping_structured_remedy() {
    let error = StoreError::InvalidWork(PROCESS_DEFAULT_WORK_SESSION_REUSE_REFUSAL.into());
    let value = store_error_value(&error);
    assert_eq!(
        value["error"]["details"]["reason"],
        PROCESS_DEFAULT_WORK_SESSION_REUSE_REFUSAL
    );
    assert_eq!(
        value["error"]["details"]["remedy"],
        PROCESS_DEFAULT_WORK_SESSION_REUSE_REFUSAL
    );
}

#[test]
fn completed_holder_word_refusal_points_to_late_note_without_reopening() {
    let error = StoreError::InvalidWork(COMPLETED_WORK_LATE_FINDING_REFUSAL.into());
    let value = store_error_value(&error);
    assert_eq!(
        value["error"]["details"]["reason"],
        COMPLETED_WORK_LATE_FINDING_REFUSAL
    );
    let remedy = value["error"]["details"]["remedy"]
        .as_str()
        .expect("late-finding remedy");
    assert_eq!(
        remedy,
        "use note to record a late finding without reopening the completed item"
    );
    assert!(!remedy.contains("next"));
}

#[test]
fn retained_work_service_survives_failure_for_agent_tools() {
    let directory = crate::test_support::temp_home().expect("temporary MCP home");
    let server = McpServer::new_with_actor_context(
        directory.path().join("engram.sqlite3"),
        ProjectId("mcp-retained-service".into()),
        "agent".into(),
        SessionId("mcp-retained-session".into()),
        Some("mcp-test".into()),
        None,
    );

    server
        .verbs()
        .next(
            &NextInput {
                limit: Some(5),
                peek: false,
                verbose: false,
                context_generation: None,
            },
            Utc::now(),
        )
        .expect("agent tool initializes the retained service");

    let refused = server.verbs().add(
        AddInput {
            title: " ".into(),
            ..AddInput::default()
        },
        Utc::now(),
    );
    assert!(refused.is_err());
    assert!(format!("{:?}", server.work_service).contains("store_initialized: true"));

    server
        .verbs()
        .next(
            &NextInput {
                limit: Some(5),
                peek: false,
                verbose: false,
                context_generation: None,
            },
            Utc::now(),
        )
        .expect("agent tool remains usable after refusal");
    let cloned_handler = server.clone();
    assert!(Arc::ptr_eq(
        &server.work_service,
        &cloned_handler.work_service
    ));
}

#[test]
fn mcp_add_refuses_an_unknown_argument_and_names_its_acceptance_field() {
    let refused = serde_json::from_value::<AddArgs>(json!({
        "title": "Misspelled criteria",
        "accept": ["Criterion"],
    }))
    .expect_err("an argument add does not list is refused");
    let message = refused.to_string();
    assert!(
        message.contains("unknown field `accept`, expected one of"),
        "{message}"
    );
    assert!(message.contains("`acceptance`"), "{message}");

    let directory = crate::test_support::temp_home().expect("temporary MCP home");
    let server = McpServer::new_with_actor_context(
        directory.path().join("engram.sqlite3"),
        ProjectId("mcp-acceptance-field".into()),
        "agent".into(),
        SessionId("mcp-acceptance-session".into()),
        Some("mcp-test".into()),
        None,
    );
    let added = server
        .verbs()
        .add(
            AddInput {
                title: "Defaulted criteria".into(),
                ..AddInput::default()
            },
            Utc::now(),
        )
        .expect("add with defaulted acceptance");
    let reminders = added.value["reminders"]
        .as_array()
        .expect("reminders")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert!(
        reminders.contains(&"acceptance defaulted to the title being done; set acceptance"),
        "{reminders:?}"
    );
    assert!(
        reminders.iter().all(|line| !line.contains("--accept")),
        "{reminders:?}"
    );
}

#[test]
fn oversized_mcp_session_refuses_the_first_tool_without_opening_the_store() {
    let directory = crate::test_support::temp_home().expect("temporary MCP home");
    let giant = "m".repeat(65);
    let server = McpServer::new_with_actor_context(
        directory.path().join("engram.sqlite3"),
        ProjectId("mcp-oversized-session".into()),
        "agent".into(),
        SessionId(giant.clone()),
        Some("mcp-test".into()),
        None,
    );
    let refused = server.verbs().next(
        &NextInput {
            limit: Some(5),
            peek: true,
            verbose: false,
            context_generation: None,
        },
        Utc::now(),
    );
    let error = refused.expect_err("oversized MCP session");
    let message = error.to_string();
    assert!(
        message.contains(crate::SessionIdAdmissionError::TooLong.as_str()),
        "{message}"
    );
    assert!(!message.contains(&giant));
    assert!(format!("{:?}", server.work_service).contains("store_initialized: false"));
}

// Every update argument Engram refuses before the tool runs carries the
// two fields every tool error Engram itself returns does: its reason, and
// no command.
#[test]
fn invalid_argument_errors_carry_reminders_and_next() {
    let directory = crate::test_support::temp_home().expect("temporary MCP home");
    let server = McpServer::new_with_actor_context(
        directory.path().join("engram.sqlite3"),
        ProjectId("mcp-invalid-argument".into()),
        "agent".into(),
        SessionId("agent".into()),
        None,
        None,
    );
    for (field, arguments) in [
        (
            "external",
            json!({ "action": "cancel", "external": "planner:x" }),
        ),
        (
            "clear_external",
            json!({ "action": "cancel", "clear_external": true }),
        ),
        (
            "acceptance",
            json!({ "action": "release", "acceptance": ["x"] }),
        ),
        ("bindings", json!({ "action": "release", "bindings": [] })),
        (
            "blocker",
            json!({ "action": "release", "blocker": "w-000000000001" }),
        ),
        (
            "evaluation_mode",
            json!({ "action": "revise", "evaluation_mode": "same_session" }),
        ),
        (
            "defer",
            json!({ "action": "revise", "defer": "not a date" }),
        ),
    ] {
        let mut arguments = arguments;
        arguments["work_ref"] = json!("w-000000000001");
        let refused = server.update(Parameters(
            serde_json::from_value(arguments).expect("update arguments"),
        ));
        assert_eq!(refused.is_error, Some(true), "{field}");
        let error = &refused.structured_content.expect("structured error")["error"];
        assert_eq!(error["code"], "invalid_argument", "{field}");
        assert_eq!(error["details"]["field"], field);
        assert_eq!(error["reminders"], json!([error["message"]]), "{field}");
        assert_eq!(error["next"], json!([]), "{field}");
    }
}

/// One MCP session holding two live claims, the second focused.
struct TwoClaims {
    server: McpServer,
    database: std::path::PathBuf,
    project: ProjectId,
    focus: String,
    other: String,
    held: Vec<String>,
    _directory: crate::test_support::TempHome,
}

fn two_claims_over_mcp(project: &str) -> TwoClaims {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let (project, session) = (ProjectId(project.into()), SessionId("runner".into()));
    let verbs = crate::verbs::AgentVerbs::new(
        database.clone(),
        project.clone(),
        "runner".into(),
        session.clone(),
        None,
    );
    let mut held = Vec::new();
    for (second, title) in [(1, "First held work"), (2, "Second held work")] {
        let added = verbs
            .add(
                AddInput {
                    external: None,
                    notes: Vec::new(),
                    title: title.into(),
                    outcome: None,
                    acceptance: vec![format!("{title} is delivered")],
                    bindings: Vec::new(),
                    under: None,
                    optional: false,
                    priority: None,
                    labels: Vec::new(),
                    assignee: None,
                    kind: None,
                    evaluation_mode: None,
                },
                Utc::now() + chrono::Duration::milliseconds(second),
            )
            .expect("add");
        let work_ref = added.value["work"]["short_ref"]
            .as_str()
            .expect("short ref")
            .to_owned();
        verbs
            .claim(
                crate::verbs::ClaimInput {
                    work_ref: work_ref.clone(),
                    ttl_seconds: Some(36_000),
                    recover: None,
                },
                Utc::now() + chrono::Duration::milliseconds(second + 10),
            )
            .expect("claim");
        held.push(work_ref);
    }
    let (other, focus) = (held[0].clone(), held[1].clone());
    held.sort();
    drop(verbs);
    let server = McpServer::new_with_actor_context(
        database.clone(),
        project.clone(),
        "runner".into(),
        session,
        None,
        None,
    );
    TwoClaims {
        server,
        database,
        project,
        focus,
        other,
        held,
        _directory: directory,
    }
}

/// Every row of the work feed and object tables, so a refusal can be shown
/// to record nothing.
fn recorded_rows(database: &std::path::Path) -> (i64, i64) {
    let connection = rusqlite::Connection::open(database).expect("store");
    let count = |table: &str| {
        connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("count rows")
    };
    (count("work_feed_entries"), count("objects"))
}

fn acted(response: rmcp::model::CallToolResult, word: &str) -> Value {
    assert_ne!(response.is_error, Some(true), "{word}: {response:?}");
    response.structured_content.expect("receipt")
}

fn bare_gate_args(work_ref: Option<&str>) -> GateArgs {
    GateArgs {
        work_ref: work_ref.map(Into::into),
        name: "unit".into(),
        failed: None,
        evidence_ref: None,
    }
}

fn done_args(work_ref: Option<&str>) -> DoneArgs {
    DoneArgs {
        work_ref: work_ref.map(Into::into),
        summary: Some("delivered".into()),
        note: None,
        links: None,
        link_basis: None,
        source_fingerprint: None,
        landing: None,
    }
}

/// Over MCP, a bare `done` or `evaluate` while the session holds two live
/// claims is a structured refusal naming every held ref, recording nothing;
/// a bare `note`, `gate` and `handoff` still act on the held focus.
#[test]
fn bare_done_and_evaluate_refuse_over_mcp_while_several_claims_are_live() {
    let TwoClaims {
        _directory: _home,
        server,
        database,
        focus,
        held,
        ..
    } = two_claims_over_mcp("mcp-bare-target");
    let before = recorded_rows(&database);

    let refused = |response: rmcp::model::CallToolResult, word: &str| {
        assert_eq!(response.is_error, Some(true), "{word}");
        let value = response.structured_content.expect("structured error");
        let error = value["error"].clone();
        assert_eq!(
            error["code"], "work_bare_target_ambiguous",
            "{word}: {value}"
        );
        assert_eq!(error["details"]["operation"], word);
        assert_eq!(
            error["details"]["held_refs"],
            json!(held),
            "{word}: {value}"
        );
        assert_eq!(error["details"]["focused_ref"], json!(focus), "{word}");
        let explicit: Vec<String> = held
            .iter()
            .map(|work_ref| match word {
                "done" => format!("engram work done {work_ref} \"…\""),
                _ => format!("engram work evaluate {work_ref} …"),
            })
            .collect();
        assert_eq!(error["next"], json!(explicit), "{word}: {value}");
        assert!(
            error["message"]
                .as_str()
                .is_some_and(|message| message.contains("nothing was recorded")),
            "{word}: {value}"
        );
    };
    refused(server.done(Parameters(done_args(None))), "done");
    refused(
        server.evaluate(Parameters(EvaluateArgs {
            work_ref: None,
            mode: "independent_session".into(),
            acceptance_basis: 1,
            evidence_basis: 1,
            verdicts: Vec::new(),
            attempt: None,
            source_fingerprint: None,
            model: None,
            execution_identity: None,
            parent_session: None,
            supersedes: None,
        })),
        "evaluate",
    );
    assert_eq!(
        recorded_rows(&database),
        before,
        "a refused bare word recorded something"
    );

    let noted = acted(
        server.note(Parameters(NoteArgs {
            status: None,
            work_ref: None,
            text: "a finding on the held focus".into(),
            refs: None,
        })),
        "note",
    );
    assert_eq!(noted["work"]["short_ref"], json!(focus), "{noted}");
    let gated = acted(server.gate(Parameters(bare_gate_args(None))), "gate");
    assert_eq!(gated["work"]["short_ref"], json!(focus), "{gated}");
    for (action, word) in [
        (HandoffActionArg::Offer, "handoff offer"),
        (HandoffActionArg::Cancel, "handoff cancel"),
    ] {
        let handed = acted(
            server.handoff(Parameters(HandoffArgs {
                work_ref: None,
                action,
                to: matches!(action, HandoffActionArg::Offer).then(|| "another-session".into()),
                summary: None,
                reason: matches!(action, HandoffActionArg::Cancel).then(|| "kept it".into()),
                ttl_seconds: Some(600),
            })),
            word,
        );
        assert!(handed.to_string().contains(&focus), "{word}: {handed}");
    }
}

/// Over MCP with two live claims, `done` and `evaluate` naming the item act
/// as they always have: the evaluation records, its exact resend replays, and
/// done completes the named item. With one live claim left, a bare `gate`,
/// `evaluate` and `done` act on it.
#[test]
fn explicit_and_single_claim_done_and_evaluate_act_over_mcp() {
    let TwoClaims {
        _directory: _home,
        server,
        database,
        project,
        focus,
        other,
        ..
    } = two_claims_over_mcp("mcp-bare-target-explicit");
    crate::SqliteStore::open(&database)
        .expect("store")
        .set_acceptance_evaluation_policy(
            &crate::AcceptanceEvaluationPolicy {
                allowed_modes: vec![crate::AcceptanceEvaluationMode::SameSession],
                mechanical_basis: crate::domain::MechanicalBasis::Asserted,
                require_source_freshness: false,
            },
            &crate::domain::ActorContext {
                actor_id: "policy-admin".into(),
                actor_kind: "host_operator".into(),
                assurance: crate::domain::AssuranceLevel::Asserted,
                run_id: None,
                session_id: None,
                source_tool: Some("mcp_test".into()),
                source_skill: None,
                provenance_chain: Vec::new(),
                reason: "enable acceptance evaluation for the MCP test".into(),
            },
            "enable-evaluated-completion",
            None,
            Utc::now(),
            &crate::DevelopmentNoopRedactor,
        )
        .expect("enable same-session evaluation");
    // A passing evaluation's arguments, citing the item's run evidence
    // through its run feed head at this moment; `named` decides whether it
    // names the item. Kept as JSON so an exact resend can repeat it.
    let evaluation = |work_ref: &str, named: bool| {
        let store = crate::SqliteStore::open(&database).expect("store");
        let run = store
            .resolve_work_ref(&project, work_ref)
            .expect("resolve")
            .active_run_id
            .expect("active run");
        let citations = store
            .work_run_evidence(run)
            .expect("run evidence")
            .into_iter()
            .map(|hash| hash.as_str().to_owned())
            .collect::<Vec<_>>();
        assert!(!citations.is_empty(), "a gate to cite");
        json!({
            "work_ref": named.then(|| work_ref.to_owned()),
            "mode": "same_session",
            "acceptance_basis": 1,
            "evidence_basis": store
                .work_feed_head(&crate::domain::FeedId::RunExecution(run))
                .expect("run feed head"),
            "verdicts": [{
                "criterion": 1,
                "verdict": "pass",
                "basis": "asserted",
                "rationale": "the gate passed",
                "evidence": citations,
            }],
            "attempt": format!("evaluate-{work_ref}"),
        })
    };
    let evaluate = |arguments: &Value| {
        server.evaluate(Parameters(
            serde_json::from_value::<EvaluateArgs>(arguments.clone()).expect("evaluate arguments"),
        ))
    };

    acted(
        server.gate(Parameters(bare_gate_args(Some(&focus)))),
        "gate",
    );
    let explicit = evaluation(&focus, true);
    let evaluated = acted(evaluate(&explicit), "explicit evaluate");
    let replayed = acted(evaluate(&explicit), "its exact resend");
    assert_eq!(evaluated["evaluation"]["replayed"], false, "{evaluated}");
    assert_eq!(replayed["evaluation"]["replayed"], true, "{replayed}");
    assert_eq!(
        replayed["evaluation"]["hash"], evaluated["evaluation"]["hash"],
        "{replayed}"
    );
    let done = acted(server.done(Parameters(done_args(Some(&focus)))), "done");
    assert_eq!(done["work"]["short_ref"], json!(focus), "{done}");

    acted(
        server.claim(Parameters(WorkClaimArgs {
            work_ref: Some(other.clone()),
            under: None,
            ttl_seconds: Some(36_000),
            recover: None,
        })),
        "claim",
    );
    let gated = acted(server.gate(Parameters(bare_gate_args(None))), "bare gate");
    assert_eq!(gated["work"]["short_ref"], json!(other), "{gated}");
    acted(evaluate(&evaluation(&other, false)), "bare evaluate");
    let completed = acted(server.done(Parameters(done_args(None))), "bare done");
    assert_eq!(completed["work"]["short_ref"], json!(other), "{completed}");
}
