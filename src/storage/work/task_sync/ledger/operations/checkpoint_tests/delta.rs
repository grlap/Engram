//! Actual incoming SQL writer fixtures. Only the private Git observation is
//! substituted; enrollment, audit, retained bytes and graph admission are real.

use super::*;
use crate::storage::work::task_sync::closure::component::MEMBER_DECODES;
use crate::task_sync::{delta::*, exchange::delta::InspectedDelta, origin::private, retention};
use std::collections::BTreeMap;

mod deferred;
mod historical;
mod recovery;
mod settled;

/// Fields drop in declaration order: the store closes its database before the
/// temporary home is removed, so Windows never meets an open file on cleanup.
struct Fixture {
    store: SqliteStore,
    input: InspectedDelta,
    selected: crate::WorkId,
    home: crate::test_support::TempHome,
}

impl Fixture {
    fn new() -> Self {
        Self::with_changes(&candidate(), |tree| {
            let mut task = tree
                .tasks
                .iter()
                .find(|task| {
                    task.lifecycle == crate::WorkLifecycle::Open && task.parent_id.is_none()
                })
                .unwrap()
                .clone();
            task.title = "Incoming planning title".into();
            BTreeMap::from([(task.id, Some(task))])
        })
    }

    fn with_changes(
        tree: &crate::task_sync::TaskTree,
        changes: impl FnOnce(
            &crate::task_sync::TaskTree,
        ) -> BTreeMap<crate::WorkId, Option<crate::task_sync::TaskDocument>>,
    ) -> Self {
        Self::with_setup(tree, changes, |_, _| {})
    }

    fn with_setup(
        tree: &crate::task_sync::TaskTree,
        changes: impl FnOnce(
            &crate::task_sync::TaskTree,
        ) -> BTreeMap<crate::WorkId, Option<crate::task_sync::TaskDocument>>,
        setup: impl FnOnce(&mut SqliteStore, crate::WorkId),
    ) -> Self {
        Self::with_repository(tree, changes, setup, |_, _, _| {})
    }

