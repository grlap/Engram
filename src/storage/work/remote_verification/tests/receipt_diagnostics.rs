//! The receipt explains the existing refusal without changing its decision.

use super::*;
use crate::source::SourceId;

fn refusal_detail(f: &mut Fixture, reason: RemoteVerificationReason) -> String {
    let receipt = f.assert_uncredited(reason);
    let detail = receipt
        .qualification
        .detail
        .expect("an uncredited receipt explains the deciding check");
    for field in ["check=", "observed", "expected", "remedy="] {
        assert!(detail.contains(field), "missing {field}: {detail}");
    }
    detail
}

fn shared_source() -> SourceId {
    SourceId {
        commit: tested_commit(),
        tree: tested_tree().as_str().to_owned(),
        submodules: Vec::new(),
        changes: Vec::new(),
    }
}

fn shared(before: Option<SourceId>, after: Option<SourceId>) -> RemoteRunInput {
    RemoteRunInput::SourceId(Box::new(RemoteSourceIdInput::new(before, after)))
}

#[test]
fn remote_receipt_diagnostics_revision_names_carried_and_current_values() {
    let mut f = fixture();
    let current = f.request.association.binding.work_revision;
    let carried = current + 7;
    f.request.association.binding.work_revision = carried;
    let detail = refusal_detail(&mut f, RemoteVerificationReason::AssociationStale);
    assert!(detail.contains(&format!("observed_work_revision={carried}")));
    assert!(detail.contains(&format!("expected_work_revision={current}")));
}

