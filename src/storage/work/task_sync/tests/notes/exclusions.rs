use super::*;
use crate::domain::{
    AcceptanceBasis, AcceptanceEvaluationPolicy, AcceptanceVerdict, CriterionVerdictInput,
    MechanicalBasis, NamedRootBindingKind, RecordAcceptanceEvaluationRequest,
    RecordGateEvidenceRequest, VerificationResult,
};
use crate::storage::work::test_support::{HostCheck, host_verification_with_outcome};

#[test]
fn task_sync_notes_exclude_each_stored_evidence_family_and_path_form() {
    let mut source = SqliteStore::open_in_memory().unwrap();
    let sentinel = || format!("excludedSentinel{}", uuid::Uuid::new_v4().simple());
    let gate_sentinel = sentinel();
    let check_sentinel = sentinel();
    let verification_sentinel = sentinel();
    let environment_sentinel = sentinel();
    let evaluation_sentinel = sentinel();
    let root_sentinel = sentinel();
    let mut request = root_request("project-a", "notes", 0);
    request.notes = vec![
        "Plain shared finding".into(),
        "See C:\\Users\\operator\\file".into(),
        "See \\\\server\\share\\file".into(),
        "See \\\\?\\C:\\file".into(),
        "See /home/operator/file".into(),
        "See ~/file".into(),
        "See file:///home/operator/file".into(),
        "See %USERPROFILE%\\file".into(),
        "See src/module.rs".into(),
        "See docs/brief.md".into(),
        "See scripts/check.mjs".into(),
        "See .worktrees/fixture".into(),
        "See target/evidence/run".into(),
        "See deadbeef:event-1".into(),
        "Commit abcdefabcdef".into(),
    ];
    let mut sentinels = vec![
        gate_sentinel.clone(),
        check_sentinel.clone(),
        verification_sentinel.clone(),
        environment_sentinel.clone(),
        evaluation_sentinel.clone(),
        root_sentinel.clone(),
    ];
    for body in request.notes.iter_mut().skip(1) {
        let token = sentinel();
        *body = format!("{token} {body}");
        sentinels.push(token);
    }
    let work = source
        .create_work(&request, &crate::DevelopmentNoopRedactor)
        .unwrap();
    let claim = claim(&mut source, &work, "holder", "claim", 1, 100);
    let work = source.get_work_item(work.work_id).unwrap();
    let gate = source
        .record_gate_evidence(
            &RecordGateEvidenceRequest {
                work_id: work.work_id,
                run_id: claim.run_id,
                expected_work_revision: work.revision,
                holder: claim.holder.clone(),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                name: gate_sentinel,
                failed: vec![],
                evidence_ref: None,
                actor: actor("holder"),
                recorded_at: at(2),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    host_verification_with_outcome(
        &mut source,
        &work,
        &claim,
        "holder",
        HostCheck {
            key: &check_sentinel,
            kind: VerificationKind::Test,
            outcome: crate::domain::ExecutionOutcome::Succeeded,
            result: VerificationResult::Passed,
            summary: &verification_sentinel,
        },
        3,
        crate::domain::ExecutionSourceBasis {
            workspace_id: environment_sentinel,
            source_revision: "fixture-cut".into(),
            source_root_generation: None,
            source_root_state: None,
        },
    );
    source
        .set_acceptance_evaluation_policy(
            &AcceptanceEvaluationPolicy {
                allowed_modes: vec![AcceptanceEvaluationMode::SameSession],
                mechanical_basis: MechanicalBasis::Asserted,
                require_source_freshness: false,
            },
            &actor("admin"),
            "policy",
            None,
            at(5),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let work = source.get_work_item(work.work_id).unwrap();
    source
        .record_acceptance_evaluation(
            &RecordAcceptanceEvaluationRequest {
                supersedes: None,
                project_id: work.project_id.clone(),
                work_id: work.work_id,
                expected_work_revision: work.revision,
                evaluated_through: source
                    .work_feed_head(&crate::FeedId::RunExecution(claim.run_id))
                    .unwrap(),
                mode: AcceptanceEvaluationMode::SameSession,
                execution_identity: None,
                parent_session: None,
                evaluator_model: None,
                source_basis: None,
                evaluator: actor("holder"),
                attempt_key: None,
                recorded_at: at(6),
                verdicts: vec![CriterionVerdictInput {
                    criterion: 1,
                    verdict: AcceptanceVerdict::Pass,
                    basis: AcceptanceBasis::Asserted,
                    rationale: evaluation_sentinel,
                    evidence: vec![gate],
                }],
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    // The evaluation has no named-root obligation. Bind the independent
    // path fixture afterwards, without weakening root admission.
    let host = crate::storage::test_support::bind_control_for(
        &mut source,
        "holder",
        "host-root",
        &[crate::domain::EffectClass::Observe],
        at(7),
    );
    source
        .bind_named_root(
            &work.project_id,
            &host.status.session_id,
            &host.connection_token,
            &host.routing_token,
            claim.claim_id,
            claim.fence,
            &root_sentinel,
            1,
            at(7),
            NamedRootBindingKind::Bound,
            None,
            &mut actor("holder"),
            "bind-root",
            at(7),
        )
        .unwrap();
    let mut protected = root_request("project-a", "restricted", 8);
    protected.labels.push("restricted".into());
    let restricted_sentinel = sentinel();
    protected.notes.push(restricted_sentinel.clone());
    sentinels.push(restricted_sentinel);
    source
        .create_work(&protected, &crate::DevelopmentNoopRedactor)
        .unwrap();
    let ids = source.connection.prepare("SELECT object_id FROM objects WHERE object_kind IN ('work_evidence','work_observation','work_restored_evidence','verification_evidence','environment_evidence','acceptance_evaluation','execution_observation','named_root_binding') ORDER BY object_id").unwrap()
        .query_map([], |row| row.get::<_,String>(0)).unwrap().collect::<Result<Vec<_>,_>>().unwrap();
    let before = state(&source);
    let result = source
        .export_task_tree_with_notes(&work.project_id, "host-A")
        .unwrap();
    assert_eq!(state(&source), before);
    for reason in [
        "gate",
        "host verification",
        "environment",
        "evaluation",
        "execution observation",
        "path binding",
        "restricted task",
    ] {
        assert_eq!(
            result.omissions.counts.get(reason).copied().unwrap_or(0),
            1,
            "{reason}: {:?}",
            result.omissions.counts
        );
    }
    assert_eq!(result.omissions.counts["machine or user path"], 7);
    assert_eq!(result.omissions.counts["repository or evidence path"], 5);
    assert_eq!(result.omissions.counts["possible locator"], 2);
    let notes = result
        .tree
        .tasks
        .iter()
        .flat_map(|t| &t.notes)
        .collect::<Vec<_>>();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].body, "Plain shared finding");
    let bytes = serde_json::to_string(&result.tree).unwrap();
    for token in &sentinels {
        assert!(!bytes.contains(token), "excluded sentinel {token}");
    }
    for id in ids {
        // The one plain note is deliberately admitted; every other seeded
        // canonical evidence identity must stay local.
        if notes[0].id.as_str() != id {
            assert!(!bytes.contains(&id), "excluded record {id}");
        }
    }
    for secret in [
        "test gate",
        "host observed",
        "fixture evaluation",
        "local-workspace",
        "Restricted task note",
        "Users",
        "deadbeef",
    ] {
        assert!(!bytes.contains(secret), "{secret}");
    }
    // Host-private retained prompts have no Engram record type, hence no
    // allow-list branch or inferred host-private count in this receipt.
    assert!(!result.omissions.counts.contains_key("retained prompt"));
    assert!(source.verify_all().unwrap().is_healthy());
}

#[test]
fn task_sync_notes_strict_profile_rejects_forbidden_text_and_metadata() {
    for text in [
        "/tmp/run",
        "C:/fixture",
        "\\\\.\\pipe\\host",
        "~\\file",
        "$HOME/file",
        "${HOME}/file",
        "$env:USERPROFILE/file",
        ".engram/tasks/item",
        "evidence/result",
        "scratch/private",
        "scripts\\test.ps1",
        "0123456789abcdef:1",
    ] {
        let mut tree = note_candidate();
        tree.tasks[0].notes[0].body = text.into();
        assert!(tree.validate().is_err(), "{text}");
    }
    for machine in ["", "a b", "../host", "C:\\host", &"a".repeat(65)] {
        let mut tree = note_candidate();
        tree.tasks[0].notes[0].origin_machine = machine.into();
        assert!(tree.validate().is_err(), "{machine}");
    }
    for label in ["restricted", "secret", "secret-ref"] {
        let mut tree = note_candidate();
        tree.tasks[0].labels.push(label.into());
        assert!(tree.validate().is_err());
    }
    let mut tree = note_candidate();
    tree.tasks[0].notes[0].body = "Progress on w-123456abcdef by greg/codex".into();
    // Task references stay allowed even though their suffix resembles a locator.
    assert!(tree.validate().is_ok());
    tree.tasks[0].notes[0].body = "Ω🙂 progress stays verbatim".into();
    assert!(tree.validate().is_ok());
    let mut documents = tree.documents().unwrap();
    let path = format!("tasks/{}.json", tree.tasks[0].id.0);
    let mut value: serde_json::Value = serde_json::from_slice(&documents[&path]).unwrap();
    value["notes"][0]["author"]["session_id"] = "host-private".into();
    documents.insert(path, serde_json::to_vec(&value).unwrap());
    assert!(TaskTree::from_documents(&documents).is_err());
}
