use super::*;
use crate::task_sync::ledger::EvidenceReceipt;

fn defer(fixture: &mut Fixture, prepared: ObjectId) -> ObjectId {
    fixture.push(Transition::PlanningDeferred {
        prepared,
        hold: ObjectId::mint(),
        candidate_fingerprint: fixture.intent.candidate.content_fingerprint.clone(),
        blocking_claims: vec![ObjectId::mint()],
        after_cut: fixture.intent.local_cut.clone(),
    })
}

#[test]
fn deferred_planning_allows_separate_evidence_without_advancing_planning_heads() {
    let mut fixture = Fixture::new(Direction::Incoming);
    let prepared = fixture.prepare();
    let operation = fixture.intent.operation.to_string();
    let deferred = defer(&mut fixture, prepared);
    let state = fixture.reduced().unwrap();
    assert!(state.head.pending.is_none());
    assert!(state.head.last_incoming.is_none());
    assert_eq!(
        state.operations[&operation].completed.as_ref(),
        Some(&deferred)
    );
    let evidence = super::evidence::selection(&fixture);
    let prepared = fixture.push(Transition::EvidencePrepared {
        intent: Box::new(evidence.clone()),
    });
    assert_eq!(
        fixture.reduced().unwrap().head.pending.as_ref(),
        Some(&prepared)
    );
    fixture.push(Transition::EvidenceImportApplied {
        prepared,
        receipt: EvidenceReceipt {
            admissions: evidence
                .packs
                .iter()
                .map(|(id, pack)| (id.clone(), pack.admission.clone()))
                .collect(),
        },
    });
    let state = fixture.reduced().unwrap();
    assert!(state.head.pending.is_none());
    assert!(state.head.last_incoming.is_none());
    assert_eq!(
        state.operations[&operation].completed.as_ref(),
        Some(&deferred)
    );
}

#[test]
fn deferred_planning_cannot_splice_another_preparation_or_outgoing_family() {
    let mut fixture = Fixture::new(Direction::Incoming);
    fixture.prepare();
    defer(&mut fixture, ObjectId::mint());
    assert!(fixture.reduced().is_err());
    let mut fixture = Fixture::new(Direction::Outgoing);
    let prepared = fixture.prepare();
    defer(&mut fixture, prepared);
    assert!(fixture.reduced().is_err());
    let mut fixture = Fixture::new(Direction::Incoming);
    fixture.prepare();
    let evidence = super::evidence::selection(&fixture);
    fixture.push(Transition::EvidencePrepared {
        intent: Box::new(evidence),
    });
    assert!(fixture.reduced().is_err());
}
