use super::*;
use crate::domain::{AcceptanceSourceMismatch as Mismatch, AcceptanceSourceRemedy as Remedy};
use chrono::{DateTime, Utc};

pub(crate) struct SourceRecoveryTransportFixture {
    owner: Fixture,
    pub database: std::path::PathBuf,
    pub work: WorkItem,
    pub evaluation: ObjectId,
    pub presented: Option<String>,
    pub mismatch: Mismatch,
}

/// A declared source past the agent projection's 128-byte bound, in a
/// multibyte script whose 128th byte falls inside a character.
pub(crate) fn long_declared_source() -> String {
    "源".repeat(43)
}

/// A presented measurement one byte past that bound.
pub(crate) fn long_presented_source() -> String {
    "m".repeat(129)
}

/// B21/B22/B77/B78: real histories reused by service and MCP tests. The
/// `long_` cases repeat `unconfirmed` and `mismatch` with host-recorded
/// strings past the projection bound.
pub(crate) fn source_recovery_transport_fixture(
    case: &str,
    now: DateTime<Utc>,
) -> SourceRecoveryTransportFixture {
    let mut fixture = fixture("source-recovery-transports");
    let second = (now - at(0)).num_seconds() - 60;
    let work = fixture.work.clone();
    let note = fixture.evidence.clone();
    let store = &mut fixture.store;
    let claim = claim(store, &work, "runner", "renew-source-claim", second, 3600);
    let named = matches!(case, "unconfirmed" | "long_unconfirmed");
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        !named,
        "source-policy",
        second + 1,
    );
    if named {
        let mut host = HostSession::bind(store, &work, &claim, second + 2);
        store
            .bind_named_root(
                &work.project_id,
                &host.session_id,
                &host.connection_token,
                &host.routing_token,
                claim.claim_id,
                claim.fence,
                "C:/database is locked/源",
                9,
                at(second + 3),
                NamedRootBindingKind::Bound,
                None,
                &mut actor("runner"),
                "source-root",
                at(second + 3),
            )
            .unwrap();
        host.basis = ExecutionSourceBasis {
            workspace_id: "C:/database is locked/源".into(),
            source_revision: "R1".into(),
            source_root_generation: Some(9),
            source_root_state: Some(SourceRootState::Named),
        };
        host.checkpoint(
            store,
            false,
            Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
            second + 10,
        );
    }
    let mut input = request(
        &work,
        cut(store, &work),
        "runner",
        Mode::SameSession,
        vec![verdict(
            1,
            AcceptanceVerdict::Pass,
            AcceptanceBasis::Judgment,
            std::slice::from_ref(&note),
        )],
        second + 20,
    );
    if case != "no_basis" {
        input.source_basis = Some(AcceptanceSourceBasis {
            workspace_id: None,
            fingerprint: match case {
                "unconfirmed" => "R2".into(),
                "long_unconfirmed" | "long_mismatch" => long_declared_source(),
                _ => "measured-A".into(),
            },
        });
    }
    let evaluation = record(store, &input).unwrap().evaluation;
    let (presented, mismatch) = match case {
        "unconfirmed" | "long_unconfirmed" => (None, Mismatch::UnconfirmedDeclaration),
        "long_mismatch" => (
            Some(long_presented_source()),
            Mismatch::CompletionFingerprintMismatch,
        ),
        "missing" => (None, Mismatch::CompletionMeasurementMissing),
        "mismatch" => (
            Some("measured-B".into()),
            Mismatch::CompletionFingerprintMismatch,
        ),
        "no_basis" => (
            Some("measured-A".into()),
            Mismatch::EvaluationSourceBasisMissing,
        ),
        _ => panic!("unknown source fixture"),
    };
    SourceRecoveryTransportFixture {
        database: fixture.directory.path().join("engram.sqlite3"),
        owner: fixture,
        work,
        evaluation,
        presented,
        mismatch,
    }
}

// B21/B22: each fingerprint failure chooses its own action and retains the snapshot.
#[test]
fn source_recovery_fingerprint_causes_are_distinct_and_reads_stay_unmeasured() {
    for (case, remedy) in [
        ("missing", Remedy::MeasureSourceAndRetry),
        ("mismatch", Remedy::EvaluateCurrentSource),
        ("no_basis", Remedy::EvaluateCurrentSource),
    ] {
        let fixture = source_recovery_transport_fixture(case, at(100));
        let store = &fixture.owner.store;
        let before = test_database_shape_snapshot(&store.connection);
        let readiness = store
            .acceptance_evaluation_readiness(
                fixture.work.work_id,
                fixture.work.active_run_id.unwrap(),
                fixture.presented.as_deref(),
            )
            .unwrap();
        let AcceptanceEvaluationReadiness::Blocked(cause, context) = readiness else {
            panic!("must refuse")
        };
        assert_eq!(
            cause,
            WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
                reason: AcceptanceStaleReason::Source
            }
        );
        let source = context.source.unwrap();
        assert_eq!(source.mismatch, fixture.mismatch);
        assert_eq!(source.remedy, remedy);
        assert_eq!(source.evaluation, fixture.evaluation);
        assert_eq!(source.presented_fingerprint, fixture.presented);
        assert_eq!(
            source.expected_fingerprint.as_deref(),
            (case != "no_basis").then_some("measured-A")
        );
        assert!(context.deciding_observation.is_none());
        let shown = store
            .acceptance_evaluation_status(fixture.work.work_id, None)
            .unwrap()
            .unwrap();
        if case == "no_basis" {
            assert_eq!(
                shown.source_recovery.unwrap().mismatch,
                Mismatch::EvaluationSourceBasisMissing
            );
        } else {
            assert_eq!(shown.stale, None);
            assert!(shown.source_checked_at_done);
            assert!(shown.source_recovery.is_none());
        }
        assert_eq!(before, test_database_shape_snapshot(&store.connection));
    }
}

// Defensive assessment only: normal admission requires a sighting at the cut.
// Keep this unreachable-input fallback separate from persisted histories.
#[test]
fn source_recovery_undeclared_unsighted_cut_requests_a_new_judgment() {
    let fixture = source_recovery_transport_fixture("unconfirmed", at(100));
    let store = &fixture.owner.store;
    let run = fixture.work.active_run_id.unwrap();
    let (_, mut record) = latest_on(&store.connection, run).unwrap().unwrap();
    let root = named_root_at_on(&store.connection, run, i64::MAX)
        .unwrap()
        .unwrap();
    record.source_basis = None;
    record.evaluated_cut.position = root.position;
    let policy = store.acceptance_evaluation_policy().unwrap();
    let before = test_database_shape_snapshot(&store.connection);
    let (reason, context) = staleness_after_move(
        &store.connection,
        &fixture.work,
        run,
        &policy,
        &fixture.evaluation,
        &record,
        SourceCheck::Unmeasured,
        Some(&root),
    )
    .unwrap();
    assert_eq!(reason, Some(AcceptanceStaleReason::Source));
    let source = context.source.unwrap();
    assert_eq!(source.mismatch, Mismatch::UnconfirmedEvaluatedRevision);
    assert_eq!(source.remedy, Remedy::ReadSourceAndEvaluate);
    assert_eq!(source.root_binding, Some(root.event_id));
    assert_eq!(source.evaluated_cut, root.position);
    assert!(source.declared_revision.is_none());
    assert!(source.reported_revision.is_none());
    assert!(source.expected_fingerprint.is_none());
    assert_eq!(before, test_database_shape_snapshot(&store.connection));
}
