//! Real fetched carriers that never acquired an exchange recipe or outcome.

use super::*;
use crate::{
    CanonicalObject, ObjectId,
    storage::{SqliteStore, exchange_fixture},
    task_sync::{
        self,
        delta::{CheckpointReason, CustodyAuditRequest, CustodySeedSelection},
        ledger::{
            Binding, Direction, EvidenceIntent, EvidencePackSelection, RetainedCommit, Transition,
        },
    },
};
use fixture::{Repositories, git, line};
use std::collections::BTreeMap;

fn audit(
    store: &SqliteStore,
    binding: &Binding,
    id: &ObjectId,
    operation: uuid::Uuid,
    seed: CustodySeedSelection,
) -> CustodyAuditRequest {
    let events = store.task_sync_exchange_events(id).unwrap();
    CustodyAuditRequest {
        binding: id.clone(),
        operation,
        key: operation.to_string(),
        reason: CheckpointReason::NamedAudit,
        previous_checkpoint: events.iter().rev().find_map(|(id, event)| {
            matches!(event.transition, Transition::CustodyCheckpoint { .. }).then(|| id.clone())
        }),
        expected_tail: events.last().unwrap().0.clone(),
        expected_local_cut: super::outgoing::cut(store, id, binding),
        seed,
    }
}

fn intent(
    store: &SqliteStore,
    binding: &Binding,
    id: &ObjectId,
    operation: uuid::Uuid,
    source: RetainedCommit,
    packs: &BTreeMap<ObjectId, EvidencePackSelection>,
) -> EvidenceIntent {
    EvidenceIntent {
        operation,
        key: operation.to_string(),
        direction: Direction::Incoming,
        actor: super::outgoing::actor(),
        producer: CanonicalObject::freeze(&"native evidence encoder")
            .unwrap()
            .key()
            .clone(),
        expected_head: Repositories::data(binding.repository.main_checkout.parent().unwrap()).head,
        source,
        local_cut: super::outgoing::cut(store, id, binding),
        packs: packs.clone(),
    }
}

