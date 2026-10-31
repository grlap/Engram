use super::super::test_support::{actor, at, claim, root_request};
use super::*;
use crate::DevelopmentNoopRedactor;
use crate::domain::*;

struct Fixture {
    store: SqliteStore,
    project: ProjectId,
    session: SessionId,
    connection: String,
    routing: String,
    request: RegisteredVerificationRequest,
}

fn fixture(assurance: crate::ControlAssurance) -> Fixture {
    fixture_offset(assurance, 0)
}

fn fixture_offset(assurance: crate::ControlAssurance, offset: i64) -> Fixture {
    fixture_in(SqliteStore::open_in_memory().unwrap(), assurance, offset)
}

fn fixture_in(store: SqliteStore, assurance: crate::ControlAssurance, offset: i64) -> Fixture {
    fixture_with_root(store, assurance, offset, true)
}

fn fixture_with_root(
    mut store: SqliteStore,
    assurance: crate::ControlAssurance,
    offset: i64,
    pin_root: bool,
) -> Fixture {
    let at = |second| at(second + offset);
    let work = store
        .create_work(
            &root_request("project-a", "registered-work", 1 + offset),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let claim = claim(&mut store, &work, "runner", "claim", 2 + offset, 3600);
    let work = store.get_work_item(work.work_id).unwrap();
    let session = SessionId("runner".into());
    let project = work.project_id.clone();
    let connection = store.resume_control_connection(&session, at(3)).unwrap();
    let bound = store
        .bind_control_session(
            &project,
            "registered-host",
            "Registered run host",
            &session,
            &connection,
            &actor("runner"),
            assurance,
            &[crate::EffectClass::Observe],
            1,
            "bind",
            at(3),
        )
        .unwrap();
    if pin_root {
        store
            .bind_named_root(
                &project,
                &session,
                &connection,
                &bound.routing_token,
                claim.claim_id,
                claim.fence,
                "workspace-a",
                1,
                at(4),
                NamedRootBindingKind::Bound,
                None,
                &mut actor("runner"),
                "name",
                at(4),
            )
            .unwrap();
    }
    let request = RegisteredVerificationRequest {
        operation_key: "termal-run:test-native".into(),
        registration: RegisteredVerificationAssociation {
            host_run_id: "test-native".into(),
            request_identity: "host-registered-request".into(),
            registered_at: at(5),
            initiating_session_id: session.clone(),
            owning_session_id: None,
            binding: ControlWorkBinding {
                root_execution_id: store.get_work_run(claim.run_id).unwrap().root_execution_id,
                work_id: work.work_id,
                run_id: claim.run_id,
                work_revision: work.revision,
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
            },
            root: RegisteredVerificationRoot {
                workspace_id: "workspace-a".into(),
                generation: 1,
                state: SourceRootState::Named,
            },
        },
        input: RegisteredVerificationInput::Commit {
            commit: "a".repeat(40),
        },
        run: RegisteredVerificationRun {
            mode: "focused".into(),
            command: "cargo test --lib".into(),
            started_at: at(5),
            finished_at: at(9),
            outcome: RegisteredVerificationOutcome::Passed,
        },
        checks: vec![RegisteredVerificationCheck {
            stage: "focused".into(),
            check_kind: VerificationKind::Test,
            command: "cargo test --lib".into(),
            check_fingerprint: crate::ObjectId::from_canonical_bytes(b"cargo test --lib"),
            started_at: Some(at(6)),
            completed_at: Some(at(9)),
            outcome: RegisteredVerificationOutcome::Passed,
            summary: Some("native test result".into()),
            refs: vec![],
            counts: None,
        }],
        post_run: RegisteredVerificationPostRun {
            outcome: RegisteredVerificationInputOutcome::Clean,
            clean_before: true,
            clean_after: true,
            dirty_paths: vec![],
            disposition: None,
        },
    };
    Fixture {
        store,
        project,
        session,
        connection,
        routing: bound.routing_token,
        request,
    }
}

impl Fixture {
    fn record(&mut self) -> Result<RegisteredVerificationReceipt, StoreError> {
        self.store.record_registered_verification(
            &self.project,
            &self.session,
            &self.connection,
            &self.routing,
            &actor("runner"),
            &self.request,
            at(10),
        )
    }

    fn snapshot(&self) -> Vec<(String, String)> {
        // The repository snapshot includes durable rows and schema, not just
        // the verification projection.
        let snapshot =
            crate::storage::test_database_shape_snapshot(&self.store.connection).unwrap();
        vec![("database".into(), format!("{snapshot:?}"))]
    }
}

#[test]
fn registered_verification_wire_and_replay_survive_root_movement() {
    let mut f = fixture(crate::ControlAssurance::Advisory);
    let mut wire = serde_json::to_value(&f.request).unwrap();
    wire["operation"] = serde_json::json!("verification_record");
    wire["routing_token"] = serde_json::json!(&f.routing);
    let parsed: crate::host::HostControlRequest = serde_json::from_value(wire).unwrap();
    assert_eq!(parsed.operation(), "verification_record");
    let receipt = f.record().unwrap();
    let binding = f.request.registration.binding.clone();
    f.store
        .bind_named_root(
            &f.project,
            &f.session,
            &f.connection,
            &f.routing,
            binding.claim_id,
            binding.claim_fence,
            "workspace-b",
            2,
            at(11),
            NamedRootBindingKind::Bound,
            None,
            &mut actor("runner"),
            "rename",
            at(11),
        )
        .unwrap();
    let snapshot = f.snapshot();
    assert_eq!(f.record().unwrap(), receipt);
    assert_eq!(f.snapshot(), snapshot);
    f.request.registration.host_run_id = "test-new".into();
    f.request.operation_key = "termal-run:test-new".into();
    assert!(
        matches!(f.record(), Err(StoreError::RegisteredVerificationRefused { field, .. }) if field == "root")
    );
    assert_eq!(f.snapshot(), snapshot);
}

#[test]
fn registered_verification_dirty_input_never_records_credit() {
    let mut f = fixture(crate::ControlAssurance::Advisory);
    f.request.post_run.clean_before = false;
    let snapshot = f.snapshot();
    assert!(
        matches!(f.record(), Err(StoreError::RegisteredVerificationRefused { field, .. }) if field == "post_run")
    );
    assert_eq!(f.snapshot(), snapshot);
}

#[test]
fn registered_verification_safe_text_refusals_have_no_effects() {
    let mut f = fixture(crate::ControlAssurance::Advisory);
    let original = f.request.clone();
    for variant in 0..6 {
        f.request = original.clone();
        match variant {
            0 => f.request.checks[0].summary = Some(" ".into()),
            1 => f.request.checks[0].refs = vec![" untrimmed".into()],
            2 => f.request.checks[0].summary = Some("bad\u{202e}text".into()),
            3 => f.request.registration.owning_session_id = Some("x".repeat(513)),
            4 => f.request.post_run.disposition = Some("bad\u{202e}text".into()),
            _ => {
                f.request.checks[0].counts =
                    Some(std::collections::BTreeMap::from([("x".repeat(129), 1)]));
            }
        }
        let before = f.snapshot();
        assert!(
            matches!(
                f.record(),
                Err(StoreError::RegisteredVerificationRefused { .. })
            ),
            "variant {variant}"
        );
        assert_eq!(f.snapshot(), before);
    }
}

#[test]
fn registered_verification_delivery_next_and_peek_for_author_and_peer() {
    use crate::work_service::{
        LocalWorkService, WorkChangeProjection, WorkNextQuery, WorkNextSection,
    };
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("work.db");
    let store =
        SqliteStore::open_with_host_path_policy(&database, crate::HostPathPolicy::host_default())
            .unwrap();
    let mut f = fixture_in(store, crate::ControlAssurance::Advisory, 0);
    f.record().unwrap();
    let project = f.project.clone();
    drop(f.store);
    for session in ["runner", "peer"] {
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            session.into(),
            SessionId(session.into()),
            None,
        );
        let query = WorkNextQuery {
            sections: vec![WorkNextSection::Changes],
            ..WorkNextQuery::default()
        };
        let peek = service
            .work_next_peek_for_agent(100, 100, true, query.clone(), at(11), |_| true)
            .unwrap();
        let next = service.work_next(100, query, at(12)).unwrap();
        if session == "peer" {
            for changes in [peek.changes, next.changes] {
                assert!(changes.unwrap().iter().any(|change| matches!(&change.delivery,
                    WorkChangeProjection::Visible(summary) if summary.change_kind == "registered_host_run"
                    && summary.summary.contains("host-asserted"))));
            }
        }
    }
}

