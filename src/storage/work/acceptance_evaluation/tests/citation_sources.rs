//! A pass on a criterion bound to a typed check rests only on checks of the
//! source the evaluation judged: record admission refuses a citation of a
//! check that ran on another source, and completion treats a record holding
//! one as stale, whichever way the binding's obligation was resolved.

use super::*;
use crate::domain::{
    AcceptanceBinding, VerificationRequirement, WaiveWorkObligationRequest, WorkObligationState,
};

/// The fixture's item with its first criterion bound to a test, under an
/// evaluated policy that requires observed mechanical passes, with a host
/// session on its live claim reporting the source at `revision-d`.
fn bound_fixture(project: &str, fresh: bool) -> (Fixture, WorkItem, WorkClaim, HostSession) {
    bound_fixture_of(project, fresh, VerificationKind::Test)
}

/// `bound_fixture` with the first criterion bound to a check of `kind`.
fn bound_fixture_of(
    project: &str,
    fresh: bool,
    kind: VerificationKind,
) -> (Fixture, WorkItem, WorkClaim, HostSession) {
    let mut fixture = fixture(project);
    let work = revise(
        &mut fixture.store,
        &fixture.work,
        &fixture.claim,
        WorkRevisionPatch {
            // Stored criteria are sorted, so the bound one stays first.
            acceptance: Some(vec!["run the tests".into(), "write the docs".into()]),
            acceptance_bindings: Some(vec![AcceptanceBinding {
                criterion: 1,
                requirement: VerificationRequirement {
                    check_kind: kind,
                    check_fingerprint: None,
                },
            }]),
            ..empty_patch()
        },
        "bind-the-check",
        5,
    )
    .expect("bind the first criterion to a check");
    enable(
        &mut fixture.store,
        &[Mode::SameSession],
        MechanicalBasis::Observed,
        fresh,
        "enable-observed",
        6,
    );
    let claim = fixture
        .store
        .current_work_claim(work.work_id)
        .expect("claim read")
        .expect("live claim");
    let mut host = HostSession::bind(&mut fixture.store, &work, &claim, 10);
    host.basis.source_revision = "revision-d".into();
    (fixture, work, claim, host)
}

/// A passing evaluation at the current cut: the bound criterion on the
/// `checks` it cites, the free-text one on `note`, declaring `declared` as
/// the source it judged.
fn evaluation(
    store: &SqliteStore,
    work: &WorkItem,
    checks: &[ObjectId],
    note: &ObjectId,
    declared: Option<AcceptanceSourceBasis>,
    second: i64,
) -> RecordAcceptanceEvaluationRequest {
    let mut request = request(
        work,
        cut(store, work),
        "runner",
        Mode::SameSession,
        vec![
            verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Observed,
                checks,
            ),
            verdict(
                2,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Judgment,
                std::slice::from_ref(note),
            ),
        ],
        second,
    );
    request.source_basis = declared;
    request
}

fn declared(fingerprint: &str, workspace: Option<&str>) -> AcceptanceSourceBasis {
    AcceptanceSourceBasis {
        workspace_id: workspace.map(Into::into),
        fingerprint: fingerprint.into(),
    }
}

/// The one check a host turn recorded.
fn one_check(checks: Vec<ObjectId>) -> ObjectId {
    let [check] = <[ObjectId; 1]>::try_from(checks).expect("the turn recorded one check");
    check
}

/// Records `request` as an earlier build admitted it: the record path
/// without the rule that a bound pass's checks ran on the judged source,
/// with the receipt an exact resend replays.
fn admitted_by_an_earlier_build(
    store: &mut SqliteStore,
    request: &RecordAcceptanceEvaluationRequest,
) -> AcceptanceEvaluationReceipt {
    let transaction = store
        .connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .expect("evaluation transaction");
    let item = load_work_item(&transaction, request.work_id).expect("evaluated item");
    let run_id = item.active_run_id.expect("active run");
    let attempt = attempt_identity(request, run_id).expect("attempt identity");
    let policy = SqliteStore::load_acceptance_evaluation_policy_on(&transaction).expect("policy");
    let verdicts = bind_verdicts(
        &transaction,
        &item,
        run_id,
        &policy,
        request.evaluated_through,
        &request.verdicts,
    )
    .expect("the verdicts an earlier build admitted");
    let receipt = append_evaluation(&transaction, &item, run_id, request, &attempt, verdicts)
        .expect("append the evaluation");
    transaction.commit().expect("commit the evaluation");
    receipt
}

