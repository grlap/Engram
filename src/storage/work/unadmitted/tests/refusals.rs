//! Every refusal records nothing: a malformed or oversized request, or one
//! whose binding, cut or root basis the store's history does not hold.

use super::*;
use crate::domain::{MAX_OBSERVED_CHECKS, NamedRootBindingKind, WorkClaimId};

fn assert_refused_without_effects(
    fixture: &mut Fixture,
    input: ExecutionObserveInput,
    expected: fn(&StoreError) -> bool,
    case: &str,
) {
    let before = footprint(&fixture.store);
    let refused = observe(fixture, input, 7).expect_err(case);
    assert!(expected(&refused), "{case}: {refused:?}");
    assert_eq!(footprint(&fixture.store), before, "{case} left effects");
}

fn invalid(error: &StoreError) -> bool {
    matches!(error, StoreError::ExecutionObservationInvalid(_))
}

fn mismatch(error: &StoreError) -> bool {
    matches!(error, StoreError::ExecutionObservationBasisMismatch(_))
}

#[test]
fn a_malformed_or_oversized_request_is_refused_whole() {
    let mut fixture = fixture();
    let base = inter_turn_change(&fixture, "malformed");
    let turn_with = |checks: Vec<ObservedCheck>| ObservedOccurrence::UnadmittedTurn {
        host_turn_ref: "turn".into(),
        source_change: None,
        observed_checks: checks,
    };
    let cases: Vec<(&str, ExecutionObserveInput)> = vec![
        ("equal revisions", {
            let mut input = base.clone();
            input.occurrence = ObservedOccurrence::InterTurnChange {
                source_change: content_change("rev-a", "rev-a"),
            };
            input
        }),
        ("passed without finished_at", {
            let mut input = base.clone();
            let mut check = passed_check("unfinished");
            check.finished_at = None;
            input.occurrence = turn_with(vec![check]);
            input
        }),
        ("duplicate check ids", {
            let mut input = base.clone();
            input.occurrence = turn_with(vec![passed_check("same"), passed_check("same")]);
            input
        }),
        ("too many checks", {
            let mut input = base.clone();
            input.occurrence = turn_with(
                (0..=MAX_OBSERVED_CHECKS)
                    .map(|index| passed_check(&format!("check-{index}")))
                    .collect(),
            );
            input
        }),
        ("window ends after recording", {
            let mut input = base.clone();
            input.observed_interval.through = at(60);
            input
        }),
        ("sighting outside the window", {
            let mut input = base.clone();
            input.observed_interval.from = at(5);
            input
        }),
        ("blank key", {
            let mut input = base.clone();
            input.idempotency_key = " ".into();
            input
        }),
        ("oversized evidence ref", {
            let mut input = base.clone();
            let mut check = passed_check("big-ref");
            check.host_evidence_ref = Some("r".repeat(2_049));
            input.occurrence = turn_with(vec![check]);
            input
        }),
        ("blank asserted basis", {
            let mut input = base.clone();
            input.causality = ObservationCausality::HostAssertion {
                claimed_actor: Box::new(actor("runner")),
                basis: String::new(),
            };
            input
        }),
        ("workspace changes inside one measurement", {
            let mut input = base;
            let ObservedOccurrence::InterTurnChange {
                source_change: ObservedSourceChange::ContentComparison { baseline, .. },
            } = &mut input.occurrence
            else {
                unreachable!()
            };
            baseline.workspace_id = "workspace-Z".into();
            input
        }),
    ];
    for (case, input) in cases {
        assert_refused_without_effects(&mut fixture, input, invalid, case);
    }
}

