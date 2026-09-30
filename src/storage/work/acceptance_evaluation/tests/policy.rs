//! Policy/pin agreement at both phases, apart from evaluator affiliation.

use super::*;

#[test]
fn mode_and_pin_matrix_preserves_admission_and_freshness() {
    let mut fixture = fixture("project-mode-pin-matrix");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-matrix",
        5,
    );
    // An outside planner supplies the mark; the runner can evaluate without
    // changing the independent-default or mark-author rules.
    let mut create = root_request("project-mode-pin-matrix", "create-marked", 6);
    create.evaluation_mode = Some(Mode::SameSession);
    let marked = store
        .create_work(&create, &DevelopmentNoopRedactor)
        .expect("outside-authored mark");
    let held = claim(store, &marked, "runner", "claim-marked", 7, 3_600);
    let note = evidence(store, &marked, &held, "runner", "marked-evidence", 8);
    let recorded = record(
        store,
        &request(
            &marked,
            cut(store, &marked),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Judgment,
                &[note],
            )],
            9,
        ),
    )
    .expect("valid marked evaluation");

    let modes = [Mode::SameSession, Mode::SubAgent, Mode::IndependentSession];
    // Explicit expected membership for every subset, in `modes` order.
    let admitted_sets: [(&[Mode], [bool; 3]); 8] = [
        (&[], [false, false, false]),
        (&[Mode::SameSession], [true, false, false]),
        (&[Mode::SubAgent], [false, true, false]),
        (&[Mode::IndependentSession], [false, false, true]),
        (&[Mode::SameSession, Mode::SubAgent], [true, true, false]),
        (
            &[Mode::SameSession, Mode::IndependentSession],
            [true, false, true],
        ),
        (
            &[Mode::SubAgent, Mode::IndependentSession],
            [false, true, true],
        ),
        (&modes, [true, true, true]),
    ];
    let pins = [None, Some(modes[0]), Some(modes[1]), Some(modes[2])];
    for (admitted, membership) in admitted_sets {
        let policy = policy(admitted, MechanicalBasis::Asserted, false);
        for (index, mode) in modes.into_iter().enumerate() {
            for pin in pins {
                // These are assessment inputs, not rewritten durable rows:
                // keep revision binding coherent so this matrix reaches the
                // policy check. Ordinary pin edits retire the old revision.
                let mut item = marked.clone();
                item.evaluation_mode = pin;
                let mut evaluation = recorded.record.clone();
                evaluation.work_revision_hash = CanonicalObject::freeze(&item)
                    .expect("matrix work revision")
                    .key()
                    .clone();
                evaluation.mode = mode;
                evaluation.evaluator = actor(if mode == Mode::SameSession {
                    "runner"
                } else {
                    "judge"
                });
                let expected = if !membership[index] {
                    Err(ModePolicyMismatch::DisallowedMode)
                } else if let Some(selected) = pin.filter(|selected| *selected != mode) {
                    Err(ModePolicyMismatch::SelectedPinMismatch(selected))
                } else {
                    Ok(())
                };
                let cell = format!("mode {mode:?}, allowed {admitted:?}, pin {pin:?}");
                assert_eq!(assess_mode_policy(&item, &policy, mode), expected, "{cell}");

                let admission = admit_mode(&item, &policy, mode);
                if admitted.is_empty() {
                    // Admission alone refuses a disabled project; public
                    // completion instead uses the self-asserted route.
                    assert!(
                        matches!(admission,
                            Err(StoreError::AcceptanceEvaluationRefused { reason, .. })
                                if reason == "the project policy does not enable acceptance evaluation; completion stays self-asserted"
                        ),
                        "{cell}"
                    );
                } else {
                    match &expected {
                        Ok(()) => assert!(admission.is_ok(), "{cell}: {admission:?}"),
                        Err(ModePolicyMismatch::DisallowedMode) => assert!(
                            matches!(admission,
                                Err(StoreError::AcceptanceEvaluationRefused { reason, .. })
                                    if reason.starts_with(&format!("mode {} is not allowed by the project policy; allowed: ", mode.word()))
                            ),
                            "{cell}"
                        ),
                        Err(ModePolicyMismatch::SelectedPinMismatch(selected)) => assert!(
                            matches!(admission,
                                Err(StoreError::AcceptanceEvaluationRefused { reason, .. })
                                    if reason == format!("this task is marked for mode {}; evaluate in that mode", selected.word())
                            ),
                            "{cell}"
                        ),
                    }
                }
                // Full freshness also enforces the known independent-default
                // rule for an unmarked same-session record. That justified
                // difference is separate from the shared mode/pin decision.
                let unmarked_affiliation =
                    mode == Mode::SameSession && pin.is_none() && admitted != [Mode::SameSession];
                let expected_freshness = (expected.is_err() || unmarked_affiliation)
                    .then_some(AcceptanceStaleReason::Policy);
                assert_eq!(
                    staleness(
                        &store.connection,
                        &item,
                        held.run_id,
                        &policy,
                        &evaluation,
                        SourceCheck::Unmeasured,
                    )
                    .expect("matrix freshness assessment"),
                    expected_freshness,
                    "{cell}"
                );
            }
        }
    }
}