    /// `repository` may select another repository for the binding before it is
    /// activated, such as a real data worktree.
    fn with_repository(
        tree: &crate::task_sync::TaskTree,
        changes: impl FnOnce(
            &crate::task_sync::TaskTree,
        ) -> BTreeMap<crate::WorkId, Option<crate::task_sync::TaskDocument>>,
        setup: impl FnOnce(&mut SqliteStore, crate::WorkId),
        repository: impl FnOnce(&std::path::Path, &crate::task_sync::TaskTree, &mut Binding),
    ) -> Self {
        let (home, mut store, mut binding, enrollment) =
            super::super::origin_tests::unenrolled_fixture();
        let empty = store
            .export_task_sync_folder(&binding.project, &binding.profile)
            .unwrap();
        store
            .apply_task_sync_folder(
                crate::storage::TaskSyncFolderApply {
                    project: &binding.project,
                    actor: &binding.actor,
                    tree,
                    export_basis: &empty.export_basis,
                    profile: &binding.profile,
                    complete: true,
                    now: at(0),
                },
                "initial-folder",
            )
            .unwrap();
        let origin = store
            .enroll_task_sync_origin(&binding.project, &binding.actor, &enrollment, at(1))
            .unwrap();
        binding.store.origin = Some(origin.origin_id);
        binding.store.cut = producing_origin::cut_on(&store.connection, &binding.project).unwrap();
        repository(home.path(), tree, &mut binding);
        let id = store.activate_task_sync_binding(&binding).unwrap();
        let initial = super::super::tests::input(&store, &id, &binding, tree.clone());
        store.prepare_task_sync_exchange(&initial, at(6)).unwrap();
        store
            .apply_task_sync_exchange(&initial, false, at(7))
            .unwrap();
        let original = store
            .completed_task_sync_exchange(&CompletedExchangeRequest {
                binding: id.clone(),
                intent: initial.intent,
            })
            .unwrap()
            .unwrap();
        let request = audit_request(
            &store,
            &binding,
            &id,
            CustodySeedSelection::InitialImport {
                outcome: original.outcome,
            },
        );
        let audited = store
            .audit_task_sync_custody(&request, &binding.actor, at(30))
            .unwrap();
        let event: Event =
            SqliteStore::get_typed_object_on(&store.connection, &audited.checkpoint, EVENT_KIND)
                .unwrap()
                .unwrap();
        let Transition::CustodyCheckpoint { checkpoint } = event.transition else {
            panic!("checkpoint expected")
        };
        let Some(CheckpointSeed::Incoming { snapshot, .. }) = checkpoint.seed else {
            panic!("incoming seed expected")
        };
        let changes = changes(tree);
        let selected = *changes.keys().next().unwrap();
        setup(&mut store, selected);
        let mut inventory_root = snapshot.membership.inventory_root;
        for (id, next) in &changes {
            if let Some(old) = tree.tasks.iter().find(|task| task.id == *id) {
                inventory_root = inventory_root.excluding(old).unwrap();
            }
            if let Some(next) = next {
                inventory_root = inventory_root.including(next).unwrap();
            }
        }
        let output = OutputMembership {
            inventory_root,
            profile: binding.profile.clone(),
        };
        let operation = uuid::Uuid::new_v4();
        let payload = DeltaCandidatePayload {
            format: DELTA_PAYLOAD_FORMAT.into(),
            project_id: binding.project.clone(),
            operation,
            output: output.clone(),
            documents: changes.clone(),
        };
        let fingerprint = CanonicalObject::freeze(&operation).unwrap().key().clone();
        let commit = crate::task_sync::ledger::RetainedCommit {
            oid: fingerprint.as_str()[..snapshot.commit.oid.len()].into(),
            tree: fingerprint.as_str()[..snapshot.commit.oid.len()].into(),
            parents: vec![snapshot.commit.oid.clone()],
            retention_ref: format!("refs/engram/task-sync/exchange/{operation}/commit"),
            raw_bytes_fingerprint: fingerprint.clone(),
        };
        let head =
            crate::storage::work::task_sync::baseline::head(&store.connection, &binding.project)
                .unwrap()
                .unwrap();
        let cut = producing_origin::cut_on(&store.connection, &binding.project).unwrap();
        let intent = DeltaIntent {
            operation,
            key: operation.to_string(),
            direction: Direction::Incoming,
            actor: binding.actor.clone(),
            producer: fingerprint,
            expected_head: None,
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
                        checkpoint: audited.checkpoint,
                    },
                    snapshot: *snapshot,
                }),
                source_after: Box::new(MembershipSnapshot {
                    commit: commit.clone(),
                    membership: output.clone(),
                }),
                source_changed: changes.keys().copied().collect(),
                local_before: Box::new(DestinationPlanningBasis {
                    baseline: head.record,
                    membership: OutputMembership {
                        inventory_root: InventoryRoot::parse(&head.inventory_root).unwrap(),
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
                changed: changes.keys().copied().collect(),
            },
        };
        payload.validate(&id, &binding, &intent).unwrap();
        retention::delta::retain_candidate(&id, &binding, &intent, &payload).unwrap();
        let commits = binding
            .repository
            .git_directory
            .join("engram-task-sync-commits");
        std::fs::create_dir_all(&commits).unwrap();
        std::fs::write(
            commits.join(format!("{operation}.json")),
            crate::canonical::canonical_bytes(&commit).unwrap(),
        )
        .unwrap();
        Self {
            home,
            store,
            selected,
            input: InspectedDelta {
                binding_id: id,
                binding,
                intent,
                payload,
            },
        }
    }

    fn apply(&mut self) -> Result<crate::task_sync::ledger::CompletedExchange, StoreError> {
        self.store
            .apply_task_sync_delta_exchange(&self.input, at(40))
    }
}