/// A completion attempt after a fresh checkpoint naming `evidence`.
fn done(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    evidence: &[ObjectId],
    fingerprint: Option<&str>,
    key: &str,
    second: i64,
) -> Result<CompletionSeal, StoreError> {
    checkpoint_then_complete(
        store,
        work,
        claim,
        "runner",
        evidence,
        false,
        fingerprint,
        key,
        second,
    )
}

fn stale_verification_source(result: Result<CompletionSeal, StoreError>) {
    let cause = recovery_cause(result);
    assert!(
        matches!(
            cause,
            WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
                reason: AcceptanceStaleReason::VerificationSource
            }
        ),
        "{cause:?}"
    );
}

// B50, case D with source freshness off: a check at R_d, an edit to R_e with no
// check after it, and an evaluation after the edit. A pass citing the R_d
// check is refused by name and nothing is appended; the check rerun at R_e
// carries the criterion and seals. B51: the refusal of this undeclared
// evaluation names no declaration remedy.
#[test]
fn a_pass_citing_a_check_of_an_older_revision_is_refused_and_a_rerun_seals() {
    let (mut fixture, work, claim, mut host) = bound_fixture("project-citation-case-d", false);
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let at_d = one_check(host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));
    host.basis.source_revision = "revision-e".into();
    assert!(host.checkpoint(store, true, None, 30).is_empty());

    let before = cut(store, &work);
    let before_database = test_database_shape_snapshot(&store.connection).expect("snapshot");
    let (refused, cause) = admission::typed_refusal(record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&at_d), &note, None, 35),
    ));
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("snapshot"),
        before_database
    );
    let AcceptanceEvaluationAdmissionCause::Citation(cause) = cause else {
        panic!("citation family");
    };
    assert_eq!(cause.mismatch, EvaluationCitationMismatch::WrongSource);
    assert_eq!(cause.criterion, 1);
    assert_eq!(cause.citation, at_d.as_str());
    assert_eq!(cause.run_id, claim.run_id);
    assert_eq!(cause.evaluated_cut, before);
    assert_eq!(cause.checked_revision.as_deref(), Some("revision-d"));
    assert_eq!(cause.judged_revision.as_deref(), Some("revision-e"));
    assert!(cause.producer_observation.is_some());
    assert_eq!(
        cause.requirement.as_ref().unwrap().check_kind,
        VerificationKind::Test
    );
    assert_eq!(
        cause.remedy,
        crate::domain::EvaluationAdmissionRemedy::RunCurrentCheckAndEvaluate
    );
    assert!(
        refused.contains(&format!(
            "criterion 1 is bound to test verification, and {at_d} ran on source revision revision-d, not the revision revision-e this evaluation judged; run the check on the current source, then evaluate again citing it"
        )),
        "{refused}"
    );
    // Nothing was declared, so the remedy names no declaration.
    assert!(!refused.contains("declare"), "{refused}");
    assert_eq!(cut(store, &work), before, "a refusal appends nothing");
    assert_eq!(
        store
            .acceptance_evaluation_status(work.work_id, None)
            .expect("status read"),
        None
    );

    let at_e = one_check(host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        40,
    ));
    let admitted = record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&at_e), &note, None, 45),
    )
    .expect("a pass on the rerun check records");
    let seal = done(
        store,
        &work,
        &claim,
        &[note, at_e.clone()],
        None,
        "complete-after-rerun",
        50,
    )
    .expect("the rerun check seals");
    assert_eq!(seal.acceptance_evaluation, Some(admitted.evaluation));
    assert_eq!(seal.acceptance[0].evidence, vec![at_e]);
}

