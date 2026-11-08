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
fn observed_check_refusals_name_indexed_fields_without_echoing_host_ids() {
    let mut fixture = fixture();
    let caller_id = format!("caller-check-{}", uuid::Uuid::new_v4());
    let good = passed_check(&caller_id);
    let checks = |check| vec![passed_check("first-valid-check"), check];
    let mut cases = vec![(
        "duplicate",
        "host_check_id",
        vec![good.clone(), good.clone()],
    )];
    let mut misordered = good.clone();
    misordered.started_at = Some(at(6));
    cases.push(("misordered", "started_at", checks(misordered)));
    let mut before_interval = good.clone();
    before_interval.started_at = Some(at(3));
    cases.push(("early start", "started_at", checks(before_interval)));
    let mut after_interval = good.clone();
    after_interval.finished_at = Some(at(7));
    cases.push(("late finish", "finished_at", checks(after_interval)));
    let mut late_observation = good.clone();
    late_observation.observed_at = at(7);
    cases.push(("late observation", "observed_at", checks(late_observation)));
    let mut unfinished = good.clone();
    unfinished.finished_at = None;
    cases.push(("unfinished", "finished_at", checks(unfinished)));
    let mut bad_basis = good.clone();
    bad_basis.source_basis = Some(crate::domain::ObservedCheckSourceBasis {
        workspace_id: String::new(),
        source_revision: None,
        git_tree: None,
        source_root_generation: None,
        source_root_state: None,
    });
    cases.push(("source basis", "source_basis", checks(bad_basis)));
    let mut bad_reference = good.clone();
    bad_reference.host_evidence_ref = Some("r".repeat(2_049));
    cases.push((
        "evidence reference",
        "host_evidence_ref",
        checks(bad_reference),
    ));
    for (case, field, observed_checks) in cases {
        let mut input = inter_turn_change(&fixture, case);
        input.occurrence = ObservedOccurrence::UnadmittedTurn {
            outcome: None,
            host_turn_ref: "turn".into(),
            source_change: None,
            observed_checks,
        };
        let before = footprint(&fixture.store);
        let error = observe_current(&mut fixture, input, 7).expect_err(case);
        let StoreError::ExecutionObservationInvalid(reason) = &error else {
            panic!("{case}: {error}");
        };
        assert!(
            reason.contains(&format!("observed_checks[1].{field}")),
            "{case}: {reason}"
        );
        assert!(
            !reason.contains(&caller_id),
            "{case}: caller check id was echoed"
        );
        assert_eq!(footprint(&fixture.store), before, "{case} left effects");
    }
    let mut valid = inter_turn_change(&fixture, "valid-checks");
    valid.occurrence = ObservedOccurrence::UnadmittedTurn {
        outcome: None,
        host_turn_ref: "turn".into(),
        source_change: None,
        observed_checks: checks(good),
    };
    let receipt = observe_current(&mut fixture, valid, 7).unwrap();
    assert_eq!(receipt.observed_checks.len(), 2);
    assert!(
        receipt
            .observed_checks
            .iter()
            .all(|check| check.credit == ObservedCheckCredit::Uncredited)
    );
}

