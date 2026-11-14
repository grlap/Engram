use super::*;
use crate::storage::work::{
    task_sync::{
        closure::{documents, graph, source},
        tests::{candidate, ledger::binding, state},
    },
    test_support::at,
};
use crate::task_sync::delta::*;
use crate::task_sync::{
    inventory::InventoryRoot,
    ledger::{BINDING_KIND, RetainedCommit},
};
use crate::{CanonicalObject, FeedId, FeedPosition, storage::SqliteStore};

fn comparison<T: serde::Serialize>(value: &T) -> ObjectId {
    CanonicalObject::freeze(value).unwrap().key().clone()
}

fn put(connection: &Connection, kind: &str, object: &CanonicalObject) -> Result<(), StoreError> {
    connection.execute("INSERT INTO objects(object_id,object_kind,canonical_json,created_at) VALUES(?1,?2,?3,'fixture')",
        params![object.key().as_str(), kind, object.bytes()])?;
    Ok(())
}

// Projection fixtures deliberately bypass ledger admission. They exercise the
// immutable recipe and transaction upkeep; they cannot manufacture an admitted
// applied receipt. The production reducer still refuses canonical deltas.
struct Fixture {
    _home: crate::test_support::TempHome,
    store: SqliteStore,
    binding: Binding,
    id: ObjectId,
    cut: OriginQualifiedCut,
    source: ObjectId,
    export: ObjectId,
    original: Members,
    membership: OutputMembership,
    source_commit: RetainedCommit,
    sequence: u64,
}

