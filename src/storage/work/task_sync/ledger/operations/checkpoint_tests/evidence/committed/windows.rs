//! Real retained Git carriers interleave at the actual committed writer edge.

use super::*;
use crate::task_sync::{
    data_worktree::DataWorktree,
    exchange::{self, Cancellation, RunLimits},
};
use std::sync::mpsc;

fn pack(binding: &Binding) -> EvidencePack {
    let origin = ObjectId::mint();
    let enrollment = crate::task_sync::origin::Enrollment {
        project_id: binding.project.clone(),
        predecessor_origin_id: None,
        enrollment_operation_id: uuid::Uuid::new_v4(),
        actor: binding.actor.clone(),
        enrolled_at: at(2),
    };
    let original = RecordEnvelope::from_canonical(
        origin.clone(),
        origin.clone(),
        RecordKind::ProducingOrigin.as_str(),
        crate::canonical::canonical_bytes(&enrollment).unwrap(),
    )
    .unwrap();
    EvidencePack::prepare(
        ObjectId::mint(),
        binding.project.clone(),
        origin,
        vec![original],
    )
    .unwrap()
}

async fn input() -> (
    crate::test_support::TempHome,
    SqliteStore,
    InspectedEvidence,
) {
    use super::super::super::data_bootstrap;
    let (home, mut store, binding, id, bootstrap) = data_bootstrap::fixture().await;
    store
        .audit_task_sync_custody(&bootstrap, &binding.actor, at(30))
        .unwrap();
    let data = DataWorktree::from_binding(&binding).unwrap();
    let original = pack(&binding);
    let files = data
        .stage_evidence_packs(&binding.project, &[original])
        .unwrap();
    data_bootstrap::git(&data.checkout, &["add", ".engram"]);
    data_bootstrap::git(
        &data.checkout,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "Publish replay evidence",
        ],
    );
    let data = DataWorktree::from_binding(&binding).unwrap();
    let operation = uuid::Uuid::new_v4();
    let source = exchange::retain_commit(
        &data.checkout,
        operation,
        data.head.as_ref().unwrap(),
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    let cut = producing_origin::cut_on(&store.connection, &binding.project).unwrap();
    let file = &files[0];
    let intent = EvidenceIntent {
        operation,
        key: operation.to_string(),
        direction: Direction::Incoming,
        actor: binding.actor.clone(),
        producer: CanonicalObject::freeze(&"replay evidence reader")
            .unwrap()
            .key()
            .clone(),
        expected_head: Some(source.oid.clone()),
        source,
        local_cut: OriginQualifiedCut {
            binding: id.clone(),
            origin: binding.store.origin.clone().unwrap(),
            restore_occurrence: None,
            position: FeedPosition {
                feed: FeedId::Project(binding.project.clone()),
                position: cut.position,
            },
            anchor: cut.anchor,
        },
        packs: BTreeMap::from([(
            file.pack.clone(),
            EvidencePackSelection {
                publishing_origin: file.publishing_origin.clone(),
                admission: ObjectId::mint(),
                content_fingerprint: file.content_fingerprint.clone(),
                encoded_bytes: file.encoded_bytes,
                records: file.records,
            },
        )]),
    };
    let recipe = store
        .retain_task_sync_evidence_recipe(&id, &binding, &intent, at(70))
        .unwrap();
    let batch = crate::task_sync::git::read_selected_evidence(
        &id,
        &binding,
        &intent,
        std::time::Duration::from_secs(60),
    )
    .unwrap();
    (
        home,
        store,
        InspectedEvidence {
            binding_id: id,
            binding,
            recipe,
            packs: batch.packs,
        },
    )
}

