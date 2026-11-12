use super::*;
use crate::storage::work::task_sync::{
    ledger::{Ledger, view},
    tests::ledger::binding,
};
use crate::task_sync::{
    delta::{
        DeltaBasis, DeltaImportReceipt, DeltaImportedTask, DestinationPlanningBasis,
        IncomingSourceAnchor, IncomingSourceBasis, MembershipSnapshot, OriginQualifiedCut,
        OutputMembership, PreparedExportAnchor, PreparedExportBasis, RetainedDeltaCandidate,
    },
    inventory::InventoryRoot,
    ledger::{ImportReceipt, RetainedCandidate},
    reconciliation::{ReconciliationRunBasis, ReconciliationWorkBasis},
};
use crate::{CanonicalObject, FeedId, FeedPosition, WorkId};

mod coverage;
mod deferred;
mod evidence;
mod outgoing;

fn comparison(label: &str) -> ObjectId {
    CanonicalObject::freeze(&label).unwrap().key().clone()
}

fn commit(operation: uuid::Uuid, label: &str) -> RetainedCommit {
    RetainedCommit {
        oid: comparison(label).to_string(),
        tree: comparison(&format!("tree {label}")).to_string(),
        parents: Vec::new(),
        retention_ref: format!("refs/engram/task-sync/exchange/{operation}/commit"),
        raw_bytes_fingerprint: comparison(&format!("raw {label}")),
    }
}

struct Fixture {
    id: ObjectId,
    binding: Binding,
    intent: DeltaIntent,
    chain: Vec<(ObjectId, Event)>,
    documents: BTreeMap<WorkId, Option<crate::task_sync::TaskDocument>>,
}

impl Fixture {
    fn new(direction: Direction) -> Self {
        let root = std::env::current_dir()
            .unwrap()
            .join("target/delta-reducer-shape");
        let mut binding = binding(&root);
        let origin = ObjectId::mint();
        binding.store.origin = Some(origin.clone());
        let id = ObjectId::mint();
        let operation = uuid::Uuid::new_v4();
        let local_cut = OriginQualifiedCut {
            binding: id.clone(),
            origin,
            restore_occurrence: None,
            position: FeedPosition {
                feed: FeedId::Project(binding.project.clone()),
                position: 0,
            },
            anchor: None,
        };
        let membership = OutputMembership {
            inventory_root: InventoryRoot::ZERO,
            profile: binding.profile.clone(),
        };
        let basis = match direction {
            Direction::Outgoing => DeltaBasis::Outgoing {
                export_before: Box::new(PreparedExportBasis {
                    anchor: PreparedExportAnchor::Bootstrap {
                        checkpoint: ObjectId::mint(),
                    },
                    membership: membership.clone(),
                    cut: local_cut.clone(),
                }),
                confirmed_before: None,
            },
            Direction::Incoming => DeltaBasis::Incoming {
                source_before: Box::new(IncomingSourceBasis {
                    anchor: IncomingSourceAnchor::Bootstrap {
                        checkpoint: ObjectId::mint(),
                    },
                    snapshot: MembershipSnapshot {
                        commit: commit(operation, "before"),
                        membership: membership.clone(),
                    },
                }),
                source_after: Box::new(MembershipSnapshot {
                    commit: commit(operation, "after"),
                    membership: membership.clone(),
                }),
                source_changed: BTreeSet::new(),
                local_before: Box::new(DestinationPlanningBasis {
                    baseline: ObjectId::mint(),
                    membership: membership.clone(),
                }),
            },
        };
        let intent = DeltaIntent {
            operation,
            key: operation.to_string(),
            direction,
            actor: binding.actor.clone(),
            producer: comparison("producer"),
            expected_head: None,
            common_commit: None,
            local_cut,
            basis,
            candidate: RetainedDeltaCandidate {
                operation,
                directory: root.join("selected"),
                content_fingerprint: comparison("payload"),
                output: membership,
                changed: BTreeSet::new(),
            },
        };
        let mut fixture = Self {
            id,
            binding,
            intent,
            chain: Vec::new(),
            documents: BTreeMap::new(),
        };
        fixture.push(Transition::Activated);
        fixture
    }