#[test]
fn a_basis_the_store_does_not_hold_is_refused() {
    let mut fixture = fixture();
    let base = inter_turn_change(&fixture, "mismatch");
    let head = run_head(&fixture);
    let cases: Vec<(&str, ExecutionObserveInput)> = vec![
        ("invented claim", {
            let mut input = base.clone();
            input.binding.claim_id = WorkClaimId::new();
            input
        }),
        ("wrong fence", {
            let mut input = base.clone();
            input.binding.claim_fence += 1;
            input
        }),
        ("wrong work revision", {
            let mut input = base.clone();
            input.binding.work_revision += 1;
            input
        }),
        ("cut beyond the head", {
            let mut input = base.clone();
            input.root_basis.capture_run_cut = head + 1;
            input
        }),
        ("cut before the claim", {
            let mut input = base.clone();
            input.root_basis.capture_run_cut = 1;
            input
        }),
        ("a root state the claim never had", {
            let mut input = base.clone();
            input.root_basis.state = NamedRootState::Bound {
                workspace_id: "workspace-A".into(),
                generation: 1,
                named_at: at(3),
            };
            input
        }),
        ("an invented root event", {
            let mut input = base.clone();
            input.root_basis.latest_event = Some(crate::ObjectId::from_canonical_bytes(b"event"));
            input
        }),
        ("a root generation never recorded", {
            let mut input = base;
            let ObservedOccurrence::InterTurnChange {
                source_change: ObservedSourceChange::ContentComparison { sighting, .. },
            } = &mut input.occurrence
            else {
                unreachable!()
            };
            sighting.source_basis.source_root_generation = Some(4);
            sighting.source_basis.source_root_state = Some(crate::domain::SourceRootState::Named);
            input
        }),
    ];
    for (case, input) in cases {
        assert_refused_without_effects(&mut fixture, input, mismatch, case);
    }
}

#[test]
fn another_projects_work_is_refused() {
    let mut fixture = fixture();
    let foreign = fixture
        .store
        .create_work(
            &root_request("project-b", "foreign-work", 3),
            &DevelopmentNoopRedactor,
        )
        .expect("foreign work");
    let foreign_claim = claim(
        &mut fixture.store,
        &foreign,
        "runner",
        "foreign-claim",
        4,
        300,
    );
    let run = load_work_run(&fixture.store.connection, foreign_claim.run_id).expect("run");
    let mut input = inter_turn_change(&fixture, "foreign");
    input.binding = ControlWorkBinding {
        root_execution_id: run.root_execution_id,
        work_id: foreign.work_id,
        run_id: run.run_id,
        work_revision: foreign_claim.accepted_work_revision,
        claim_id: foreign_claim.claim_id,
        claim_fence: foreign_claim.fence,
    };
    assert_refused_without_effects(&mut fixture, input, mismatch, "another project");
}

// The basis is history at the cut: a root named after the capture does not
// count against it, and one named before it must be stated.
#[test]
fn the_root_basis_is_read_at_the_capture_cut() {
    let mut fixture = fixture();
    let before_name = inter_turn_change(&fixture, "before-name");
    let host = &fixture.host;
    let named = fixture
        .store
        .bind_named_root(
            &fixture.work.project_id,
            &host.status.session_id,
            &host.connection_token,
            &host.routing_token,
            fixture.claim.claim_id,
            fixture.claim.fence,
            "workspace-A",
            3,
            at(4),
            NamedRootBindingKind::Bound,
            None,
            &mut actor("observer"),
            "name-3",
            at(4),
        )
        .expect("named");
    // Captured before the name: no root, and that is what the cut held.
    observe(&mut fixture, before_name, 7).expect("captured before the name");
    // Captured after it, the basis must name the bound root and its event.
    let mut after_name = inter_turn_change(&fixture, "after-name");
    assert_refused_without_effects(&mut fixture, after_name.clone(), mismatch, "stale state");
    after_name.idempotency_key = "after-name-stated".into();
    after_name.root_basis.state = NamedRootState::Bound {
        workspace_id: "workspace-A".into(),
        generation: 3,
        named_at: at(4),
    };
    after_name.root_basis.latest_event = Some(named.event);
    let ObservedOccurrence::InterTurnChange {
        source_change: ObservedSourceChange::ContentComparison { sighting, .. },
    } = &mut after_name.occurrence
    else {
        unreachable!()
    };
    sighting.source_basis.source_root_generation = Some(3);
    sighting.source_basis.source_root_state = Some(crate::domain::SourceRootState::Named);
    observe(&mut fixture, after_name, 8).expect("stated basis");
}