// B50: source freshness (F4) ties the evaluated content to the content at
// completion; it says nothing about the check a pass cites. A host that
// measures R_e at evaluation and again at completion still gets a pass on
// an R_d check refused, and a record an earlier build admitted is stale.
#[test]
fn the_completion_fingerprint_does_not_stand_in_for_the_citation_rule() {
    let (mut fixture, work, claim, mut host) = bound_fixture("project-citation-freshness", true);
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let at_d = one_check(host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));
    host.basis.source_revision = "revision-e".into();
    host.checkpoint(store, true, None, 30);

    let stale = evaluation(
        store,
        &work,
        std::slice::from_ref(&at_d),
        &note,
        Some(declared("revision-e", None)),
        35,
    );
    let refused = refusal(record(store, &stale));
    assert!(
        refused.contains("ran on source revision revision-d, not the revision revision-e"),
        "{refused}"
    );
    let earlier = admitted_by_an_earlier_build(store, &stale);
    assert_eq!(
        store
            .acceptance_evaluation_status(work.work_id, Some("revision-e"))
            .expect("status read")
            .map(|status| (status.evaluation, status.stale)),
        Some((
            earlier.evaluation,
            Some(AcceptanceStaleReason::VerificationSource)
        ))
    );
    stale_verification_source(done(
        store,
        &work,
        &claim,
        &[note.clone(), at_d],
        Some("revision-e"),
        "complete-on-the-old-check",
        40,
    ));

    let at_e = one_check(host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        45,
    ));
    record(
        store,
        &evaluation(
            store,
            &work,
            std::slice::from_ref(&at_e),
            &note,
            Some(declared("revision-e", None)),
            50,
        ),
    )
    .expect("a pass on the rerun check records");
    done(
        store,
        &work,
        &claim,
        &[note, at_e],
        Some("revision-e"),
        "complete-after-rerun",
        55,
    )
    .expect("the rerun check seals with the measured fingerprint");
}

// B50: a record an earlier build admitted is judged again when completion
// consumes it. On the satisfied path the binding's obligation was met by the
// R_d check; the evaluation is refused as stale before any obligation rule
// runs. An exact resend still recovers that record, since replay comes
// before admission, and any other resend is admitted fresh and refused.
#[test]
fn an_earlier_build_record_is_stale_at_done_and_replays_before_the_refusal() {
    let (mut fixture, work, claim, mut host) = bound_fixture("project-citation-satisfied", false);
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let at_d = one_check(host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));
    let run_id = claim.run_id;
    assert!(
        store
            .work_run_obligations(run_id)
            .expect("obligations")
            .iter()
            .all(|record| record.state == WorkObligationState::Satisfied),
        "the R_d check satisfies the binding"
    );
    host.basis.source_revision = "revision-e".into();
    host.checkpoint(store, true, None, 30);

    let stale = evaluation(store, &work, std::slice::from_ref(&at_d), &note, None, 35);
    let earlier = admitted_by_an_earlier_build(store, &stale);
    let replayed = record(store, &stale).expect("an exact resend replays");
    assert!(replayed.replayed);
    assert_eq!(replayed.evaluation, earlier.evaluation);

    let mut changed = stale.clone();
    changed.verdicts[1].rationale = "the docs were read again".into();
    let refused = refusal(record(store, &changed));
    assert!(refused.contains(at_d.as_str()), "{refused}");

    stale_verification_source(done(
        store,
        &work,
        &claim,
        &[note, at_d],
        None,
        "complete-on-the-old-check",
        40,
    ));
}

