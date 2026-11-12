//! Incoming half, enabled only with the checked explicit data-commit bootstrap.

use super::*;
use crate::{
    CanonicalObject, ProjectId,
    storage::{SqliteStore, TaskSyncFolderApply},
    task_sync::{
        self, TaskSyncGraphView,
        delta::*,
        driver::{DeltaRequest, StepReceipt, StepRequest},
        exchange,
        inventory::InventoryRoot,
        ledger::{Binding, Direction, RetainedCommit, StoreBasis, TargetBasis, Transition},
        progress::{ExportCut, ExportProfile},
    },
};
use fixture::Repositories;
use std::collections::BTreeMap;

async fn public_step(
    binding: &Binding,
    request: &task_sync::driver::StepRequest,
) -> anyhow::Result<task_sync::driver::StepReceipt> {
    let _phase = super::timing::phase(match request {
        StepRequest::RetainIncomingDelta { .. } => "public-retain",
        StepRequest::ApplyDelta { .. } => "public-apply",
        _ => "public-step",
    });
    let service = crate::LocalWorkService::new(
        binding.store.database.clone(),
        binding.project.clone(),
        binding.actor.actor_id.clone(),
        binding.actor.session_id.clone().unwrap(),
        None,
    );
    service
        .task_sync_settings(
            &task_sync::settings::Request {
                action: task_sync::settings::Action::Enable,
                user_instruction: Some(
                    "Enable explicit exchange in the disposable two-clone fixture.".into(),
                ),
            },
            chrono::Utc::now(),
        )
        .unwrap();
    let wire = serde_json::to_vec(request).unwrap();
    let decoded = serde_json::from_slice(&wire).unwrap();
    service
        .task_sync_exchange_step(&decoded, RunLimits::default(), &Cancellation::default())
        .await
}

async fn assert_retention_admission_refusals(
    store: &SqliteStore,
    binding: &Binding,
    request: &DeltaRequest,
    inspection: &rusqlite::Connection,
) {
    let id = &request.binding;
    let intent = &request.intent;
    let before_retention = store.task_sync_exchange_events(id).unwrap();
    for fault in ["source", "profile", "actor"] {
        let mut altered = request.clone();
        match fault {
            "source" => {
                let DeltaBasis::Incoming { source_after, .. } = &mut altered.intent.basis else {
                    unreachable!()
                };
                source_after.commit.raw_bytes_fingerprint =
                    CanonicalObject::freeze(&"another commit body")
                        .unwrap()
                        .key()
                        .clone();
            }
            "profile" => altered.intent.candidate.output.profile.include_notes = true,
            _ => altered.intent.actor.session_id = Some(crate::SessionId("another-holder".into())),
        }
        let before = crate::storage::test_database_shape_snapshot(inspection).unwrap();
        assert!(
            public_step(
                binding,
                &StepRequest::RetainIncomingDelta {
                    request: Box::new(altered)
                }
            )
            .await
            .is_err(),
            "{fault}"
        );
        assert!(!intent.candidate.directory.exists(), "{fault}");
        assert_eq!(
            store.task_sync_exchange_events(id).unwrap(),
            before_retention
        );
        // The service's first validly attributed invocation may register its
        // own session. It must never change planning or retain a bad candidate.
        if fault == "actor" {
            assert_eq!(
                crate::storage::test_database_shape_snapshot(inspection).unwrap(),
                before
            );
        }
    }
}

pub(super) struct Receiver {
    pub(super) store: SqliteStore,
    pub(super) binding: Binding,
    pub(super) id: crate::ObjectId,
}

pub(super) async fn prepare(repos: &Repositories) -> Receiver {
    prepare_with_profile(
        repos,
        ExportProfile {
            include_notes: false,
            origin_machine: None,
        },
    )
    .await
}

pub(super) async fn prepare_with_profile(repos: &Repositories, profile: ExportProfile) -> Receiver {
    let project = ProjectId("https-data-fixture".into());
    let data = Repositories::data(&repos.right);
    let target = super::workflow::target(&repos.right).await;
    let mut initial = super::workflow::empty_tree();
    initial.project.notes_included = profile.include_notes;
    let database = crate::project_database_path(&repos.right.join("target/home"), &project);
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    let mut store = SqliteStore::open(&database).unwrap();
    let empty = store.export_task_sync_folder(&project, &profile).unwrap();
    store
        .apply_task_sync_folder(
            TaskSyncFolderApply {
                project: &project,
                actor: &super::outgoing::actor(),
                tree: &initial,
                export_basis: &empty.export_basis,
                profile: &profile,
                complete: true,
                now: chrono::Utc::now(),
            },
            "explicit-observed-folder-bootstrap",
        )
        .unwrap();
    let origin = store
        .enroll_task_sync_origin(
            &project,
            &super::outgoing::actor(),
            &task_sync::origin::EnrollRequest {
                operation: uuid::Uuid::new_v4(),
                predecessor: None,
            },
            chrono::Utc::now(),
        )
        .unwrap();
    let binding = Binding {
        schema_version: 1,
        project: project.clone(),
        store: StoreBasis {
            database,
            origin: Some(origin.origin_id),
            restore_occurrence: None,
            cut: store
                .capture_task_changes(
                    &project,
                    &ExportCut {
                        position: 0,
                        anchor: None,
                    },
                )
                .unwrap()
                .cut,
        },
        repository: data.repository_basis(uuid::Uuid::new_v4()),
        branch: fixture::BRANCH.into(),
        target: TargetBasis {
            remote: "origin".into(),
            reference: fixture::BRANCH.into(),
            configuration_fingerprint: target.fingerprint().unwrap(),
        },
        profile: profile.clone(),
        supersedes: None,
        superseded_tail: None,
        actor: super::outgoing::actor(),
        created_at: chrono::Utc::now(),
    };
    let id = store.activate_task_sync_binding(&binding).unwrap();
    Receiver { store, binding, id }
}

