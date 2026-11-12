//! Real carriers and native source movement exercise explicit retirement.

use super::*;
use crate::task_sync::{
    data_worktree::DataWorktree,
    delta::{
        DELTA_PAYLOAD_FORMAT, DeltaBasis, DeltaCandidatePayload, DeltaIntent,
        DestinationPlanningBasis, IncomingSourceAnchor, IncomingSourceBasis,
        RetainedConstructionKind, RetainedDeltaCandidate,
    },
    evidence::{EvidencePack, RecordEnvelope, RecordKind},
    exchange::{self, Cancellation, RunLimits, evidence::InspectedEvidence},
    ledger::{EvidenceIntent, EvidencePackSelection},
};
use std::collections::BTreeMap;

async fn incoming_delta(
    store: &mut SqliteStore,
    binding: &Binding,
    id: &ObjectId,
    source_checkpoint: &ObjectId,
    title: &str,
) -> (
    crate::task_sync::exchange::delta::InspectedDelta,
    crate::WorkId,
) {
    let event: Event =
        SqliteStore::get_typed_object_on(&store.connection, source_checkpoint, EVENT_KIND)
            .unwrap()
            .unwrap();
    let Transition::CustodyCheckpoint { checkpoint } = event.transition else {
        panic!("source checkpoint missing")
    };
    let Some(CheckpointSeed::Incoming { snapshot, .. }) = checkpoint.seed else {
        panic!("incoming bootstrap seed missing")
    };
    let data = DataWorktree::from_binding(binding).unwrap();
    let mut tree = crate::task_sync::directory::read_folder(&data.checkout.join(".engram"))
        .unwrap()
        .tree;
    let mut task = tree
        .tasks
        .iter()
        .find(|task| task.lifecycle == crate::WorkLifecycle::Open && task.parent_id.is_none())
        .unwrap()
        .clone();
    let selected = task.id;
    task.title = title.into();
    let operation = uuid::Uuid::new_v4();
    *tree
        .tasks
        .iter_mut()
        .find(|entry| entry.id == selected)
        .unwrap() = task.clone();
    // Use the folder writer's envelope producer so the edited task and its
    // inventory fingerprint describe the same committed source snapshot.
    let (documents, _) = crate::task_sync::directory::prepare_folder_documents(
        &tree.folder_documents().unwrap(),
        &binding.profile,
    )
    .unwrap();
    for (path, bytes) in documents {
        std::fs::write(data.checkout.join(".engram").join(path), bytes).unwrap();
    }
    data_bootstrap::git(&data.checkout, &["add", ".engram"]);
    data_bootstrap::git(
        &data.checkout,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "Publish current planning selection",
        ],
    );
    let data = DataWorktree::from_binding(binding).unwrap();
    let source = exchange::retain_commit(
        &data.checkout,
        operation,
        data.head.as_ref().unwrap(),
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    let observed = crate::task_sync::git::read_task_delta(
        &data.checkout,
        &binding.project,
        &snapshot.commit.oid,
        &source.oid,
    )
    .unwrap();
    assert_eq!(observed.upserts.get(&selected), Some(&task));
    assert!(observed.removed.is_empty());
    let original = store
        .capture_task_sync_membership_component(
            id,
            crate::task_sync::TaskSyncMembershipView::Source,
            [selected],
        )
        .unwrap();
    let output = OutputMembership {
        inventory_root: snapshot
            .membership
            .inventory_root
            .excluding(&original.component.documents[&selected])
            .unwrap()
            .including(&task)
            .unwrap(),
        profile: binding.profile.clone(),
    };
    let payload = DeltaCandidatePayload {
        format: DELTA_PAYLOAD_FORMAT.into(),
        project_id: binding.project.clone(),
        operation,
        output: output.clone(),
        documents: BTreeMap::from([(selected, Some(task))]),
    };
    let baseline =
        crate::storage::work::task_sync::baseline::head(&store.connection, &binding.project)
            .unwrap()
            .unwrap();
    let cut = producing_origin::cut_on(&store.connection, &binding.project).unwrap();
    let intent = DeltaIntent {
        operation,
        key: operation.to_string(),
        direction: Direction::Incoming,
        actor: binding.actor.clone(),
        producer: CanonicalObject::freeze(&"retirement planning reader")
            .unwrap()
            .key()
            .clone(),
        expected_head: Some(source.oid.clone()),
        common_commit: None,
        local_cut: OriginQualifiedCut {
            binding: id.clone(),
            origin: binding.store.origin.clone().unwrap(),
            restore_occurrence: binding.store.restore_occurrence,
            position: FeedPosition {
                feed: FeedId::Project(binding.project.clone()),
                position: cut.position,
            },
            anchor: cut.anchor,
        },
        basis: DeltaBasis::Incoming {
            source_before: Box::new(IncomingSourceBasis {
                anchor: IncomingSourceAnchor::Bootstrap {
                    checkpoint: source_checkpoint.clone(),
                },
                snapshot: *snapshot,
            }),
            source_after: Box::new(MembershipSnapshot {
                commit: source,
                membership: output.clone(),
            }),
            source_changed: [selected].into_iter().collect(),
            local_before: Box::new(DestinationPlanningBasis {
                baseline: baseline.record,
                membership: OutputMembership {
                    inventory_root: InventoryRoot::parse(&baseline.inventory_root).unwrap(),
                    profile: binding.profile.clone(),
                },
            }),
        },
        candidate: RetainedDeltaCandidate {
            operation,
            directory: retention::delta::candidate_directory(
                &binding.repository.git_directory,
                operation,
            ),
            content_fingerprint: CanonicalObject::freeze(&payload).unwrap().key().clone(),
            output,
            changed: [selected].into_iter().collect(),
        },
    };
    let input = crate::task_sync::exchange::delta::InspectedDelta {
        binding_id: id.clone(),
        binding: binding.clone(),
        intent,
        payload,
    };
    store
        .retain_task_sync_incoming_delta(&input, at(60))
        .unwrap();
    (input, selected)
}

