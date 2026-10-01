//! A check assessed stale names the source record that decided it, in the
//! verification record's reconstructed assessment and in the completion
//! refusal, without changing what the matcher decides.

use super::*;
use crate::control::ObligationAssessment;
use crate::domain::{
    AcceptanceBinding, ExecutionSourceBasis, OpenObligationCheck, StaleSourceDecider,
    StaleVerificationSource, VerificationEvidenceMismatch as Mismatch, VerificationRequirement,
};

fn unbound(workspace: &str, revision: &str) -> ExecutionSourceBasis {
    ExecutionSourceBasis {
        workspace_id: workspace.into(),
        source_revision: revision.into(),
        source_root_generation: None,
        source_root_state: None,
    }
}

/// A root whose first criterion is bound to a test, claimed by `runner`.
fn bound_work(store: &mut SqliteStore, project: &str) -> (WorkItem, WorkClaim) {
    let mut request = root_request(project, "create-stale-source", 1);
    request.acceptance = vec!["tests pass".into(), "docs written".into()];
    request.acceptance_bindings = vec![AcceptanceBinding {
        criterion: 1,
        requirement: VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: None,
        },
    }];
    let work = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("create bound work");
    let claim = claim(store, &work, "runner", "claim-stale-source", 2, 300);
    (work, claim)
}

fn position(store: &SqliteStore, claim: &WorkClaim, record: &ObjectId) -> i64 {
    run_feed_position_for_object_on(&store.connection, claim.run_id, record)
        .expect("run-feed position")
        .position
}

/// Done's recovery answer, after checkpointing everything on the run.
fn done_recovery(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    second: i64,
) -> crate::domain::WorkCompletionRecovery {
    let generic = evidence(store, work, claim, "runner", "generic", second);
    let all = store.work_run_evidence(claim.run_id).expect("run evidence");
    checkpoint(store, work, claim, "runner", "checkpoint", second + 1, &all);
    let answer = store
        .complete_work_for_protocol(
            &completion_request(work, claim, "runner", &generic, "complete", second + 2),
            &DevelopmentNoopRedactor,
        )
        .expect("an open obligation is a typed recovery");
    let CompleteWorkStorageResult::Recovery(snapshot) = answer else {
        panic!("an open bound obligation must return a recovery answer");
    };
    assert!(
        matches!(
            snapshot.recovery.cause,
            WorkCompletionRecoveryCause::OpenObligation {
                required_check: VerificationKind::Test,
                ..
            }
        ),
        "{:?}",
        snapshot.recovery.cause
    );
    snapshot.recovery
}

/// A host's real case: a flagged change in one workspace at one revision; the
/// host never reports the move to another workspace and revision as a
/// change, so only quiet sightings sit there, and a passed check of the new
/// revision follows. The quiet sightings do not replace the flagged change,
/// so the check stays stale, and both the record's assessment and done's
/// refusal name the flagged change it must follow beside its own source.
#[test]
fn a_check_after_an_unreported_move_names_the_flagged_change_it_must_follow() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let (work, claim) = bound_work(&mut store, "project-unreported-move");
    let change = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "old-change",
        3,
        Some(unbound("workspace-old", "R1")),
        None,
    );
    // A quiet sighting of the new source, then the check of it, whose
    // producer is a quiet sighting too.
    host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "new-build",
        VerificationKind::Build,
        VerificationResult::Passed,
        4,
        unbound("workspace-new", "R2"),
    );
    let check = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "new-test",
        VerificationKind::Test,
        VerificationResult::Passed,
        5,
        unbound("workspace-new", "R2"),
    );
    let expected = StaleVerificationSource {
        decider: StaleSourceDecider::LatestChange,
        position: position(&store, &claim, &change),
        source_changed: Some(true),
        workspace: Some("workspace-old".into()),
        revision: Some("R1".into()),
        root_generation: None,
        verification_workspace: "workspace-new".into(),
        verification_revision: "R2".into(),
    };

    let view = store
        .verification_assessment(work.work_id, &check, None, usize::MAX)
        .expect("assessment read")
        .expect("a verification of the item");
    let stale = view
        .rows
        .iter()
        .filter(|row| {
            row.assessment == ObligationAssessment::Mismatch(Mismatch::StaleSourceRevision)
        })
        .collect::<Vec<_>>();
    // The bound criterion's obligation and the change's own obligation.
    assert_eq!(stale.len(), 2, "{:?}", view.rows);
    for row in &stale {
        assert_eq!(row.stale_source.as_ref(), Some(&expected), "{row:?}");
    }
    for row in view.rows.iter().filter(|row| {
        row.assessment != ObligationAssessment::Mismatch(Mismatch::StaleSourceRevision)
    }) {
        assert_eq!(row.stale_source, None, "{row:?}");
    }

    let recovery = done_recovery(&mut store, &work, &claim, 6);
    assert_eq!(
        recovery.open_obligation_check.as_deref(),
        Some(&OpenObligationCheck::Newest {
            verification: check.clone(),
            position: position(&store, &claim, &check),
            mismatch: Some(Mismatch::StaleSourceRevision),
            left_out: None,
            stale_source: Some(expected),
        })
    );
    // The cause keeps its shape: the check is named beside it.
    let value = serde_json::to_value(&recovery).expect("recovery JSON");
    assert_eq!(value["cause"]["kind"], "open_obligation");
    assert!(value["cause"].get("stale_source").is_none());
    assert_eq!(
        value["open_obligation_check"]["stale_source"]["decider"],
        "latest_change"
    );
    // Nothing of it is stored: the obligation stays open, unresolved.
    let obligations = store
        .work_run_obligations(claim.run_id)
        .expect("obligations");
    assert!(
        obligations
            .iter()
            .any(|record| record.state == WorkObligationState::Open)
    );
}