// B50: on the waived path the binding's obligation was waived before any check,
// so no obligation rule ever matches the cited check to the run's latest
// change: only the evaluation's own citation stands between an R_d check
// and a seal over the R_e edit, whether the evaluation declared nothing or
// declared the older revision the check ran on.
#[test]
fn a_stale_citation_is_refused_at_done_when_the_binding_obligation_was_waived() {
    let (mut fixture, work, claim, mut host) = bound_fixture("project-citation-waived", false);
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let run_id = claim.run_id;
    let binding = store
        .work_run_obligations(run_id)
        .expect("obligations")
        .into_iter()
        .find(|record| record.state == WorkObligationState::Open)
        .expect("the binding's open obligation");
    store
        .waive_work_obligation(
            &WaiveWorkObligationRequest {
                obligation_id: binding.obligation.obligation_id,
                expected_definition: binding.definition_id.clone(),
                waived_by: "operator".into(),
                reason: "the tests are checked by hand".into(),
                actor: actor("operator"),
                idempotency_key: "waive-the-binding".into(),
                waived_at: at(15),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("waive the binding's obligation");
    let at_d = one_check(host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));
    host.basis.source_revision = "revision-e".into();
    host.checkpoint(store, true, None, 30);

    admitted_by_an_earlier_build(
        store,
        &evaluation(store, &work, std::slice::from_ref(&at_d), &note, None, 35),
    );
    stale_verification_source(done(
        store,
        &work,
        &claim,
        &[note.clone(), at_d.clone()],
        None,
        "complete-on-the-old-check",
        40,
    ));
    // Declaring the revision the old check ran on, behind the R_e sighting
    // before the cut, does not bring it back.
    admitted_by_an_earlier_build(
        store,
        &evaluation(
            store,
            &work,
            std::slice::from_ref(&at_d),
            &note,
            Some(declared("revision-d", None)),
            41,
        ),
    );
    stale_verification_source(done(
        store,
        &work,
        &claim,
        &[note.clone(), at_d],
        None,
        "complete-on-a-declared-old-check",
        42,
    ));

    let at_e = one_check(host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        45,
    ));
    record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&at_e), &note, None, 50),
    )
    .expect("a pass on the rerun check records");
    let seal = done(
        store,
        &work,
        &claim,
        &[note, at_e.clone()],
        None,
        "complete-after-rerun",
        55,
    )
    .expect("the rerun check seals");
    assert_eq!(seal.acceptance[0].evidence, vec![at_e]);
}

// B51: the judged source is the declared one whenever a declaration exists,
// workspace included when it names one, and the newest sighting is only the
// fallback: an older sighting never stands in for a declared tree. Nor does
// a declaration hide a sighting: an evaluation that declares the revision
// the check ran on, behind a later sighting at another one, is refused.
#[test]
fn a_declared_source_is_matched_and_never_hides_a_later_sighting() {
    let (mut fixture, work, _claim, mut host) = bound_fixture("project-citation-declared", false);
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let at_d = one_check(host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));

    // The run was last seen at R_d, but the evaluator declares it judged
    // R_e: the older sighting does not stand in for the declared tree.
    let ahead = refusal(record(
        store,
        &evaluation(
            store,
            &work,
            std::slice::from_ref(&at_d),
            &note,
            Some(declared("revision-e", None)),
            25,
        ),
    ));
    assert!(
        ahead.contains("ran on source revision revision-d, not the revision revision-e"),
        "{ahead}"
    );
    // A declaration may be in a form the host never reports, so the remedy
    // names correcting it too.
    assert!(
        ahead.ends_with(
            ", or, when the declared fingerprint is not the source revision the host reports, declare that revision"
        ),
        "{ahead}"
    );
    // A declared workspace must be the one the check ran in.
    let elsewhere = refusal(record(
        store,
        &evaluation(
            store,
            &work,
            std::slice::from_ref(&at_d),
            &note,
            Some(declared("revision-d", Some("another-workspace"))),
            26,
        ),
    ));
    assert!(
        elsewhere.contains(&format!(
            "{at_d} ran in workspace workspace-evaluated, not the workspace another-workspace this evaluation judged"
        )),
        "{elsewhere}"
    );

    // Declared exactly as the check ran, it records.
    record(
        store,
        &evaluation(
            store,
            &work,
            std::slice::from_ref(&at_d),
            &note,
            Some(declared("revision-d", Some("workspace-evaluated"))),
            27,
        ),
    )
    .expect("a pass on a check of the declared source records");

    // The run is then seen at R_e. Declaring R_d, the revision the check ran
    // on, matches the check but not the run after it.
    host.basis.source_revision = "revision-e".into();
    host.checkpoint(store, true, None, 30);
    let behind = refusal(record(
        store,
        &evaluation(
            store,
            &work,
            std::slice::from_ref(&at_d),
            &note,
            Some(declared("revision-d", Some("workspace-evaluated"))),
            35,
        ),
    ));
    assert!(
        behind.contains(&format!(
            "{at_d} ran on source revision revision-d, but the run was last seen at revision revision-e after it"
        )),
        "{behind}"
    );
}