async fn apply(
    store: &mut SqliteStore,
    binding: &Binding,
    id: &ObjectId,
    request: &EvidenceIntent,
) -> anyhow::Result<crate::task_sync::ledger::CompletedExchange> {
    task_sync::exchange::apply_evidence_exchange(
        store,
        id,
        binding,
        request,
        chrono::Utc::now(),
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
}

pub(super) async fn run(root: &Path, case: &str) {
    let repos = Repositories::new(root);
    let incoming::Receiver {
        mut store,
        mut binding,
        mut id,
    } = incoming::prepare_with_profile(
        &repos,
        task_sync::progress::ExportProfile {
            include_notes: true,
            origin_machine: Some("https-data-fixture-sender".into()),
        },
    )
    .await;
    let old_binding = binding.clone();
    let old_id = id.clone();
    let code_before = (
        Repositories::code_snapshot(&repos.left),
        Repositories::code_snapshot(&repos.right),
    );
    if case == "superseded" {
        binding.supersedes = Some(id.clone());
        binding.superseded_tail = Some(
            store
                .task_sync_exchange_events(&id)
                .unwrap()
                .last()
                .unwrap()
                .0
                .clone(),
        );
        binding.created_at = chrono::Utc::now();
        id = store.activate_task_sync_binding(&binding).unwrap();
    }
    let carrier = carrier(root, &repos, &binding, case).await;
    let target = workflow::target(&repos.right).await;
    let operation = uuid::Uuid::new_v4();
    let fetched =
        workflow::fetch_operation(&repos.right, &target, &carrier.selected, operation).await;
    let before = store.task_sync_exchange_events(&id).unwrap();
    let old_before = store.task_sync_exchange_events(&old_id).unwrap();
    if case != "unrequested" {
        let request = intent(
            &store,
            &old_binding,
            &old_id,
            operation,
            fetched.clone(),
            &carrier.packs,
        );
        request.validate(&old_id, &old_binding).unwrap();
        let error = apply(&mut store, &old_binding, &old_id, &request)
            .await
            .unwrap_err();
        let error = format!("{error:#}");
        if case == "superseded" {
            assert!(error.contains("superseded"), "{error}");
        } else {
            assert!(
                error.contains("data branch must have orphan history separate from code"),
                "{error}"
            );
        }
    }
    assert_eq!(store.task_sync_exchange_events(&id).unwrap(), before);
    assert_eq!(
        store.task_sync_exchange_events(&old_id).unwrap(),
        old_before
    );
    recover(&mut store, &binding, &id, &old_id, operation, &fetched);
    assert_eq!(
        &store.task_sync_exchange_events(&id).unwrap()[..before.len()],
        before.as_slice()
    );
    assert_eq!(
        &store.task_sync_exchange_events(&old_id).unwrap()[..old_before.len()],
        old_before.as_slice()
    );
    later_exchange(root, &repos, &mut store, &binding, &id, &target, &carrier).await;
    let abandoned = intent(
        &store,
        &old_binding,
        &old_id,
        operation,
        fetched.clone(),
        &carrier.packs,
    );
    assert!(
        apply(&mut store, &old_binding, &old_id, &abandoned)
            .await
            .is_err()
    );
    assert_eq!(
        task_sync::retention::commit_selection(&binding.repository.git_directory, operation)
            .unwrap(),
        Some(fetched)
    );
    assert!(store.verify_all().unwrap().is_healthy());
    assert_eq!(
        (
            Repositories::code_snapshot(&repos.left),
            Repositories::code_snapshot(&repos.right)
        ),
        code_before
    );
    println!("data-branch-workflow complete");
}

fn recover(
    store: &mut SqliteStore,
    binding: &Binding,
    id: &ObjectId,
    old_id: &ObjectId,
    operation: uuid::Uuid,
    fetched: &RetainedCommit,
) {
    let inventory_before =
        task_sync::retention::inventory(&binding.repository.git_directory).unwrap();
    assert!(inventory_before.evidence_imports.is_empty());
    let later = audit(
        store,
        binding,
        id,
        uuid::Uuid::new_v4(),
        CustodySeedSelection::CustodyOnly,
    );
    let error = store
        .audit_task_sync_custody(&later, &binding.actor, chrono::Utc::now())
        .unwrap_err();
    assert!(
        error.to_string().contains("retained operations are ahead"),
        "{error}"
    );
    let recovery = audit(
        store,
        binding,
        id,
        operation,
        CustodySeedSelection::AbandonFetchedCommit {
            binding: old_id.clone(),
            commit: fetched.clone(),
        },
    );
    let receipt = store
        .audit_task_sync_custody(&recovery, &binding.actor, chrono::Utc::now())
        .unwrap();
    assert!(receipt.acknowledged && !receipt.repeated);
    let replay = store
        .audit_task_sync_custody(&recovery, &binding.actor, chrono::Utc::now())
        .unwrap();
    assert_eq!(receipt.checkpoint, replay.checkpoint);
    assert!(replay.repeated && replay.acknowledged);
    let events = store.task_sync_exchange_events(id).unwrap();
    let Transition::CustodyCheckpoint { checkpoint } = &events.last().unwrap().1.transition else {
        panic!("checkpoint missing");
    };
    assert!(checkpoint.seed.is_none() && checkpoint.abandoned_commit().is_some());
    assert_eq!(
        task_sync::retention::inventory(&binding.repository.git_directory).unwrap(),
        inventory_before
    );
    let later = audit(
        store,
        binding,
        id,
        later.operation,
        CustodySeedSelection::CustodyOnly,
    );
    store
        .audit_task_sync_custody(&later, &binding.actor, chrono::Utc::now())
        .unwrap();
}

struct Carrier {
    valid: RetainedCommit,
    selected: RetainedCommit,
    packs: BTreeMap<ObjectId, EvidencePackSelection>,
    originals: Vec<task_sync::evidence::RecordEnvelope>,
}

async fn carrier(root: &Path, repos: &Repositories, binding: &Binding, case: &str) -> Carrier {
    let (valid, packs, originals) = native_carrier(root, repos, binding);
    let selected = if case == "rejected" {
        let oid = line(&repos.left, &["rev-parse", "HEAD"]);
        task_sync::exchange::retain_commit(
            &repos.left.join(".engram-data"),
            uuid::Uuid::new_v4(),
            &oid,
            RunLimits::default(),
            &Cancellation::default(),
        )
        .await
        .unwrap()
    } else {
        valid.clone()
    };
    // Fixture-only publication to its private bare repository. The production
    // fetch below still runs through confined HTTPS transport.
    git(
        &repos.left,
        &[
            "push",
            root.join("data.git").to_str().unwrap(),
            &format!("{}:{}", selected.oid, fixture::BRANCH),
        ],
    );
    Carrier {
        valid,
        selected,
        packs,
        originals,
    }
}

fn native_carrier(
    root: &Path,
    repos: &Repositories,
    binding: &Binding,
) -> (
    RetainedCommit,
    BTreeMap<ObjectId, EvidencePackSelection>,
    Vec<task_sync::evidence::RecordEnvelope>,
) {
    let database = crate::project_database_path(&root.join("sender-home"), &binding.project);
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    let mut sender = SqliteStore::open(&database).unwrap();
    let native = exchange_fixture::native_sender(&mut sender, &binding.project, &binding.profile);
    let captured = exchange_fixture::capture(&mut sender, &binding.project, &native.request);
    let page = sender
        .export_native_evidence_capture(&binding.project, &captured.cursor, 32, &binding.profile)
        .unwrap();
    assert!(page.next.is_none() && !page.packs.is_empty());
    let originals = page
        .packs
        .iter()
        .flat_map(|pack| pack.records().iter().cloned())
        .collect();
    let mut tree = workflow::empty_tree();
    tree.project.notes_included = binding.profile.include_notes;
    task_sync::directory::write_folder(
        &repos.left.join(".engram-data/.engram"),
        &tree,
        &binding.profile,
    )
    .unwrap();
    let data = Repositories::data(&repos.left);
    let packs = data
        .stage_evidence_packs(&binding.project, &page.packs)
        .unwrap()
        .into_iter()
        .map(|file| {
            (
                file.pack,
                EvidencePackSelection {
                    publishing_origin: file.publishing_origin,
                    admission: ObjectId::mint(),
                    content_fingerprint: file.content_fingerprint,
                    encoded_bytes: file.encoded_bytes,
                    records: file.records,
                },
            )
        })
        .collect();
    let valid = Repositories::commit(
        &repos.left,
        uuid::Uuid::new_v4(),
        Vec::new(),
        "Publish captured native evidence",
    );
    (valid, packs, originals)
}

async fn later_exchange(
    root: &Path,
    repos: &Repositories,
    store: &mut SqliteStore,
    binding: &Binding,
    id: &ObjectId,
    target: &ExchangeTarget,
    carrier: &Carrier,
) {
    // A fresh actual exchange now admits the valid carrier. The rejected
    // carrier and its original operation remain intact and cannot be reused.
    git(
        &repos.left,
        &[
            "push",
            root.join("data.git").to_str().unwrap(),
            &format!("{}:refs/heads/valid-carrier", carrier.valid.oid),
        ],
    );
    let next_operation = uuid::Uuid::new_v4();
    let next =
        workflow::fetch_operation(&repos.right, target, &carrier.valid, next_operation).await;
    let request = intent(store, binding, id, next_operation, next, &carrier.packs);
    let native_before = exchange_fixture::native_counts(store);
    let completed = apply(store, binding, id, &request).await.unwrap();
    assert_eq!(
        apply(store, binding, id, &request).await.unwrap(),
        completed
    );
    assert_eq!(exchange_fixture::native_counts(store), native_before);
    for record in &carrier.originals {
        assert_eq!(
            exchange_fixture::object(store, record.object_id()),
            Some((
                record.kind().as_str().to_owned(),
                record.canonical_bytes().to_vec()
            ))
        );
        assert!(exchange_fixture::imported_only(store, record.object_id()));
    }
}