#[test]
fn registered_verification_on_and_off_is_grant_free_and_replays() {
    for assurance in [
        crate::ControlAssurance::TurnGated,
        crate::ControlAssurance::Advisory,
    ] {
        let mut f = fixture(assurance);
        let receipt = f.record().unwrap();
        assert_eq!(receipt.verifications.len(), 1);
        let snapshot = f.snapshot();
        assert_eq!(f.record().unwrap(), receipt);
        assert_eq!(f.snapshot(), snapshot);
        let evidence: VerificationEvidence = super::super::feeds::load_typed_work_object(
            &f.store.connection,
            &receipt.verifications[0].record_id,
            "verification_evidence",
        )
        .unwrap();
        let producer =
            super::super::load_verification_producer_on(&f.store.connection, &evidence).unwrap();
        assert_eq!(producer.outcome, ExecutionOutcome::Succeeded);
        assert_eq!(
            producer.source_basis.unwrap().source_revision,
            f.request.input.revision()
        );
        assert_eq!(f.store.connection.query_row("SELECT count(*) FROM objects WHERE object_kind IN ('execution_observation', 'turn_grant')", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
        assert!(f.store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn registered_verification_failure_is_not_a_pass_and_unrun_has_no_credit() {
    let mut f = fixture(crate::ControlAssurance::Advisory);
    f.request.run.outcome = RegisteredVerificationOutcome::Failed;
    f.request.checks[0].outcome = RegisteredVerificationOutcome::Failed;
    let mut unrun = f.request.checks[0].clone();
    unrun.stage = "later".into();
    unrun.started_at = None;
    unrun.completed_at = None;
    unrun.outcome = RegisteredVerificationOutcome::NotRun;
    f.request.checks.push(unrun);
    let receipt = f.record().unwrap();
    assert_eq!(receipt.verifications.len(), 1);
    let evidence: VerificationEvidence = super::super::feeds::load_typed_work_object(
        &f.store.connection,
        &receipt.verifications[0].record_id,
        "verification_evidence",
    )
    .unwrap();
    assert_eq!(evidence.result, VerificationResult::Failed);
    assert_eq!(receipt.run_outcome, RegisteredVerificationOutcome::Failed);
}

pub(in crate::storage::work) fn registered_verification_case(
    outcome: RegisteredVerificationOutcome,
) -> (SqliteStore, RegisteredVerificationReceipt) {
    let mut f = fixture(crate::ControlAssurance::Advisory);
    f.request.run.outcome = outcome;
    let receipt = f.record().unwrap();
    (f.store, receipt)
}

#[test]
fn registered_verification_never_pinned_association_refuses_without_effects() {
    for assurance in [
        crate::ControlAssurance::Advisory,
        crate::ControlAssurance::TurnGated,
    ] {
        let mut f = fixture_with_root(SqliteStore::open_in_memory().unwrap(), assurance, 0, false);
        let before = f.snapshot();
        assert!(matches!(
            f.record(),
            Err(StoreError::RegisteredVerificationRefused { field, reason })
                if field == "root" && reason.contains("no named root")
        ));
        assert_eq!(
            f.snapshot(),
            before,
            "a never-pinned association records nothing"
        );
    }
}

#[test]
fn registered_verification_refuses_moved_pin_and_changed_intent_without_effects() {
    let mut f = fixture(crate::ControlAssurance::Advisory);
    f.request.registration.root.generation += 1;
    let snapshot = f.snapshot();
    assert!(matches!(
        f.record(),
        Err(StoreError::RegisteredVerificationRefused { .. })
    ));
    assert_eq!(f.snapshot(), snapshot);
    f.request.registration.root.generation = 1;
    let receipt = f.record().unwrap();
    let snapshot = f.snapshot();
    f.request.run.command.push_str(" changed");
    assert!(matches!(
        f.record(),
        Err(StoreError::ControlOperationIdempotencyConflict { .. })
    ));
    assert_eq!(f.snapshot(), snapshot);
    f.request.run.command = "cargo test --lib".into();
    assert_eq!(f.record().unwrap(), receipt);
}

#[test]
fn registered_verification_emits_host_wire_fixtures() {
    // Host transport uses the real current time. The registration and claim
    // must therefore also be current, rather than the fixed unit-test clock.
    let offset = (chrono::Utc::now() - at(10)).num_seconds();
    let mut f = fixture_offset(crate::ControlAssurance::Advisory, offset);
    f.request.checks[0].counts = Some(std::collections::BTreeMap::from([
        ("passed".into(), 1),
        ("failed".into(), 0),
        ("unrun".into(), 0),
    ]));
    let mut request = serde_json::to_value(&f.request).unwrap();
    request["operation"] = serde_json::json!("verification_record");
    request["routing_token"] = serde_json::json!(&f.routing);
    let mut refused = request.clone();
    refused["operation_key"] = serde_json::json!("termal-run:test-refused");
    refused["registration"]["host_run_id"] = serde_json::json!("test-refused");
    refused["post_run"]["clean_before"] = serde_json::json!(false);
    let mut failed = request.clone();
    failed["operation_key"] = serde_json::json!("termal-run:test-failed");
    failed["registration"]["host_run_id"] = serde_json::json!("test-failed");
    failed["run"]["outcome"] = serde_json::json!("failed");
    failed["checks"][0]["outcome"] = serde_json::json!("failed");
    failed["checks"][0]["counts"] = serde_json::json!({"passed": 0, "failed": 1, "unrun": 0});
    let mut unrun = f.request.checks[0].clone();
    unrun.stage = "later".into();
    unrun.started_at = None;
    unrun.completed_at = None;
    unrun.outcome = RegisteredVerificationOutcome::NotRun;
    unrun.counts = None;
    failed["checks"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::to_value(unrun).unwrap());
    let mut server = crate::host::HostControlServer::from_test_store(
        f.store,
        f.project,
        f.session,
        f.connection,
    );
    let input = format!("{request}\n{refused}\n{failed}\n");
    let mut output = Vec::new();
    server
        .serve(std::io::Cursor::new(input), &mut output)
        .unwrap();
    let replies: Vec<serde_json::Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(replies[0]["status"], "ok", "{}", replies[0]);
    let receipt: RegisteredVerificationReceipt =
        serde_json::from_value(replies[0]["result"].clone()).unwrap();
    assert_eq!(receipt.operation, receipt.producer);
    assert_eq!(receipt.verifications.len(), 1);
    assert_eq!(replies[1]["error"]["code"], "verification_record_refused");
    assert_eq!(replies[1]["error"]["details"]["field"], "post_run");
    assert_eq!(replies[2]["status"], "ok", "{}", replies[2]);
    assert_eq!(replies[2]["result"]["run_outcome"], "failed");
    assert_eq!(
        replies[2]["result"]["verifications"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let fixtures = serde_json::json!({
        "request": request,
        "response": replies[0],
        "refused_request": refused,
        "refusal": replies[1],
        "failed_request": failed,
        "failed_response": replies[2],
    });
    let directory = std::path::Path::new("target/tmp");
    std::fs::create_dir_all(directory).unwrap();
    std::fs::write(
        directory.join("registered-verification-wire.json"),
        serde_json::to_vec_pretty(&fixtures).unwrap(),
    )
    .unwrap();
}

#[test]
fn registered_verification_stale_claim_refuses_but_committed_retry_replays() {
    let mut f = fixture(crate::ControlAssurance::Advisory);
    let receipt = f.record().unwrap();
    let binding = f.request.registration.binding.clone();
    let item = f.store.get_work_item(binding.work_id).unwrap();
    // Retaking after expiry advances the fence, even for the same holder.
    let retaken = claim(&mut f.store, &item, "runner", "retake", 4000, 3600);
    assert!(retaken.fence > binding.claim_fence);
    let snapshot = f.snapshot();
    assert_eq!(f.record().unwrap(), receipt);
    assert_eq!(f.snapshot(), snapshot);
    f.request.operation_key = "termal-run:test-stale".into();
    f.request.registration.host_run_id = "test-stale".into();
    assert!(
        matches!(f.record(), Err(StoreError::RegisteredVerificationRefused { field, .. }) if field == "claim")
    );
    assert_eq!(f.snapshot(), snapshot);
}

#[test]
fn registered_verification_import_and_projection_repair_preserve_receipt_and_ids() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let target = directory.path().join("target.db");
    let export = directory.path().join("store.jsonl");
    let store =
        SqliteStore::open_with_host_path_policy(&source, crate::HostPathPolicy::host_default())
            .unwrap();
    let mut f = fixture_in(store, crate::ControlAssurance::Advisory, 0);
    let receipt = f.record().unwrap();
    let producer_bytes: Vec<u8> = f
        .store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id = ?1",
            [receipt.producer.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    drop(f.store);
    crate::storage::migration::export_json(&source, &export).unwrap();
    crate::storage::migration::import_json(&export, &target).unwrap();
    assert!(
        SqliteStore::repair_rebuildable_projections(&target)
            .unwrap()
            .is_healthy()
    );
    f.store =
        SqliteStore::open_with_host_path_policy(&target, crate::HostPathPolicy::host_default())
            .unwrap();
    f.connection = f
        .store
        .resume_control_connection(&f.session, at(11))
        .unwrap();
    assert_eq!(f.record().unwrap(), receipt);
    let imported_bytes: Vec<u8> = f
        .store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id = ?1",
            [receipt.producer.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(imported_bytes, producer_bytes);
    assert!(f.store.verify_all().unwrap().is_healthy());
}