// B51: a change the host reported without a revision after the check may
// have moved the source anywhere, so the check no longer carries a bound
// pass though every revision the run shows is the one it ran on.
#[test]
fn a_change_without_a_revision_after_the_check_retires_it() {
    let (mut fixture, work, _claim, mut host) = bound_fixture("project-citation-unrevised", false);
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let at_d = one_check(host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));
    host.report(store, &[(true, None)], 30);
    let refused = refusal(record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&at_d), &note, None, 35),
    ));
    assert!(
        refused.contains(&format!(
            "{at_d} ran on source revision revision-d, but the run reported a source change without a revision after it"
        )),
        "{refused}"
    );
}

// B51: with no declaration, a quiet sighting at R_e before the cut moves the
// judged source even though no change was reported. Seen back at R_d, the
// newest sighting decides, as in F3, and the check stands again. A check of
// R_d stays stale beside a fresh check of R_e, cited or not; the fresh one
// alone carries the criterion.
#[test]
fn a_quiet_move_or_a_fresh_check_does_not_rescue_a_stale_citation() {
    let (mut fixture, work, _claim, mut host) = bound_fixture("project-citation-quiet", false);
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let at_d = one_check(host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));
    host.report(store, &[(false, Some("revision-e"))], 30);
    let quiet = refusal(record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&at_d), &note, None, 35),
    ));
    assert!(
        quiet.contains("ran on source revision revision-d, not the revision revision-e"),
        "{quiet}"
    );
    // Seen back at R_d, the source holds the content the check ran on.
    host.report(store, &[(false, Some("revision-d"))], 36);
    record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&at_d), &note, None, 39),
    )
    .expect("a pass on the check records once the source is back where it ran");

    host.basis.source_revision = "revision-e".into();
    let at_e = one_check(host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        40,
    ));
    for (checks, second) in [
        (vec![at_d.clone()], 45),
        (vec![at_e.clone(), at_d.clone()], 46),
    ] {
        let refused = refusal(record(
            store,
            &evaluation(store, &work, &checks, &note, None, second),
        ));
        assert!(
            refused.contains(&format!("{at_d} ran on source revision revision-d")),
            "{refused}"
        );
    }
    record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&at_e), &note, None, 47),
    )
    .expect("a pass on the fresh check alone records");
}

/// The newest verification recorded on the item's run.
fn newest_verification(store: &SqliteStore, work: &WorkItem) -> ObjectId {
    let stored: String = store
        .connection
        .query_row(
            "SELECT object_id FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1
               AND object_kind = 'verification_evidence'
             ORDER BY position DESC LIMIT 1",
            [work.active_run_id.expect("active run").0.to_string()],
            |row| row.get(0),
        )
        .expect("a verification on the run");
    ObjectId::from_stored(stored).expect("stored id")
}

// B51: later sightings of the check's own revision leave it standing: the
// turn's own closing sighting and one from another workspace. The pass
// records and seals.
#[test]
fn a_check_stands_through_later_sightings_of_its_own_revision() {
    let (mut fixture, work, claim, mut host) =
        bound_fixture("project-citation-same-revision", false);
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let at_d = one_check(host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));
    host.report(store, &[(false, Some("revision-d"))], 25);
    host.basis.workspace_id = "another-workspace".into();
    host.report(store, &[(false, Some("revision-d"))], 30);
    record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&at_d), &note, None, 40),
    )
    .expect("a pass on the check records");
    let seal = done(
        store,
        &work,
        &claim,
        &[note, at_d.clone()],
        None,
        "complete-on-the-check",
        45,
    )
    .expect("the check seals");
    assert_eq!(seal.acceptance[0].evidence, vec![at_d]);
}

