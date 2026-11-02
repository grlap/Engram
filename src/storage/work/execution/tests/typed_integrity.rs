use super::*;
use crate::{OBLIGATION_RULE_SET_SCHEMA_VERSION, ObligationRuleSet};

fn historical_checks(store: &mut SqliteStore) -> (WorkRun, HistoricalEvidenceBatch) {
    let work = store
        .create_work(
            &root_request("typed-integrity", "create", 0),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let claim = claim(store, &work, "runner", "claim", 1, 300);
    let run = load_work_run(&store.connection, claim.run_id).unwrap();
    let capture = HistoricalCapture {
        project: work.project_id,
        session: claim.holder.clone(),
        binding: ControlWorkBinding {
            root_execution_id: run.root_execution_id,
            work_id: work.work_id,
            run_id: run.run_id,
            work_revision: claim.accepted_work_revision,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
        },
        capture_id: "historical-evidence-batch".into(),
    };
    let basis = ExecutionSourceBasis {
        git_tree: None,
        workspace_id: "historical-workspace".into(),
        source_revision: "measured-source".into(),
        source_root_generation: None,
        source_root_state: None,
    };
    let observation = |label: &str, changed, second| ExecutionObservationInput {
        observation_id: label.into(),
        action_fingerprint: ObjectId::from_canonical_bytes(label.as_bytes()),
        effect: EffectClass::MutateLocal,
        outcome: ExecutionOutcome::Succeeded,
        source_changed: changed,
        reported_source_change: None,
        source_basis: Some(basis.clone()),
        observed_at: Some(at(second)),
    };
    let components = EnvironmentComponents {
        toolchain: "fixture-rustc".into(),
        sandbox: None,
        workspace_id: basis.workspace_id.clone(),
        capability_map_revision: 1,
    };
    let batch = append_historical_capture(
        store,
        &capture,
        &[
            observation("mutation", true, 3),
            observation("test", false, 5),
        ],
        &[VerificationEvidenceInput {
            applicability: None,
            producer_observation: ExecutionObservationReference::ObservationId {
                observation_id: "test".into(),
            },
            check_kind: VerificationKind::Test,
            environment: Some(EnvironmentEvidenceReference::Index { index: 0 }),
            summary: Some("retained historical test result".into()),
            refs: vec![],
        }],
        &[EnvironmentEvidenceInput {
            source_basis: basis,
            environment_fingerprint: CanonicalObject::freeze(&components).unwrap().key().clone(),
            components: Some(components),
            observed_at: at(5),
        }],
        at(6),
    )
    .unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
    (run, batch)
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "retain all 22 independent typed-evidence column corruption regressions"
)]
fn doctor_and_evidence_reads_refuse_each_typed_projection_column_corruption() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let (run, batch) = historical_checks(&mut store);
    let observation_id = &batch.execution_observations[0];
    let producer_hash = &batch.execution_observations[1];
    let verification_hash = &batch.verification_evidence[0];
    let environment_hash = &batch.environment_evidence[0];
    let projection_corruptions = [
        (
            verification_hash,
            "verification-kind",
            "UPDATE work_run_evidence SET evidence_kind = 'environment'
             WHERE evidence_id = ?1",
            None,
        ),
        (
            verification_hash,
            "verification-workspace",
            "UPDATE work_run_evidence SET workspace_id = 'forged-workspace'
             WHERE evidence_id = ?1",
            None,
        ),
        (
            verification_hash,
            "verification-revision",
            "UPDATE work_run_evidence SET source_revision = 'forged-revision'
             WHERE evidence_id = ?1",
            None,
        ),
        (
            verification_hash,
            "verification-session",
            "UPDATE work_run_evidence SET producer_session_id = 'forged-session'
             WHERE evidence_id = ?1",
            None,
        ),
        (
            verification_hash,
            "verification-producer",
            "UPDATE work_run_evidence SET producer_observation_id = ?2
             WHERE evidence_id = ?1",
            Some(observation_id.as_str()),
        ),
        (
            verification_hash,
            "verification-check",
            "UPDATE work_run_evidence SET check_fingerprint = ?2
             WHERE evidence_id = ?1",
            Some(environment_hash.as_str()),
        ),
        (
            verification_hash,
            "verification-result",
            "UPDATE work_run_evidence SET verification_result = 'failed'
             WHERE evidence_id = ?1",
            None,
        ),
        (
            verification_hash,
            "verification-time",
            "UPDATE work_run_evidence SET observed_at_ms = observed_at_ms + 1
             WHERE evidence_id = ?1",
            None,
        ),
        (
            verification_hash,
            "verification-environment-fingerprint",
            "UPDATE work_run_evidence SET environment_fingerprint = ?2
             WHERE evidence_id = ?1",
            Some(environment_hash.as_str()),
        ),
        (
            verification_hash,
            "verification-environment-link",
            "UPDATE work_run_evidence SET environment_evidence_id = ?2
             WHERE evidence_id = ?1",
            Some(producer_hash.as_str()),
        ),
        (
            verification_hash,
            "verification-components",
            "UPDATE work_run_evidence SET components_json = X'7B7D'
             WHERE evidence_id = ?1",
            None,
        ),
        (
            environment_hash,
            "environment-kind",
            "UPDATE work_run_evidence SET evidence_kind = 'verification'
             WHERE evidence_id = ?1",
            None,
        ),
        (
            environment_hash,
            "environment-workspace",
            "UPDATE work_run_evidence SET workspace_id = 'forged-workspace'
             WHERE evidence_id = ?1",
            None,
        ),
        (
            environment_hash,
            "environment-revision",
            "UPDATE work_run_evidence SET source_revision = 'forged-revision'
             WHERE evidence_id = ?1",
            None,
        ),
        (
            environment_hash,
            "environment-session",
            "UPDATE work_run_evidence SET producer_session_id = 'forged-session'
             WHERE evidence_id = ?1",
            None,
        ),
        (
            environment_hash,
            "environment-producer",
            "UPDATE work_run_evidence SET producer_observation_id = ?2
             WHERE evidence_id = ?1",
            Some(producer_hash.as_str()),
        ),
        (
            environment_hash,
            "environment-check",
            "UPDATE work_run_evidence SET check_fingerprint = ?2
             WHERE evidence_id = ?1",
            Some(verification_hash.as_str()),
        ),
        (
            environment_hash,
            "environment-result",
            "UPDATE work_run_evidence SET verification_result = 'passed'
             WHERE evidence_id = ?1",
            None,
        ),
        (
            environment_hash,
            "environment-time",
            "UPDATE work_run_evidence SET observed_at_ms = observed_at_ms + 1
             WHERE evidence_id = ?1",
            None,
        ),
        (
            environment_hash,
            "environment-fingerprint",
            "UPDATE work_run_evidence SET environment_fingerprint = ?2
             WHERE evidence_id = ?1",
            Some(verification_hash.as_str()),
        ),
        (
            environment_hash,
            "environment-link",
            "UPDATE work_run_evidence SET environment_evidence_id = ?2
             WHERE evidence_id = ?1",
            Some(verification_hash.as_str()),
        ),
        (
            environment_hash,
            "environment-components",
            "UPDATE work_run_evidence SET components_json = X'7B7D'
             WHERE evidence_id = ?1",
            None,
        ),
    ];
    assert_eq!(projection_corruptions.len(), 22);
    for (evidence_id, label, sql, second_value) in projection_corruptions {
        store
            .connection
            .execute_batch("SAVEPOINT corrupt_typed_evidence")
            .expect("start typed-evidence corruption savepoint");
        match second_value {
            Some(value) => store
                .connection
                .execute(sql, params![evidence_id.as_str(), value]),
            None => store.connection.execute(sql, [evidence_id.as_str()]),
        }
        .unwrap_or_else(|error| panic!("corrupt {label}: {error}"));
        assert!(
            work_evidence_kind_on(&store.connection, run.run_id, evidence_id).is_err(),
            "{label} remained readable through the lifecycle path"
        );
        assert!(
            ensure_run_evidence(
                &store.connection,
                run.run_id,
                std::slice::from_ref(evidence_id),
            )
            .is_ok(),
            "{label} lost its run-membership projection"
        );
        let corrupt_report = store
            .verify_all()
            .unwrap_or_else(|error| panic!("verify {label}: {error}"));
        assert!(
            corrupt_report
                .invalid_work_records
                .iter()
                .any(|record| { record == &format!("work_evidence:{evidence_id}:run_binding") }),
            "{label} was not reported: {corrupt_report:?}"
        );
        store
            .connection
            .execute_batch("ROLLBACK TO corrupt_typed_evidence; RELEASE corrupt_typed_evidence")
            .unwrap_or_else(|error| panic!("restore {label}: {error}"));
    }
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn doctor_and_obligation_reads_refuse_a_different_rule_set_projection() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let (run, _) = historical_checks(&mut store);
    let obligations = store.work_run_obligations(run.run_id).unwrap();
    assert_eq!(obligations.len(), 1);
    let changed = store
        .set_obligation_rule_set(
            &ObligationRuleSet {
                schema_version: OBLIGATION_RULE_SET_SCHEMA_VERSION,
                rules: vec![],
            },
            &actor("admin"),
            "empty-rules",
            None,
            at(7),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    assert_ne!(
        obligations[0].obligation.rule_set,
        changed.obligation_rule_set
    );
    store
        .connection
        .execute_batch("SAVEPOINT corrupt_rule_set")
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE work_run_obligations SET rule_set_id=?1 WHERE definition_id=?2",
            params![
                changed.obligation_rule_set.as_str(),
                obligations[0].definition_id.as_str()
            ],
        )
        .unwrap();
    assert!(store.work_run_obligations(run.run_id).is_err());
    let report = store.verify_all().unwrap();
    assert!(
        report
            .invalid_work_records
            .iter()
            .any(|label| label.contains("work_obligation")),
        "{report:?}"
    );
    store
        .connection
        .execute_batch("ROLLBACK TO corrupt_rule_set; RELEASE corrupt_rule_set")
        .unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
}