    fn push(&mut self, transition: Transition) -> ObjectId {
        let id = ObjectId::mint();
        self.chain.push((
            id.clone(),
            Event {
                schema_version: 1,
                binding: self.id.clone(),
                sequence: self.chain.len() as u64 + 1,
                predecessor: self.chain.last().map(|(id, _)| id.clone()),
                actor: self.binding.actor.clone(),
                recorded_at: self.binding.created_at,
                transition,
            },
        ));
        id
    }

    fn prepare(&mut self) -> ObjectId {
        let payload = crate::task_sync::delta::DeltaCandidatePayload {
            format: crate::task_sync::delta::DELTA_PAYLOAD_FORMAT.into(),
            project_id: self.binding.project.clone(),
            operation: self.intent.operation,
            output: self.intent.candidate.output.clone(),
            documents: self.documents.clone(),
        };
        self.intent.candidate.content_fingerprint =
            CanonicalObject::freeze(&payload).unwrap().key().clone();
        self.push(Transition::DeltaPrepared {
            intent: Box::new(self.intent.clone()),
            payload: Box::new(payload),
        })
    }

    fn reduced(&self) -> Result<ChainView, StoreError> {
        reduce(
            None,
            &self.binding,
            &self
                .chain
                .iter()
                .map(|(id, event)| (id, event))
                .collect::<Vec<_>>(),
        )
    }

    fn receipt(&self) -> DeltaImportReceipt {
        DeltaImportReceipt {
            planning: ObjectId::mint(),
            after_cut: self.intent.local_cut.clone(),
            before: Vec::new(),
            tasks: Vec::new(),
            holds: Vec::new(),
            terminal_evidence: Vec::new(),
        }
    }

    fn legacy(&self) -> Intent {
        Intent {
            operation: uuid::Uuid::new_v4(),
            key: "legacy request".into(),
            direction: self.intent.direction,
            actor: self.intent.actor.clone(),
            producer: comparison("producer"),
            expected_head: None,
            common_commit: None,
            candidate: RetainedCandidate {
                operation: uuid::Uuid::new_v4(),
                directory: self.intent.candidate.directory.clone(),
                documents_fingerprint: comparison("documents"),
                planning_fingerprint: comparison("planning"),
                source_cut: self.binding.store.cut.clone(),
            },
            commit: commit(uuid::Uuid::new_v4(), "legacy"),
            local_planning_basis: comparison("local"),
        }
    }
}

#[test]
fn delta_outgoing_selects_one_real_commit_and_keeps_unknown_publication_unresolved() {
    let mut f = Fixture::new(Direction::Outgoing);
    let prepared = f.prepare();
    let result = f.reduced().unwrap();
    assert_eq!(result.head.stage, Stage::Prepared);
    assert_eq!(result.head.pending, Some(prepared.clone()));
    f.push(Transition::PushStarted {
        prepared: prepared.clone(),
    });
    assert!(f.reduced().is_err());
    f.chain.pop();
    f.push(Transition::LocalCommitRecorded {
        prepared: prepared.clone(),
    });
    assert!(f.reduced().is_err());
    f.chain.pop();
    let selected = commit(f.intent.operation, "actual outgoing commit");
    f.push(Transition::DeltaLocalCommitRecorded {
        prepared: prepared.clone(),
        commit: selected.clone(),
    });
    assert_eq!(f.reduced().unwrap().head.stage, Stage::Committed);
    f.push(Transition::DeltaLocalCommitRecorded {
        prepared: prepared.clone(),
        commit: commit(f.intent.operation, "replacement"),
    });
    assert!(f.reduced().is_err());
    f.chain.pop();
    f.push(Transition::PushStarted {
        prepared: prepared.clone(),
    });
    f.push(Transition::PushObserved {
        prepared: prepared.clone(),
        observation: PublicationObservation::Unknown { observed_tip: None },
    });
    let result = f.reduced().unwrap();
    assert_eq!(result.head.stage, Stage::Unknown);
    assert!(
        result.operations[&f.intent.operation.to_string()]
            .completed
            .is_none()
    );
    f.prepare();
    assert!(f.reduced().is_err());
    f.chain.pop();
    f.push(Transition::PushObserved {
        prepared: prepared.clone(),
        observation: PublicationObservation::Confirmed {
            observed_tip: comparison("other tip").to_string(),
        },
    });
    assert!(f.reduced().is_err());
    f.chain.pop();
    let outcome = f.push(Transition::PushObserved {
        prepared,
        observation: PublicationObservation::Confirmed {
            observed_tip: selected.oid,
        },
    });
    let result = f.reduced().unwrap();
    assert_eq!(result.head.stage, Stage::Settled);
    assert_eq!(result.head.last_outgoing, Some(outcome.clone()));
    assert_eq!(
        result.operations[&f.intent.operation.to_string()].completed,
        Some(outcome)
    );
}