/// The same revision in another workspace is the same source: without a
/// named root the check matches, and nothing is named.
#[test]
fn the_same_revision_in_another_workspace_still_matches_unbound() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let (work, claim) = bound_work(&mut store, "project-same-revision");
    source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "change",
        3,
        Some(unbound("workspace-old", "R1")),
        None,
    );
    let check = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "same-revision",
        VerificationKind::Test,
        VerificationResult::Passed,
        4,
        unbound("workspace-new", "R1"),
    );
    let view = store
        .verification_assessment(work.work_id, &check, None, usize::MAX)
        .expect("assessment read")
        .expect("a verification of the item");
    assert!(!view.rows.is_empty());
    for row in &view.rows {
        assert_eq!(row.assessment, ObligationAssessment::Matches, "{row:?}");
        assert_eq!(row.stale_source, None);
    }
}

/// With no passed check of the kind after the obligation opened, done's
/// refusal says so, and assesses nothing.
#[test]
fn done_says_when_no_passed_check_followed_the_obligation() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let (work, claim) = bound_work(&mut store, "project-none-followed");
    source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "change",
        3,
        Some(unbound("workspace-old", "R1")),
        None,
    );
    // A failed check of the kind is not a passed one.
    host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "failed-test",
        VerificationKind::Test,
        VerificationResult::Failed,
        4,
        unbound("workspace-old", "R1"),
    );
    let recovery = done_recovery(&mut store, &work, &claim, 5);
    assert_eq!(
        recovery.open_obligation_check.as_deref(),
        Some(&OpenObligationCheck::NoneFollowed)
    );
    let value = serde_json::to_value(&recovery).expect("recovery JSON");
    assert_eq!(value["open_obligation_check"]["state"], "none_followed");
}

/// Host-recorded text in the deciding record never breaks the refusal's one
/// line or reads as a locked store, and a long field is cut on its own, so
/// the check's revision always survives beside it.
#[test]
fn stored_text_in_the_deciding_record_stays_one_bounded_line() {
    let hostile = format!("ws\u{1b}[31m\nDatabase  is\tLOCKED {}", "x".repeat(400));
    for decider in [
        StaleSourceDecider::LatestChange,
        StaleSourceDecider::RootSighting,
        StaleSourceDecider::RootBinding,
    ] {
        let source = StaleVerificationSource {
            decider,
            position: 7,
            source_changed: Some(true),
            workspace: Some(hostile.clone()),
            revision: Some("R1\r\n".into()),
            root_generation: Some(3),
            verification_workspace: hostile.clone(),
            verification_revision: "R2".into(),
        };
        let sentence = source.sentence();
        assert!(
            !sentence.contains(['\n', '\r', '\u{1b}', '\t']),
            "{sentence:?}"
        );
        let normalized = sentence
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        assert!(!normalized.contains("database is locked"), "{sentence}");
        assert!(sentence.contains("bytes stored)"), "{sentence}");
        assert!(
            sentence.contains("the check ran on revision R2"),
            "{sentence}"
        );
        assert!(sentence.len() < 1_500, "{}", sentence.len());
    }
}