/// Each execution observation on the item's run after `position`, oldest
/// first: whether it records a source change, and its revision.
fn observations_after(
    store: &SqliteStore,
    work: &WorkItem,
    position: i64,
) -> Vec<(bool, Option<String>)> {
    let mut statement = store
        .connection
        .prepare(
            "SELECT json_extract(object.canonical_json, '$.source_changed'),
                    json_extract(object.canonical_json, '$.source_basis.source_revision')
             FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
             WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
               AND entry.position > ?2 AND entry.object_kind = 'execution_observation'
             ORDER BY entry.position",
        )
        .expect("observation query");
    statement
        .query_map(
            rusqlite::params![
                work.active_run_id.expect("active run").0.to_string(),
                position
            ],
            |row| Ok((row.get::<_, bool>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .expect("observation rows")
        .map(|row| row.expect("observation row"))
        .collect()
}

// B51: reported changes that move the source to R_e and back to R_d after the
// check, both kept as changes, leave the source holding the content the
// check ran on, so the evaluation records and stays fresh. Completion keeps
// its own rule for a satisfied binding: a check must follow the run's latest
// source change, so `done` asks for one.
#[test]
fn a_reported_move_and_revert_leave_the_check_standing() {
    let (mut fixture, work, claim, mut host) =
        bound_fixture("project-citation-reported-revert", false);
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let at_d = one_check(host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));
    let after_check = cut(store, &work);
    host.report(store, &[(true, Some("revision-e"))], 25);
    host.report(store, &[(true, Some("revision-d"))], 30);
    assert_eq!(
        observations_after(store, &work, after_check),
        vec![
            (true, Some("revision-e".to_owned())),
            (true, Some("revision-d".to_owned())),
        ],
        "both reported changes are kept as changes"
    );
    let admitted = record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&at_d), &note, None, 35),
    )
    .expect("a pass on the check records once the source is back where it ran");
    assert_eq!(
        store
            .acceptance_evaluation_status(work.work_id, None)
            .expect("status read")
            .map(|status| (status.evaluation, status.stale)),
        Some((admitted.evaluation, None))
    );
    let Err(StoreError::WorkBoundVerificationRefused { reason, cause, .. }) = done(
        store,
        &work,
        &claim,
        &[note, at_d.clone()],
        None,
        "complete-after-revert",
        40,
    ) else {
        panic!("the satisfied binding needs a check after the latest change");
    };
    assert!(
        reason.contains("does not verify the run's latest source change"),
        "{reason}"
    );
    assert_eq!(cause.verification, at_d);
    // The revision returned to the checked source, but both reported changes
    // remain ordered after this check.
    assert_eq!(
        cause.mismatch,
        crate::domain::VerificationEvidenceMismatch::NotAfterMutation
    );
    assert_eq!(
        cause.remedy,
        crate::domain::BoundVerificationRemedy::RunCurrentCheck
    );
}

// B51: a check the host records in a later turn, citing the observation that
// ran it, is judged from that observation, and sightings of its revision in
// between leave it standing.
#[test]
fn a_late_record_of_a_check_stands_through_sightings_of_its_revision() {
    let (mut fixture, work, _claim, mut host) = bound_fixture_of(
        "project-citation-late-record",
        false,
        VerificationKind::Build,
    );
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let first = one_check(host.checkpoint(
        store,
        true,
        Some((VerificationKind::Build, ExecutionOutcome::Succeeded)),
        20,
    ));
    let producer = store
        .load_verification_evidence(&first)
        .expect("load the first build")
        .producer_observation;
    host.report(store, &[(false, Some("revision-d"))], 25);
    host.cite_earlier_producer(store, &producer, Some("revision-d"), 30);
    let late = newest_verification(store, &work);
    assert_ne!(late, first, "the later turn recorded its own verification");
    record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&late), &note, None, 35),
    )
    .expect("a pass on the late record of the build records");
}
