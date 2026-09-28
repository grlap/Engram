//! Completion under an evaluated policy of an item that has no acceptance
//! criteria is refused by name, before any recovery cause is built. A
//! self-asserted policy still completes such an item, and a recovery cause
//! that names a criterion the item does not have stays a projection error.

use super::*;

/// A claimed root with no acceptance criteria and one checkpointed evidence
/// object, like an item imported without criteria.
fn criteria_less(project: &str) -> Fixture {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let mut store = SqliteStore::open(&database).expect("store");
    let mut request = root_request(project, "create-criteria-less-work", 1);
    request.acceptance = Vec::new();
    let work = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("create work without acceptance criteria");
    assert!(work.acceptance.is_empty());
    let claim = claim(
        &mut store,
        &work,
        "runner",
        "claim-criteria-less-work",
        2,
        3_600,
    );
    let evidence = evidence(
        &mut store,
        &work,
        &claim,
        "runner",
        "evidence-criteria-less",
        3,
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-criteria-less",
        4,
        std::slice::from_ref(&evidence),
    );
    Fixture {
        store,
        directory,
        work,
        claim,
        evidence,
    }
}

// B52: refused by name before any cause is built, capturing nothing.
#[test]
fn an_item_without_criteria_is_refused_by_name_under_an_evaluated_policy() {
    let mut fixture = criteria_less("project-criteria-required");
    enable(
        &mut fixture.store,
        &[Mode::IndependentSession],
        MechanicalBasis::Asserted,
        false,
        "enable-criteria-required",
        5,
    );
    let work_id = fixture.work.work_id;

    // The read the ambient word runs before capturing anything.
    let readiness =
        fixture
            .store
            .acceptance_evaluation_readiness(work_id, fixture.claim.run_id, None);
    assert!(
        matches!(&readiness, Err(StoreError::AcceptanceCriteriaRequired { work }) if *work == work_id),
        "{readiness:?}"
    );

    // The transactional completion itself, with its final checkpoint in place.
    let head = cut(&fixture.store, &fixture.work);
    let mut request = completion_request(
        &fixture.work,
        &fixture.claim,
        "runner",
        &fixture.evidence,
        "complete-criteria-less",
        6,
    );
    request.acceptance = Vec::new();
    let error = fixture
        .store
        .complete_work(&request, &DevelopmentNoopRedactor)
        .expect_err("an evaluated policy has nothing to evaluate");
    assert!(
        matches!(&error, StoreError::AcceptanceCriteriaRequired { work } if *work == work_id),
        "{error:?}"
    );
    assert_eq!(
        cut(&fixture.store, &fixture.work),
        head,
        "a refused completion appends nothing"
    );
    assert_eq!(
        fixture
            .store
            .get_work_item(work_id)
            .expect("item")
            .lifecycle,
        WorkLifecycle::Open
    );

    // The refusal names the missing criteria and the way out, never a
    // projection error or a recovery criterion.
    let message = error.to_string();
    assert!(message.contains("no acceptance criteria"), "{message}");
    assert!(message.contains("at least one criterion"), "{message}");
    assert!(message.contains("host also refuses"), "{message}");
    let payload = crate::mcp::store_error_value(&error);
    assert_eq!(payload["error"]["code"], "acceptance_criteria_required");
    let remedy = payload["error"]["details"]["remedy"]
        .as_str()
        .expect("remedy");
    let update = remedy.find("--accept").expect("the update word");
    let evaluate = remedy.find("evaluate").expect("the host evaluation");
    let done = remedy.find("work done").expect("the done word");
    assert!(update < evaluate && evaluate < done, "{remedy}");
    assert!(fixture.store.verify_all().expect("scan").is_healthy());
}

// B52: the self-asserted policy seals such an item as before.
#[test]
fn an_item_without_criteria_still_completes_under_a_self_asserted_policy() {
    let mut fixture = criteria_less("project-criteria-optional");
    let seal = complete(
        &mut fixture.store,
        &fixture.work,
        &fixture.claim,
        "runner",
        &fixture.evidence,
        "complete-self-asserted-criteria-less",
        5,
    )
    .expect("self-asserted completion needs no criteria");
    assert_eq!(seal.acceptance_evaluation, None);
    assert!(fixture.store.verify_all().expect("scan").is_healthy());
}

#[test]
fn a_recovery_cause_naming_an_absent_criterion_stays_a_projection_error() {
    let fixture = fixture("project-criteria-absent-cause");
    assert!(!fixture.work.acceptance.is_empty());
    let error = fixture
        .store
        .work_completion_recovery(
            &fixture.work,
            &fixture.claim,
            at(5),
            &WorkCompletionRecoveryCause::MissingAcceptanceEvaluation {
                criterion: "a criterion this item does not have".into(),
            },
        )
        .expect_err("an inconsistent cause is refused");
    assert!(
        matches!(&error, StoreError::InvalidWorkProjection(reason) if reason.contains("absent from the bound work revision")),
        "{error:?}"
    );
}
