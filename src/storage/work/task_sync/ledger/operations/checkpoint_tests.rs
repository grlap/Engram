use super::*;

#[cfg(windows)]
mod audit_deadlines;
#[cfg(windows)]
mod data_bootstrap;
mod delta;
mod evidence;
mod export;
mod managed;
#[cfg(windows)]
mod orphan;
#[cfg(windows)]
mod retained_construction;
mod store_binding;
#[cfg(windows)]
mod writer_probe;
use crate::{
    FeedId, FeedPosition,
    storage::work::{
        task_sync::{
            producing_origin,
            tests::{candidate, state},
        },
        test_support::at,
    },
    task_sync::{
        delta::{
            BootstrapMembers, CheckpointReason, CheckpointSeed, CustodyAuditRequest,
            CustodySeedSelection, ManagedCustodyCheckpoint, MembershipSnapshot, OriginQualifiedCut,
            OutputMembership,
        },
        inventory::InventoryRoot,
        ledger::CompletedExchangeRequest,
    },
};

fn unrelated(store: &mut SqliteStore, project: &crate::ProjectId) {
    store
        .create_work(
            &crate::storage::work::test_support::root_request(
                &project.0,
                "Unrelated local work",
                80,
            ),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
}

fn audit_healthy(store: &mut SqliteStore, binding: &Binding, id: &ObjectId) {
    let request = audit_request(store, binding, id, CustodySeedSelection::CustodyOnly);
    store
        .audit_task_sync_custody(&request, &binding.actor, at(100))
        .unwrap();
    let integrity = store.verify_all().unwrap();
    assert!(integrity.is_healthy(), "{integrity:#?}");
}

fn initial_fixture() -> (
    crate::test_support::TempHome,
    SqliteStore,
    Binding,
    crate::task_sync::TaskTree,
) {
    let (home, mut store, mut binding, request) = super::origin_tests::unenrolled_fixture();
    let tree = candidate();
    // The legacy plain import may seed only a project whose feed has not begun.
    // Enrollment follows it; the exchange records an exact-repeat receipt.
    store
        .import_task_tree(&binding.project, &binding.actor, &tree, false, at(0))
        .unwrap();
    let origin = store
        .enroll_task_sync_origin(&binding.project, &binding.actor, &request, at(1))
        .unwrap();
    binding.store.origin = Some(origin.origin_id);
    binding.store.cut = producing_origin::cut_on(&store.connection, &binding.project).unwrap();
    (home, store, binding, tree)
}

fn checkpoint(
    store: &SqliteStore,
    binding: &Binding,
    id: &ObjectId,
    previous: Option<ObjectId>,
    seed: Option<CheckpointSeed>,
) -> ManagedCustodyCheckpoint {
    let cut = producing_origin::cut_on(&store.connection, &binding.project).unwrap();
    let local_cut = OriginQualifiedCut {
        binding: id.clone(),
        origin: binding.store.origin.clone().unwrap(),
        restore_occurrence: binding.store.restore_occurrence,
        position: FeedPosition {
            feed: FeedId::Project(binding.project.clone()),
            position: cut.position,
        },
        anchor: cut.anchor,
    };
    let operation = uuid::Uuid::new_v4();
    let tail = super::super::view::read::tail_on(
        &store.connection,
        id,
        binding,
        &mut super::super::view::read::ReadBudget::default(),
    )
    .unwrap();
    let request = CustodyAuditRequest {
        binding: id.clone(),
        operation,
        key: operation.to_string(),
        previous_checkpoint: previous.clone(),
        expected_tail: tail.id,
        expected_local_cut: local_cut.clone(),
        seed: CustodySeedSelection::CustodyOnly,
        reason: CheckpointReason::NamedAudit,
    };
    ManagedCustodyCheckpoint {
        operation,
        key: request.key.clone(),
        request_fingerprint: request.fingerprint().unwrap(),
        previous_checkpoint: previous,
        local_cut,
        reason: CheckpointReason::NamedAudit,
        seed,
        artifacts: Vec::new(),
    }
}

fn append_checkpoint(
    store: &mut SqliteStore,
    binding: &Binding,
    id: &ObjectId,
    checkpoint: ManagedCustodyCheckpoint,
) -> Result<ObjectId, StoreError> {
    let tx = store.begin_work_mutation()?;
    let mut ledger = Ledger::load(&tx)?;
    let (previous, tail) = ledger
        .events
        .iter()
        .filter(|(_, event)| &event.binding == id)
        .max_by_key(|(_, event)| event.sequence)
        .unwrap();
    let event = Event {
        schema_version: 1,
        binding: id.clone(),
        sequence: tail.sequence + 1,
        predecessor: Some(previous.clone()),
        actor: binding.actor.clone(),
        recorded_at: at(30),
        transition: Transition::CustodyCheckpoint {
            checkpoint: Box::new(checkpoint),
        },
    };
    let object = CanonicalObject::mint(&event)?;
    store_event(&tx, &mut ledger, &object)?;
    tx.commit()?;
    Ok(object.key().clone())
}

#[test]
fn custody_checkpoint_preserves_completed_receipt_and_operation_rows() {
    let (_home, mut store, binding, tree) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let input = super::tests::input(&store, &id, &binding, tree);
    store.prepare_task_sync_exchange(&input, at(6)).unwrap();
    store
        .apply_task_sync_exchange(&input, false, at(7))
        .unwrap();
    let request = CompletedExchangeRequest {
        binding: id.clone(),
        intent: input.intent.clone(),
    };
    let original = store
        .completed_task_sync_exchange(&request)
        .unwrap()
        .unwrap();
    let rows_before: String = store.connection.query_row(
        "SELECT group_concat(prepared_id || ':' || latest_event_id || ':' || completed_event_id)
         FROM work_task_sync_exchange_operations", [], |row| row.get(0)).unwrap();
    let first = checkpoint(&store, &binding, &id, None, None);
    let first_id = append_checkpoint(&mut store, &binding, &id, first.clone()).unwrap();
    let second = checkpoint(&store, &binding, &id, Some(first_id.clone()), None);
    append_checkpoint(&mut store, &binding, &id, second).unwrap();
    assert_eq!(
        store
            .completed_task_sync_exchange(&request)
            .unwrap()
            .unwrap(),
        original
    );
    let rows_after: String = store.connection.query_row(
        "SELECT group_concat(prepared_id || ':' || latest_event_id || ':' || completed_event_id)
         FROM work_task_sync_exchange_operations", [], |row| row.get(0)).unwrap();
    assert_eq!(rows_after, rows_before);
    let before = state(&store);
    let duplicate = checkpoint(&store, &binding, &id, Some(first_id), None);
    let error = append_checkpoint(&mut store, &binding, &id, duplicate).unwrap_err();
    assert!(
        error.to_string().contains("checkpoint predecessor differs"),
        "{error}"
    );
    assert_eq!(state(&store), before);
    let mut collision = checkpoint(
        &store,
        &binding,
        &id,
        store
            .connection
            .query_row(
                "SELECT last_checkpoint_id FROM work_task_sync_exchange_heads WHERE binding_id=?1",
                [id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
            .parse()
            .ok(),
        None,
    );
    collision.operation = input.intent.operation;
    collision.key = input.intent.key.clone();
    assert!(
        append_checkpoint(&mut store, &binding, &id, collision)
            .unwrap_err()
            .to_string()
            .contains("duplicate exchange request")
    );
    assert_eq!(state(&store), before);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn initial_import_checkpoint_membership_repairs_without_retained_git() {
    let (home, mut store, binding, tree) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let input = super::tests::input(&store, &id, &binding, tree);
    store.prepare_task_sync_exchange(&input, at(6)).unwrap();
    store
        .apply_task_sync_exchange(&input, false, at(7))
        .unwrap();
    let original = store
        .completed_task_sync_exchange(&CompletedExchangeRequest {
            binding: id.clone(),
            intent: input.intent.clone(),
        })
        .unwrap()
        .unwrap();
    let mut root = InventoryRoot::ZERO;
    for task in &input.tree.tasks {
        root = root.including(task).unwrap();
    }
    let seed = CheckpointSeed::Incoming {
        snapshot: Box::new(MembershipSnapshot {
            commit: input.intent.commit.clone(),
            membership: OutputMembership {
                inventory_root: root,
                profile: binding.profile.clone(),
            },
        }),
        members: BootstrapMembers::InitialImport {
            outcome: original.outcome,
        },
    };
    let checkpoint = checkpoint(&store, &binding, &id, None, Some(seed));
    let checkpoint_id = append_checkpoint(&mut store, &binding, &id, checkpoint).unwrap();
    let count: i64 = store.connection.query_row("SELECT count(*) FROM work_task_sync_graph_nodes WHERE view_kind='source' AND scope_id=?1", [id.as_str()], |row| row.get(0)).unwrap();
    assert_eq!(count, i64::try_from(input.tree.tasks.len()).unwrap());
    let selected = store
        .capture_task_sync_membership_component(
            &id,
            crate::task_sync::TaskSyncMembershipView::Source,
            input.tree.tasks.iter().map(|task| task.id),
        )
        .unwrap();
    assert_eq!(selected.head, checkpoint_id);
    assert_eq!(selected.component.documents.len(), input.tree.tasks.len());
    assert!(selected.component.baseline.is_none());
    std::fs::rename(
        &binding.repository.git_directory,
        home.path().join("offline-git"),
    )
    .unwrap();
    store
        .connection
        .execute("DELETE FROM work_task_sync_source_heads", [])
        .unwrap();
    store
        .connection
        .execute(
            "DELETE FROM work_task_sync_graph_nodes WHERE view_kind='source'",
            [],
        )
        .unwrap();
    assert!(!store.verify_all().unwrap().is_healthy());
    drop(store);
    SqliteStore::repair_rebuildable_projections(&binding.store.database).unwrap();
    let store = SqliteStore::open(&binding.store.database).unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
    let restored: String = store.connection.query_row("SELECT checkpoint_id FROM work_task_sync_source_heads WHERE binding_id=?1 AND role='source'", [id.as_str()], |row| row.get(0)).unwrap();
    assert_eq!(restored, checkpoint_id.as_str());
    assert_eq!(
        store
            .capture_task_sync_membership_component(
                &id,
                crate::task_sync::TaskSyncMembershipView::Source,
                selected.component.documents.keys().copied(),
            )
            .unwrap()
            .component
            .documents,
        selected.component.documents
    );
}

#[test]
fn membership_components_keep_source_and_export_separate_and_skip_disconnected_members() {
    use crate::storage::work::task_sync::closure::component::{
        EDGE_READS, MEMBER_DECODES, NODE_LOOKUPS,
    };
    use crate::task_sync::{TaskSyncMembershipView, delta::BootstrapMember};
    let (_home, mut store, binding, tree) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let input = super::tests::input(&store, &id, &binding, tree.clone());
    assert!(
        store
            .capture_task_sync_membership_component(
                &id,
                TaskSyncMembershipView::Source,
                [tree.tasks[0].id],
            )
            .unwrap_err()
            .to_string()
            .contains("no admitted binding membership")
    );

    let template = tree
        .tasks
        .iter()
        .find(|task| task.lifecycle == crate::WorkLifecycle::Open && task.parent_id.is_none())
        .unwrap();
    let mut documents = (0..2000)
        .map(|index| {
            let mut task = template.clone();
            task.id = crate::WorkId(uuid::Uuid::new_v4());
            task.external_ref = None;
            task.parent_id = None;
            task.prerequisites.clear();
            task.title = format!("Source member {index}");
            task
        })
        .collect::<Vec<_>>();
    let target = documents[1].id;
    documents[0].prerequisites.push(target);
    let source_root = documents
        .iter()
        .try_fold(InventoryRoot::ZERO, InventoryRoot::including)
        .unwrap();
    let members = |tasks: &[crate::task_sync::TaskDocument]| BootstrapMembers::Captured {
        members: tasks
            .iter()
            .map(|task| {
                (
                    task.id,
                    BootstrapMember::Document {
                        task: Box::new(task.clone()),
                    },
                )
            })
            .collect(),
    };
    let first = checkpoint(
        &store,
        &binding,
        &id,
        None,
        Some(CheckpointSeed::Incoming {
            snapshot: Box::new(MembershipSnapshot {
                commit: input.intent.commit,
                membership: OutputMembership {
                    inventory_root: source_root,
                    profile: binding.profile.clone(),
                },
            }),
            members: members(&documents),
        }),
    );
    let source_head = append_checkpoint(&mut store, &binding, &id, first).unwrap();
    let mut exported = documents.clone();
    exported[0].prerequisites.clear();
    exported[0].title = "Prepared local text".into();
    let export_root = exported
        .iter()
        .try_fold(InventoryRoot::ZERO, InventoryRoot::including)
        .unwrap();
    let second = checkpoint(
        &store,
        &binding,
        &id,
        Some(source_head.clone()),
        Some(CheckpointSeed::PreparedExport {
            membership: OutputMembership {
                inventory_root: export_root,
                profile: binding.profile.clone(),
            },
            members: members(&exported),
        }),
    );
    let export_head = append_checkpoint(&mut store, &binding, &id, second).unwrap();
    // The metadata read must use the expression index without loading the
    // objects table's checkpoint body, which contains every bootstrap task.
    let header = crate::storage::work::task_sync::closure::membership::head::select_sql();
    let table_root: i64 = store
        .connection
        .query_row(
            "SELECT rootpage FROM sqlite_schema WHERE type='table' AND name='objects'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let bytecode = store
        .connection
        .prepare(&format!("EXPLAIN {header}"))
        .unwrap()
        .query_map([source_head.as_str()], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let object_cursors = bytecode
        .iter()
        .filter(|(op, _, root)| op == "OpenRead" && *root == table_root)
        .map(|(_, cursor, _)| *cursor)
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        !bytecode
            .iter()
            .any(|(op, cursor, _)| op == "Column" && object_cursors.contains(cursor))
    );
    let before = state(&store);
    let reset = || {
        MEMBER_DECODES.with(|count| count.set(0));
        NODE_LOOKUPS.with(|count| count.set(0));
        EDGE_READS.with(|count| count.set(0));
        crate::canonical::reset_canonical_decode_count();
    };
    reset();
    // Selecting the target must discover its unchanged incoming prerequisite.
    let source = store
        .capture_task_sync_membership_component(
            &id,
            TaskSyncMembershipView::Source,
            [documents[1].id],
        )
        .unwrap();
    assert_eq!(source.head, source_head);
    assert_eq!(source.membership.inventory_root, source_root);
    assert_eq!(source.component.documents.len(), 2);
    assert_eq!(MEMBER_DECODES.with(std::cell::Cell::get), 2);
    assert_eq!(NODE_LOOKUPS.with(std::cell::Cell::get), 2);
    assert_eq!(EDGE_READS.with(std::cell::Cell::get), 2);
    assert_eq!(crate::canonical::canonical_decode_count(), 1);
    assert!(source.component.baseline.is_none());
    reset();
    let export = store
        .capture_task_sync_membership_component(
            &id,
            TaskSyncMembershipView::Export,
            [documents[0].id],
        )
        .unwrap();
    assert_eq!(export.head, export_head);
    assert_eq!(export.membership.inventory_root, export_root);
    assert_eq!(export.component.documents[&documents[0].id], exported[0]);
    assert_eq!(MEMBER_DECODES.with(std::cell::Cell::get), 1);
    assert_eq!(NODE_LOOKUPS.with(std::cell::Cell::get), 1);
    assert_eq!(EDGE_READS.with(std::cell::Cell::get), 0);
    assert_eq!(state(&store), before);
    let tx = store.connection.transaction().unwrap();
    let inside = crate::storage::work::task_sync::closure::membership::capture_on(
        &tx,
        &id,
        TaskSyncMembershipView::Source,
        std::collections::BTreeSet::from([documents[1].id]),
    )
    .unwrap();
    assert_eq!(inside.component.documents, source.component.documents);
    tx.rollback().unwrap();
    let absent = crate::WorkId(uuid::Uuid::new_v4());
    let missing = store
        .capture_task_sync_membership_component(&id, TaskSyncMembershipView::Source, [absent])
        .unwrap();
    assert_eq!(
        missing.component.missing,
        std::collections::BTreeSet::from([absent])
    );
    store.connection.execute("UPDATE work_task_sync_document_members SET document_json=X'7b' WHERE record_id=?1 AND work_id=?2",
        rusqlite::params![source_head.as_str(), documents[1999].id.0.to_string()]).unwrap();
    reset();
    assert_eq!(
        store
            .capture_task_sync_membership_component(
                &id,
                TaskSyncMembershipView::Source,
                [documents[1].id]
            )
            .unwrap()
            .component
            .documents,
        source.component.documents
    );
    assert_eq!(MEMBER_DECODES.with(std::cell::Cell::get), 2);
    store.connection.execute("UPDATE work_task_sync_document_members SET member_path='/task' WHERE record_id=?1 AND work_id=?2",
        rusqlite::params![source_head.as_str(), documents[0].id.0.to_string()]).unwrap();
    assert!(
        store
            .capture_task_sync_membership_component(
                &id,
                TaskSyncMembershipView::Source,
                [documents[1].id]
            )
            .unwrap_err()
            .to_string()
            .contains("selected member provenance differs")
    );
    store.connection.execute("UPDATE work_task_sync_source_heads SET inventory_root=?1 WHERE binding_id=?2 AND role='source'",
        rusqlite::params![export_root.hex(), id.as_str()]).unwrap();
    assert!(
        store
            .capture_task_sync_membership_component(
                &id,
                TaskSyncMembershipView::Source,
                [documents[1].id]
            )
            .unwrap_err()
            .to_string()
            .contains("head root or profile differs")
    );
}

fn retained_audit(
    store: &SqliteStore,
    id: &ObjectId,
) -> crate::task_sync::retention::audit::RetainedAudit {
    crate::task_sync::retention::audit::RetainedAudit {
        event_id: id.clone(),
        event: SqliteStore::get_typed_object_on(&store.connection, id, EVENT_KIND)
            .unwrap()
            .unwrap(),
    }
}

#[test]
fn audit_retention_preserves_exact_bytes_and_never_resets_a_later_acknowledgement() {
    use crate::task_sync::retention::audit::{self, AuditMode, AuditReservation};
    let (_home, mut store, binding, tree) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let input = super::tests::input(&store, &id, &binding, tree);
    store.prepare_task_sync_exchange(&input, at(6)).unwrap();
    store
        .apply_task_sync_exchange(&input, false, at(7))
        .unwrap();
    let first = checkpoint(&store, &binding, &id, None, None);
    let first = append_checkpoint(&mut store, &binding, &id, first).unwrap();
    let first = retained_audit(&store, &first);
    let reservation = AuditReservation::acquire(&binding.repository.git_directory).unwrap();
    assert!(AuditReservation::acquire(&binding.repository.git_directory).is_err());
    reservation.retain(&first).unwrap();
    reservation.retain(&first).unwrap();
    let first_ack = reservation.acknowledge(&first, None).unwrap();
    assert_eq!(first_ack.mode, AuditMode::AuditOnly);
    assert_eq!(reservation.acknowledge(&first, None).unwrap(), first_ack);
    let second = checkpoint(&store, &binding, &id, Some(first.event_id.clone()), None);
    let second = append_checkpoint(&mut store, &binding, &id, second).unwrap();
    let second = retained_audit(&store, &second);
    reservation.retain(&second).unwrap();
    let second_ack = reservation.acknowledge(&second, Some(&first_ack)).unwrap();
    let before = audit::inventory(&binding.repository.git_directory).unwrap();
    assert_eq!(before.events.len(), 2);
    assert_eq!(before.acknowledgements.len(), 2);
    assert_eq!(before.head, Some(second_ack));
    assert!(reservation.acknowledge(&first, None).is_err());
    let after = audit::inventory(&binding.repository.git_directory).unwrap();
    assert_eq!(after.events, before.events);
    assert_eq!(after.acknowledgements, before.acknowledgements);
    assert_eq!(after.head, before.head);
}

#[test]
fn audit_retention_reads_large_events_but_refuses_capacity_and_partial_files() {
    use crate::task_sync::{
        delta::{CustodyArtifactKind, CustodyArtifactRef},
        retention::audit::{self, AuditReservation},
    };
    let (_home, mut store, binding, _) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let checkpoint = checkpoint(&store, &binding, &id, None, None);
    let event = Event {
        schema_version: 1,
        binding: id,
        sequence: 2,
        predecessor: Some(
            store
                .task_sync_exchange_events(&checkpoint.local_cut.binding)
                .unwrap()
                .last()
                .unwrap()
                .0
                .clone(),
        ),
        actor: binding.actor.clone(),
        recorded_at: at(30),
        transition: Transition::CustodyCheckpoint {
            checkpoint: Box::new(checkpoint),
        },
    };
    // File-layer tests supply retained observations only, without claiming SQL
    // admission. The actual writer must independently validate this payload.
    let mut large = audit::RetainedAudit {
        event_id: ObjectId::mint(),
        event,
    };
    let comparison = CanonicalObject::freeze(&"artifact comparison")
        .unwrap()
        .key()
        .clone();
    let artifact = |operation| CustodyArtifactRef {
        operation,
        kind: CustodyArtifactKind::Candidate,
        content_fingerprint: comparison.clone(),
        record: None,
    };
    let Transition::CustodyCheckpoint { checkpoint } = &mut large.event.transition else {
        unreachable!()
    };
    checkpoint.artifacts = (0..20_000)
        .map(|_| artifact(uuid::Uuid::new_v4()))
        .collect();
    checkpoint
        .artifacts
        .sort_by_key(|entry| (entry.kind, entry.operation));
    let bytes = large.preflight().unwrap();
    assert!(bytes.len() > 2 * 1024 * 1024);
    let reservation = AuditReservation::acquire(&binding.repository.git_directory).unwrap();
    reservation.retain(&large).unwrap();
    assert_eq!(
        audit::inventory(&binding.repository.git_directory)
            .unwrap()
            .events
            .get(&large.event_id),
        Some(&large)
    );
    let observed =
        super::super::custody::Observed::capture(&binding.repository.git_directory).unwrap();
    assert!(
        super::super::custody::check(
            &Ledger::load(&store.connection).unwrap(),
            &binding.repository.git_directory,
            &observed,
            None,
            |left, right| left == right,
        )
        .unwrap_err()
        .to_string()
        .contains("retained custody audit is ahead")
    );
    let mut oversized = large.clone();
    oversized.event_id = ObjectId::mint();
    let Transition::CustodyCheckpoint { checkpoint } = &mut oversized.event.transition else {
        unreachable!()
    };
    checkpoint.artifacts = (0..100_000)
        .map(|_| artifact(uuid::Uuid::new_v4()))
        .collect();
    assert!(
        oversized
            .preflight()
            .unwrap_err()
            .to_string()
            .contains("byte bound")
    );
    assert!(reservation.retain(&oversized).is_err());
    assert_eq!(
        audit::inventory(&binding.repository.git_directory)
            .unwrap()
            .events
            .len(),
        1
    );
    let file = binding
        .repository
        .git_directory
        .join("engram-task-sync-custody-audits")
        .join(format!("{}.json", large.event_id));
    std::fs::write(&file, b"partial retained event").unwrap();
    let partial = std::fs::read(&file).unwrap();
    assert!(reservation.retain(&large).is_err());
    assert!(audit::inventory(&binding.repository.git_directory).is_err());
    assert_eq!(std::fs::read(file).unwrap(), partial);
}

fn audit_request(
    store: &SqliteStore,
    binding: &Binding,
    id: &ObjectId,
    seed: CustodySeedSelection,
) -> CustodyAuditRequest {
    let previous: Option<String> = store
        .connection
        .query_row(
            "SELECT last_checkpoint_id FROM work_task_sync_exchange_heads WHERE binding_id=?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    let checkpoint = checkpoint(
        store,
        binding,
        id,
        previous.map(|id| id.parse().unwrap()),
        None,
    );
    let tail = super::super::view::read::tail_on(
        &store.connection,
        id,
        binding,
        &mut super::super::view::read::ReadBudget::default(),
    )
    .unwrap();
    CustodyAuditRequest {
        binding: id.clone(),
        operation: checkpoint.operation,
        key: checkpoint.key,
        reason: checkpoint.reason,
        previous_checkpoint: checkpoint.previous_checkpoint,
        expected_tail: tail.id,
        expected_local_cut: checkpoint.local_cut,
        seed,
    }
}

#[test]
fn named_custody_only_audit_replays_original_result_without_creating_source_membership() {
    use crate::task_sync::{origin::private, retention::audit};
    let (_home, mut store, binding, _) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let request = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
    let first = store
        .audit_task_sync_custody(&request, &binding.actor, at(30))
        .unwrap();
    assert!(first.acknowledged);
    assert!(!first.repeated);
    let private = private::read(&binding.store.database, &binding.project).unwrap();
    assert_eq!(private.audit, None);
    assert!(private.witness.as_ref().unwrap().pending_exchange.is_none());
    let source: i64 = store
        .connection
        .query_row(
            "SELECT count(*) FROM work_task_sync_source_heads",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(source, 0);
    let before = state(&store);
    let custody = audit::inventory(&binding.repository.git_directory).unwrap();
    let again = store
        .audit_task_sync_custody(&request, &binding.actor, at(50))
        .unwrap();
    assert!(again.repeated && again.acknowledged);
    assert_eq!(again.checkpoint, first.checkpoint);
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&binding.store.database, &binding.project).unwrap(),
        private
    );
    assert_eq!(
        audit::inventory(&binding.repository.git_directory)
            .unwrap()
            .head,
        custody.head
    );
    for changed in [
        CustodyAuditRequest {
            key: "another request key".into(),
            ..request.clone()
        },
        CustodyAuditRequest {
            operation: uuid::Uuid::new_v4(),
            ..request.clone()
        },
        CustodyAuditRequest {
            reason: crate::task_sync::delta::CheckpointReason::NamedRebuild,
            ..request.clone()
        },
    ] {
        assert!(
            store
                .audit_task_sync_custody(&changed, &binding.actor, at(60))
                .is_err()
        );
        assert_eq!(state(&store), before);
        assert_eq!(
            private::read(&binding.store.database, &binding.project).unwrap(),
            private
        );
    }
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn named_audit_source_and_newer_local_export_repair_without_git_and_keep_original_receipt() {
    use crate::storage::work::test_support::root_request;
    let (home, mut store, binding, tree) = initial_fixture();
    let initial_members = tree.tasks.len();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let input = super::tests::input(&store, &id, &binding, tree);
    store.prepare_task_sync_exchange(&input, at(6)).unwrap();
    store
        .apply_task_sync_exchange(&input, false, at(7))
        .unwrap();
    let original = store
        .completed_task_sync_exchange(&CompletedExchangeRequest {
            binding: id.clone(),
            intent: input.intent.clone(),
        })
        .unwrap()
        .unwrap();
    let incoming = audit_request(
        &store,
        &binding,
        &id,
        CustodySeedSelection::InitialImport {
            outcome: original.outcome.clone(),
        },
    );
    let first = store
        .audit_task_sync_custody(&incoming, &binding.actor, at(30))
        .unwrap();
    store
        .create_work(
            &root_request(&binding.project.0, "later native planning", 40),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let export = audit_request(&store, &binding, &id, CustodySeedSelection::LocalExport);
    let second = store
        .audit_task_sync_custody(&export, &binding.actor, at(50))
        .unwrap();
    assert_ne!(first.checkpoint, second.checkpoint);
    let source: i64 = store
        .connection
        .query_row(
            "SELECT count(*) FROM work_task_sync_graph_nodes WHERE view_kind='source'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let exported: i64 = store
        .connection
        .query_row(
            "SELECT count(*) FROM work_task_sync_graph_nodes WHERE view_kind='export'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(source, i64::try_from(initial_members).unwrap());
    assert_eq!(exported, i64::try_from(initial_members + 1).unwrap());
    let before = state(&store);
    assert_eq!(
        store
            .audit_task_sync_custody(&incoming, &binding.actor, at(60))
            .unwrap()
            .checkpoint,
        first.checkpoint
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        store
            .completed_task_sync_exchange(&CompletedExchangeRequest {
                binding: id.clone(),
                intent: input.intent
            })
            .unwrap()
            .unwrap(),
        original
    );
    std::fs::rename(
        &binding.repository.git_directory,
        home.path().join("offline-audit-git"),
    )
    .unwrap();
    store
        .connection
        .execute("DELETE FROM work_task_sync_source_heads", [])
        .unwrap();
    store
        .connection
        .execute(
            "DELETE FROM work_task_sync_graph_nodes WHERE view_kind IN ('source','export')",
            [],
        )
        .unwrap();
    drop(store);
    SqliteStore::repair_rebuildable_projections(&binding.store.database).unwrap();
    let store = SqliteStore::open(&binding.store.database).unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
    let heads: Vec<(String, String)> = store
        .connection
        .prepare("SELECT role,checkpoint_id FROM work_task_sync_source_heads ORDER BY role")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        heads,
        vec![
            ("export".into(), second.checkpoint.to_string()),
            ("source".into(), first.checkpoint.to_string())
        ]
    );
}

fn fail_checkpoint_insert(store: &SqliteStore) {
    store.connection.execute_batch("CREATE TEMP TRIGGER refuse_audit BEFORE INSERT ON objects
        WHEN NEW.object_kind='task_sync_exchange_event' AND json_type(NEW.canonical_json,'$.transition.checkpoint')='object'
        BEGIN SELECT RAISE(ABORT,'injected checkpoint insertion failure'); END;").unwrap();
}

#[test]
fn pending_audit_retries_original_export_after_sql_rollback_and_later_native_work() {
    use crate::storage::work::test_support::root_request;
    use crate::task_sync::origin::private;
    let (_home, mut store, binding, _) = initial_fixture();
    let initial_members = store
        .export_task_tree(&binding.project)
        .unwrap()
        .tasks
        .len();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let request = audit_request(&store, &binding, &id, CustodySeedSelection::LocalExport);
    fail_checkpoint_insert(&store);
    let before = state(&store);
    assert!(matches!(
        store.audit_task_sync_custody(&request, &binding.actor, at(30)),
        Err(StoreError::Sqlite(_))
    ));
    assert_eq!(state(&store), before);
    let pending = private::read(&binding.store.database, &binding.project)
        .unwrap()
        .audit
        .unwrap();
    assert_eq!(pending.audit_request, request);
    assert!(
        store
            .activate_task_sync_binding(&binding)
            .unwrap_err()
            .to_string()
            .contains("custody audit")
    );
    store
        .connection
        .execute_batch("DROP TRIGGER refuse_audit")
        .unwrap();
    store
        .create_work(
            &root_request(&binding.project.0, "native work after failed audit", 40),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let result = store
        .audit_task_sync_custody(&request, &binding.actor, at(50))
        .unwrap();
    assert_eq!(result.checkpoint, pending.event_id);
    let event: Event =
        SqliteStore::get_typed_object_on(&store.connection, &result.checkpoint, EVENT_KIND)
            .unwrap()
            .unwrap();
    assert_eq!(event.recorded_at, at(30));
    let exported: i64 = store
        .connection
        .query_row(
            "SELECT count(*) FROM work_task_sync_graph_nodes WHERE view_kind='export'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(exported, i64::try_from(initial_members).unwrap());
    assert_eq!(
        store
            .export_task_tree(&binding.project)
            .unwrap()
            .tasks
            .len(),
        initial_members + 1
    );
    assert_eq!(
        private::read(&binding.store.database, &binding.project)
            .unwrap()
            .audit,
        None
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn committed_audit_retries_every_acknowledgement_boundary_without_reminting() {
    use crate::task_sync::origin::private;
    for stage in ["repository_ack", "origin_ack", "private_cleanup"] {
        let (_home, mut store, binding, _) = initial_fixture();
        let id = store.activate_task_sync_binding(&binding).unwrap();
        let request = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
        super::super::audit::FAIL_STAGE.with(|failure| failure.set(Some(stage)));
        assert!(
            store
                .audit_task_sync_custody(&request, &binding.actor, at(30))
                .unwrap_err()
                .to_string()
                .contains(stage)
        );
        let pending = private::read(&binding.store.database, &binding.project)
            .unwrap()
            .audit
            .unwrap();
        let before = state(&store);
        assert!(store.activate_task_sync_binding(&binding).is_err());
        assert_eq!(state(&store), before);
        let result = store
            .audit_task_sync_custody(&request, &binding.actor, at(50))
            .unwrap();
        assert!(result.repeated && result.acknowledged);
        assert_eq!(result.checkpoint, pending.event_id);
        assert_eq!(state(&store), before);
        assert_eq!(
            private::read(&binding.store.database, &binding.project)
                .unwrap()
                .audit,
            None
        );
        let event: Event =
            SqliteStore::get_typed_object_on(&store.connection, &result.checkpoint, EVENT_KIND)
                .unwrap()
                .unwrap();
        assert_eq!(event.recorded_at, at(30));
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn pending_audit_missing_or_partial_event_is_never_recaptured() {
    use crate::task_sync::origin::private;
    let (_home, mut store, binding, _) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let request = audit_request(&store, &binding, &id, CustodySeedSelection::LocalExport);
    fail_checkpoint_insert(&store);
    assert!(
        store
            .audit_task_sync_custody(&request, &binding.actor, at(30))
            .is_err()
    );
    store
        .connection
        .execute_batch("DROP TRIGGER refuse_audit")
        .unwrap();
    let private = private::read(&binding.store.database, &binding.project).unwrap();
    let pending = private.audit.as_ref().unwrap();
    let file = binding
        .repository
        .git_directory
        .join("engram-task-sync-custody-audits")
        .join(format!("{}.json", pending.event_id));
    std::fs::remove_file(&file).unwrap();
    let before = state(&store);
    assert!(
        store
            .audit_task_sync_custody(&request, &binding.actor, at(40))
            .unwrap_err()
            .to_string()
            .contains("event missing")
    );
    assert!(!file.exists());
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&binding.store.database, &binding.project).unwrap(),
        private
    );
    std::fs::write(&file, b"partial audit event").unwrap();
    assert!(
        store
            .audit_task_sync_custody(&request, &binding.actor, at(50))
            .is_err()
    );
    assert_eq!(std::fs::read(file).unwrap(), b"partial audit event");
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&binding.store.database, &binding.project).unwrap(),
        private
    );
}

#[test]
fn missing_descriptor_does_not_settle_an_unacknowledged_canonical_audit() {
    let (_home, mut store, binding, _) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let request = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
    super::super::audit::FAIL_STAGE.with(|failure| failure.set(Some("origin_ack")));
    assert!(
        store
            .audit_task_sync_custody(&request, &binding.actor, at(30))
            .is_err()
    );
    std::fs::remove_file(
        binding
            .store
            .database
            .parent()
            .unwrap()
            .join("task-sync-custody-audit-pending.json"),
    )
    .unwrap();
    let before = state(&store);
    let committed = store
        .audit_task_sync_custody(&request, &binding.actor, at(40))
        .unwrap();
    assert!(committed.repeated);
    assert!(!committed.acknowledged);
    assert!(store.activate_task_sync_binding(&binding).is_err());
    assert_eq!(state(&store), before);
}

#[test]
fn cold_audit_uses_shared_restore_occurrence_for_copy_and_local_file_sources() {
    use crate::backup::{
        restore::{self, PreviousRestore, RestoreRecord, RestoreSource, RestoreState},
        target::{RECORD_FORMAT_VERSION, Statement},
    };
    use crate::task_sync::origin::private;
    for local_file in [false, true] {
        let (home, mut store, mut binding, _) = initial_fixture();
        let occurrence = uuid::Uuid::new_v4();
        binding.store.restore_occurrence = Some(occurrence);
        let id = store.activate_task_sync_binding(&binding).unwrap();
        let request = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
        let path = restore::restore_record_path(home.path(), &binding.project);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let record = RestoreRecord {
            format_version: RECORD_FORMAT_VERSION,
            occurrence_id: occurrence,
            project: binding.project.0.clone(),
            source: if local_file {
                RestoreSource::LocalFile {
                    path: "historical-input.db".into(),
                    replace: true,
                }
            } else {
                RestoreSource::Copy {
                    copy: "recorded-copy".into(),
                    origin_host: Some("old-host".into()),
                    origin_retired: Statement {
                        by: "fixture operator".into(),
                        at: at(1),
                    },
                }
            },
            // This is acquisition provenance, not a hash of the changing live DB.
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
        let before = state(&store);
        let private_before = private::read(&binding.store.database, &binding.project).unwrap();
        for change in [
            "absent",
            "unreadable",
            "other_occurrence",
            "pending",
            "pending_with_previous",
        ] {
            match change {
                "absent" => {}
                "unreadable" => std::fs::write(&path, b"partial restore record").unwrap(),
                _ => {
                    let mut changed = record.clone();
                    if change == "other_occurrence" {
                        changed.occurrence_id = uuid::Uuid::new_v4();
                    } else {
                        changed.state = RestoreState::Pending;
                        changed.completed_at = None;
                        if change == "pending_with_previous" {
                            changed.previous_completed = Some(PreviousRestore {
                                occurrence_id: uuid::Uuid::new_v4(),
                                file: "store.restore-previous.json".into(),
                            });
                        }
                    }
                    std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
                }
            }
            let bytes = std::fs::read(&path).ok();
            let error = store
                .audit_task_sync_custody(&request, &binding.actor, at(30))
                .unwrap_err();
            assert!(
                error.to_string().contains("restore occurrence"),
                "{change}: {error}"
            );
            assert_eq!(state(&store), before);
            assert_eq!(
                private::read(&binding.store.database, &binding.project).unwrap(),
                private_before
            );
            assert_eq!(std::fs::read(&path).ok(), bytes);
        }
        let bytes = serde_json::to_vec(&record).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let result = store
            .audit_task_sync_custody(&request, &binding.actor, at(40))
            .unwrap();
        assert!(result.acknowledged);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn pending_audit_restore_change_refuses_without_replacing_retained_event() {
    use crate::backup::restore;
    use crate::task_sync::{origin::private, retention::audit};
    let (home, mut store, binding, _) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let request = audit_request(&store, &binding, &id, CustodySeedSelection::LocalExport);
    fail_checkpoint_insert(&store);
    assert!(
        store
            .audit_task_sync_custody(&request, &binding.actor, at(30))
            .is_err()
    );
    store
        .connection
        .execute_batch("DROP TRIGGER refuse_audit")
        .unwrap();
    let pending = private::read(&binding.store.database, &binding.project).unwrap();
    let event = audit::inventory(&binding.repository.git_directory)
        .unwrap()
        .events;
    let path = restore::restore_record_path(home.path(), &binding.project);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"unreadable restore occurrence").unwrap();
    let before = state(&store);
    assert!(
        store
            .audit_task_sync_custody(&request, &binding.actor, at(40))
            .unwrap_err()
            .to_string()
            .contains("restore occurrence")
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&binding.store.database, &binding.project).unwrap(),
        pending
    );
    assert_eq!(
        audit::inventory(&binding.repository.git_directory)
            .unwrap()
            .events,
        event
    );
    std::fs::remove_file(path).unwrap();
    let result = store
        .audit_task_sync_custody(&request, &binding.actor, at(50))
        .unwrap();
    assert_eq!(result.checkpoint, pending.audit.unwrap().event_id);
}

#[test]
fn cold_audit_refuses_competing_project_and_repository_reservations_before_retention() {
    use crate::task_sync::{
        origin::{continuity::PreparedContinuity, private},
        retention::audit::AuditReservation,
    };
    let (_home, mut store, binding, _) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let request = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
    let before = state(&store);
    let private_before = private::read(&binding.store.database, &binding.project).unwrap();
    let project_guard =
        PreparedContinuity::acquire(&binding.store.database, &binding.project).unwrap();
    assert!(
        store
            .audit_task_sync_custody(&request, &binding.actor, at(30))
            .is_err()
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&binding.store.database, &binding.project).unwrap(),
        private_before
    );
    drop(project_guard);
    let repository_guard = AuditReservation::acquire(&binding.repository.git_directory).unwrap();
    assert!(
        store
            .audit_task_sync_custody(&request, &binding.actor, at(40))
            .unwrap_err()
            .to_string()
            .contains("busy")
    );
    assert!(repository_guard.inventory().unwrap().events.is_empty());
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&binding.store.database, &binding.project).unwrap(),
        private_before
    );
    drop(repository_guard);
    assert!(
        store
            .audit_task_sync_custody(&request, &binding.actor, at(50))
            .unwrap()
            .acknowledged
    );
}

#[test]
fn new_audit_refuses_missing_repository_head_before_retaining_another_event() {
    use crate::task_sync::{origin::private, retention::audit};
    let (_home, mut store, binding, _) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let first = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
    store
        .audit_task_sync_custody(&first, &binding.actor, at(30))
        .unwrap();
    let next = audit_request(&store, &binding, &id, CustodySeedSelection::LocalExport);
    std::fs::remove_file(
        binding
            .repository
            .git_directory
            .join("engram-task-sync-custody-audits/head.json"),
    )
    .unwrap();
    let before = state(&store);
    let private_before = private::read(&binding.store.database, &binding.project).unwrap();
    let retained = audit::inventory(&binding.repository.git_directory)
        .unwrap()
        .events;
    assert!(
        store
            .audit_task_sync_custody(&next, &binding.actor, at(40))
            .unwrap_err()
            .to_string()
            .contains("repository head differs")
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&binding.store.database, &binding.project).unwrap(),
        private_before
    );
    assert_eq!(
        audit::inventory(&binding.repository.git_directory)
            .unwrap()
            .events,
        retained
    );
}

#[test]
fn cold_audit_detects_changed_historical_artifacts_before_retaining_a_checkpoint() {
    use crate::task_sync::{ledger::ImportPlan, origin::private, retention};
    for change in ["commit", "plan", "member", "unknown"] {
        let (_home, mut store, binding, tree) = initial_fixture();
        let id = store.activate_task_sync_binding(&binding).unwrap();
        let input = super::tests::input(&store, &id, &binding, tree);
        store.prepare_task_sync_exchange(&input, at(6)).unwrap();
        store
            .apply_task_sync_exchange(&input, false, at(7))
            .unwrap();
        let request = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
        let git = &binding.repository.git_directory;
        let changed_bytes = match change {
            "commit" => {
                let mut selected = input.intent.commit.clone();
                selected.tree = CanonicalObject::freeze(&uuid::Uuid::new_v4())
                    .unwrap()
                    .key()
                    .to_string();
                let path = git
                    .join("engram-task-sync-commits")
                    .join(format!("{}.json", input.intent.operation));
                let bytes = crate::canonical::canonical_bytes(&selected).unwrap();
                std::fs::write(&path, &bytes).unwrap();
                vec![(path, bytes)]
            }
            "unknown" => {
                let path = git.join("engram-task-sync-commits/unknown.json");
                let bytes = b"unexplained custody".to_vec();
                std::fs::write(&path, &bytes).unwrap();
                vec![(path, bytes)]
            }
            _ => {
                let mut plan: ImportPlan = retention::read_import_plan(git, input.intent.operation)
                    .unwrap()
                    .unwrap();
                if change == "plan" {
                    plan.sequence += 1;
                } else {
                    plan.tasks[0].restored_fingerprint =
                        CanonicalObject::freeze(&uuid::Uuid::new_v4())
                            .unwrap()
                            .key()
                            .clone();
                }
                let bytes = crate::canonical::canonical_bytes(&plan).unwrap();
                ["plan.json", "plan.pending"]
                    .into_iter()
                    .map(|name| {
                        let path = git
                            .join("engram-task-sync-import-plans")
                            .join(input.intent.operation.to_string())
                            .join(name);
                        std::fs::write(&path, &bytes).unwrap();
                        (path, bytes.clone())
                    })
                    .collect()
            }
        };
        let before = state(&store);
        let private_before = private::read(&binding.store.database, &binding.project).unwrap();
        assert!(
            store
                .audit_task_sync_custody(&request, &binding.actor, at(30))
                .is_err(),
            "{change}"
        );
        assert_eq!(state(&store), before);
        assert_eq!(
            private::read(&binding.store.database, &binding.project).unwrap(),
            private_before
        );
        for (path, bytes) in changed_bytes {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
        assert!(retention::audit::inventory(git).unwrap().events.is_empty());
    }
}

#[test]
fn pending_audit_refuses_changed_independent_witness_or_canonical_event() {
    use crate::task_sync::origin::private;
    for canonical in [false, true] {
        let (_home, mut store, binding, _) = initial_fixture();
        let id = store.activate_task_sync_binding(&binding).unwrap();
        let request = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
        super::super::audit::FAIL_STAGE.with(|failure| failure.set(Some("origin_ack")));
        assert!(
            store
                .audit_task_sync_custody(&request, &binding.actor, at(30))
                .is_err()
        );
        let pending = private::read(&binding.store.database, &binding.project)
            .unwrap()
            .audit
            .unwrap();
        if canonical {
            let mut event: Event =
                SqliteStore::get_typed_object_on(&store.connection, &pending.event_id, EVENT_KIND)
                    .unwrap()
                    .unwrap();
            event.recorded_at = at(31);
            let object = CanonicalObject::identified(&pending.event_id, &event).unwrap();
            store
                .connection
                .execute(
                    "UPDATE objects SET canonical_json=?2 WHERE object_id=?1",
                    rusqlite::params![pending.event_id.as_str(), object.bytes()],
                )
                .unwrap();
        } else {
            let path = binding
                .store
                .database
                .parent()
                .unwrap()
                .join("task-sync-origin-registration.json");
            let mut witness: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            witness["exchange"]["head"] = serde_json::to_value(ObjectId::mint()).unwrap();
            std::fs::write(path, serde_json::to_vec(&witness).unwrap()).unwrap();
        }
        let before = state(&store);
        let private_before = private::read(&binding.store.database, &binding.project).unwrap();
        assert!(
            store
                .audit_task_sync_custody(&request, &binding.actor, at(40))
                .is_err()
        );
        assert_eq!(state(&store), before);
        assert_eq!(
            private::read(&binding.store.database, &binding.project).unwrap(),
            private_before
        );
    }
}

// The real retention owner uses the existing Windows process controller.
// Portable cold Git reads have separate snapshot-reader coverage.
#[cfg(windows)]
#[tokio::test]
async fn incoming_commit_audit_reads_complete_real_git_selection_and_requires_its_canonical_owner()
{
    use crate::task_sync::{directory, exchange, retention};
    let (_home, mut store, binding, tree) = initial_fixture();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(&binding.repository.main_checkout)
            .args([
                "-c",
                "user.name=Audit fixture",
                "-c",
                "user.email=audit@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "--initial-branch=main"]);
    directory::write_folder(
        &binding.repository.main_checkout.join(".engram"),
        &tree,
        &binding.profile,
    )
    .unwrap();
    git(&["add", ".engram"]);
    git(&["commit", "-m", "Capture complete audit source fixture"]);
    let commit = git(&["rev-parse", "HEAD"]);
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let mut input = super::tests::input(&store, &id, &binding, tree.clone());
    // Replace only the transaction fixture's synthetic observation, before any
    // canonical preparation, using the real existing retention owner.
    std::fs::remove_file(
        binding
            .repository
            .git_directory
            .join("engram-task-sync-commits")
            .join(format!("{}.json", input.intent.operation)),
    )
    .unwrap();
    input.intent.commit = exchange::retain_commit(
        &binding.repository.main_checkout,
        input.intent.operation,
        &commit,
        exchange::RunLimits::default(),
        &exchange::Cancellation::default(),
    )
    .await
    .unwrap();
    let prepared = store.prepare_task_sync_exchange(&input, at(6)).unwrap();
    store
        .apply_task_sync_exchange(&input, false, at(7))
        .unwrap();
    let request = audit_request(
        &store,
        &binding,
        &id,
        CustodySeedSelection::IncomingCommit {
            commit: commit.clone(),
            prepared: prepared.clone(),
        },
    );
    let mut missing = request.clone();
    missing.seed = CustodySeedSelection::IncomingCommit {
        commit: commit.clone(),
        prepared: ObjectId::mint(),
    };
    let before = state(&store);
    assert!(
        store
            .audit_task_sync_custody(&missing, &binding.actor, at(20))
            .is_err()
    );
    assert_eq!(state(&store), before);
    let result =
        audit_deadlines::expires_then_completes(&mut store, &binding, &request, "show-ref");
    let event: Event =
        SqliteStore::get_typed_object_on(&store.connection, &result.checkpoint, EVENT_KIND)
            .unwrap()
            .unwrap();
    let Transition::CustodyCheckpoint { checkpoint } = event.transition else {
        panic!("checkpoint expected")
    };
    let Some(CheckpointSeed::Incoming {
        snapshot,
        members: BootstrapMembers::Captured { members },
    }) = checkpoint.seed
    else {
        panic!("captured incoming source expected")
    };
    assert_eq!(snapshot.commit, input.intent.commit);
    assert_eq!(members.len(), tree.tasks.len());
    assert_eq!(
        retention::commit_selection(&binding.repository.git_directory, input.intent.operation)
            .unwrap(),
        Some(snapshot.commit)
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

// A merged resolution is admitted as a change to the original preparation's
// export membership, checked on the component it touches: an unchanged child
// still naming a removed parent is found through the reverse edge, without a
// read of every member.
#[test]
fn an_export_change_that_removes_a_parent_its_unchanged_child_names_is_refused() {
    use crate::storage::work::task_sync::closure::overlay::check_export_change_on;
    use crate::task_sync::delta::BootstrapMember;
    let (_home, mut store, binding, tree) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let template = tree
        .tasks
        .iter()
        .find(|task| task.lifecycle == crate::WorkLifecycle::Open && task.parent_id.is_none())
        .unwrap();
    let member = |title: &str, parent: Option<crate::WorkId>| {
        let mut task = template.clone();
        task.id = crate::WorkId(uuid::Uuid::new_v4());
        task.external_ref = None;
        task.parent_id = parent;
        task.prerequisites.clear();
        task.title = title.into();
        task
    };
    let parent = member("Parent", None);
    let child = member("Child", Some(parent.id));
    let unrelated = member("Unrelated", None);
    let exported = [parent.clone(), child.clone(), unrelated];
    let membership = OutputMembership {
        inventory_root: exported
            .iter()
            .try_fold(InventoryRoot::ZERO, InventoryRoot::including)
            .unwrap(),
        profile: binding.profile.clone(),
    };
    let seed = checkpoint(
        &store,
        &binding,
        &id,
        None,
        Some(CheckpointSeed::PreparedExport {
            membership: membership.clone(),
            members: BootstrapMembers::Captured {
                members: exported
                    .iter()
                    .map(|task| {
                        (
                            task.id,
                            BootstrapMember::Document {
                                task: Box::new(task.clone()),
                            },
                        )
                    })
                    .collect(),
            },
        }),
    );
    append_checkpoint(&mut store, &binding, &id, seed).unwrap();
    let check = |expected: &OutputMembership,
                 changes: std::collections::BTreeMap<
        crate::WorkId,
        Option<crate::task_sync::TaskDocument>,
    >| {
        let transaction = store.connection.unchecked_transaction().unwrap();
        check_export_change_on(&transaction, &id, &binding.project, expected, &changes)
    };
    let error = check(
        &membership,
        std::collections::BTreeMap::from([(parent.id, None)]),
    )
    .unwrap_err();
    assert_eq!(
        crate::store_error_value(&error)["error"]["code"],
        "task_sync_invalid_graph",
        "{error:#}"
    );
    let mut orphan = child.clone();
    orphan.parent_id = None;
    check(
        &membership,
        std::collections::BTreeMap::from([(parent.id, None), (child.id, Some(orphan))]),
    )
    .unwrap();
    let mut stale = membership.clone();
    stale.inventory_root = InventoryRoot::ZERO;
    let error = check(
        &stale,
        std::collections::BTreeMap::from([(parent.id, None)]),
    )
    .unwrap_err();
    assert_eq!(
        crate::store_error_value(&error)["error"]["code"],
        "task_sync_baseline_head_moved",
        "{error:#}"
    );
}