#[tokio::test]
async fn late_real_carrier_after_sql_commit_recovers_exact_ack_then_requires_explicit_audit() {
    let (_home, mut store, input) = input().await;
    let binding = &input.binding;
    let checkout = binding.repository.main_checkout.clone();
    let oid = input.recipe.intent().unwrap().source.oid.clone();
    let operation = uuid::Uuid::new_v4();
    let (send, receive) = mpsc::sync_channel(1);
    super::super::super::super::super::evidence::after_commit::set(move || {
        let carrier = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(exchange::retain_commit(
                    &checkout,
                    operation,
                    &oid,
                    RunLimits::default(),
                    &Cancellation::default(),
                ))
                .unwrap()
        })
        .join()
        .unwrap();
        send.send(carrier).unwrap();
    });
    let error = store.apply_task_sync_evidence_import(&input).unwrap_err();
    assert!(
        error.to_string().contains("retained operations are ahead"),
        "{error}"
    );
    let carrier = receive.recv().unwrap();
    let original = store
        .completed_task_sync_evidence(&input.binding_id, input.recipe.intent().unwrap())
        .unwrap()
        .unwrap();
    let pending = private::read(&binding.store.database, &binding.project)
        .unwrap()
        .witness
        .unwrap();
    assert_eq!(
        pending.pending_exchange.as_ref(),
        Some(&retention::evidence::request_fingerprint(&input.recipe).unwrap())
    );
    let path = binding
        .repository
        .git_directory
        .join("engram-task-sync-commits")
        .join(format!("{operation}.json"));
    let bytes = std::fs::read(&path).unwrap();
    let before = objects(&store);
    let mut request = audit_request(
        &store,
        binding,
        &input.binding_id,
        CustodySeedSelection::CustodyOnly,
    );
    request.seed = CustodySeedSelection::AbandonFetchedCommit {
        binding: input.binding_id.clone(),
        commit: carrier.clone(),
    };
    request.operation = operation;
    assert!(
        store
            .audit_task_sync_custody(&request, &binding.actor, at(71))
            .is_err()
    );
    assert_eq!(objects(&store), before);
    // The raw receipt API neither opens Git nor clears the private pending bit.
    assert_eq!(
        store
            .completed_task_sync_evidence(&input.binding_id, input.recipe.intent().unwrap())
            .unwrap(),
        Some(original.clone())
    );
    assert_eq!(
        private::read(&binding.store.database, &binding.project)
            .unwrap()
            .witness,
        Some(pending)
    );
    crate::task_sync::git::BEFORE_QUERY.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|command| {
            panic!("receipt acknowledgement launched Git {command}")
        }));
    });
    let replay = exchange::apply_evidence_exchange(
        &mut store,
        &input.binding_id,
        binding,
        input.recipe.intent().unwrap(),
        at(72),
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await;
    crate::task_sync::git::BEFORE_QUERY.with(|hook| *hook.borrow_mut() = None);
    assert_eq!(replay.unwrap(), original);
    let after = private::read(&binding.store.database, &binding.project)
        .unwrap()
        .witness
        .unwrap();
    assert!(after.pending_exchange.is_none());
    assert_eq!(after.exchange.as_ref().unwrap().head, original.outcome);
    assert_eq!(objects(&store), before);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(
        retention::commit_selection(&binding.repository.git_directory, operation).unwrap(),
        Some(carrier.clone())
    );
    assert_eq!(
        store.apply_task_sync_evidence_import(&input).unwrap(),
        original
    );
    assert_eq!(
        private::read(&binding.store.database, &binding.project)
            .unwrap()
            .witness,
        Some(after)
    );
    let guard = store.prepare_task_sync_origin(&binding.project).unwrap();
    let transaction = store.begin_work_mutation().unwrap();
    assert!(producing_origin::check_on(&transaction, &binding.project, &guard).is_err());
    drop(transaction);
    drop(guard);
    // Construct the audit against the current committed successor, not the old pending predecessor.
    let mut audit = audit_request(&store, binding, &input.binding_id, request.seed);
    audit.operation = operation;
    store
        .audit_task_sync_custody(&audit, &binding.actor, at(73))
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    let planning = store.export_task_tree(&binding.project).unwrap();
    let next = super::super::super::super::tests::input(
        &store,
        &input.binding_id,
        binding,
        planning.clone(),
    );
    store.prepare_task_sync_exchange(&next, at(80)).unwrap();
    store
        .apply_task_sync_exchange(&next, false, at(81))
        .unwrap();
    assert_eq!(store.export_task_tree(&binding.project).unwrap(), planning);
    assert!(store.verify_all().unwrap().is_healthy());
    drop(store);
    SqliteStore::repair_rebuildable_projections(&binding.store.database).unwrap();
    let reopened = SqliteStore::open(&binding.store.database).unwrap();
    assert_eq!(
        reopened
            .completed_task_sync_evidence(&input.binding_id, input.recipe.intent().unwrap())
            .unwrap(),
        Some(original)
    );
    assert!(reopened.verify_all().unwrap().is_healthy());
}
