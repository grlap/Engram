use super::*;

pub(super) struct Prepared {
    pub(super) store: SqliteStore,
    pub(super) data: task_sync::data_worktree::DataWorktree,
    pub(super) target: ExchangeTarget,
    pub(super) id: ObjectId,
    pub(super) binding: Binding,
    pub(super) intent: DeltaIntent,
    pub(super) selection: DeltaCommitSelection,
    pub(super) tree: task_sync::TaskTree,
}

pub(super) async fn prepare(repos: &Repositories) -> Prepared {
    let project = ProjectId("https-data-fixture".into());
    let database = crate::project_database_path(&repos.left.join("target/home"), &project);
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    let mut store = SqliteStore::open(&database).unwrap();
    let origin = store
        .enroll_task_sync_origin(
            &project,
            &actor(),
            &task_sync::origin::EnrollRequest {
                operation: uuid::Uuid::new_v4(),
                predecessor: None,
            },
            chrono::Utc::now(),
        )
        .unwrap();
    let data = Repositories::data(&repos.left);
    let target = ExchangeTarget::capture(
        &TargetSelection {
            repository: data.checkout.clone(),
            remote: "origin".into(),
            reference: fixture::BRANCH.into(),
        },
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    let profile = ExportProfile {
        include_notes: false,
        origin_machine: None,
    };
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
        repository: RepositoryBasis {
            enrollment: uuid::Uuid::new_v4(),
            main_checkout: data.checkout.clone(),
            git_directory: data.git_directory.clone(),
        },
        branch: fixture::BRANCH.into(),
        target: TargetBasis {
            remote: "origin".into(),
            reference: fixture::BRANCH.into(),
            configuration_fingerprint: target.fingerprint().unwrap(),
        },
        profile: profile.clone(),
        supersedes: None,
        superseded_tail: None,
        actor: actor(),
        created_at: chrono::Utc::now(),
    };
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let tail = store
        .task_sync_exchange_events(&id)
        .unwrap()
        .last()
        .unwrap()
        .0
        .clone();
    let operation = uuid::Uuid::new_v4();
    let audit = store
        .audit_task_sync_custody(
            &CustodyAuditRequest {
                binding: id.clone(),
                operation,
                key: operation.to_string(),
                reason: CheckpointReason::Bootstrap,
                previous_checkpoint: None,
                expected_tail: tail,
                expected_local_cut: cut(&store, &id, &binding),
                seed: CustodySeedSelection::LocalExport,
            },
            &actor(),
            chrono::Utc::now(),
        )
        .unwrap();
    let events = store.task_sync_exchange_events(&id).unwrap();
    let Transition::CustodyCheckpoint { checkpoint } = &events.last().unwrap().1.transition else {
        panic!("bootstrap checkpoint missing")
    };
    let Some(CheckpointSeed::PreparedExport { membership, .. }) = &checkpoint.seed else {
        panic!("prepared export seed missing")
    };
    assert_eq!(membership.inventory_root, InventoryRoot::ZERO);
    let before = PreparedExportBasis {
        anchor: PreparedExportAnchor::Bootstrap {
            checkpoint: audit.checkpoint,
        },
        membership: membership.clone(),
        cut: checkpoint.local_cut.clone(),
    };
    let task = create(&mut store, &project, "selected-native-task");
    let selected = BTreeSet::from([task.work_id]);
    let captured = store
        .capture_task_documents(&project, &profile, selected.clone())
        .unwrap();
    let documents = captured
        .documents
        .into_iter()
        .map(|(id, task)| {
            let CapturedTask::Present(task) = task else {
                panic!("native task missing")
            };
            (id, Some(*task))
        })
        .collect::<BTreeMap<_, _>>();
    let output = OutputMembership {
        inventory_root: documents
            .values()
            .flatten()
            .try_fold(InventoryRoot::ZERO, InventoryRoot::including)
            .unwrap(),
        profile: profile.clone(),
    };
    let operation = uuid::Uuid::new_v4();
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
        direction: Direction::Outgoing,
        actor: actor(),
        producer: CanonicalObject::freeze(&(operation, "observed fixture producer"))
            .unwrap()
            .key()
            .clone(),
        expected_head: None,
        common_commit: None,
        local_cut: cut(&store, &id, &binding),
        basis: DeltaBasis::Outgoing {
            export_before: Box::new(before),
            confirmed_before: None,
        },
        candidate: RetainedDeltaCandidate {
            operation,
            directory: task_sync::retention::delta::candidate_directory(
                &data.git_directory,
                operation,
            ),
            content_fingerprint: CanonicalObject::freeze(&payload).unwrap().key().clone(),
            output,
            changed: selected,
        },
    };
    let prepared = store
        .prepare_task_sync_delta_export(&id, &intent, chrono::Utc::now())
        .unwrap();
    let tree = store
        .export_task_sync_folder(&project, &profile)
        .unwrap()
        .tree;
    task_sync::directory::write_folder(&data.checkout.join(".engram"), &tree, &profile).unwrap();
    let commit = Repositories::commit(
        &repos.left,
        operation,
        Vec::new(),
        "Retain selected strict delta",
    );
    let selection = DeltaCommitSelection {
        prepared,
        commit: commit.clone(),
    };
    exchange::record_delta_export_commit(
        &mut store,
        &id,
        &binding,
        &intent,
        &selection,
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    Prepared {
        store,
        data,
        target,
        id,
        binding,
        intent,
        selection,
        tree,
    }
}

pub(in super::super) async fn incoming_carrier(
    repos: &Repositories,
) -> (
    task_sync::ledger::RetainedCommit,
    task_sync::ledger::RetainedCommit,
) {
    let prepared = Box::pin(prepare(repos)).await;
    prepared
        .data
        .advance_retained(&prepared.selection.commit, None)
        .unwrap();
    let workflow = &repos.right;
    super::super::workflow::stage(workflow, &super::super::workflow::empty_tree());
    let before = Repositories::commit(
        workflow,
        uuid::Uuid::new_v4(),
        Vec::new(),
        "Seed incoming data baseline",
    );
    Repositories::data(workflow)
        .advance_retained(&before, None)
        .unwrap();
    let target = super::super::workflow::target(workflow).await;
    super::super::workflow::transport(
        &target,
        ExchangeOperation::Push {
            commit: before.oid.clone(),
        },
    )
    .await
    .unwrap();
    super::super::workflow::fetch(&repos.left, &prepared.target, &before).await;
    let after = Repositories::commit(
        &repos.left,
        uuid::Uuid::new_v4(),
        vec![prepared.selection.commit.oid, before.oid.clone()],
        "Carry captured native task text to the receiving store",
    );
    super::super::workflow::transport(
        &prepared.target,
        ExchangeOperation::Push {
            commit: after.oid.clone(),
        },
    )
    .await
    .unwrap();
    (before, after)
}