#[tokio::test]
async fn atomic_delta_applies_receipt_and_replays_before_git_after_later_local_work() {
    let mut f = Fixture::new();
    let original = f.apply().unwrap();
    let Transition::DeltaImportApplied { prepared, receipt } = &original.event.transition else {
        panic!("applied outcome expected")
    };
    assert_eq!(prepared, &original.prepared);
    assert_eq!(receipt.tasks.len(), 1);
    assert_eq!(receipt.tasks[0].task, f.selected);
    assert_ne!(receipt.tasks[0].local_record_ids, Vec::<ObjectId>::new());
    assert_ne!(
        receipt.tasks[0].previous_head,
        receipt.tasks[0].imported_head
    );
    assert_eq!(
        f.store.get_work_item(f.selected).unwrap().title,
        "Incoming planning title"
    );
    let witness = private::read(&f.input.binding.store.database, &f.input.binding.project)
        .unwrap()
        .witness
        .unwrap();
    assert!(witness.pending_exchange.is_none());
    assert_eq!(witness.exchange.as_ref().unwrap().head, original.outcome);
    assert!(f.store.verify_all().unwrap().is_healthy());
    f.store
        .create_work(
            &crate::storage::work::test_support::root_request(
                &f.input.binding.project.0,
                "later disconnected planning",
                50,
            ),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    std::fs::rename(
        &f.input.binding.repository.git_directory,
        f.home.path().join("offline-git"),
    )
    .unwrap();
    let before = state(&f.store);
    let replay = crate::task_sync::exchange::apply_delta_exchange(
        &mut f.store,
        &f.input.binding_id,
        &f.input.binding,
        &f.input.intent,
        at(60),
        crate::task_sync::exchange::RunLimits::default(),
        &crate::task_sync::exchange::Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(replay, original);
    assert_eq!(state(&f.store), before);
    drop(f.store);
    SqliteStore::repair_rebuildable_projections(&f.input.binding.store.database).unwrap();
    let store = SqliteStore::open(&f.input.binding.store.database).unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
    assert_eq!(
        store
            .completed_task_sync_delta_import(&f.input.binding_id, &f.input.intent)
            .unwrap(),
        Some(original)
    );
}

#[test]
fn atomic_delta_final_outcome_failure_rolls_back_native_planning_and_preparation() {
    let mut f = Fixture::new();
    f.store.connection.execute_batch("CREATE TEMP TRIGGER refuse_delta BEFORE INSERT ON objects
        WHEN NEW.object_kind='task_sync_exchange_event' AND json_type(NEW.canonical_json,'$.transition.receipt.planning')='text'
        BEGIN SELECT RAISE(ABORT,'injected delta outcome failure'); END;").unwrap();
    let before = state(&f.store);
    let custody = retention::inventory(&f.input.binding.repository.git_directory).unwrap();
    let private_before =
        private::read(&f.input.binding.store.database, &f.input.binding.project).unwrap();
    let error = f.apply().unwrap_err();
    assert!(
        error.to_string().contains("injected delta outcome failure"),
        "{error}"
    );
    assert_eq!(state(&f.store), before);
    assert_eq!(
        retention::inventory(&f.input.binding.repository.git_directory).unwrap(),
        custody
    );
    assert_eq!(
        private::read(&f.input.binding.store.database, &f.input.binding.project).unwrap(),
        private_before
    );
    f.store
        .connection
        .execute_batch("DROP TRIGGER refuse_delta")
        .unwrap();
    let applied = f.apply().unwrap();
    assert_eq!(
        f.store
            .completed_task_sync_delta_import(&f.input.binding_id, &f.input.intent)
            .unwrap(),
        Some(applied)
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn atomic_delta_retained_candidate_survives_unrelated_feed_movement() {
    let mut f = Fixture::new();
    f.store
        .create_work(
            &crate::storage::work::test_support::root_request(
                &f.input.binding.project.0,
                "intervening local planning",
                35,
            ),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let retained = retention::inventory(&f.input.binding.repository.git_directory).unwrap();
    let applied = f.apply().unwrap();
    assert_eq!(f.apply().unwrap(), applied);
    assert_eq!(
        retention::inventory(&f.input.binding.repository.git_directory).unwrap(),
        retained
    );
    super::audit_healthy(&mut f.store, &f.input.binding, &f.input.binding_id);
}

#[test]
fn atomic_delta_shared_restore_owner_refuses_pending_unreadable_and_new_occurrences() {
    use crate::backup::{
        restore::{self, RestoreRecord, RestoreSource, RestoreState},
        target::RECORD_FORMAT_VERSION,
    };
    let mut f = Fixture::new();
    let path = restore::restore_record_path(f.home.path(), &f.input.binding.project);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let before = state(&f.store);
    let private_before =
        private::read(&f.input.binding.store.database, &f.input.binding.project).unwrap();
    let record = RestoreRecord {
        format_version: RECORD_FORMAT_VERSION,
        occurrence_id: uuid::Uuid::new_v4(),
        project: f.input.binding.project.0.clone(),
        source: RestoreSource::LocalFile {
            path: "historical-acquisition.db".into(),
            replace: true,
        },
        sha256: CanonicalObject::freeze(&uuid::Uuid::new_v4())
            .unwrap()
            .key()
            .to_string(),
        staging: "historical-staging".into(),
        state: RestoreState::Completed,
        pending_at: at(1),
        completed_at: Some(at(2)),
        previous_completed: None,
    };
    for bytes in [
        b"unreadable restore".to_vec(),
        serde_json::to_vec(&RestoreRecord {
            state: RestoreState::Pending,
            completed_at: None,
            ..record.clone()
        })
        .unwrap(),
        serde_json::to_vec(&record).unwrap(),
    ] {
        std::fs::write(&path, &bytes).unwrap();
        let error = f.apply().unwrap_err();
        assert!(error.to_string().contains("restore occurrence"), "{error}");
        assert_eq!(state(&f.store), before);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            private::read(&f.input.binding.store.database, &f.input.binding.project).unwrap(),
            private_before
        );
    }
}

#[test]
fn atomic_delta_exact_recovery_acknowledges_committed_request_without_sql_changes() {
    let mut f = Fixture::new();
    let mut reserved = private::read(&f.input.binding.store.database, &f.input.binding.project)
        .unwrap()
        .witness
        .unwrap();
    reserved.pending_exchange =
        Some(retention::delta::request_fingerprint(&f.input.binding_id, &f.input.intent).unwrap());
    let original = f.apply().unwrap();
    let completed = private::read(&f.input.binding.store.database, &f.input.binding.project)
        .unwrap()
        .witness
        .unwrap();
    // Inject the exact private bytes from immediately before the SQL commit,
    // as the existing E1 recovery regression does. All SQL and custody are real.
    private::advance(&f.input.binding.store.database, &completed, &reserved).unwrap();
    let before = state(&f.store);
    assert_eq!(
        f.store
            .completed_task_sync_delta_import(&f.input.binding_id, &f.input.intent)
            .unwrap(),
        Some(original.clone())
    );
    assert_eq!(
        private::read(&f.input.binding.store.database, &f.input.binding.project)
            .unwrap()
            .witness,
        Some(reserved)
    );
    assert_eq!(
        f.store
            .recover_task_sync_delta_import(&f.input.binding_id, &f.input.intent)
            .unwrap(),
        original
    );
    assert_eq!(state(&f.store), before);
    assert_eq!(
        private::read(&f.input.binding.store.database, &f.input.binding.project)
            .unwrap()
            .witness,
        Some(completed.clone())
    );
    assert_eq!(
        f.store
            .recover_task_sync_delta_import(&f.input.binding_id, &f.input.intent)
            .unwrap(),
        original
    );
    assert_eq!(state(&f.store), before);
    assert_eq!(
        private::read(&f.input.binding.store.database, &f.input.binding.project)
            .unwrap()
            .witness,
        Some(completed)
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn atomic_delta_recovery_preserves_wrong_missing_and_restore_changed_reservations() {
    let mut f = Fixture::new();
    let before_uncommitted = state(&f.store);
    let private_uncommitted =
        private::read(&f.input.binding.store.database, &f.input.binding.project).unwrap();
    assert!(
        f.store
            .recover_task_sync_delta_import(&f.input.binding_id, &f.input.intent)
            .is_err()
    );
    assert_eq!(state(&f.store), before_uncommitted);
    assert_eq!(
        private::read(&f.input.binding.store.database, &f.input.binding.project).unwrap(),
        private_uncommitted
    );
    let mut reserved = private_uncommitted.witness.unwrap();
    reserved.pending_exchange =
        Some(retention::delta::request_fingerprint(&f.input.binding_id, &f.input.intent).unwrap());
    let original = f.apply().unwrap();
    let mut current = private::read(&f.input.binding.store.database, &f.input.binding.project)
        .unwrap()
        .witness
        .unwrap();
    let before = state(&f.store);
    for pending in [Some(ObjectId::mint()), None] {
        let mut invalid = reserved.clone();
        invalid.pending_exchange = pending;
        private::advance(&f.input.binding.store.database, &current, &invalid).unwrap();
        current = invalid;
        assert!(
            f.store
                .recover_task_sync_delta_import(&f.input.binding_id, &f.input.intent)
                .is_err()
        );
        assert_eq!(state(&f.store), before);
        assert_eq!(
            private::read(&f.input.binding.store.database, &f.input.binding.project)
                .unwrap()
                .witness
                .as_ref(),
            Some(&current)
        );
    }
    private::advance(&f.input.binding.store.database, &current, &reserved).unwrap();
    let mut changed = f.input.intent.clone();
    changed.producer = CanonicalObject::freeze(&"another valid producer")
        .unwrap()
        .key()
        .clone();
    assert!(
        f.store
            .recover_task_sync_delta_import(&f.input.binding_id, &changed)
            .is_err()
    );
    assert_eq!(state(&f.store), before);
    assert_eq!(
        private::read(&f.input.binding.store.database, &f.input.binding.project)
            .unwrap()
            .witness,
        Some(reserved.clone())
    );
    let path = crate::backup::restore::restore_record_path(f.home.path(), &f.input.binding.project);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"unreadable restore occurrence").unwrap();
    let error = f
        .store
        .recover_task_sync_delta_import(&f.input.binding_id, &f.input.intent)
        .unwrap_err();
    assert!(error.to_string().contains("restore occurrence"), "{error}");
    assert_eq!(state(&f.store), before);
    assert_eq!(
        private::read(&f.input.binding.store.database, &f.input.binding.project)
            .unwrap()
            .witness,
        Some(reserved)
    );
    assert_eq!(
        f.store
            .completed_task_sync_delta_import(&f.input.binding_id, &f.input.intent)
            .unwrap(),
        Some(original)
    );
}

fn independent_tree(count: usize) -> crate::task_sync::TaskTree {
    let mut tree = candidate();
    let mut template = tree
        .tasks
        .iter()
        .find(|task| task.lifecycle == crate::WorkLifecycle::Open && task.parent_id.is_none())
        .unwrap()
        .clone();
    template.external_ref = None;
    template.parent_id = None;
    template.prerequisites.clear();
    template.blockers.clear();
    template.superseded_by = None;
    template.notes.clear();
    template.activity.clear();
    tree.tasks = (0..count)
        .map(|index| {
            let mut task = template.clone();
            task.id = crate::WorkId::new();
            task.title = format!("Disconnected task {index}");
            task
        })
        .collect();
    tree.tasks.sort_by_key(|task| task.id);
    tree
}

#[test]
fn atomic_delta_selected_edit_addition_and_removal_keep_disconnected_components_unread() {
    let tree = independent_tree(2_000);
    let changed = tree.tasks[0].id;
    let removed = tree.tasks[1].id;
    let unchanged = tree.tasks[2].clone();
    let mut addition = tree.tasks[0].clone();
    addition.id = crate::WorkId::new();
    addition.title = "Incoming added task".into();
    let added = addition.id;
    let mut f = Fixture::with_changes(&tree, |tree| {
        let mut task = tree.tasks[0].clone();
        task.title = "Incoming selected edit".into();
        BTreeMap::from([
            (changed, Some(task)),
            (removed, None),
            (added, Some(addition)),
        ])
    });
    MEMBER_DECODES.with(|count| count.set(0));
    let result = f.apply().unwrap();
    let component_reads = MEMBER_DECODES.with(std::cell::Cell::get);
    assert!(
        component_reads <= 12,
        "connected component member decodes: {component_reads}"
    );
    let Transition::DeltaImportApplied { receipt, .. } = result.event.transition else {
        panic!("applied outcome expected")
    };
    assert_eq!(receipt.tasks.len(), 3);
    let added_receipt = receipt
        .tasks
        .iter()
        .find(|task| task.task == added)
        .unwrap();
    assert_eq!(added_receipt.previous_head, None);
    assert!(added_receipt.imported_head.is_some());
    assert_eq!(added_receipt.local_record_ids.len(), 2);
    let removed_receipt = receipt
        .tasks
        .iter()
        .find(|task| task.task == removed)
        .unwrap();
    assert!(removed_receipt.imported_head.is_some());
    assert_eq!(
        f.store.get_work_item(removed).unwrap().lifecycle,
        crate::WorkLifecycle::Cancelled
    );
    assert_eq!(
        f.store.get_work_item(changed).unwrap().title,
        "Incoming selected edit"
    );
    assert_eq!(
        f.store.get_work_item(unchanged.id).unwrap().title,
        unchanged.title
    );
    let source = f
        .store
        .capture_task_sync_membership_component(
            &f.input.binding_id,
            crate::task_sync::TaskSyncMembershipView::Source,
            [changed, removed, added],
        )
        .unwrap();
    assert_eq!(source.membership, f.input.intent.candidate.output);
    assert!(source.component.documents.contains_key(&added));
    assert!(!source.component.documents.contains_key(&removed));
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn atomic_delta_two_thousand_changed_members_commit_with_exact_receipts() {
    let tree = independent_tree(2_000);
    let mut f = Fixture::with_changes(&tree, |tree| {
        tree.tasks
            .iter()
            .map(|task| {
                let mut next = task.clone();
                next.title = format!("Incoming {}", task.title);
                (task.id, Some(next))
            })
            .collect()
    });
    let result = f.apply().unwrap();
    let Transition::DeltaImportApplied { receipt, .. } = &result.event.transition else {
        panic!("applied outcome expected")
    };
    assert_eq!(receipt.tasks.len(), 2_000);
    assert!(
        receipt
            .tasks
            .iter()
            .all(|task| task.imported_head.is_some() && task.previous_head != task.imported_head)
    );
    for task in &tree.tasks {
        assert_eq!(
            f.store.get_work_item(task.id).unwrap().title,
            format!("Incoming {}", task.title)
        );
    }
    assert_eq!(
        f.store
            .completed_task_sync_delta_import(&f.input.binding_id, &f.input.intent)
            .unwrap(),
        Some(result)
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn atomic_delta_retained_incoming_edge_refuses_removal_without_sql_effects() {
    let tree = candidate();
    let referenced = tree
        .tasks
        .iter()
        .find(|task| task.lifecycle == crate::WorkLifecycle::Open && task.parent_id.is_none())
        .unwrap()
        .id;
    let mut f = Fixture::with_changes(&tree, |_| BTreeMap::from([(referenced, None)]));
    let before = state(&f.store);
    let private_before =
        private::read(&f.input.binding.store.database, &f.input.binding.project).unwrap();
    assert!(f.apply().is_err());
    assert_eq!(state(&f.store), before);
    assert_eq!(
        private::read(&f.input.binding.store.database, &f.input.binding.project).unwrap(),
        private_before
    );
    assert_eq!(
        f.store
            .completed_task_sync_delta_import(&f.input.binding_id, &f.input.intent)
            .unwrap(),
        None
    );
}