#[test]
fn remote_receipt_diagnostics_pending_handoff_names_the_offer_and_recovery() {
    let mut f = fixture();
    let binding = f.request.association.binding.clone();
    let offer = f
        .store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: binding.work_id,
                run_id: binding.run_id,
                expected_work_revision: binding.work_revision,
                from: f.session.clone(),
                to: SessionId("recipient".into()),
                claim_id: binding.claim_id,
                claim_fence: binding.claim_fence,
                ttl_seconds: 60,
                checkpoint_summary: "Hand over the current work".into(),
                actor: actor("runner"),
                idempotency_key: "diagnostic-handoff".into(),
                offered_at: at(8),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let detail = refusal_detail(&mut f, RemoteVerificationReason::AssociationStale);
    for field in [
        format!("current_handoff_offer_id={}", offer.offer_id.0),
        "current_handoff_state=offered".into(),
        format!(
            "current_handoff_expires_at={}",
            offer.expires_at.to_rfc3339()
        ),
        "cancel".into(),
        "accept".into(),
        "expiry".into(),
    ] {
        assert!(detail.contains(&field), "missing {field}: {detail}");
    }
    f.store
        .cancel_work_handoff(
            &CancelWorkHandoffRequest {
                work_id: binding.work_id,
                run_id: binding.run_id,
                expected_work_revision: binding.work_revision,
                holder: f.session.clone(),
                offer_id: offer.offer_id,
                claim_id: binding.claim_id,
                claim_fence: binding.claim_fence,
                reason: "Continue on the same holder".into(),
                actor: actor("runner"),
                idempotency_key: "diagnostic-handoff-cancel".into(),
                cancelled_at: at(11),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    f.request.operation_key = "remote:after-handoff-cancel".into();
    assert!(f.record_at(12).unwrap().qualification.credited);
    f.assert_healthy();
}

#[test]
fn remote_receipt_diagnostics_root_names_carried_and_current_values() {
    let mut f = fixture();
    let current = f.request.association.root.clone();
    f.request.association.root.workspace_id = "workspace-other".into();
    f.request.association.root.generation += 7;
    let carried = f.request.association.root.clone();
    let detail = refusal_detail(&mut f, RemoteVerificationReason::AssociationStale);
    for value in [
        format!("observed_workspace_id={}", carried.workspace_id),
        format!("expected_workspace_id={}", current.workspace_id),
        format!("observed_generation={}", carried.generation),
        format!("expected_generation={}", current.generation),
    ] {
        assert!(detail.contains(&value), "missing {value}: {detail}");
    }
}

#[test]
fn remote_receipt_diagnostics_fence_names_carried_and_current_values() {
    let mut f = fixture();
    let current = f.request.association.binding.claim_fence;
    let carried = current + 7;
    f.request.association.binding.claim_fence = carried;
    let detail = refusal_detail(&mut f, RemoteVerificationReason::AssociationStale);
    assert!(detail.contains(&format!("observed_claim_fence={carried}")));
    assert!(detail.contains(&format!("expected_claim_fence={current}")));
    assert!(
        detail.contains(
            f.request
                .association
                .binding
                .claim_id
                .0
                .to_string()
                .as_str()
        )
    );
    assert!(detail.contains(f.request.association.binding.run_id.0.to_string().as_str()));
    assert!(detail.contains(&f.session.0));
}

#[test]
fn remote_receipt_diagnostics_invalid_binding_names_the_current_run_and_claim() {
    type Change = fn(&mut ControlWorkBinding);
    let changes: [Change; 3] = [
        |binding| binding.run_id = WorkRunId::new(),
        |binding| binding.claim_id = WorkClaimId::new(),
        |binding| binding.root_execution_id = RootExecutionId::new(),
    ];
    for change in changes {
        let mut f = fixture();
        let expected = f.request.association.binding.clone();
        change(&mut f.request.association.binding);
        let observed = f.request.association.binding.clone();
        let detail = refusal_detail(&mut f, RemoteVerificationReason::AssociationStale);
        for value in [
            format!("observed_run_id={}", observed.run_id.0),
            format!("expected_run_id={}", expected.run_id.0),
            format!("observed_claim_id={}", observed.claim_id.0),
            format!("expected_claim_id={}", expected.claim_id.0),
            format!("expected_claim_fence={}", expected.claim_fence),
            format!(
                "expected_root_execution_id={}",
                expected.root_execution_id.0
            ),
        ] {
            assert!(detail.contains(&value), "missing {value}: {detail}");
        }
    }
}

#[test]
fn remote_receipt_diagnostics_unknown_or_foreign_item_does_not_leak_authority() {
    for foreign in [false, true] {
        let mut f = fixture();
        let destination = if foreign {
            f.store
                .create_work(
                    &root_request("private-foreign-project", "private foreign title", 1),
                    &DevelopmentNoopRedactor,
                )
                .unwrap()
                .work_id
        } else {
            WorkId::new()
        };
        f.request.association.binding.work_id = destination;
        let detail = refusal_detail(&mut f, RemoteVerificationReason::AssociationStale);
        assert!(detail.contains(&destination.0.to_string()));
        assert!(detail.contains(&f.project.0));
        assert!(!detail.contains("private-foreign-project"));
        assert!(!detail.contains("private foreign title"));
    }
}

#[test]
fn remote_receipt_diagnostics_unsupported_schema_names_versions() {
    let mut f = fixture();
    let current = REMOTE_COMPLETION_SCHEMA_VERSION;
    let carried = current + 7;
    f.request.completion.schema_version = carried;
    let detail = refusal_detail(&mut f, RemoteVerificationReason::ProducerUnsupported);
    assert!(detail.contains(&format!("observed_schema_version={carried}")));
    assert!(detail.contains(&format!("expected_schema_version={current}")));
}

#[test]
fn remote_receipt_diagnostics_commit_mismatch_names_both_commits_and_absence() {
    for shared_input in [false, true] {
        for missing in [false, true] {
            if shared_input && missing {
                continue;
            }
            let mut f = fixture();
            let other = object_name("other tested commit");
            if shared_input {
                let mut source = shared_source();
                source.commit.clone_from(&other);
                f.request.completion.input = shared(Some(source.clone()), Some(source));
            } else {
                legacy_input(&mut f.request.completion.input).commit =
                    (!missing).then_some(other.clone());
            }
            let detail = refusal_detail(&mut f, RemoteVerificationReason::CommitMismatch);
            assert!(detail.contains(&format!("expected_commit={}", tested_commit())));
            let observed = if missing { "missing" } else { &other };
            assert!(detail.contains(&format!("observed_commit={observed}")));
        }
    }
}

#[test]
fn remote_receipt_diagnostics_tree_mismatch_names_both_trees_and_absence() {
    for shared_input in [false, true] {
        for missing in [false, true] {
            if shared_input && missing {
                continue;
            }
            let mut f = fixture();
            f.request.association.intended_use = test_ref();
            let other = object_name("other tested tree");
            if shared_input {
                let mut source = shared_source();
                source.tree.clone_from(&other);
                f.request.completion.input = shared(Some(source.clone()), Some(source));
            } else {
                legacy_input(&mut f.request.completion.input).tree =
                    (!missing).then(|| GitTreeId::parse(other.clone()).unwrap());
            }
            let detail = refusal_detail(&mut f, RemoteVerificationReason::TreeMismatch);
            assert!(detail.contains(&format!("expected_tree={}", tested_tree().as_str())));
            let observed = if missing { "missing" } else { &other };
            assert!(detail.contains(&format!("observed_tree={observed}")));
        }
    }
}

#[test]
fn remote_receipt_diagnostics_source_endpoints_name_availability_and_change() {
    let before = shared_source();
    let mut after = before.clone();
    after.tree = object_name("tree changed while the run executed");
    for (first, last) in [
        (Some(before.clone()), Some(after.clone())),
        (Some(before.clone()), None),
        (None, Some(after.clone())),
        (None, None),
    ] {
        let mut f = fixture();
        f.request.completion.input = shared(first.clone(), last.clone());
        let detail = refusal_detail(&mut f, RemoteVerificationReason::SourceIdMissingOrChanged);
        for (field, source) in [("before", first), ("after", last)] {
            assert!(detail.contains(field));
            if let Some(source) = source {
                assert!(detail.contains(&source.commit));
                assert!(detail.contains(&source.tree));
            } else {
                assert!(detail.contains("unavailable") || detail.contains("missing"));
            }
        }
    }
    let mut f = fixture();
    f.request.association.intended_use = committed(RemoteVerificationPurpose::LandingGate);
    let input = legacy_input(&mut f.request.completion.input);
    input.before_fingerprint = Some("a".repeat(64));
    input.after_fingerprint = Some("b".repeat(64));
    let detail = refusal_detail(&mut f, RemoteVerificationReason::SourceIdMissingOrChanged);
    assert!(detail.contains(&"a".repeat(64)));
    assert!(detail.contains(&"b".repeat(64)));
}

#[test]
fn remote_receipt_diagnostics_focused_landing_gate_names_modes_and_purpose() {
    let mut f = fixture();
    f.request.association.intended_use = committed(RemoteVerificationPurpose::LandingGate);
    f.request.completion.mode = RemoteRunMode::Focused;
    let detail = refusal_detail(&mut f, RemoteVerificationReason::NotFullMode);
    for value in [
        "observed_mode=focused",
        "expected_mode=full",
        "landing_gate",
    ] {
        assert!(detail.contains(value), "missing {value}: {detail}");
    }
}

#[test]
fn remote_receipt_diagnostics_result_names_verdict_terminal_and_finish() {
    for missing_finish in [false, true] {
        let mut f = fixture();
        if missing_finish {
            f.request.completion.finished_at = None;
        } else {
            f.request.completion.result.verdict = RemoteRunVerdict::Fail;
            f.request.completion.result.reason = "stage-failed".into();
        }
        let verdict = if missing_finish { "PASS" } else { "FAIL" };
        let detail = refusal_detail(&mut f, RemoteVerificationReason::ResultNotPass);
        assert!(detail.contains(&format!("observed_verdict={verdict}")));
        assert!(detail.contains("expected_verdict=PASS"));
        assert!(detail.contains("terminal"));
        assert!(detail.contains("finished_at"));
    }
}

#[test]
fn remote_receipt_diagnostics_conflicting_terminal_keeps_the_original_credit() {
    let mut f = fixture();
    let first = f.record().unwrap();
    f.request.operation_key = "remote:conflicting-terminal-detail".into();
    f.request.completion.result.verdict = RemoteRunVerdict::Fail;
    f.request.completion.result.reason = "stage-failed".into();
    let detail = refusal_detail(&mut f, RemoteVerificationReason::ResultConflict);
    assert!(detail.contains(first.origin.as_ref().unwrap().as_str()));
    assert!(detail.contains("PASS"));
    assert!(detail.contains("FAIL"));
    assert_eq!(f.count("verification_evidence"), 1);
}

#[test]
fn remote_receipt_diagnostics_conflicting_source_names_the_different_blob() {
    let mut f = fixture();
    let original_blob = object_name("original blob");
    let changed_blob = object_name("changed blob");
    let mut source = shared_source();
    source.changes.push(crate::source::Change {
        path: "file".into(),
        status: " M".into(),
        origin: None,
        blob: Some(original_blob.clone()),
        mode: Some(crate::source::Mode::Regular),
    });
    f.request.completion.input = shared(Some(source.clone()), Some(source.clone()));
    let first = f.record().unwrap();
    assert!(first.qualification.credited);
    let description = f.request.completion.describe();
    source.changes[0].blob = Some(changed_blob.clone());
    f.request.operation_key = "remote:conflicting-source-detail".into();
    f.request.completion.input = shared(Some(source.clone()), Some(source));
    assert_eq!(description, f.request.completion.describe());
    let detail = refusal_detail(&mut f, RemoteVerificationReason::ResultConflict);
    assert!(detail.contains("changes[0].blob"));
    assert!(detail.contains(&format!("observed_value=\"{changed_blob}\"")));
    assert!(detail.contains(&format!("expected_value=\"{original_blob}\"")));
    assert_eq!(f.count("verification_evidence"), 1);
    f.assert_healthy();
}

#[test]
fn remote_receipt_diagnostics_debris_names_the_native_failure() {
    let mut f = fixture();
    f.request.completion.result.verdict = RemoteRunVerdict::Fail;
    f.request.completion.result.reason = NEW_UNTRACKED_ONLY_REASON.into();
    let detail = refusal_detail(
        &mut f,
        RemoteVerificationReason::UntrackedDebrisPendingDisposition,
    );
    assert!(detail.contains(NEW_UNTRACKED_ONLY_REASON));
    assert!(detail.contains("FAIL"));
}

#[test]
fn remote_receipt_diagnostics_ended_root_requires_a_new_generation() {
    let mut f = fixture();
    let binding = f.request.association.binding.clone();
    let root = f.request.association.root.clone();
    f.store
        .bind_named_root(
            &f.project,
            &f.session,
            &f.connection,
            crate::storage::HostSessionAuthority::Work,
            binding.claim_id,
            binding.claim_fence,
            &root.workspace_id,
            root.generation,
            at(4),
            NamedRootBindingKind::Ended,
            Some(NamedRootEndReason::ExplicitClear),
            &mut actor("runner"),
            "end-diagnostic-root",
            at(8),
        )
        .unwrap();
    let detail = refusal_detail(&mut f, RemoteVerificationReason::AssociationStale);
    for fact in [
        "current_state=ended",
        "strictly greater",
        "new source root generation",
    ] {
        assert!(detail.contains(fact), "{detail}");
    }
    f.store
        .bind_named_root(
            &f.project,
            &f.session,
            &f.connection,
            crate::storage::HostSessionAuthority::Work,
            binding.claim_id,
            binding.claim_fence,
            &root.workspace_id,
            root.generation + 1,
            at(11),
            NamedRootBindingKind::Bound,
            None,
            &mut actor("runner"),
            "bind-replacement-root",
            at(11),
        )
        .unwrap();
    f.request.association.root.generation += 1;
    f.request.operation_key = "remote:after-root-replacement".into();
    assert!(f.record_at(12).unwrap().qualification.credited);
    f.assert_healthy();
}

#[test]
fn remote_receipt_diagnostics_debris_recovery_accepts_any_valid_pass_reason() {
    let mut f = fixture();
    f.request.completion.result.verdict = RemoteRunVerdict::Fail;
    f.request.completion.result.reason = NEW_UNTRACKED_ONLY_REASON.into();
    f.assert_uncredited(RemoteVerificationReason::UntrackedDebrisPendingDisposition);
    f.request.completion.run_id = "after-debris-disposition".into();
    f.request.completion.result.verdict = RemoteRunVerdict::Pass;
    f.request.completion.result.reason = "checks succeeded after disposition".into();
    f.request.operation_key = "remote:after-debris-disposition".into();
    assert!(f.record_at(12).unwrap().qualification.credited);
    f.assert_healthy();
}
