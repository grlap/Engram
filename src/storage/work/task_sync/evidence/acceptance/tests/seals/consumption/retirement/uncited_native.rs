//! A check unrelated to the retired import cannot inherit its obligation credit.
use super::*;
use crate::domain::{VerificationKind, VerificationResult};

fn native(
    data: &mut Selected,
    holder: &str,
    kind: VerificationKind,
    result: VerificationResult,
    second: i64,
) -> ObjectId {
    let work = data
        .target
        .store
        .get_work_item(data.target.binding.work_id)
        .unwrap();
    let held = data
        .target
        .store
        .current_work_claim(work.work_id)
        .unwrap()
        .unwrap();
    let key = format!("native-{second}");
    crate::storage::work::test_support::host_verification_with_outcome(
        &mut data.target.store,
        &work,
        &held,
        holder,
        crate::storage::work::test_support::HostCheck {
            key: &key,
            kind,
            outcome: match result {
                VerificationResult::Passed => crate::domain::ExecutionOutcome::Succeeded,
                VerificationResult::Failed => crate::domain::ExecutionOutcome::Failed,
                VerificationResult::Indeterminate => crate::domain::ExecutionOutcome::Unknown,
            },
            result,
            summary: &format!("host observed {key}"),
        },
        second,
        crate::domain::ExecutionSourceBasis {
            git_tree: None,
            workspace_id: format!("workspace-{key}"),
            source_revision: "revision-as-it-stands".into(),
            source_root_generation: None,
            source_root_state: None,
        },
    )
}

fn no_root(data: &Selected) {
    let held = data
        .target
        .store
        .current_work_claim(data.target.binding.work_id)
        .unwrap()
        .unwrap();
    let cut = data
        .target
        .store
        .work_feed_head(&FeedId::RunExecution(held.run_id))
        .unwrap();
    assert!(
        crate::storage::work::completion::named_root_at_cut_on(
            &data.target.store.connection,
            held.run_id,
            held.claim_id,
            cut,
        )
        .unwrap()
        .is_none()
    );
    assert!(
        crate::storage::work::feeds::latest_source_mutation_on(
            &data.target.store.connection,
            held.run_id,
            cut,
        )
        .unwrap()
        .is_none()
    );
}

fn handoff_without_root(data: &mut Selected) -> (String, i64) {
    let binding = &data.target.binding;
    let root = data
        .target
        .store
        .read_named_root(
            &project(),
            &session(),
            &data.target.token,
            HostSessionAuthority::Work,
            binding.run_id,
            binding.claim_id,
        )
        .unwrap();
    let NamedRootState::Bound {
        generation,
        named_at,
        ..
    } = root.named_root
    else {
        panic!("fixture must first have a bound root")
    };
    data.target
        .store
        .bind_named_root(
            &project(),
            &session(),
            &data.target.token,
            HostSessionAuthority::Work,
            binding.claim_id,
            binding.claim_fence,
            &data.target.workspace,
            generation,
            named_at,
            NamedRootBindingKind::Ended,
            Some(crate::domain::NamedRootEndReason::ExplicitClear),
            &mut actor("runner"),
            "uncited-native-end-root",
            at(24),
        )
        .unwrap();
    no_root(data);
    move_destination(data, "handoff")
}

fn history(data: &Selected) -> Vec<(String, Vec<u8>)> {
    let mut statement = data
        .target
        .store
        .connection
        .prepare(
            "SELECT object_id,canonical_json FROM objects WHERE object_kind IN
        ('imported_evidence_consumption','work_obligation_resolution') ORDER BY object_id",
        )
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap()
}

#[test]
fn no_root_retired_import_refuses_an_older_native_pass_without_source_mutation() {
    for acceptance in [false, true] {
        let mut data = selected(false);
        let old = native(
            &mut data,
            "runner",
            VerificationKind::Test,
            VerificationResult::Passed,
            22,
        );
        let decision = if acceptance {
            data.acceptance.decision.clone()
        } else {
            data.verification.decision.clone()
        };
        consume(&mut data.target, &decision, acceptance);
        let (holder, now) = handoff_without_root(&mut data);
        no_root(&data);
        let before = history(&data);
        assert!(finish(&mut data, &holder, &old, now + 1).is_err());
        assert_eq!(history(&data), before);
        assert_ne!(
            data.target
                .store
                .get_work_item(data.target.binding.work_id)
                .unwrap()
                .lifecycle,
            WorkLifecycle::Completed
        );
        healthy(&data.target.store);
    }
}

#[test]
fn no_root_native_replacement_after_import_carries_its_own_id() {
    for acceptance in [false, true] {
        let mut data = selected(false);
        native(
            &mut data,
            "runner",
            VerificationKind::Test,
            VerificationResult::Passed,
            22,
        );
        let decision = if acceptance {
            data.acceptance.decision.clone()
        } else {
            data.verification.decision.clone()
        };
        let used = consume(&mut data.target, &decision, acceptance);
        let before = history(&data);
        let (holder, now) = handoff_without_root(&mut data);
        no_root(&data);
        let fresh = native(
            &mut data,
            &holder,
            VerificationKind::Test,
            VerificationResult::Passed,
            now,
        );
        let seal = finish(&mut data, &holder, &fresh, now + 1).unwrap();
        assert_eq!(seal.acceptance[0].evidence, vec![fresh]);
        assert!(!seal.evidence.contains(&used.consumption));
        for row in before {
            assert!(history(&data).contains(&row));
        }
        healthy(&data.target.store);
        crate::storage::work::task_sync::evidence::consumption::rebuild(
            &data.target.store.connection,
        )
        .unwrap();
        healthy(&data.target.store);
    }
}

#[test]
fn no_root_replacement_keeps_latest_failure_and_kind_mismatch_visible() {
    for kind in [VerificationKind::Test, VerificationKind::Build] {
        let mut data = selected(false);
        let old = native(
            &mut data,
            "runner",
            VerificationKind::Test,
            VerificationResult::Passed,
            22,
        );
        let decision = data.verification.decision.clone();
        consume(&mut data.target, &decision, false);
        let (holder, now) = handoff_without_root(&mut data);
        no_root(&data);
        let result = if kind == VerificationKind::Test {
            VerificationResult::Failed
        } else {
            VerificationResult::Passed
        };
        native(&mut data, &holder, kind, result, now);
        healthy(&data.target.store);
        let before = history(&data);
        assert!(finish(&mut data, &holder, &old, now + 1).is_err());
        assert_eq!(history(&data), before);
        healthy(&data.target.store);
    }
}
