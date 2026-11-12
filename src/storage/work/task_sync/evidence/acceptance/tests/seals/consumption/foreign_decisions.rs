//! Consumed use decisions that travel on stay inert where they are foreign and
//! keep their native projections when they come back to the host that made them.
use super::*;
use crate::storage::work::task_sync::evidence::verification::applicability::{
    VerificationUse, eligible_for_on,
};
use crate::task_sync::evidence::verification::VerificationApplicabilityDecision;

/// Ask the shared eligibility reader about a decision in its own recorded context.
fn eligible_in_own_context(store: &SqliteStore, id: &ObjectId) -> bool {
    let (_, bytes) = object_on(&store.connection, id).unwrap().unwrap();
    let decision: VerificationApplicabilityDecision = serde_json::from_slice(&bytes).unwrap();
    let context = VerificationUse {
        project: &decision.project_id,
        origin: &decision.origin,
        binding: &decision.binding,
        original: &decision.original,
        criterion: decision.criterion,
        requirement: Some(&decision.requirement),
        governing: &decision.governing_fingerprint,
        root: &decision.named_root,
        source: &decision.source,
    };
    eligible_for_on(&store.connection, id, &context).unwrap()
}

fn projection_rows(store: &SqliteStore) -> Vec<String> {
    let mut statement = store
        .connection
        .prepare(
            "SELECT 'verification:'||decision_id||':'||eligible||':'||invalidated||':'||COALESCE(consumed_by,'-')
               FROM work_task_sync_evidence_applicability
             UNION ALL SELECT 'acceptance:'||decision_id||':'||eligible FROM work_task_sync_acceptance_uses
             ORDER BY 1",
        )
        .unwrap();
    statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn mentions(rows: &[String], id: &ObjectId) -> bool {
    rows.iter().any(|row| row.contains(id.as_str()))
}

/// Pack the selected records with the complete run feed of `source`, under a
/// newly enrolled origin, as a peer exporting that history would.
fn exported(source: &Fixture, ids: &[&ObjectId]) -> EvidencePack {
    let (origin, enrollment) = enrollment();
    let mut records = vec![enrollment];
    records.extend(ids.iter().map(|id| envelope(&source.store, &origin, id)));
    pack(
        &origin,
        with_feed_witnesses(
            source,
            &origin,
            &FeedId::RunExecution(source.binding.run_id),
            records,
        ),
    )
}

fn imported(store: &SqliteStore, id: &ObjectId) -> bool {
    crate::storage::work::task_sync::evidence::is_imported_only(&store.connection, id).unwrap()
}

fn assert_foreign_inert(store: &SqliteStore, verification: &ObjectId, acceptance: &ObjectId) {
    healthy(store);
    assert!(imported(store, verification) && imported(store, acceptance));
    let rows = projection_rows(store);
    assert!(!mentions(&rows, verification), "{rows:?}");
    assert!(!mentions(&rows, acceptance), "{rows:?}");
    assert!(!eligible_in_own_context(store, verification));
    assert!(store.imported_verification_summary(verification).is_err());
    assert!(
        store
            .imported_acceptance_summary(acceptance, at(40))
            .is_err()
    );
}

#[test]
fn consumed_use_decisions_stay_inert_abroad_and_native_when_they_return() {
    let mut data = selected(false);
    let verification = data.verification.decision.clone();
    let acceptance = data.acceptance.decision.clone();
    let consumed = consume(&mut data.target, &acceptance, true);
    let input = completion(
        &mut data.target,
        data.root.path(),
        &consumed.consumption,
        true,
    );
    data.target
        .store
        .complete_work(&input, &DevelopmentNoopRedactor)
        .unwrap();
    healthy(&data.target.store);
    let local = projection_rows(&data.target.store);
    assert!(mentions(&local, &verification) && mentions(&local, &acceptance));
    assert!(eligible_in_own_context(&data.target.store, &verification));
    let ids = [&verification, &acceptance, &consumed.consumption];
    let bytes: Vec<_> = ids
        .iter()
        .map(|id| {
            object_on(&data.target.store.connection, id)
                .unwrap()
                .unwrap()
        })
        .collect();

    // The destination's consumed decisions travel back to the original
    // producer, where they are foreign: no row, no eligibility, no summary.
    let onward = exported(&data.target, &ids);
    retain(&mut data.origin.store, &onward);
    assert_foreign_inert(&data.origin.store, &verification, &acceptance);
    crate::storage::work::schema::repair_rebuildable_schema_on(&data.origin.store.connection)
        .unwrap();
    assert_foreign_inert(&data.origin.store, &verification, &acceptance);
    for (id, original) in ids.iter().zip(&bytes) {
        assert_eq!(
            &object_on(&data.origin.store.connection, id)
                .unwrap()
                .unwrap(),
            original
        );
    }

    // The same decisions return to the host that made them. They are already
    // present, so the admission does not list them as imported: they stay native.
    let back = exported(&data.origin, &ids);
    retain(&mut data.target.store, &back);
    healthy(&data.target.store);
    assert!(ids.iter().all(|id| !imported(&data.target.store, id)));
    assert_eq!(projection_rows(&data.target.store), local);
    assert!(eligible_in_own_context(&data.target.store, &verification));
    data.target
        .store
        .imported_verification_summary(&verification)
        .unwrap();
    data.target
        .store
        .imported_acceptance_summary(&acceptance, at(40))
        .unwrap();
    crate::storage::work::schema::repair_rebuildable_schema_on(&data.target.store.connection)
        .unwrap();
    healthy(&data.target.store);
    assert_eq!(projection_rows(&data.target.store), local);
    for (id, original) in ids.iter().zip(&bytes) {
        assert_eq!(
            &object_on(&data.target.store.connection, id)
                .unwrap()
                .unwrap(),
            original
        );
    }
}

fn consumption_membership(store: &SqliteStore, request: &ConsumptionRequest) -> (i64, i64) {
    store
        .connection
        .query_row(
            "SELECT
               (SELECT count(*) FROM work_feed_entries
                WHERE feed_kind='run_execution' AND feed_id=?2 AND object_id=?1),
               (SELECT count(*) FROM work_run_evidence
                WHERE evidence_id=?1 AND run_id=?2 AND evidence_kind='imported')",
            rusqlite::params![
                request.consumption.as_str(),
                request.binding.run_id.0.to_string()
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

fn replay_consumption(
    store: &mut SqliteStore,
    record: &crate::task_sync::evidence::consumption::ImportedEvidenceConsumption,
    acceptance: bool,
) -> std::result::Result<
    crate::task_sync::evidence::consumption::ConsumptionReceipt,
    crate::StoreError,
> {
    if acceptance {
        store.consume_imported_acceptance(
            &record.project_id,
            &record.session_id,
            &record.actor,
            &record.request,
            at(40),
        )
    } else {
        store.consume_imported_verification(
            &record.project_id,
            &record.session_id,
            &record.actor,
            &record.request,
            at(40),
        )
    }
}

fn assert_foreign_consumption_replay_refused(acceptance: bool) {
    let mut data = selected(false);
    let decision = if acceptance {
        data.acceptance.decision.clone()
    } else {
        data.verification.decision.clone()
    };
    let request = consume(&mut data.target, &decision, acceptance);
    let record = data
        .target
        .store
        .imported_consumption_record(&request.consumption)
        .unwrap();
    let original = object_on(&data.target.store.connection, &request.consumption)
        .unwrap()
        .unwrap();
    let ids = [
        &data.verification.decision,
        &data.acceptance.decision,
        &request.consumption,
    ];
    let onward = exported(&data.target, &ids);
    retain(&mut data.origin.store, &onward);
    assert!(imported(&data.origin.store, &request.consumption));
    assert_eq!(consumption_membership(&data.origin.store, &request), (0, 0));
    healthy(&data.origin.store);
    let before =
        crate::storage::test_database_shape_snapshot(&data.origin.store.connection).unwrap();

    // Match every asserted field of the retained original, rather than using
    // the receiving host's session or a new request identity.
    replay_consumption(&mut data.origin.store, &record, acceptance)
        .expect_err("foreign-only consumption must not return a successful local replay");

    assert_eq!(
        crate::storage::test_database_shape_snapshot(&data.origin.store.connection).unwrap(),
        before
    );
    assert_eq!(consumption_membership(&data.origin.store, &request), (0, 0));
    assert_eq!(
        object_on(&data.origin.store.connection, &request.consumption)
            .unwrap()
            .unwrap(),
        original
    );
    healthy(&data.origin.store);
}

#[test]
fn consumption_replay_refuses_foreign_verification_without_local_attachment() {
    assert_foreign_consumption_replay_refused(false);
}

#[test]
fn consumption_replay_refuses_foreign_acceptance_without_local_attachment() {
    assert_foreign_consumption_replay_refused(true);
}

#[test]
fn consumption_replay_preserves_genuine_local_use_and_membership() {
    for acceptance in [false, true] {
        let mut data = selected(false);
        let decision = if acceptance {
            data.acceptance.decision.clone()
        } else {
            data.verification.decision.clone()
        };
        let request = consume(&mut data.target, &decision, acceptance);
        let record = data
            .target
            .store
            .imported_consumption_record(&request.consumption)
            .unwrap();
        assert!(!imported(&data.target.store, &request.consumption));
        assert_eq!(consumption_membership(&data.target.store, &request), (1, 1));
        let before =
            crate::storage::test_database_shape_snapshot(&data.target.store.connection).unwrap();

        let receipt = replay_consumption(&mut data.target.store, &record, acceptance).unwrap();
        assert!(receipt.replayed);
        assert_eq!(receipt.consumption, request.consumption);
        assert_eq!(receipt.decision, request.decision);
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&data.target.store.connection).unwrap(),
            before
        );
        assert_eq!(consumption_membership(&data.target.store, &request), (1, 1));
        healthy(&data.target.store);
    }
}