#[test]
fn an_asserted_actor_is_only_ever_asserted_and_bounded() {
    let mut fixture = fixture();
    let base = inter_turn_change(&fixture, "asserted-actor");
    let asserting = |actor: crate::domain::ActorContext| {
        let mut input = base.clone();
        input.causality = ObservationCausality::HostAssertion {
            claimed_actor: Box::new(actor),
            basis: "terminal ownership".into(),
        };
        input
    };
    let mut authenticated = actor("runner");
    authenticated.assurance = crate::domain::AssuranceLevel::Authenticated;
    let mut long_kind = actor("runner");
    long_kind.actor_kind = "k".repeat(crate::domain::MAX_CLAIMED_ACTOR_TEXT_BYTES + 1);
    let mut many_links = actor("runner");
    many_links.provenance_chain = (0..=crate::domain::MAX_CLAIMED_ACTOR_PROVENANCE_LINKS)
        .map(|index| crate::domain::ProvenanceLink {
            relation: crate::domain::ProvenanceRelation::AssertedBy,
            source: format!("source-{index}"),
            reference: None,
        })
        .collect();
    for (case, actor) in [
        ("a stronger assurance", authenticated),
        ("an oversized actor kind", long_kind),
        ("too many provenance links", many_links),
    ] {
        assert_refused_without_effects(&mut fixture, asserting(actor), invalid, case);
    }
}

pub(super) fn name_root(
    fixture: &mut Fixture,
    generation: i64,
    kind: NamedRootBindingKind,
    named_second: i64,
    second: i64,
) -> crate::domain::NamedRootBindingReceipt {
    let host = &fixture.host;
    fixture
        .store
        .bind_named_root(
            &fixture.work.project_id,
            &host.status.session_id,
            &host.connection_token,
            &host.routing_token,
            fixture.claim.claim_id,
            fixture.claim.fence,
            "workspace-A",
            generation,
            at(named_second),
            kind,
            (kind == NamedRootBindingKind::Ended)
                .then_some(crate::domain::NamedRootEndReason::RootInvalid),
            &mut actor("observer"),
            &format!("root-{generation}-{kind:?}"),
            at(second),
        )
        .expect("root event")
}

pub(super) fn sighting_under(
    fixture: &Fixture,
    key: &str,
    state: NamedRootState,
    latest_event: crate::ObjectId,
    sighting: (i64, crate::domain::SourceRootState),
) -> ExecutionObserveInput {
    let mut input = inter_turn_change(fixture, key);
    input.root_basis.state = state;
    input.root_basis.latest_event = Some(latest_event);
    let ObservedOccurrence::InterTurnChange {
        source_change:
            ObservedSourceChange::ContentComparison {
                sighting: closing, ..
            },
    } = &mut input.occurrence
    else {
        unreachable!()
    };
    closing.source_basis.source_root_generation = Some(sighting.0);
    closing.source_basis.source_root_state = Some(sighting.1);
    input
}

// The closing sighting is what the root basis describes: a sighting stated
// under an older generation, or named after its root ended, is refused.
#[test]
fn the_closing_sighting_must_agree_with_the_root_basis() {
    use crate::domain::SourceRootState::{Ended, Named};
    let mut fixture = fixture();
    name_root(&mut fixture, 1, NamedRootBindingKind::Bound, 4, 4);
    let second = name_root(&mut fixture, 2, NamedRootBindingKind::Bound, 5, 5);
    let bound_two = NamedRootState::Bound {
        workspace_id: "workspace-A".into(),
        generation: 2,
        named_at: at(5),
    };
    let stale = sighting_under(
        &fixture,
        "stale",
        bound_two.clone(),
        second.event.clone(),
        (1, Named),
    );
    assert_refused_without_effects(&mut fixture, stale, mismatch, "an older generation");
    let current = sighting_under(&fixture, "current", bound_two, second.event, (2, Named));
    observe(&mut fixture, current, 7).expect("the bound generation agrees");

    let ended = name_root(&mut fixture, 2, NamedRootBindingKind::Ended, 5, 6);
    let named_after_end = sighting_under(
        &fixture,
        "named-after-end",
        NamedRootState::NoRoot,
        ended.event.clone(),
        (2, Named),
    );
    assert_refused_without_effects(
        &mut fixture,
        named_after_end,
        mismatch,
        "named after its end",
    );
    let stated_ended = sighting_under(
        &fixture,
        "stated-ended",
        NamedRootState::NoRoot,
        ended.event,
        (2, Ended),
    );
    observe(&mut fixture, stated_ended, 8).expect("an ended generation agrees");
}
