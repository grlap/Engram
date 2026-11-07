//! Ordinary measured source captures preserve freshness without inventing changes.

use super::super::test_support::{actor, at, claim, root_request};
use crate::domain::*;
use crate::storage::{HostSessionAuthority, SqliteStore};
use crate::{DevelopmentNoopRedactor, ObjectId};

struct Fixture {
    store: SqliteStore,
    project: ProjectId,
    session: SessionId,
    connection: String,
    input: ExecutionObserveInput,
}

fn fixture() -> Fixture {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let work = store
        .create_work(
            &root_request("project-a", "source-sighting", 1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let claim = claim(&mut store, &work, "runner", "claim", 2, 300);
    let run = store.get_work_run(claim.run_id).unwrap();
    let session = SessionId("runner".into());
    let connection = store.resume_work_connection(&session, at(3)).unwrap();
    let policy = SqliteStore::load_active_control_policy(&store.connection).unwrap();
    let input = ExecutionObserveInput {
        idempotency_key: "quiet-source".into(),
        binding: ControlWorkBinding {
            root_execution_id: run.root_execution_id,
            work_id: work.work_id,
            run_id: run.run_id,
            work_revision: claim.accepted_work_revision,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
        },
        root_basis: ObservationRootBasis {
            capture_run_cut: store
                .work_feed_head(&FeedId::RunExecution(run.run_id))
                .unwrap(),
            latest_event: None,
            state: NamedRootState::NoRoot,
        },
        observed_interval: ObservedInterval {
            from: at(4),
            through: at(5),
        },
        occurrence: ObservedOccurrence::SourceSighting {
            source_basis: (ExecutionSourceBasis {
                workspace_id: "workspace-a".into(),
                source_revision: "revision-a".into(),
                source_root_generation: None,
                source_root_state: None,
                git_tree: None,
            })
            .into(),
            observed_at: at(5),
        },
        causality: ObservationCausality::Unknown {},
        policy_basis: ObservationPolicyBasis::AccountIfEligible {
            project_policy_epoch: policy.epoch,
            policy: policy.policy_id,
            obligation_rule_set: policy.obligation_rule_set,
        },
    };
    Fixture {
        store,
        project: work.project_id,
        session,
        connection,
        input,
    }
}

fn record(f: &mut Fixture) -> Result<ExecutionObservationReceipt, crate::StoreError> {
    let mut observer = actor(&f.session.0);
    observer.run_id = Some(f.input.binding.run_id.0.to_string());
    f.store.record_unadmitted_execution_observation(
        &f.project,
        &f.session,
        &f.connection,
        HostSessionAuthority::Work,
        &observer,
        f.input.clone(),
        at(6),
    )
}

fn view(f: &Fixture, id: &ObjectId) -> Option<SourceObservation> {
    let stored: UnadmittedExecutionObservation =
        SqliteStore::get_typed_object(&f.store, id, "unadmitted_execution_observation")
            .unwrap()
            .unwrap();
    SourceObservation::unadmitted(id.clone(), &stored)
}

#[test]
fn measured_quiet_source_is_freshness_evidence_without_mutation_or_check_credit() {
    let mut f = fixture();
    let receipt = record(&mut f).unwrap();
    assert_eq!(receipt.accounting, ObservationAccounting::Sighting {});
    assert_eq!(receipt.opened_obligations.len(), 0);
    assert_eq!(receipt.observed_checks.len(), 0);
    let sighting = view(&f, &receipt.observation).unwrap();
    assert!(!sighting.source_changed);
    assert_eq!(sighting.reported_source_change, None);
    assert_eq!(
        sighting.source_basis.unwrap().legacy_revision(),
        Some("revision-a")
    );
    let newest = super::super::feeds::newest_measured_sighting_on(
        &f.store.connection,
        f.input.binding.run_id,
        "workspace-a",
        i64::MAX,
    )
    .unwrap();
    assert_eq!(newest.unwrap().1.legacy_revision(), Some("revision-a"));
    assert!(
        super::super::feeds::latest_source_mutation_on(
            &f.store.connection,
            f.input.binding.run_id,
            i64::MAX
        )
        .unwrap()
        .is_none()
    );
    let before = crate::storage::test_database_shape_snapshot(&f.store.connection).unwrap();
    assert_eq!(record(&mut f).unwrap(), receipt);
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&f.store.connection).unwrap(),
        before
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn audit_only_quiet_source_does_not_qualify_as_freshness() {
    let mut f = fixture();
    f.input.policy_basis = ObservationPolicyBasis::AuditOnly {};
    let receipt = record(&mut f).unwrap();
    assert!(matches!(
        receipt.accounting,
        ObservationAccounting::AuditOnly {
            reason: ObservationAuditReason::ExplicitAudit
        }
    ));
    assert!(view(&f, &receipt.observation).is_none());
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn a_capture_before_a_later_root_name_remains_audit_only() {
    let mut f = fixture();
    let binding = &f.input.binding;
    f.store
        .bind_named_root(
            &f.project,
            &f.session,
            &f.connection,
            HostSessionAuthority::Work,
            binding.claim_id,
            binding.claim_fence,
            "workspace-a",
            1,
            at(4),
            NamedRootBindingKind::Bound,
            None,
            &mut actor("runner"),
            "name-after-capture",
            at(4),
        )
        .unwrap();
    let receipt = record(&mut f).unwrap();
    assert!(matches!(
        receipt.accounting,
        ObservationAccounting::AuditOnly {
            reason: ObservationAuditReason::RootBasisMoved
        }
    ));
    assert!(view(&f, &receipt.observation).is_none());
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn a_capture_of_a_released_claim_remains_audit_only() {
    let mut f = fixture();
    let binding = &f.input.binding;
    f.store
        .release_work(
            &ReleaseWorkRequest {
                work_id: binding.work_id,
                run_id: binding.run_id,
                expected_work_revision: binding.work_revision,
                holder: f.session.clone(),
                claim_id: binding.claim_id,
                claim_fence: binding.claim_fence,
                reason: "release after capture".into(),
                waiver_reason: Some("the fixture abandons the remaining work after capture".into()),
                actor: actor("runner"),
                idempotency_key: "release".into(),
                released_at: at(4),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let receipt = record(&mut f).unwrap();
    assert!(matches!(
        receipt.accounting,
        ObservationAccounting::AuditOnly {
            reason: ObservationAuditReason::HistoricalBinding
        }
    ));
    assert!(view(&f, &receipt.observation).is_none());
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn quiet_source_refuses_an_invented_named_root_without_recording() {
    let mut f = fixture();
    let ObservedOccurrence::SourceSighting { source_basis, .. } = &mut f.input.occurrence else {
        unreachable!()
    };
    source_basis.source_root_generation = Some(1);
    source_basis.source_root_state = Some(SourceRootState::Named);
    let before = crate::storage::test_database_shape_snapshot(&f.store.connection).unwrap();
    assert!(matches!(
        record(&mut f),
        Err(crate::StoreError::ExecutionObservationBasisMismatch(_))
    ));
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&f.store.connection).unwrap(),
        before
    );
}

#[test]
fn quiet_capture_time_must_be_in_the_reported_interval() {
    let mut f = fixture();
    let ObservedOccurrence::SourceSighting { observed_at, .. } = &mut f.input.occurrence else {
        unreachable!()
    };
    *observed_at = at(6);
    let before = crate::storage::test_database_shape_snapshot(&f.store.connection).unwrap();
    assert!(
        matches!(record(&mut f), Err(crate::StoreError::ExecutionObservationInvalid(reason)) if reason.contains("source_sighting.observed_at"))
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&f.store.connection).unwrap(),
        before
    );
}