#[test]
fn legacy_and_delta_settle_in_one_chain_but_cannot_use_each_others_transitions() {
    let mut f = Fixture::new(Direction::Incoming);
    let legacy = f.legacy();
    let prepared = f.push(Transition::Prepared {
        intent: Box::new(legacy.clone()),
    });
    f.push(Transition::DeltaImportApplied {
        prepared: prepared.clone(),
        receipt: Box::new(f.receipt()),
    });
    assert!(f.reduced().is_err());
    f.chain.pop();
    let receipt = ImportReceipt {
        source_commit: legacy.commit.oid,
        source_tree: legacy.commit.tree,
        before_basis: legacy.local_planning_basis,
        after_basis: legacy.candidate.planning_fingerprint,
        notes_included: false,
        tasks: Vec::new(),
    };
    f.push(Transition::ImportApplied {
        prepared,
        receipt: receipt.clone(),
    });
    let delta = f.prepare();
    f.push(Transition::ImportApplied {
        prepared: delta.clone(),
        receipt,
    });
    assert!(f.reduced().is_err());
    f.chain.pop();
    let outcome = f.push(Transition::DeltaImportApplied {
        prepared: delta.clone(),
        receipt: Box::new(f.receipt()),
    });
    let full = f.reduced().unwrap();
    assert_eq!(full.operations.len(), 2);
    assert_eq!(full.head.last_incoming, Some(outcome));
    let selected = operation(
        &f.binding,
        &f.chain[3..]
            .iter()
            .map(|(id, event)| (id, event))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert_eq!(
        selected.operations[&f.intent.operation.to_string()],
        full.operations[&f.intent.operation.to_string()]
    );
    let connection = Connection::open_in_memory().unwrap();
    assert!(
        reduce(
            Some(&connection),
            &f.binding,
            &f.chain
                .iter()
                .map(|(id, event)| (id, event))
                .collect::<Vec<_>>(),
        )
        .is_err()
    );
    // Pure shape replay cannot supply the canonical source/baseline links the
    // incoming writer requires, even when no imported member needs resolving.
    assert_eq!(delta, f.chain[3].0);
}

#[test]
fn delta_receipt_refuses_changed_observations_duplicate_members_and_bad_run_bases() {
    let mut f = Fixture::new(Direction::Incoming);
    f.intent.local_cut.position.position = 1;
    f.intent.local_cut.anchor = Some(crate::task_sync::progress::FeedAnchor {
        object_id: ObjectId::mint(),
        object_kind: "work_event".into(),
    });
    let prepared = f.prepare();
    let receipt = f.receipt();
    let task = WorkId::new();
    for defect in [
        "binding",
        "origin",
        "restore",
        "backwards",
        "anchor",
        "task",
        "before",
        "revision",
        "active_run",
        "run_revision",
        "hold",
        "evidence",
    ] {
        let mut invalid = receipt.clone();
        match defect {
            "binding" => invalid.after_cut.binding = ObjectId::mint(),
            "origin" => invalid.after_cut.origin = ObjectId::mint(),
            "restore" => invalid.after_cut.restore_occurrence = Some(uuid::Uuid::new_v4()),
            "backwards" => {
                invalid.after_cut.position.position = 0;
                invalid.after_cut.anchor = None;
            }
            "anchor" => {
                invalid.after_cut.anchor = Some(crate::task_sync::progress::FeedAnchor {
                    object_id: ObjectId::mint(),
                    object_kind: "work_event".into(),
                });
            }
            "task" => {
                let member = DeltaImportedTask {
                    task,
                    previous_head: None,
                    imported_head: Some(ObjectId::mint()),
                    local_record_ids: Vec::new(),
                };
                invalid.tasks = vec![member.clone(), member];
            }
            "before" | "revision" => {
                let before = ReconciliationWorkBasis {
                    work_id: task,
                    revision: i64::from(defect != "revision"),
                    head: None,
                    imported_head: None,
                    active_run_id: None,
                    runs: Vec::new(),
                };
                invalid.before = if defect == "before" {
                    vec![before.clone(), before]
                } else {
                    vec![before]
                };
            }
            "hold" => {
                let id = ObjectId::mint();
                invalid.holds = vec![id.clone(), id];
            }
            "active_run" | "run_revision" => {
                let run_id = crate::WorkRunId(uuid::Uuid::new_v4());
                invalid.before.push(ReconciliationWorkBasis {
                    work_id: task,
                    revision: 1,
                    head: None,
                    imported_head: None,
                    active_run_id: Some(run_id),
                    runs: if defect == "active_run" {
                        Vec::new()
                    } else {
                        vec![ReconciliationRunBasis {
                            run_id,
                            generation: 1,
                            revision: 0,
                            claim_id: None,
                            claim_fence: 0,
                        }]
                    },
                });
            }
            "evidence" => {
                let id = ObjectId::mint();
                invalid.terminal_evidence = vec![id.clone(), id];
            }
            _ => unreachable!(),
        }
        f.push(Transition::DeltaImportApplied {
            prepared: prepared.clone(),
            receipt: Box::new(invalid),
        });
        assert!(f.reduced().is_err(), "{defect}");
        f.chain.pop();
    }
    f.push(Transition::DeltaImportApplied {
        prepared,
        receipt: Box::new(receipt),
    });
    assert_eq!(f.reduced().unwrap().head.stage, Stage::Settled);
}

#[test]
fn keyed_delta_read_uses_the_shared_reducer_but_requires_canonical_bases_for_admission() {
    let directory = crate::test_support::temp_home().unwrap();
    let mut store = crate::storage::SqliteStore::open(directory.path().join("work.db")).unwrap();
    let mut f = Fixture::new(Direction::Incoming);
    let prepared = f.prepare();
    let outcome = f.push(Transition::DeltaImportApplied {
        prepared: prepared.clone(),
        receipt: Box::new(f.receipt()),
    });
    let expected = f.reduced().unwrap();
    let transaction = store.connection.transaction().unwrap();
    let object = CanonicalObject::identified(&f.id, &f.binding).unwrap();
    crate::storage::SqliteStore::insert_object(&transaction, super::super::BINDING_KIND, &object)
        .unwrap();
    for (id, event) in &f.chain {
        let object = CanonicalObject::identified(id, event).unwrap();
        crate::storage::SqliteStore::insert_object(&transaction, super::super::EVENT_KIND, &object)
            .unwrap();
    }
    let ledger = Ledger {
        bindings: BTreeMap::from([(f.id.clone(), f.binding.clone())]),
        events: f.chain.iter().cloned().collect(),
        bytes: 0,
    };
    view::record(&transaction, &ledger, &f.chain.last().unwrap().1).unwrap();
    let selected = view::read::operation_on(
        &transaction,
        &f.id,
        &f.binding,
        &prepared,
        &mut view::read::ReadBudget::default(),
    )
    .unwrap();
    assert!(matches!(
        selected.intent,
        view::read::CheckedRequest::Delta(_)
    ));
    assert_eq!(
        selected.progress,
        expected.operations[&f.intent.operation.to_string()]
    );
    assert_eq!(selected.progress.completed, Some(outcome.clone()));
    let tail = view::read::tail_on(
        &transaction,
        &f.id,
        &f.binding,
        &mut view::read::ReadBudget::default(),
    )
    .unwrap();
    assert_eq!(tail.id, outcome);
    assert!(ledger.validate(&transaction).is_err());
    transaction.rollback().unwrap();
    let count: i64 = store
        .connection
        .query_row(
            "SELECT count(*) FROM objects WHERE object_kind='task_sync_exchange_event'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn delta_preparation_owns_selected_documents_and_rebuilds_without_admitting_membership() {
    use crate::storage::work::task_sync::closure::documents;
    let mut store = crate::storage::SqliteStore::open_in_memory().unwrap();
    let mut f = Fixture::new(Direction::Incoming);
    let mut selected = crate::storage::work::task_sync::tests::candidate()
        .tasks
        .remove(0);
    selected.parent_id = None;
    let mut removed = selected.clone();
    removed.id = WorkId::new();
    let old_root = InventoryRoot::ZERO.including(&removed).unwrap();
    let new_root = InventoryRoot::ZERO.including(&selected).unwrap();
    f.intent.candidate.output.inventory_root = new_root;
    f.intent.candidate.changed = BTreeSet::from([selected.id, removed.id]);
    let DeltaBasis::Incoming {
        source_before,
        source_after,
        source_changed,
        local_before,
    } = &mut f.intent.basis
    else {
        panic!("incoming")
    };
    source_before.snapshot.membership.inventory_root = old_root;
    source_after.membership.inventory_root = new_root;
    *source_changed = f.intent.candidate.changed.clone();
    local_before.membership.inventory_root = old_root;
    f.documents = BTreeMap::from([(selected.id, Some(selected.clone())), (removed.id, None)]);
    let prepared = f.prepare();
    f.reduced().unwrap();
    let event = f.chain.last().unwrap().1.clone();
    let object = CanonicalObject::identified(&prepared, &event).unwrap();
    assert_ne!(object.key(), &f.intent.candidate.content_fingerprint);
    let transaction = store.connection.transaction().unwrap();
    crate::storage::SqliteStore::insert_object(&transaction, super::super::EVENT_KIND, &object)
        .unwrap();
    view::READS.with(|count| count.set(0));
    documents::record_on(&transaction, super::super::EVENT_KIND, &object).unwrap();
    assert_eq!(
        view::READS.with(std::cell::Cell::get),
        0,
        "incoming document preparation does not read a binding or admit membership"
    );
    let canonical =
        documents::canonical_members(super::super::EVENT_KIND, &object, &f.binding.project)
            .unwrap();
    assert_eq!(canonical, BTreeMap::from([(selected.id, selected.clone())]));
    let rows = transaction.prepare("SELECT work_id,member_path,document_json IS NOT NULL FROM work_task_sync_document_members WHERE record_id=?1 ORDER BY work_id").unwrap()
        .query_map([prepared.as_str()], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, bool>(2)?))).unwrap()
        .collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(rows.len(), 2);
    for (work, path, present) in &rows {
        assert_eq!(path, &format!("/transition/payload/documents/{work}"));
        assert_eq!(*present, work == &selected.id.0.to_string());
    }
    for table in [
        "work_task_sync_source_heads",
        "work_task_sync_graph_nodes",
        "work_task_sync_exchange_heads",
    ] {
        let count: i64 = transaction
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "document ownership does not admit {table}");
    }
    let mut checked = 0;
    let mut findings = Vec::new();
    documents::verify(&transaction, &mut checked, &mut findings).unwrap();
    assert!(findings.is_empty(), "{findings:?}");
    transaction
        .execute(
            "UPDATE work_task_sync_document_members SET member_path='/wrong',document_json=NULL",
            [],
        )
        .unwrap();
    documents::verify(&transaction, &mut checked, &mut findings).unwrap();
    assert!(
        findings
            .iter()
            .any(|finding| finding == "work_task_sync_document_members:content")
    );
    assert_eq!(
        documents::rebuild(&transaction).unwrap(),
        Vec::<String>::new()
    );
    findings.clear();
    documents::verify(&transaction, &mut checked, &mut findings).unwrap();
    assert!(findings.is_empty(), "{findings:?}");
    let bytes: Vec<u8> = transaction
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id=?1",
            [prepared.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(bytes.as_slice(), object.bytes());
    let mut changed = event;
    let Transition::DeltaPrepared { intent, payload } = &mut changed.transition else {
        panic!("prepared")
    };
    payload
        .documents
        .get_mut(&selected.id)
        .unwrap()
        .as_mut()
        .unwrap()
        .title = "different bytes".into();
    assert!(payload.validate(&f.id, &f.binding, intent).is_err());
    let changed = CanonicalObject::mint(&changed).unwrap();
    assert_eq!(
        documents::canonical_members(super::super::EVENT_KIND, &changed, &f.binding.project)
            .unwrap()[&selected.id]
            .title,
        "different bytes"
    );
    let mut wrong: Event = changed.decode().unwrap();
    let Transition::DeltaPrepared { payload, .. } = &mut wrong.transition else {
        unreachable!()
    };
    payload.documents.values_mut().flatten().next().unwrap().id = WorkId::new();
    assert!(
        documents::canonical_members(
            super::super::EVENT_KIND,
            &CanonicalObject::mint(&wrong).unwrap(),
            &f.binding.project,
        )
        .is_err()
    );
    transaction.rollback().unwrap();
}