#[tokio::test]
async fn retained_incoming_delta_after_unclaimed_native_edit_is_retired_without_losing_its_carrier()
{
    let (_home, mut store, binding, id, bootstrap) = data_bootstrap::fixture().await;
    let bootstrap = store
        .audit_task_sync_custody(&bootstrap, &binding.actor, at(30))
        .unwrap();
    let destination = store
        .export_task_sync_folder(&binding.project, &binding.profile)
        .unwrap();
    store
        .apply_task_sync_folder(
            crate::storage::TaskSyncFolderApply {
                project: &binding.project,
                actor: &binding.actor,
                tree: &destination.tree,
                export_basis: &destination.export_basis,
                profile: &binding.profile,
                complete: true,
                now: at(31),
            },
            "retirement-destination-baseline",
        )
        .unwrap();
    let (input, selected) = incoming_delta(
        &mut store,
        &binding,
        &id,
        &bootstrap.checkpoint,
        "First incoming planning title",
    )
    .await;
    let item = store.get_work_item(selected).unwrap();
    store
        .revise_work(
            &crate::ReviseWorkRequest {
                work_id: selected,
                expected_revision: item.revision,
                patch: crate::WorkRevisionPatch {
                    title: Some("Native title after retention".into()),
                    ..Default::default()
                },
                authority: crate::WorkPlanningAuthority::Project,
                actor: binding.actor.clone(),
                idempotency_key: "native-after-retention".into(),
                updated_at: at(70),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let before = state(&store);
    assert!(
        store
            .apply_task_sync_delta_exchange(&input, at(80))
            .is_err()
    );
    assert_eq!(state(&store), before);
    let observed = super::super::super::audit::retained::ObservedConstruction::capture(
        &binding.repository.git_directory,
        &retention::inventory(&binding.repository.git_directory).unwrap(),
        input.intent.operation,
    )
    .unwrap();
    let mut request = audit_request(
        &store,
        &binding,
        &id,
        CustodySeedSelection::AbandonRetainedConstruction {
            binding: id.clone(),
            family: RetainedConstructionKind::Delta,
            construction_fingerprint: observed.fingerprint().unwrap(),
        },
    );
    request.operation = input.intent.operation;
    request.key = input.intent.key.clone();
    store
        .audit_task_sync_custody(&request, &binding.actor, at(90))
        .unwrap();
    assert_eq!(
        store.get_work_item(selected).unwrap().title,
        "Native title after retention"
    );
    assert!(
        store
            .apply_task_sync_delta_exchange(&input, at(91))
            .unwrap_err()
            .to_string()
            .contains("retired")
    );
    assert_eq!(
        retention::delta::read_candidate(&id, &binding, &input.intent).unwrap(),
        Some(input.payload.clone())
    );
    let (fresh, _) = incoming_delta(
        &mut store,
        &binding,
        &id,
        &bootstrap.checkpoint,
        "Fresh current planning title",
    )
    .await;
    store
        .apply_task_sync_delta_exchange(&fresh, at(100))
        .unwrap();
    assert_eq!(
        store.get_work_item(selected).unwrap().title,
        "Fresh current planning title"
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

async fn evidence_input(
    store: &mut SqliteStore,
    binding: &Binding,
    id: &ObjectId,
) -> InspectedEvidence {
    let origin = ObjectId::mint();
    let enrollment = crate::task_sync::origin::Enrollment {
        project_id: binding.project.clone(),
        predecessor_origin_id: None,
        enrollment_operation_id: uuid::Uuid::new_v4(),
        actor: binding.actor.clone(),
        enrolled_at: at(40),
    };
    let record = RecordEnvelope::from_canonical(
        origin.clone(),
        origin.clone(),
        RecordKind::ProducingOrigin.as_str(),
        crate::canonical::canonical_bytes(&enrollment).unwrap(),
    )
    .unwrap();
    let pack = EvidencePack::prepare(
        ObjectId::mint(),
        binding.project.clone(),
        origin,
        vec![record],
    )
    .unwrap();
    let data = DataWorktree::from_binding(binding).unwrap();
    let files = data
        .stage_evidence_packs(&binding.project, std::slice::from_ref(&pack))
        .unwrap();
    data_bootstrap::git(&data.checkout, &["add", ".engram"]);
    data_bootstrap::git(
        &data.checkout,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "Publish custody recovery evidence",
        ],
    );
    let data = DataWorktree::from_binding(binding).unwrap();
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
        producer: CanonicalObject::freeze(&"custody recovery evidence reader")
            .unwrap()
            .key()
            .clone(),
        expected_head: Some(source.oid.clone()),
        source,
        local_cut: OriginQualifiedCut {
            binding: id.clone(),
            origin: binding.store.origin.clone().unwrap(),
            restore_occurrence: binding.store.restore_occurrence,
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
        .retain_task_sync_evidence_recipe(id, binding, &intent, at(70))
        .unwrap();
    let batch = crate::task_sync::git::read_selected_evidence(
        id,
        binding,
        &intent,
        std::time::Duration::from_secs(60),
    )
    .unwrap();
    InspectedEvidence {
        binding_id: id.clone(),
        binding: binding.clone(),
        recipe,
        packs: batch.packs,
    }
}

fn retirement_request(store: &SqliteStore, input: &InspectedEvidence) -> CustodyAuditRequest {
    let intent = input.recipe.intent().unwrap();
    let mut request = audit_request(
        store,
        &input.binding,
        &input.binding_id,
        CustodySeedSelection::AbandonRetainedConstruction {
            binding: input.binding_id.clone(),
            family: RetainedConstructionKind::Evidence,
            construction_fingerprint: retention::evidence::request_fingerprint(&input.recipe)
                .unwrap(),
        },
    );
    request.operation = intent.operation;
    request.key = intent.key.clone();
    request
}

#[tokio::test]
async fn retained_evidence_after_native_movement_is_audited_then_fresh_work_succeeds() {
    let (_home, mut store, binding, id, bootstrap) = data_bootstrap::fixture().await;
    store
        .audit_task_sync_custody(&bootstrap, &binding.actor, at(30))
        .unwrap();
    let input = evidence_input(&mut store, &binding, &id).await;
    let original = input.recipe.clone();
    let carrier = original.intent().unwrap().source.clone();
    let recipe_path = binding
        .repository
        .git_directory
        .join("engram-task-sync-evidence-imports")
        .join(format!("{}.json", original.intent().unwrap().operation));
    let original_bytes = std::fs::read(&recipe_path).unwrap();
    unrelated(&mut store, &binding.project);
    // Unrelated native work does not stale an inert evidence recipe. An
    // explicit retirement may still account for that unapplied construction.
    let planning_before = store.export_task_tree(&binding.project).unwrap();
    let request = retirement_request(&store, &input);
    let receipt = store
        .audit_task_sync_custody(&request, &binding.actor, at(90))
        .unwrap();
    assert!(receipt.acknowledged && !receipt.repeated);
    assert_eq!(
        store.export_task_tree(&binding.project).unwrap(),
        planning_before
    );
    assert_eq!(
        retention::evidence::read_recipe(&id, &binding, input.recipe.intent().unwrap()).unwrap(),
        Some(original.clone())
    );
    assert_eq!(
        retention::commit_selection(&binding.repository.git_directory, request.operation).unwrap(),
        Some(carrier.clone())
    );
    assert_eq!(std::fs::read(&recipe_path).unwrap(), original_bytes);
    assert_eq!(
        data_bootstrap::git(
            &binding.repository.main_checkout,
            &["rev-parse", &carrier.retention_ref]
        ),
        carrier.oid
    );
    let after = state(&store);
    let error = store.apply_task_sync_evidence_import(&input).unwrap_err();
    assert!(error.to_string().contains("retired"), "{error}");
    assert_eq!(state(&store), after);
    let repeated = store
        .audit_task_sync_custody(&request, &binding.actor, at(91))
        .unwrap();
    assert_eq!(repeated.checkpoint, receipt.checkpoint);
    assert!(repeated.repeated);
    let fresh = evidence_input(&mut store, &binding, &id).await;
    store.apply_task_sync_evidence_import(&fresh).unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
    drop(store);
    SqliteStore::repair_rebuildable_projections(&binding.store.database).unwrap();
    let mut store = SqliteStore::open(&binding.store.database).unwrap();
    assert!(
        store
            .apply_task_sync_evidence_import(&input)
            .unwrap_err()
            .to_string()
            .contains("retired")
    );
    assert_eq!(
        store
            .audit_task_sync_custody(&request, &binding.actor, at(95))
            .unwrap()
            .checkpoint,
        receipt.checkpoint
    );
    assert_eq!(
        retention::evidence::read_recipe(&id, &binding, original.intent().unwrap()).unwrap(),
        Some(original)
    );
    assert_eq!(
        retention::commit_selection(&binding.repository.git_directory, request.operation).unwrap(),
        Some(carrier)
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[tokio::test]
async fn canonical_evidence_operation_cannot_be_retired_as_a_private_construction() {
    let (_home, mut store, binding, id, bootstrap) = data_bootstrap::fixture().await;
    store
        .audit_task_sync_custody(&bootstrap, &binding.actor, at(30))
        .unwrap();
    let input = evidence_input(&mut store, &binding, &id).await;
    store.apply_task_sync_evidence_import(&input).unwrap();
    let request = retirement_request(&store, &input);
    let before = state(&store);
    let files = retention::inventory(&binding.repository.git_directory).unwrap();
    assert!(
        store
            .audit_task_sync_custody(&request, &binding.actor, at(90))
            .is_err()
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        retention::inventory(&binding.repository.git_directory).unwrap(),
        files
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[tokio::test]
async fn changed_retained_recipe_is_preserved_and_refused_by_the_original_audit_selection() {
    let (_home, mut store, binding, id, bootstrap) = data_bootstrap::fixture().await;
    store
        .audit_task_sync_custody(&bootstrap, &binding.actor, at(30))
        .unwrap();
    let input = evidence_input(&mut store, &binding, &id).await;
    unrelated(&mut store, &binding.project);
    let request = retirement_request(&store, &input);
    let path = binding
        .repository
        .git_directory
        .join("engram-task-sync-evidence-imports")
        .join(format!("{}.json", request.operation));
    let original = std::fs::read(&path).unwrap();
    let mut changed = input.recipe.clone();
    changed.outcome_id = ObjectId::mint();
    let changed_bytes = crate::canonical::canonical_bytes(&changed).unwrap();
    std::fs::write(&path, &changed_bytes).unwrap();
    let before = state(&store);
    assert!(
        store
            .audit_task_sync_custody(&request, &binding.actor, at(90))
            .is_err()
    );
    assert_eq!(state(&store), before);
    assert_eq!(std::fs::read(&path).unwrap(), changed_bytes);
    std::fs::write(&path, original).unwrap();
    store
        .audit_task_sync_custody(&request, &binding.actor, at(91))
        .unwrap();
}

#[tokio::test]
async fn construction_audit_refuses_wrong_selection_and_preserves_all_retained_artifacts() {
    let (_home, mut store, binding, id, bootstrap) = data_bootstrap::fixture().await;
    store
        .audit_task_sync_custody(&bootstrap, &binding.actor, at(30))
        .unwrap();
    let input = evidence_input(&mut store, &binding, &id).await;
    unrelated(&mut store, &binding.project);
    let request = retirement_request(&store, &input);
    let before = state(&store);
    let files = retention::inventory(&binding.repository.git_directory).unwrap();
    let mut bad_key = request.clone();
    bad_key.key.push_str("-different");
    let mut bad_operation = request.clone();
    bad_operation.operation = uuid::Uuid::new_v4();
    let mut bad_family = request.clone();
    if let CustodySeedSelection::AbandonRetainedConstruction { family, .. } = &mut bad_family.seed {
        *family = RetainedConstructionKind::Successor;
    }
    let mut bad_bytes = request.clone();
    if let CustodySeedSelection::AbandonRetainedConstruction {
        construction_fingerprint,
        ..
    } = &mut bad_bytes.seed
    {
        *construction_fingerprint = CanonicalObject::freeze(&"different retained construction")
            .unwrap()
            .key()
            .clone();
    }
    let mut bad_binding = request.clone();
    if let CustodySeedSelection::AbandonRetainedConstruction { binding, .. } = &mut bad_binding.seed
    {
        *binding = ObjectId::mint();
    }
    let mut bad_origin = request.clone();
    bad_origin.expected_local_cut.origin = ObjectId::mint();
    let mut bad_restore = request.clone();
    bad_restore.expected_local_cut.restore_occurrence = Some(uuid::Uuid::new_v4());
    for changed in [
        bad_key,
        bad_operation,
        bad_family,
        bad_bytes,
        bad_binding,
        bad_origin,
        bad_restore,
    ] {
        assert!(
            store
                .audit_task_sync_custody(&changed, &binding.actor, at(90))
                .is_err()
        );
        assert_eq!(state(&store), before);
        assert_eq!(
            retention::inventory(&binding.repository.git_directory).unwrap(),
            files
        );
    }
    store
        .audit_task_sync_custody(&request, &binding.actor, at(91))
        .unwrap();
}

#[tokio::test]
async fn construction_audit_observes_git_before_any_sql_writer_and_preserves_other_uncertainty() {
    let (_home, mut store, binding, id, bootstrap) = data_bootstrap::fixture().await;
    store
        .audit_task_sync_custody(&bootstrap, &binding.actor, at(30))
        .unwrap();
    let input = evidence_input(&mut store, &binding, &id).await;
    unrelated(&mut store, &binding.project);
    let request = retirement_request(&store, &input);
    let database = binding.store.database.clone();
    let queries = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = queries.clone();
    let previous = crate::task_sync::git::BEFORE_QUERY.replace(Some(Box::new(move |_| {
        let other = rusqlite::Connection::open(&database).unwrap();
        other.busy_timeout(std::time::Duration::ZERO).unwrap();
        other
            .execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
            .expect("Git observation held the first SQLite writer");
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    })));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        store.audit_task_sync_custody(&request, &binding.actor, at(90))
    }));
    crate::task_sync::git::BEFORE_QUERY.replace(previous);
    result.unwrap().unwrap();
    assert!(queries.load(std::sync::atomic::Ordering::SeqCst) > 0);
    let next = evidence_input(&mut store, &binding, &id).await;
    let unknown = uuid::Uuid::new_v4();
    exchange::retain_commit(
        &binding.repository.main_checkout,
        unknown,
        &next.recipe.intent().unwrap().source.oid,
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    let next_request = retirement_request(&store, &next);
    let before = state(&store);
    let files = retention::inventory(&binding.repository.git_directory).unwrap();
    assert!(
        store
            .audit_task_sync_custody(&next_request, &binding.actor, at(100))
            .is_err()
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        retention::inventory(&binding.repository.git_directory).unwrap(),
        files
    );
}

#[tokio::test]
async fn construction_retirement_recovers_only_its_exact_audit_after_ack_failures() {
    for stage in ["repository_ack", "origin_ack", "private_cleanup"] {
        let (_home, mut store, binding, id, bootstrap) = data_bootstrap::fixture().await;
        store
            .audit_task_sync_custody(&bootstrap, &binding.actor, at(30))
            .unwrap();
        let input = evidence_input(&mut store, &binding, &id).await;
        unrelated(&mut store, &binding.project);
        let request = retirement_request(&store, &input);
        super::super::super::audit::FAIL_STAGE.with(|failure| failure.set(Some(stage)));
        assert!(
            store
                .audit_task_sync_custody(&request, &binding.actor, at(90))
                .is_err()
        );
        let receipt = store
            .audit_task_sync_custody(&request, &binding.actor, at(91))
            .unwrap();
        assert!(receipt.repeated && receipt.acknowledged);
        assert!(
            store
                .apply_task_sync_evidence_import(&input)
                .unwrap_err()
                .to_string()
                .contains("retired")
        );
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[tokio::test]
async fn retirement_migration_preserves_canonical_bytes_without_the_repository() {
    let (home, mut store, binding, id, bootstrap) = data_bootstrap::fixture().await;
    store
        .audit_task_sync_custody(&bootstrap, &binding.actor, at(30))
        .unwrap();
    let input = evidence_input(&mut store, &binding, &id).await;
    unrelated(&mut store, &binding.project);
    let request = retirement_request(&store, &input);
    store
        .audit_task_sync_custody(&request, &binding.actor, at(90))
        .unwrap();
    let canonical = |store: &SqliteStore| {
        let mut query = store
            .connection
            .prepare("SELECT object_id,object_kind,canonical_json FROM objects ORDER BY object_id")
            .unwrap();
        query
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    let original = canonical(&store);
    let file = home.path().join("retirement-export.jsonl");
    let destination = home.path().join("retirement-import.db");
    crate::storage::migration::export_json(&binding.store.database, &file).unwrap();
    let git = binding.repository.git_directory.canonicalize().unwrap();
    let fixture = home.path().canonicalize().unwrap();
    let hidden = git.with_file_name("retirement-git-unavailable");
    assert!(git.starts_with(&fixture) && hidden.starts_with(&fixture));
    assert!(
        !std::fs::symlink_metadata(&git)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(!hidden.exists());
    std::fs::rename(&git, &hidden).unwrap();
    let previous = crate::task_sync::git::BEFORE_QUERY
        .replace(Some(Box::new(|_| panic!("migration queried Git custody"))));
    let imported = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::storage::migration::import_json(&file, &destination)
    }));
    crate::task_sync::git::BEFORE_QUERY.replace(previous);
    std::fs::rename(&hidden, &git).unwrap();
    imported.unwrap().unwrap();
    let migrated = SqliteStore::open(&destination).unwrap();
    assert_eq!(canonical(&migrated), original);
    assert!(migrated.verify_all().unwrap().is_healthy());
    assert!(store.verify_all().unwrap().is_healthy());
}

#[tokio::test]
async fn retired_operation_and_key_cannot_be_recycled_after_binding_succession() {
    let (_home, mut store, binding, id, bootstrap) = data_bootstrap::fixture().await;
    store
        .audit_task_sync_custody(&bootstrap, &binding.actor, at(30))
        .unwrap();
    let input = evidence_input(&mut store, &binding, &id).await;
    unrelated(&mut store, &binding.project);
    let request = retirement_request(&store, &input);
    let retired = store
        .audit_task_sync_custody(&request, &binding.actor, at(90))
        .unwrap();
    let mut successor = binding.clone();
    successor.supersedes = Some(id.clone());
    successor.superseded_tail = Some(retired.checkpoint.clone());
    successor.created_at = at(100);
    successor.store.cut = producing_origin::cut_on(&store.connection, &binding.project).unwrap();
    let next = store.activate_task_sync_binding(&successor).unwrap();
    let before = state(&store);
    for (operation, key) in [
        (request.operation, request.key.clone()),
        (request.operation, "different retry key".into()),
        (uuid::Uuid::new_v4(), request.key.clone()),
    ] {
        let error =
            super::super::super::view::read::request_by(&store.connection, &next, operation, &key)
                .unwrap_err();
        assert!(error.to_string().contains("retired"), "{error}");
    }
    assert_eq!(state(&store), before);
    assert_eq!(
        store
            .audit_task_sync_custody(&request, &binding.actor, at(101))
            .unwrap()
            .checkpoint,
        retired.checkpoint
    );
    assert!(store.verify_all().unwrap().is_healthy());
}