impl Fixture {
    fn new(count: usize) -> Self {
        let home = crate::test_support::temp_home().unwrap();
        let mut binding = binding(home.path());
        binding.store.origin = Some(ObjectId::mint());
        let store = SqliteStore::open_in_memory().unwrap();
        let object = CanonicalObject::mint(&binding).unwrap();
        let id = object.key().clone();
        put(&store.connection, BINDING_KIND, &object).unwrap();
        let cut = OriginQualifiedCut {
            binding: id.clone(),
            origin: binding.store.origin.clone().unwrap(),
            restore_occurrence: None,
            position: FeedPosition {
                feed: FeedId::Project(binding.project.clone()),
                position: 0,
            },
            anchor: None,
        };
        let template = candidate()
            .tasks
            .into_iter()
            .find(|task| task.lifecycle == crate::WorkLifecycle::Open && task.parent_id.is_none())
            .unwrap();
        let mut tasks = Vec::new();
        for index in 0..count {
            let mut task = template.clone();
            task.id = WorkId::new();
            task.external_ref = None;
            task.parent_id = None;
            task.prerequisites.clear();
            task.title = format!("Original member {index}");
            tasks.push(task);
        }
        let membership = OutputMembership {
            inventory_root: tasks
                .iter()
                .try_fold(InventoryRoot::ZERO, InventoryRoot::including)
                .unwrap(),
            profile: binding.profile.clone(),
        };
        let members = BootstrapMembers::Captured {
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
        let mut fixture = Self {
            _home: home,
            store,
            binding,
            id,
            cut,
            source: ObjectId::mint(),
            export: ObjectId::mint(),
            original: BTreeMap::new(),
            membership: membership.clone(),
            source_commit: commit(uuid::Uuid::new_v4()),
            sequence: 0,
        };
        for role in ["source", "export"] {
            let operation = uuid::Uuid::new_v4();
            let seed = if role == "source" {
                CheckpointSeed::Incoming {
                    snapshot: Box::new(MembershipSnapshot {
                        commit: fixture.source_commit.clone(),
                        membership: membership.clone(),
                    }),
                    members: members.clone(),
                }
            } else {
                CheckpointSeed::PreparedExport {
                    membership: membership.clone(),
                    members: members.clone(),
                }
            };
            let checkpoint = ManagedCustodyCheckpoint {
                operation,
                key: operation.to_string(),
                request_fingerprint: comparison(&operation),
                previous_checkpoint: (role == "export").then(|| fixture.source.clone()),
                local_cut: fixture.cut.clone(),
                reason: CheckpointReason::NamedAudit,
                seed: Some(seed),
                artifacts: Vec::new(),
            };
            let (_, object) = fixture.event(Transition::CustodyCheckpoint {
                checkpoint: Box::new(checkpoint),
            });
            fixture.append(&object).unwrap();
            if role == "source" {
                fixture.source = object.key().clone();
            } else {
                fixture.export = object.key().clone();
            }
        }
        fixture.original = tasks
            .into_iter()
            .map(|task| (task.id, (fixture.source.clone(), task)))
            .collect();
        fixture
    }

    fn event(&mut self, transition: Transition) -> (Event, CanonicalObject) {
        self.sequence += 1;
        let event = Event {
            schema_version: 1,
            binding: self.id.clone(),
            sequence: self.sequence,
            predecessor: None,
            actor: self.binding.actor.clone(),
            recorded_at: at(30),
            transition,
        };
        let object = CanonicalObject::mint(&event).unwrap();
        (event, object)
    }

    fn append(&mut self, object: &CanonicalObject) -> Result<(), StoreError> {
        let tx = self.store.connection.transaction()?;
        put(&tx, EVENT_KIND, object)?;
        documents::record_on(&tx, EVENT_KIND, object)?;
        tx.commit()?;
        Ok(())
    }

    fn intent(
        &self,
        basis: DeltaBasis,
        changes: BTreeMap<WorkId, Option<TaskDocument>>,
        output: OutputMembership,
        local_keys: std::collections::BTreeSet<WorkId>,
    ) -> (DeltaIntent, DeltaCandidatePayload) {
        let operation = uuid::Uuid::new_v4();
        let payload = DeltaCandidatePayload {
            format: DELTA_PAYLOAD_FORMAT.into(),
            project_id: self.binding.project.clone(),
            operation,
            output: output.clone(),
            documents: changes,
        };
        let direction = match &basis {
            DeltaBasis::Incoming { .. } => Direction::Incoming,
            DeltaBasis::Outgoing { .. } => Direction::Outgoing,
        };
        let intent = DeltaIntent {
            operation,
            key: operation.to_string(),
            direction,
            actor: self.binding.actor.clone(),
            producer: comparison(&"producer"),
            expected_head: None,
            common_commit: None,
            local_cut: self.cut.clone(),
            basis,
            candidate: RetainedDeltaCandidate {
                operation,
                directory: self.binding.repository.git_directory.join("selected"),
                content_fingerprint: comparison(&payload),
                output,
                changed: local_keys,
            },
        };
        (intent, payload)
    }
}

fn commit(operation: uuid::Uuid) -> RetainedCommit {
    let content = comparison(&operation);
    RetainedCommit {
        oid: content.as_str()[..40].into(),
        tree: comparison(&(operation, "tree")).as_str()[..40].into(),
        parents: Vec::new(),
        retention_ref: format!("refs/engram/task-sync/exchange/{operation}/commit"),
        raw_bytes_fingerprint: content,
    }
}

#[test]
fn delta_membership_advances_source_against_its_own_basis_and_rebuilds_without_projections() {
    let mut f = Fixture::new(2000);
    let original = f
        .original
        .values()
        .map(|(_, task)| task.clone())
        .collect::<Vec<_>>();
    let mut edited = original[0].clone();
    edited.title = "Incoming edited text".into();
    let mut addition = edited.clone();
    addition.id = WorkId::new();
    addition.title = "Incoming addition".into();
    let changes = BTreeMap::from([
        (edited.id, Some(edited.clone())),
        (original[1].id, None),
        (addition.id, Some(addition.clone())),
    ]);
    let mut expected = f.original.clone();
    expected.remove(&original[1].id);
    expected.insert(edited.id, (ObjectId::mint(), edited.clone()));
    expected.insert(addition.id, (ObjectId::mint(), addition.clone()));
    let output = OutputMembership {
        inventory_root: expected
            .values()
            .map(|(_, task)| task)
            .try_fold(InventoryRoot::ZERO, InventoryRoot::including)
            .unwrap(),
        profile: f.binding.profile.clone(),
    };
    let baseline = CanonicalObject::mint(&TaskSyncImportPlanning {
        project_id: f.binding.project.clone(),
        previous: None,
        changes: expected
            .iter()
            .map(|(id, (_, task))| (*id, Some(task.clone())))
            .collect(),
        inventory_root: output.inventory_root.hex(),
        export_basis: Some("already newer local baseline".into()),
        profile: f.binding.profile.clone(),
        created_at: at(29),
    })
    .unwrap();
    put(&f.store.connection, IMPORT_PLANNING_KIND, &baseline).unwrap();
    let baseline = baseline.key().clone();
    // Local planning already carries the incoming result. Its delta is empty;
    // source membership still has three actual changes against the old source.
    let basis = DeltaBasis::Incoming {
        source_before: Box::new(IncomingSourceBasis {
            anchor: IncomingSourceAnchor::Bootstrap {
                checkpoint: f.source.clone(),
            },
            snapshot: MembershipSnapshot {
                commit: f.source_commit.clone(),
                membership: f.membership.clone(),
            },
        }),
        source_after: Box::new(MembershipSnapshot {
            commit: commit(uuid::Uuid::new_v4()),
            membership: output.clone(),
        }),
        source_changed: changes.keys().copied().collect(),
        local_before: Box::new(DestinationPlanningBasis {
            baseline: baseline.clone(),
            membership: output.clone(),
        }),
    };
    let (intent, payload) = f.intent(
        basis,
        changes.clone(),
        output.clone(),
        std::collections::BTreeSet::new(),
    );
    let (prepared_event, prepared) = f.event(Transition::DeltaPrepared {
        intent: Box::new(intent.clone()),
        payload: Box::new(payload),
    });
    f.append(&prepared).unwrap();
    let local = CanonicalObject::mint(&TaskSyncImportPlanning {
        project_id: f.binding.project.clone(),
        previous: Some(baseline),
        changes: BTreeMap::new(),
        inventory_root: output.inventory_root.hex(),
        export_basis: Some("selected local destination".into()),
        profile: f.binding.profile.clone(),
        created_at: at(31),
    })
    .unwrap();
    put(&f.store.connection, IMPORT_PLANNING_KIND, &local).unwrap();
    let planning = CanonicalObject::mint(&DeltaPlanning {
        project_id: f.binding.project.clone(),
        prepared: prepared.key().clone(),
        local_planning: local.key().clone(),
        source_changes: changes
            .iter()
            .map(|(id, task)| {
                (
                    *id,
                    task.as_ref().map(|_| SourceMemberRef {
                        record_id: prepared.key().clone(),
                    }),
                )
            })
            .collect(),
    })
    .unwrap();
    put(&f.store.connection, DELTA_PLANNING_KIND, &planning).unwrap();
    let receipt = DeltaImportReceipt {
        planning: planning.key().clone(),
        after_cut: f.cut.clone(),
        before: Vec::new(),
        tasks: Vec::new(),
        holds: Vec::new(),
        terminal_evidence: Vec::new(),
    };
    let mut wrong: DeltaPlanning = planning.decode().unwrap();
    wrong.source_changes.insert(
        edited.id,
        Some(SourceMemberRef {
            record_id: f.source.clone(),
        }),
    );
    let wrong = CanonicalObject::mint(&wrong).unwrap();
    put(&f.store.connection, DELTA_PLANNING_KIND, &wrong).unwrap();
    let (_, refused) = f.event(Transition::DeltaImportApplied {
        prepared: prepared.key().clone(),
        receipt: Box::new(DeltaImportReceipt {
            planning: wrong.key().clone(),
            ..receipt.clone()
        }),
    });
    let before = state(&f.store);
    assert!(
        f.append(&refused)
            .unwrap_err()
            .to_string()
            .contains("source link differs")
    );
    assert_eq!(state(&f.store), before);
    let (outcome, applied) = f.event(Transition::DeltaImportApplied {
        prepared: prepared.key().clone(),
        receipt: Box::new(receipt),
    });
    MEMBER_READS.with(|count| count.set(0));
    graph::DELTA_NODE_READS.with(|count| count.set(0));
    graph::DELTA_MEMBER_DECODES.with(|count| count.set(0));
    f.append(&applied).unwrap();
    assert_eq!(MEMBER_READS.with(std::cell::Cell::get), 2);
    assert_eq!(graph::DELTA_NODE_READS.with(std::cell::Cell::get), 3);
    assert_eq!(graph::DELTA_MEMBER_DECODES.with(std::cell::Cell::get), 2);
    crate::canonical::reset_canonical_decode_count();
    let selected = f
        .store
        .capture_task_sync_membership_component(
            &f.id,
            crate::task_sync::TaskSyncMembershipView::Source,
            [edited.id],
        )
        .unwrap();
    assert_eq!(selected.head, *applied.key());
    assert_eq!(selected.membership, output);
    assert_eq!(selected.component.documents[&edited.id], edited);
    assert_eq!(crate::canonical::canonical_decode_count(), 1);
    let heads = source::expected(&f.store.connection).unwrap();
    let (head, source) = &heads[&(f.id.to_string(), "source".into())];
    assert_eq!(head, applied.key());
    assert_eq!(source.members.len(), 2000);
    assert_eq!(source.output, output);
    assert_eq!(
        source.members[&edited.id],
        (prepared.key().clone(), edited.clone())
    );
    assert_eq!(
        source.members[&addition.id],
        (prepared.key().clone(), addition.clone())
    );
    assert!(!source.members.contains_key(&original[1].id));
    assert_eq!(heads[&(f.id.to_string(), "export".into())].0, f.export);
    assert!(
        resolve_on(
            &f.store.connection,
            prepared.key(),
            &prepared_event,
            &f.binding,
            false
        )
        .unwrap()
        .is_none()
    );
    let resolved = resolve_on(
        &f.store.connection,
        applied.key(),
        &outcome,
        &f.binding,
        false,
    )
    .unwrap()
    .unwrap();
    assert_eq!(resolved.changes.len(), 3);

    let (outgoing, payload) = f.intent(
        DeltaBasis::Outgoing {
            export_before: Box::new(PreparedExportBasis {
                anchor: PreparedExportAnchor::Bootstrap {
                    checkpoint: f.export.clone(),
                },
                membership: f.membership.clone(),
                cut: f.cut.clone(),
            }),
            confirmed_before: None,
        },
        changes,
        output.clone(),
        resolved.changes.keys().copied().collect(),
    );
    let (_, exported) = f.event(Transition::DeltaPrepared {
        intent: Box::new(outgoing),
        payload: Box::new(payload),
    });
    f.append(&exported).unwrap();
    crate::canonical::reset_canonical_decode_count();
    let selected = f
        .store
        .capture_task_sync_membership_component(
            &f.id,
            crate::task_sync::TaskSyncMembershipView::Export,
            [edited.id],
        )
        .unwrap();
    assert_eq!(selected.head, *exported.key());
    assert_eq!(selected.membership, output);
    assert_eq!(selected.component.documents[&edited.id], edited);
    assert_eq!(crate::canonical::canonical_decode_count(), 1);
    let mut invalid = Vec::new();
    let mut checked = 0;
    source::verify(&f.store.connection, &mut checked, &mut invalid).unwrap();
    graph::verify(&f.store.connection, &mut checked, &mut invalid).unwrap();
    assert_eq!(invalid, Vec::<String>::new());
    let before = state(&f.store);
    assert!(f.append(&exported).is_err());
    assert_eq!(state(&f.store), before);
    let mut later = edited.clone();
    later.title = "Later local export".into();
    let later_output = OutputMembership {
        inventory_root: output
            .inventory_root
            .excluding(&edited)
            .unwrap()
            .including(&later)
            .unwrap(),
        profile: output.profile.clone(),
    };
    let (outgoing, payload) = f.intent(
        DeltaBasis::Outgoing {
            export_before: Box::new(PreparedExportBasis {
                anchor: PreparedExportAnchor::Prepared {
                    event: exported.key().clone(),
                },
                membership: output.clone(),
                cut: f.cut.clone(),
            }),
            confirmed_before: None,
        },
        BTreeMap::from([(later.id, Some(later.clone()))]),
        later_output,
        std::collections::BTreeSet::from([later.id]),
    );
    let (_, interrupted) = f.event(Transition::DeltaPrepared {
        intent: Box::new(outgoing),
        payload: Box::new(payload),
    });
    f.store.connection.execute_batch("CREATE TEMP TRIGGER refuse_delta_node BEFORE INSERT ON work_task_sync_graph_nodes
        WHEN NEW.view_kind='export' BEGIN SELECT RAISE(ABORT,'injected delta projection failure'); END;").unwrap();
    let before = state(&f.store);
    assert!(matches!(f.append(&interrupted), Err(StoreError::Sqlite(_))));
    assert_eq!(state(&f.store), before);
    f.store
        .connection
        .execute_batch("DROP TRIGGER refuse_delta_node")
        .unwrap();
    f.store
        .connection
        .execute(
            "DELETE FROM work_task_sync_graph_nodes WHERE view_kind IN ('source','export')",
            [],
        )
        .unwrap();
    f.store
        .connection
        .execute("DELETE FROM work_task_sync_source_heads", [])
        .unwrap();
    source::rebuild(&f.store.connection).unwrap();
    graph::rebuild(&f.store.connection).unwrap();
    let rebuilt = source::expected(&f.store.connection).unwrap();
    assert_eq!(
        rebuilt[&(f.id.to_string(), "source".into())].0,
        *applied.key()
    );
    assert_eq!(
        rebuilt[&(f.id.to_string(), "export".into())].0,
        *exported.key()
    );
    invalid.clear();
    graph::verify(&f.store.connection, &mut checked, &mut invalid).unwrap();
    assert_eq!(invalid, Vec::<String>::new());
}

#[test]
fn source_heads_derive_a_history_of_more_than_half_the_admitted_records() {
    // Every selected event names the same binding. Charging that binding once
    // per event used to exhaust the shared read budget at just over half of the
    // ledger's admitted record bound, although the unique history fits it.
    let mut f = Fixture::new(1);
    let count = crate::storage::work::task_sync::ledger::MAX_RECORDS / 2 + 1;
    let events: Vec<_> = (0..count)
        .map(|_| {
            let operation = uuid::Uuid::new_v4();
            f.event(Transition::CustodyCheckpoint {
                checkpoint: Box::new(ManagedCustodyCheckpoint {
                    operation,
                    key: operation.to_string(),
                    request_fingerprint: comparison(&operation),
                    previous_checkpoint: None,
                    local_cut: f.cut.clone(),
                    reason: CheckpointReason::NamedAudit,
                    seed: None,
                    artifacts: Vec::new(),
                }),
            })
            .1
        })
        .collect();
    let tx = f.store.connection.transaction().unwrap();
    for object in &events {
        put(&tx, EVENT_KIND, object).unwrap();
    }
    tx.commit().unwrap();
    let heads = source::expected(&f.store.connection).unwrap();
    let (head, membership) = &heads[&(f.id.to_string(), "source".into())];
    assert_eq!(*head, f.source);
    assert_eq!(membership.members, f.original);
    assert_eq!(heads[&(f.id.to_string(), "export".into())].0, f.export);
    let mut invalid = Vec::new();
    let mut checked = 0;
    source::rebuild(&f.store.connection).unwrap();
    source::verify(&f.store.connection, &mut checked, &mut invalid).unwrap();
    assert_eq!(invalid, Vec::<String>::new());

    // A damaged chain still refuses: an event whose binding is missing.
    let missing = ObjectId::mint();
    let operation = uuid::Uuid::new_v4();
    let (_, damaged) = f.event(Transition::CustodyCheckpoint {
        checkpoint: Box::new(ManagedCustodyCheckpoint {
            operation,
            key: operation.to_string(),
            request_fingerprint: comparison(&operation),
            previous_checkpoint: None,
            local_cut: f.cut.clone(),
            reason: CheckpointReason::NamedAudit,
            seed: None,
            artifacts: Vec::new(),
        }),
    });
    let mut event: Event = damaged.decode().unwrap();
    event.binding = missing;
    let damaged = CanonicalObject::mint(&event).unwrap();
    put(&f.store.connection, EVENT_KIND, &damaged).unwrap();
    let Err(error) = source::expected(&f.store.connection) else {
        panic!("a missing binding must refuse derivation");
    };
    assert!(
        error.to_string().contains("exchange binding is missing"),
        "{error}"
    );
    source::verify(&f.store.connection, &mut checked, &mut invalid).unwrap();
    assert_eq!(invalid.len(), 1, "{invalid:?}");
    assert!(
        invalid[0].starts_with("work_task_sync_source_heads:underivable"),
        "{invalid:?}"
    );
    // Repair reports the same finding and keeps the heads it cannot rederive.
    let before = state(&f.store);
    let findings = source::rebuild(&f.store.connection).unwrap();
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(
        findings[0].starts_with("work_task_sync_source_heads:underivable"),
        "{findings:?}"
    );
    assert_eq!(state(&f.store), before);
}

#[test]
fn delta_membership_bad_root_and_missing_removal_preserve_the_old_view() {
    let f = Fixture::new(2);
    let before = f.original.clone();
    let mut delta = MembershipDelta {
        role: "source",
        before_head: f.source.clone(),
        before: f.membership.clone(),
        output: f.membership.clone(),
        changes: BTreeMap::from([(WorkId::new(), None)]),
    };
    let mut held = before.clone();
    assert!(
        delta
            .apply(&mut held, &f.membership)
            .unwrap_err()
            .to_string()
            .contains("absent source/export member")
    );
    assert_eq!(held, before);
    delta.changes = BTreeMap::from([(*before.keys().next().unwrap(), None)]);
    assert!(
        delta
            .apply(&mut held, &f.membership)
            .unwrap_err()
            .to_string()
            .contains("membership root differs")
    );
    assert_eq!(held, before);
    assert!(
        delta
            .require_before(&ObjectId::mint(), &f.membership)
            .is_err()
    );
}