pub(super) async fn exercise(
    repos: &Repositories,
    before: &RetainedCommit,
    after: &RetainedCommit,
    receiver: Receiver,
) {
    let Receiver {
        mut store,
        binding,
        id,
    } = receiver;
    let project = binding.project.clone();
    let profile = binding.profile.clone();
    let data = Repositories::data(&repos.right);
    let target = super::workflow::target(&repos.right).await;
    let original_snapshot =
        task_sync::git::read_task_snapshot(&data.checkout, &project, &before.oid).unwrap();
    assert_eq!(original_snapshot.tree, super::workflow::empty_tree());
    // Activation preceded the competing commit's retention. This exact
    // explicit audit now admits that operation without hiding unowned facts.
    let operation = before
        .retention_ref
        .strip_prefix("refs/engram/task-sync/exchange/")
        .and_then(|text| text.strip_suffix("/commit"))
        .unwrap()
        .parse::<uuid::Uuid>()
        .unwrap();
    let tail = store
        .task_sync_exchange_events(&id)
        .unwrap()
        .last()
        .unwrap()
        .0
        .clone();
    let local_cut = super::outgoing::cut(&store, &id, &binding);
    let retained_before =
        super::workflow::fetch_operation(&repos.right, &target, before, operation).await;
    let request = CustodyAuditRequest {
        binding: id.clone(),
        operation,
        key: operation.to_string(),
        reason: CheckpointReason::Bootstrap,
        previous_checkpoint: None,
        expected_tail: tail,
        expected_local_cut: local_cut,
        seed: CustodySeedSelection::IncomingDataCommit {
            commit: retained_before,
        },
    };
    let audited = exchange::audit_incoming_data_commit(
        &mut store,
        &request,
        &binding.actor,
        chrono::Utc::now(),
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        exchange::audit_incoming_data_commit(
            &mut store,
            &request,
            &binding.actor,
            chrono::Utc::now(),
            RunLimits::default(),
            &Cancellation::default()
        )
        .await
        .unwrap()
        .checkpoint,
        audited.checkpoint
    );
    let events = store.task_sync_exchange_events(&id).unwrap();
    assert!(
        !events
            .iter()
            .any(|(_, event)| matches!(event.transition, Transition::Prepared { .. })),
        "data bootstrap must need no invented E1 preparation"
    );
    let Transition::CustodyCheckpoint { checkpoint } = &events.last().unwrap().1.transition else {
        panic!("incoming checkpoint missing")
    };
    let Some(CheckpointSeed::Incoming { snapshot, .. }) = &checkpoint.seed else {
        panic!("incoming snapshot missing")
    };
    let source_before = IncomingSourceBasis {
        anchor: IncomingSourceAnchor::Bootstrap {
            checkpoint: audited.checkpoint,
        },
        snapshot: *snapshot.clone(),
    };
    let operation = uuid::Uuid::new_v4();
    let retained_after =
        super::workflow::fetch_operation(&repos.right, &target, after, operation).await;
    let selected =
        task_sync::git::read_task_snapshot(&data.checkout, &project, &retained_after.oid).unwrap();
    let delta =
        task_sync::git::read_task_delta(&data.checkout, &project, &before.oid, &after.oid).unwrap();
    let documents = delta
        .upserts
        .into_iter()
        .map(|(id, task)| (id, Some(task)))
        .chain(delta.removed.into_iter().map(|id| (id, None)))
        .collect::<BTreeMap<_, _>>();
    let output = OutputMembership {
        inventory_root: selected
            .tree
            .tasks
            .iter()
            .try_fold(InventoryRoot::ZERO, InventoryRoot::including)
            .unwrap(),
        profile: profile.clone(),
    };
    let component = store
        .capture_task_sync_graph_component(
            &project,
            TaskSyncGraphView::Baseline,
            documents.keys().copied(),
        )
        .unwrap();
    let payload = DeltaCandidatePayload {
        format: DELTA_PAYLOAD_FORMAT.into(),
        project_id: project.clone(),
        operation,
        output: output.clone(),
        documents,
    };
    let intent = DeltaIntent {
        operation,
        key: operation.to_string(),
        direction: Direction::Incoming,
        actor: binding.actor.clone(),
        producer: CanonicalObject::freeze(&(operation, "observed incoming fixture"))
            .unwrap()
            .key()
            .clone(),
        expected_head: data.head.clone(),
        common_commit: None,
        local_cut: super::outgoing::cut(&store, &id, &binding),
        basis: DeltaBasis::Incoming {
            source_before: Box::new(source_before),
            source_after: Box::new(MembershipSnapshot {
                commit: retained_after,
                membership: output.clone(),
            }),
            source_changed: payload.documents.keys().copied().collect(),
            local_before: Box::new(DestinationPlanningBasis {
                baseline: component.baseline.unwrap(),
                membership: OutputMembership {
                    inventory_root: InventoryRoot::ZERO,
                    profile,
                },
            }),
        },
        candidate: RetainedDeltaCandidate {
            operation,
            directory: task_sync::retention::delta::candidate_directory(
                &data.git_directory,
                operation,
            ),
            content_fingerprint: CanonicalObject::freeze(&payload).unwrap().key().clone(),
            output,
            changed: payload.documents.keys().copied().collect(),
        },
    };
    let request = DeltaRequest {
        binding: id.clone(),
        intent: intent.clone(),
    };
    let retain = StepRequest::RetainIncomingDelta {
        request: Box::new(request.clone()),
    };
    let before_retention = store.task_sync_exchange_events(&id).unwrap();
    let inspection = rusqlite::Connection::open_with_flags(
        &binding.store.database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    Box::pin(assert_retention_admission_refusals(
        &store,
        &binding,
        &request,
        &inspection,
    ))
    .await;
    for _ in 0..2 {
        let StepReceipt::DeltaRetained { candidate } =
            public_step(&binding, &retain).await.unwrap()
        else {
            panic!("incoming candidate receipt missing");
        };
        assert_eq!(*candidate, intent.candidate);
        assert_eq!(
            task_sync::retention::delta::read_candidate(&id, &binding, &intent).unwrap(),
            Some(payload.clone())
        );
        assert_eq!(
            store.task_sync_exchange_events(&id).unwrap(),
            before_retention
        );
    }
    let mut changed_retry = request.clone();
    changed_retry.intent.key.push_str("-changed");
    let before = crate::storage::test_database_shape_snapshot(&inspection).unwrap();
    assert!(
        public_step(
            &binding,
            &StepRequest::RetainIncomingDelta {
                request: Box::new(changed_retry)
            }
        )
        .await
        .is_err()
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&inspection).unwrap(),
        before
    );
    assert_eq!(
        task_sync::retention::delta::read_candidate(&id, &binding, &intent).unwrap(),
        Some(payload)
    );
    let apply = StepRequest::ApplyDelta {
        request: Box::new(request),
    };
    let StepReceipt::Outcome {
        outcome: Some(applied),
    } = public_step(&binding, &apply).await.unwrap()
    else {
        panic!("incoming outcome missing");
    };
    let original = store
        .completed_task_sync_delta_import(&id, &intent)
        .unwrap()
        .unwrap();
    assert_eq!(applied.prepared, original.prepared);
    assert_eq!(applied.outcome, original.outcome);
    let actual = store
        .export_task_sync_folder(&project, &binding.profile)
        .unwrap();
    assert_eq!(actual.tree, selected.tree);
    assert!(store.verify_all().unwrap().is_healthy());
    super::outgoing::create(&mut store, &project, "later-incoming-local-work");
    let events = store.task_sync_exchange_events(&id).unwrap();
    let hidden_candidate = data.git_directory.join("fixture-hidden-incoming-candidate");
    fs::rename(&intent.candidate.directory, &hidden_candidate).unwrap();
    let hidden_data = repos.right.join("hidden-data-checkout");
    fs::rename(&data.checkout, &hidden_data).unwrap();
    let launches = exchange::test_support::LAUNCHES.with(std::cell::Cell::get);
    let StepReceipt::Outcome {
        outcome: Some(replayed),
    } = public_step(&binding, &apply).await.unwrap()
    else {
        panic!("incoming replay outcome missing");
    };
    assert_eq!(replayed, applied);
    let cancelled = Cancellation::default();
    cancelled.cancel();
    assert_eq!(
        exchange::apply_delta_exchange(
            &mut store,
            &id,
            &binding,
            &intent,
            chrono::Utc::now(),
            RunLimits::default(),
            &cancelled
        )
        .await
        .unwrap(),
        original
    );
    assert_eq!(
        exchange::test_support::LAUNCHES.with(std::cell::Cell::get),
        launches
    );
    assert_eq!(store.task_sync_exchange_events(&id).unwrap(), events);
    fs::rename(hidden_data, &data.checkout).unwrap();
    fs::rename(hidden_candidate, &intent.candidate.directory).unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
}
