use super::{Fixture, evidence};
use crate::task_sync::ledger::{
    CoverageMeaning, Direction, EvidenceReceipt, EvidenceRefusal, EvidenceStop, ExchangeFamily,
    FamilyFrontiers, FamilyProgress, FamilyRequestState, PublicationObservation, Transition,
};

fn status(fixture: &Fixture) -> FamilyFrontiers {
    // The summary is derived only after the ordinary canonical reducer accepts
    // the chain. Its separate family accounting must not reinterpret settlement.
    fixture.reduced().unwrap();
    super::super::super::coverage::derive(&fixture.id, &fixture.chain).unwrap()
}

fn incoming(status: &FamilyFrontiers, family: ExchangeFamily) -> &FamilyProgress {
    status
        .families
        .iter()
        .find(|row| row.family == family && row.direction == Direction::Incoming)
        .unwrap()
}

fn retain(fixture: &mut Fixture) {
    let intent = evidence::selection(fixture);
    let prepared = fixture.push(Transition::EvidencePrepared {
        intent: Box::new(intent.clone()),
    });
    fixture.push(Transition::EvidenceImportApplied {
        prepared,
        receipt: EvidenceReceipt {
            admissions: intent
                .packs
                .iter()
                .map(|(id, selected)| (id.clone(), selected.admission.clone()))
                .collect(),
        },
    });
}

#[test]
fn retained_evidence_does_not_cover_a_deferred_planning_selection() {
    let mut fixture = Fixture::new(Direction::Incoming);
    let prepared = fixture.prepare();
    fixture.push(Transition::PlanningDeferred {
        prepared: prepared.clone(),
        hold: crate::ObjectId::mint(),
        candidate_fingerprint: fixture.intent.candidate.content_fingerprint.clone(),
        blocking_claims: vec![crate::ObjectId::mint()],
        after_cut: fixture.intent.local_cut.clone(),
    });
    retain(&mut fixture);
    let summary = status(&fixture);
    let planning = incoming(&summary, ExchangeFamily::Planning);
    assert_eq!(planning.captured.as_ref().unwrap().prepared, prepared);
    assert_eq!(planning.latest_state, Some(FamilyRequestState::Deferred));
    assert_eq!(planning.uncovered_requests, 1);
    assert!(planning.covered.is_none());
    let originals = incoming(&summary, ExchangeFamily::Evidence);
    assert_eq!(originals.covered_requests, 1);
    assert_eq!(originals.uncovered_requests, 0);
    assert_eq!(
        originals.covered.as_ref().unwrap().meaning,
        CoverageMeaning::EvidenceRetained
    );
    assert_eq!(status(&fixture), summary);
}

#[test]
fn a_later_retained_pack_does_not_hide_a_refused_selection() {
    let mut fixture = Fixture::new(Direction::Incoming);
    let intent = evidence::selection(&fixture);
    let prepared = fixture.push(Transition::EvidencePrepared {
        intent: Box::new(intent),
    });
    fixture.push(Transition::EvidenceStopped {
        prepared,
        outcome: EvidenceStop::Refused {
            reason: EvidenceRefusal::Dependency,
        },
    });
    let refused = status(&fixture);
    let row = incoming(&refused, ExchangeFamily::Evidence);
    assert_eq!(row.refused_requests, 1);
    assert_eq!(row.uncovered_requests, 1);
    assert!(row.covered.is_none());
    retain(&mut fixture);
    let later = status(&fixture);
    let row = incoming(&later, ExchangeFamily::Evidence);
    assert_eq!(row.latest_state, Some(FamilyRequestState::Covered));
    assert_eq!(row.captured_requests, 2);
    assert_eq!(row.covered_requests, 1);
    assert_eq!(row.refused_requests, 1);
    assert_eq!(row.uncovered_requests, 1);
}

#[test]
fn committed_and_unknown_publication_are_not_covered_until_confirmed() {
    let mut fixture = Fixture::new(Direction::Outgoing);
    let intent = fixture.legacy();
    let tip = intent.commit.oid.clone();
    let prepared = fixture.push(Transition::Prepared {
        intent: Box::new(intent),
    });
    fixture.push(Transition::LocalCommitRecorded {
        prepared: prepared.clone(),
    });
    let committed = status(&fixture);
    let outgoing = |summary: &FamilyFrontiers| {
        summary
            .families
            .iter()
            .find(|row| {
                row.family == ExchangeFamily::Planning && row.direction == Direction::Outgoing
            })
            .unwrap()
            .clone()
    };
    assert_eq!(outgoing(&committed).uncovered_requests, 1);
    assert!(outgoing(&committed).covered.is_none());
    fixture.push(Transition::PushStarted {
        prepared: prepared.clone(),
    });
    fixture.push(Transition::PushObserved {
        prepared: prepared.clone(),
        observation: PublicationObservation::Unknown { observed_tip: None },
    });
    let unknown = status(&fixture);
    assert_eq!(
        outgoing(&unknown).latest_state,
        Some(FamilyRequestState::PublicationUnknown)
    );
    assert!(outgoing(&unknown).covered.is_none());
    fixture.push(Transition::PushObserved {
        prepared,
        observation: PublicationObservation::Confirmed { observed_tip: tip },
    });
    let confirmed = status(&fixture);
    assert_eq!(outgoing(&confirmed).uncovered_requests, 0);
    assert_eq!(
        outgoing(&confirmed).covered.unwrap().meaning,
        CoverageMeaning::PublicationConfirmed
    );
}
