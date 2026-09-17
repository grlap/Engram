//! Regression cases for the two parent-inspection findings: independence
//! against every former holder, and admission bounds that keep an admitted
//! record from committing into an unusable response.

use super::*;
use crate::domain::{
    AcceptWorkHandoffRequest, AcceptanceSourceBasis, EvaluatorModel, OfferWorkHandoffRequest,
};

fn pass_judgment(note: &ObjectHash) -> Vec<CriterionVerdictInput> {
    vec![verdict(
        1,
        AcceptanceVerdict::Pass,
        AcceptanceBasis::Judgment,
        std::slice::from_ref(note),
    )]
}

fn passes(count: usize, note: &ObjectHash) -> Vec<CriterionVerdictInput> {
    (1..=count)
        .map(|position| {
            verdict(
                position,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Judgment,
                std::slice::from_ref(note),
            )
        })
        .collect()
}

// R4: an independent evaluator must be a session that never held or executed
// the run. The mutable claim row forgets former holders after a handoff or a
// recovery; the run's immutable history does not.
#[test]
fn former_holders_are_never_independent_evaluators() {
    let mut fixture = fixture("project-evaluation-former-holders");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession, Mode::IndependentSession],
        MechanicalBasis::Asserted,
        false,
        "enable-former-holders",
        5,
    );
    let work = fixture.work.clone();
    let first = fixture.claim.clone();
    let note = fixture.evidence.clone();

    // Same-run handoff: runner -> second.
    let offer = store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: work.work_id,
                run_id: first.run_id,
                expected_work_revision: work.revision,
                from: first.holder.clone(),
                to: SessionId("second".into()),
                claim_id: first.claim_id,
                claim_fence: first.fence,
                ttl_seconds: 300,
                checkpoint_summary: "handing the run to second".into(),
                actor: actor("runner"),
                idempotency_key: "offer-to-second".into(),
                offered_at: at(6),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("offer handoff");
    let second = store
        .accept_work_handoff(
            &AcceptWorkHandoffRequest {
                work_id: work.work_id,
                offer_id: offer.offer_id,
                to: SessionId("second".into()),
                actor: actor("second"),
                idempotency_key: "accept-as-second".into(),
                accepted_at: at(7),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("accept handoff");
    assert_eq!(second.holder, SessionId("second".into()));
    let work = store
        .get_work_item(work.work_id)
        .expect("item after handoff");

    let former_after_handoff = refusal(record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::IndependentSession,
            pass_judgment(&note),
            8,
        ),
    ));
    assert!(
        former_after_handoff.contains("neither holds nor executes"),
        "{former_after_handoff}"
    );
    let current_after_handoff = refusal(record(
        store,
        &request(
            &work,
            cut(store, &work),
            "second",
            Mode::IndependentSession,
            pass_judgment(&note),
            8,
        ),
    ));
    assert!(
        current_after_handoff.contains("neither holds nor executes"),
        "{current_after_handoff}"
    );

    // Recovery: second's claim lapses and third recovers the same run.
    let third = claim(store, &work, "third", "recover-as-third", 400, 3_600);
    assert_eq!(third.holder, SessionId("third".into()));
    let work = store
        .get_work_item(work.work_id)
        .expect("item after recovery");
    let former_after_recovery = refusal(record(
        store,
        &request(
            &work,
            cut(store, &work),
            "second",
            Mode::IndependentSession,
            pass_judgment(&note),
            401,
        ),
    ));
    assert!(
        former_after_recovery.contains("neither holds nor executes"),
        "{former_after_recovery}"
    );
    let original_after_recovery = refusal(record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::IndependentSession,
            pass_judgment(&note),
            401,
        ),
    ));
    assert!(
        original_after_recovery.contains("neither holds nor executes"),
        "{original_after_recovery}"
    );

    // Positive control: a session that never held or executed the run.
    let independent = record(
        store,
        &request(
            &work,
            cut(store, &work),
            "reviewer",
            Mode::IndependentSession,
            pass_judgment(&note),
            402,
        ),
    )
    .expect("a never-holding session evaluates independently");
    assert_eq!(independent.record.mode, Mode::IndependentSession);
    // The current holder still evaluates in its own name.
    record(
        store,
        &request(
            &work,
            cut(store, &work),
            "third",
            Mode::SameSession,
            pass_judgment(&note),
            403,
        ),
    )
    .expect("the current holder evaluates as same_session");
}

