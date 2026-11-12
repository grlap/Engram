//! Custody accepts historical metadata within the canonical discovery capacity.

use super::*;
use crate::storage::work::task_sync::ledger;
use crate::task_sync::origin::private;

fn select_large(
    f: &ExportFixture,
    previous: Option<&ConfirmedExportBasis>,
    bytes: usize,
) -> (DeltaIntent, DeltaCandidatePayload) {
    let (mut intent, mut payload) = previous.map_or_else(
        || (f.intent.clone(), f.payload.clone()),
        |basis| f.next_intent(basis),
    );
    let old = f
        .payload
        .documents
        .values()
        .find_map(Option::as_ref)
        .unwrap();
    let DeltaBasis::Outgoing { export_before, .. } = &intent.basis else {
        unreachable!()
    };
    let root = if previous.is_some() {
        export_before
            .membership
            .inventory_root
            .excluding(old)
            .unwrap()
    } else {
        export_before.membership.inventory_root
    };
    let mut document = old.clone();
    document.outcome = "x".into();
    let (tail, sequence): (String, i64) = f
        .store
        .connection
        .query_row(
            "SELECT tail_id,sequence FROM work_task_sync_exchange_heads WHERE binding_id=?1",
            [f.id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let sequence = u64::try_from(sequence).unwrap();
    // Recompute comparisons for each actual document, never pad hashes.
    let mut select = |document: crate::task_sync::TaskDocument| {
        payload.output.inventory_root = root.including(&document).unwrap();
        intent.candidate.output = payload.output.clone();
        intent.candidate.changed = BTreeSet::from([document.id]);
        payload.documents = BTreeMap::from([(document.id, Some(document))]);
        intent.candidate.content_fingerprint =
            CanonicalObject::freeze(&payload).unwrap().key().clone();
        let event = Event {
            schema_version: 1,
            binding: f.id.clone(),
            sequence: sequence + 1,
            predecessor: Some(ObjectId::from_stored(tail.clone()).unwrap()),
            actor: intent.actor.clone(),
            recorded_at: at(40),
            transition: Transition::DeltaPrepared {
                intent: Box::new(intent.clone()),
                payload: Box::new(payload.clone()),
            },
        };
        CanonicalObject::freeze(&event).unwrap().bytes().len()
    };
    let overhead = select(document.clone()) - document.outcome.len();
    document.outcome = "x".repeat(bytes - overhead);
    assert_eq!(select(document), bytes);
    payload.validate(&f.id, &f.binding, &intent).unwrap();
    (intent, payload)
}

fn retain_commit(f: &ExportFixture, selected: &RetainedCommit) {
    let directory = f
        .binding
        .repository
        .git_directory
        .join("engram-task-sync-commits");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join(format!("{}.json", f.intent.operation)),
        crate::canonical::canonical_bytes(selected).unwrap(),
    )
    .unwrap();
}

fn history() -> ExportFixture {
    let mut f = ExportFixture::new(1);
    let mut previous: Option<ConfirmedExportBasis> = None;
    for index in 0..9 {
        let (intent, payload) = select_large(&f, previous.as_ref(), 14 * 1024 * 1024);
        f.intent = intent;
        f.payload = payload;
        retention::delta::retain_candidate(&f.id, &f.binding, &f.intent, &f.payload).unwrap();
        // Existing fixture admission uses the real capacity predicate, reducer,
        // canonical writer and projection owner; no Git execution is asserted.
        let prepared = f.event(Transition::DeltaPrepared {
            intent: Box::new(f.intent.clone()),
            payload: Box::new(f.payload.clone()),
        });
        let mut selected = commit(f.intent.operation, &format!("budget history {index}"));
        if let Some(previous) = &previous {
            selected.parents.push(previous.snapshot.commit.oid.clone());
        }
        retain_commit(&f, &selected);
        f.event(Transition::DeltaLocalCommitRecorded {
            prepared: prepared.clone(),
            commit: selected.clone(),
        });
        let start = f.event(Transition::PushStarted {
            prepared: prepared.clone(),
        });
        let event = SqliteStore::get_typed_object_on(&f.store.connection, &start, EVENT_KIND)
            .unwrap()
            .unwrap();
        retention::retain_publication_witness(
            &f.binding.repository.git_directory,
            &retention::PublicationWitness {
                operation: f.intent.operation,
                event_id: start,
                event,
            },
        )
        .unwrap();
        let outcome = f.event(Transition::PushObserved {
            prepared: prepared.clone(),
            observation: PublicationObservation::Confirmed {
                observed_tip: selected.oid.clone(),
            },
        });
        previous = Some(ConfirmedExportBasis {
            prepared,
            outcome,
            snapshot: MembershipSnapshot {
                commit: selected,
                membership: f.intent.candidate.output.clone(),
            },
        });
    }
    let canonical = ledger::Ledger::load(&f.store.connection).unwrap();
    let actual: (i64, i64) = f
        .store
        .connection
        .query_row(
            "SELECT count(*),sum(length(canonical_json)) FROM objects WHERE object_kind IN (?1,?2)",
            params![crate::task_sync::ledger::BINDING_KIND, EVENT_KIND],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(usize::try_from(actual.1).unwrap(), canonical.bytes);
    assert!(canonical.bytes > ledger::MAX_BYTES - 2 * 1024 * 1024);
    assert!(canonical.bytes < ledger::MAX_BYTES - 64 * 1024);
    assert!(usize::try_from(actual.0).unwrap() < ledger::MAX_RECORDS);
    // Do not leave an unrelated stale private witness as the reason the real
    // custody admission rejects this canonical historical fixture.
    let before = private::read(&f.binding.store.database, &f.binding.project)
        .unwrap()
        .witness
        .unwrap();
    let mut after = before.clone();
    after.cut = producing_origin::cut_on(&f.store.connection, &f.binding.project).unwrap();
    after.exchange = ledger::origin_anchor_on(
        &f.store.connection,
        &f.binding.project,
        f.binding.store.origin.as_ref().unwrap(),
    )
    .unwrap();
    private::advance(&f.binding.store.database, &before, &after).unwrap();
    f
}

#[test]
fn custody_budget_accepts_valid_near_capacity_history_and_exact_audit_replay() {
    let mut f = history();
    let request = audit_request(
        &f.store,
        &f.binding,
        &f.id,
        CustodySeedSelection::CustodyOnly,
    );
    let receipt = f
        .store
        .audit_task_sync_custody(&request, &f.binding.actor, at(80))
        .unwrap();
    assert!(receipt.acknowledged && !receipt.repeated);
    ledger::Ledger::load(&f.store.connection).unwrap();
    let before = state(&f.store);
    let private = private::read(&f.binding.store.database, &f.binding.project).unwrap();
    let replay = f
        .store
        .audit_task_sync_custody(&request, &f.binding.actor, at(81))
        .unwrap();
    assert!(replay.acknowledged && replay.repeated);
    assert_eq!(replay.checkpoint, receipt.checkpoint);
    assert_eq!(state(&f.store), before);
    assert_eq!(
        private::read(&f.binding.store.database, &f.binding.project).unwrap(),
        private
    );
}