#[test]
fn a_malformed_or_oversized_request_is_refused_whole() {
    let mut fixture = fixture();
    let base = inter_turn_change(&fixture, "malformed");
    let turn_with = |checks: Vec<ObservedCheck>| ObservedOccurrence::UnadmittedTurn {
        outcome: None,
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
    let holder = fixture.claim.holder.clone();
    let host = open_work_host(&mut fixture.store, &holder.0, at(4));
    let named = fixture
        .store
        .bind_named_root(
            &fixture.work.project_id,
            &host.session_id,
            &host.connection_token,
            crate::storage::HostSessionAuthority::Work,
            fixture.claim.claim_id,
            fixture.claim.fence,
            "workspace-A",
            3,
            at(4),
            NamedRootBindingKind::Bound,
            None,
            &mut actor(&holder.0),
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
    let holder = fixture.claim.holder.clone();
    let host = open_work_host(&mut fixture.store, &holder.0, at(second));
    fixture
        .store
        .bind_named_root(
            &fixture.work.project_id,
            &host.session_id,
            &host.connection_token,
            crate::storage::HostSessionAuthority::Work,
            fixture.claim.claim_id,
            fixture.claim.fence,
            "workspace-A",
            generation,
            at(named_second),
            kind,
            (kind == NamedRootBindingKind::Ended)
                .then_some(crate::domain::NamedRootEndReason::RootInvalid),
            &mut actor(&holder.0),
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

/// The endpoint's own invalid-request refusal, which its host handles as
/// definitive, carrying the text rule's reason.
fn source_text_refused(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::ExecutionObservationInvalid(reason)
            if reason.contains("control or bidirectional formatting character")
    )
}

// New host source text holding a control or bidirectional formatting
// character is refused whole, naming the field and recording nothing, in
// every place the request carries a workspace or a revision; an observed
// check's source and a lone watcher change included. Other format
// characters and Windows extended paths are kept.
#[test]
fn host_source_text_with_a_control_or_bidi_character_is_refused_whole() {
    let mut fixture = fixture();
    let base = inter_turn_change(&fixture, "source-text");
    let unsafe_text = "rev\u{202E}b";
    let mut cases: Vec<(&str, &str, ExecutionObserveInput)> = Vec::new();
    {
        let mut input = base.clone();
        input.occurrence = ObservedOccurrence::InterTurnChange {
            source_change: content_change("rev-a", unsafe_text),
        };
        cases.push((
            "sighting revision",
            "source_change.sighting.source_basis.source_revision",
            input,
        ));
    }
    {
        let mut input = base.clone();
        input.occurrence = ObservedOccurrence::InterTurnChange {
            source_change: content_change("rev\u{1}a", "rev-b"),
        };
        cases.push((
            "baseline revision",
            "source_change.baseline.source_revision",
            input,
        ));
    }
    {
        let mut input = base.clone();
        input.occurrence = ObservedOccurrence::InterTurnChange {
            source_change: ObservedSourceChange::WatcherOnly {
                workspace_id: "workspace\u{061C}A".into(),
                observed_at: at(5),
            },
        };
        cases.push(("watcher workspace", "source_change.workspace_id", input));
    }
    {
        let mut input = base.clone();
        let mut check = passed_check("checked");
        check.source_basis = Some(
            ExecutionSourceBasis {
                git_tree: None,
                workspace_id: "workspace\u{2066}A".into(),
                source_revision: "rev-b".into(),
                source_root_generation: None,
                source_root_state: None,
            }
            .into(),
        );
        input.occurrence = ObservedOccurrence::ObservedCheck {
            host_turn_ref: "turn".into(),
            check,
        };
        // A single observed check is named by the path its request carries.
        cases.push((
            "observed check workspace",
            "check.source_basis.workspace_id",
            input,
        ));
    }
    {
        let mut input = base.clone();
        let mut second = passed_check("second");
        second.source_basis = Some(
            ExecutionSourceBasis {
                git_tree: None,
                workspace_id: "workspace-a".into(),
                source_revision: "rev\u{200F}b".into(),
                source_root_generation: None,
                source_root_state: None,
            }
            .into(),
        );
        input.occurrence = ObservedOccurrence::UnadmittedTurn {
            outcome: None,
            host_turn_ref: "turn".into(),
            source_change: None,
            observed_checks: vec![passed_check("first"), second],
        };
        cases.push((
            "unadmitted turn check revision",
            "observed_checks[1].source_basis.source_revision",
            input,
        ));
    }
    for (case, field, input) in cases {
        let before = footprint(&fixture.store);
        let refused = observe(&mut fixture, input, 7).expect_err(case);
        assert!(source_text_refused(&refused), "{case}: {refused:?}");
        let reason = refused.to_string();
        assert!(reason.contains(field), "{case}: {refused}");
        assert_eq!(
            reason.contains("observed_checks"),
            field.starts_with("observed_checks"),
            "{case}: {refused}"
        );
        assert!(
            !refused
                .to_string()
                .chars()
                .any(crate::domain::is_refused_source_text_char),
            "{case}: the refused text is echoed"
        );
        assert_eq!(footprint(&fixture.store), before, "{case} left effects");
    }

    // A Windows extended path, a zero-width joiner and a variation selector
    // are kept byte for byte.
    let kept = "\\\\?\\C:\\work\u{200D}tree\u{FE0F}";
    let mut input = inter_turn_change(&fixture, "kept-text");
    let mut change = content_change("rev-a", "rev-b");
    if let ObservedSourceChange::ContentComparison {
        workspace_id,
        baseline,
        sighting,
    } = &mut change
    {
        *workspace_id = kept.into();
        baseline.workspace_id = kept.into();
        sighting.source_basis.workspace_id = kept.into();
    }
    input.occurrence = ObservedOccurrence::InterTurnChange {
        source_change: change,
    };
    let receipt = observe(&mut fixture, input, 7).expect("kept text records");
    let recorded = stored(&fixture.store, &receipt.observation);
    let RecordedOccurrence::InterTurnChange { source_change } = &recorded.occurrence else {
        panic!("an inter-turn change: {recorded:?}");
    };
    assert_eq!(source_change.workspace_id(), kept);
}

// The text rule runs after replay: an exact retry of an observation admitted
// before the rule, whose text the rule now refuses, still answers with its
// stored receipt.
#[test]
fn an_exact_retry_of_an_observation_admitted_before_the_text_rule_replays() {
    let mut fixture = fixture();
    let key = "admitted-before-the-rule";
    let first = inter_turn_change(&fixture, key);
    let admitted = observe(&mut fixture, first.clone(), 7).expect("recorded");
    let mut retry = first;
    retry.occurrence = ObservedOccurrence::InterTurnChange {
        source_change: content_change("rev-a", "rev\u{202E}b"),
    };
    // A fresh request with this text is refused.
    let mut fresh = retry.clone();
    fresh.idempotency_key = "fresh".into();
    let refused = observe(&mut fixture, fresh, 7).expect_err("refused text");
    assert!(source_text_refused(&refused), "{refused:?}");
    // Model the observation itself as stored before the text rule too. The
    // receipt verifier binds its frozen intent to this canonical record.
    let mut historical = stored(&fixture.store, &admitted.observation);
    historical.occurrence = RecordedOccurrence::record(retry.occurrence.clone());
    fixture
        .store
        .connection
        .execute(
            "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
            rusqlite::params![
                crate::canonical::canonical_bytes(&historical).expect("historical record"),
                admitted.observation.as_str()
            ],
        )
        .expect("the historical observation carries the same admitted text");
    // The stored operation as an admission before the rule left it: the
    // key's row answers for the retry's exact intent.
    let intent = CanonicalObject::freeze(
        &crate::storage::unadmitted_observation::ExecutionObserveIntent::of(
            &fixture.host.session_id,
            &retry,
        ),
    )
    .expect("intent");
    let rows = fixture
        .store
        .connection
        .execute(
            "UPDATE work_operation_results SET request_hash = ?1,
             result_json = CAST(json_set(CAST(result_json AS TEXT), '$.intent', json(CAST(?2 AS TEXT))) AS BLOB)
             WHERE operation = 'host_execution_observe' AND idempotency_key = ?3",
            rusqlite::params![
                intent.key().as_str(),
                intent.bytes(),
                serde_json::to_string(&(&fixture.work.project_id, &fixture.host.session_id, key)).unwrap()
            ],
        )
        .expect("rewrite the stored intent");
    assert_eq!(rows, 1);
    let report = fixture.store.verify_all().expect("historical fixture");
    assert!(report.is_healthy(), "{report:?}");
    let before = footprint(&fixture.store);
    let replayed = observe(&mut fixture, retry, 9).expect("the exact retry replays");
    assert_eq!(replayed, admitted);
    assert_eq!(stored(&fixture.store, &admitted.observation), historical);
    assert_eq!(
        footprint(&fixture.store),
        before,
        "a replay records nothing"
    );
}