// R9: the evaluation object has an explicit canonical cap and bounded
// metadata; a refusal leaves the run feed and the newest record untouched,
// while the largest admissible shapes still record.
#[test]
fn oversized_evaluations_are_refused_with_nothing_appended() {
    let mut fixture = fixture("project-evaluation-cap");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-cap",
        5,
    );
    let work = revise(
        store,
        &fixture.work,
        &fixture.claim,
        WorkRevisionPatch {
            acceptance: Some(vec!["root accepted".into(), "second accepted".into()]),
            ..empty_patch()
        },
        "two-criteria",
        5,
    )
    .expect("revise to two criteria");
    let note = fixture.evidence.clone();
    let head_before = cut(store, &work);

    let long_fingerprint = refusal(record(
        store,
        &RecordAcceptanceEvaluationRequest {
            source_basis: Some(AcceptanceSourceBasis {
                workspace_id: None,
                fingerprint: "f".repeat(257),
            }),
            ..request(
                &work,
                head_before,
                "runner",
                Mode::SameSession,
                passes(2, &note),
                7,
            )
        },
    ));
    assert!(
        long_fingerprint.contains("fingerprint"),
        "{long_fingerprint}"
    );
    assert_eq!(cut(store, &work), head_before);

    // One rationale near the note bound, maximal metadata, escaping-heavy
    // text: admissible, and recorded in full.
    let mut maximal = passes(2, &note);
    maximal[0].rationale = format!("\"quoted\" \\backslash\\ ünïcödé {}", "r".repeat(60 * 1024));
    let admitted = record(
        store,
        &RecordAcceptanceEvaluationRequest {
            source_basis: Some(AcceptanceSourceBasis {
                workspace_id: Some("w".repeat(256)),
                fingerprint: "f".repeat(256),
            }),
            evaluator_model: Some(EvaluatorModel {
                provider: "p".repeat(128),
                model: "m".repeat(128),
                version: Some("v".repeat(128)),
            }),
            attempt_key: Some("a".repeat(256)),
            ..request(&work, head_before, "runner", Mode::SameSession, maximal, 8)
        },
    )
    .expect("the largest admissible evaluation records");
    assert!(admitted.record.verdicts[0].rationale.len() > 60 * 1024);
    assert_eq!(cut(store, &work), head_before + 1);

    // Seventeen admissible rationales together exceed the object cap.
    let many = revise(
        store,
        &work,
        &fixture.claim,
        WorkRevisionPatch {
            acceptance: Some(
                (0..17)
                    .map(|index| format!("criterion {index:02}"))
                    .collect(),
            ),
            ..empty_patch()
        },
        "seventeen-criteria",
        9,
    )
    .expect("revise to seventeen criteria");
    let head_before = cut(store, &many);
    let mut oversized = passes(17, &note);
    for verdict in &mut oversized {
        verdict.rationale = "x".repeat(63 * 1024);
    }
    let refused = refusal(record(
        store,
        &request(
            &many,
            head_before,
            "runner",
            Mode::SameSession,
            oversized,
            10,
        ),
    ));
    assert!(refused.contains("bytes"), "{refused}");
    assert_eq!(cut(store, &many), head_before);
    assert_eq!(
        store
            .acceptance_evaluation_status(many.work_id, None)
            .expect("status read")
            .map(|status| status.evaluation),
        Some(admitted.evaluation)
    );
}
