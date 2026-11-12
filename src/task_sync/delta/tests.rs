use super::*;
use crate::domain::AssuranceLevel;
use crate::task_sync::{
    SharedActor,
    ledger::{Binding, RepositoryBasis, StoreBasis, TargetBasis},
    notes::{SharedNote, SharedNoteKind},
    progress::ExportCut,
};
use crate::{CanonicalObject, ChildRequirement, FeedId, SessionId, WorkItemKind, WorkLifecycle};

mod checkpoint;
mod historical;
mod retention;
mod successor;

#[test]
fn delta_receipt_heads_allow_shared_planning_owners_without_reinterpreting_legacy_versions() {
    let fixture = fixture();
    let shared_planning = ObjectId::mint();
    let mut receipt = DeltaImportReceipt {
        planning: ObjectId::mint(),
        after_cut: fixture.intent.local_cut.clone(),
        before: Vec::new(),
        tasks: fixture
            .intent
            .candidate
            .changed
            .iter()
            .map(|&task| DeltaImportedTask {
                task,
                previous_head: Some(ObjectId::mint()),
                imported_head: Some(shared_planning.clone()),
                local_record_ids: vec![ObjectId::mint()],
            })
            .collect(),
        holds: Vec::new(),
        terminal_evidence: Vec::new(),
    };
    // Shape only: canonical storage must still resolve each work's member,
    // including an explicit None member for a removed exchange task.
    receipt
        .validate(&fixture.binding_id, &fixture.binding, &fixture.intent)
        .unwrap();
    assert_eq!(receipt.tasks.len(), 2);
    let duplicate = receipt.tasks[0].clone();
    receipt.tasks.push(duplicate);
    assert!(
        receipt
            .validate(&fixture.binding_id, &fixture.binding, &fixture.intent)
            .is_err()
    );
    receipt.tasks.pop();
    receipt.tasks[1].local_record_ids = receipt.tasks[0].local_record_ids.clone();
    assert!(
        receipt
            .validate(&fixture.binding_id, &fixture.binding, &fixture.intent)
            .is_err()
    );
    receipt.tasks[1].local_record_ids.clear();
    receipt.tasks[1].imported_head = None;
    receipt
        .validate(&fixture.binding_id, &fixture.binding, &fixture.intent)
        .unwrap();

    let legacy = crate::task_sync::reconciliation::ReconciledTask {
        task: receipt.tasks[0].task,
        previous_head: receipt.tasks[0].previous_head.clone(),
        shared_version: ObjectId::mint(),
        local_record_ids: Vec::new(),
    };
    let legacy_wire = serde_json::to_value(&legacy).unwrap();
    assert!(legacy_wire.get("shared_version").is_some());
    assert!(serde_json::from_value::<DeltaImportedTask>(legacy_wire).is_err());
    let delta_wire = serde_json::to_value(&receipt.tasks[0]).unwrap();
    assert!(delta_wire.get("imported_head").is_some());
    assert!(
        serde_json::from_value::<crate::task_sync::reconciliation::ReconciledTask>(delta_wire)
            .is_err()
    );
}

fn comparison(value: &str) -> ObjectId {
    CanonicalObject::freeze(&value).unwrap().key().clone()
}

// Shape-only fixtures deliberately supply no actual Git or store admission.
fn commit(operation: uuid::Uuid, label: &str) -> RetainedCommit {
    RetainedCommit {
        oid: comparison(label).as_str()[..40].into(),
        tree: comparison(&format!("tree {label}")).as_str()[..40].into(),
        parents: Vec::new(),
        retention_ref: format!("refs/engram/task-sync/exchange/{operation}/commit"),
        raw_bytes_fingerprint: comparison(&format!("raw {label}")),
    }
}

fn document(title: &str) -> TaskDocument {
    TaskDocument {
        id: WorkId::new(),
        external_ref: None,
        title: title.into(),
        outcome: "Deliver work".into(),
        acceptance: vec!["Delivered".into()],
        acceptance_bindings: Vec::new(),
        evaluation_mode: None,
        kind: WorkItemKind::Task,
        priority: 1,
        labels: Vec::new(),
        assigned_to: None,
        deferred_until: None,
        lifecycle: WorkLifecycle::Open,
        parent_id: None,
        child_requirement: ChildRequirement::Required,
        prerequisites: Vec::new(),
        blockers: Vec::new(),
        superseded_by: None,
        disposal_reason: None,
        completion: None,
        notes: Vec::new(),
        activity: BTreeMap::new(),
    }
}

struct Fixture {
    binding_id: ObjectId,
    binding: Binding,
    intent: DeltaIntent,
    payload: DeltaCandidatePayload,
}

fn fixture() -> Fixture {
    let root = std::env::current_dir()
        .unwrap()
        .join("target/delta-contract-fixture");
    let actor = ActorContext {
        actor_id: "fixture".into(),
        actor_kind: "test_agent".into(),
        assurance: AssuranceLevel::Asserted,
        session_id: Some(SessionId("fixture".into())),
        run_id: None,
        source_tool: None,
        source_skill: None,
        provenance_chain: Vec::new(),
        reason: "contract fixture".into(),
    };
    let profile = ExportProfile {
        include_notes: false,
        origin_machine: None,
    };
    let origin = ObjectId::mint();
    let binding_id = ObjectId::mint();
    let binding = Binding {
        schema_version: 1,
        project: ProjectId("delta-contract".into()),
        store: StoreBasis {
            database: root.join("work.db"),
            origin: Some(origin.clone()),
            restore_occurrence: None,
            cut: ExportCut {
                position: 0,
                anchor: None,
            },
        },
        repository: RepositoryBasis {
            enrollment: uuid::Uuid::new_v4(),
            main_checkout: root.clone(),
            git_directory: root.join(".git"),
        },
        branch: "refs/heads/main".into(),
        target: TargetBasis {
            remote: "origin".into(),
            reference: "refs/heads/main".into(),
            configuration_fingerprint: comparison("target"),
        },
        profile: profile.clone(),
        supersedes: None,
        superseded_tail: None,
        actor: actor.clone(),
        created_at: chrono::Utc::now(),
    };
    let local_cut = OriginQualifiedCut {
        binding: binding_id.clone(),
        origin,
        restore_occurrence: None,
        position: FeedPosition {
            feed: FeedId::Project(binding.project.clone()),
            position: 0,
        },
        anchor: None,
    };
    let operation = uuid::Uuid::new_v4();
    let source_only = document("source-only document");
    let local_only = document("destination-only document");
    let shared = document("shared changed document");
    let source_changed = BTreeSet::from([source_only.id, shared.id]);
    let changed = BTreeSet::from([local_only.id, shared.id]);
    let documents = [source_only, local_only, shared]
        .into_iter()
        .map(|doc| (doc.id, Some(doc)))
        .collect::<BTreeMap<_, _>>();
    let output = OutputMembership {
        inventory_root: documents
            .values()
            .filter_map(Option::as_ref)
            .try_fold(InventoryRoot::ZERO, InventoryRoot::including)
            .unwrap(),
        profile: profile.clone(),
    };
    let payload = DeltaCandidatePayload {
        format: DELTA_PAYLOAD_FORMAT.into(),
        project_id: binding.project.clone(),
        operation,
        output: output.clone(),
        documents,
    };
    let intent = DeltaIntent {
        operation,
        key: operation.to_string(),
        direction: Direction::Incoming,
        actor,
        producer: comparison("producer"),
        expected_head: None,
        common_commit: None,
        local_cut,
        basis: DeltaBasis::Incoming {
            source_before: Box::new(IncomingSourceBasis {
                anchor: IncomingSourceAnchor::Applied {
                    outcome: ObjectId::mint(),
                },
                snapshot: MembershipSnapshot {
                    commit: commit(operation, "source-before"),
                    membership: OutputMembership {
                        inventory_root: InventoryRoot::ZERO
                            .including(&document("old source"))
                            .unwrap(),
                        profile: profile.clone(),
                    },
                },
            }),
            source_after: Box::new(MembershipSnapshot {
                commit: commit(operation, "resolved source-after"),
                membership: output.clone(),
            }),
            source_changed,
            local_before: Box::new(DestinationPlanningBasis {
                baseline: ObjectId::mint(),
                membership: OutputMembership {
                    inventory_root: InventoryRoot::ZERO
                        .including(&document("newer local baseline"))
                        .unwrap(),
                    profile,
                },
            }),
        },
        candidate: RetainedDeltaCandidate {
            operation,
            directory: root.join("candidate"),
            content_fingerprint: CanonicalObject::freeze(&payload).unwrap().key().clone(),
            output,
            changed,
        },
    };
    Fixture {
        binding_id,
        binding,
        intent,
        payload,
    }
}

fn refresh(fixture: &mut Fixture) {
    fixture.intent.candidate.content_fingerprint = CanonicalObject::freeze(&fixture.payload)
        .unwrap()
        .key()
        .clone();
}

#[test]
fn incoming_contracts_preserve_distinct_source_and_local_changes_with_one_payload() {
    let fixture = fixture();
    fixture
        .payload
        .validate(&fixture.binding_id, &fixture.binding, &fixture.intent)
        .unwrap();
    let DeltaBasis::Incoming {
        source_before,
        source_after,
        source_changed,
        local_before,
    } = &fixture.intent.basis
    else {
        panic!("incoming fixture")
    };
    assert_ne!(
        source_before.snapshot.membership.inventory_root,
        local_before.membership.inventory_root
    );
    assert_ne!(source_changed, &fixture.intent.candidate.changed);
    assert_eq!(
        source_after.membership.inventory_root,
        fixture.intent.candidate.output.inventory_root
    );
    assert_eq!(fixture.payload.documents.len(), 3);
    let encoded = serde_json::to_string(&fixture.intent).unwrap();
    for document in fixture.payload.documents.values().flatten() {
        assert!(!encoded.contains(&document.title));
    }
    assert_eq!(
        serde_json::from_str::<DeltaIntent>(&encoded).unwrap(),
        fixture.intent
    );
    let prepared = ObjectId::mint();
    let mut planning = DeltaPlanning {
        project_id: fixture.binding.project.clone(),
        prepared: prepared.clone(),
        local_planning: ObjectId::mint(),
        source_changes: source_changed
            .iter()
            .map(|id| {
                (
                    *id,
                    Some(SourceMemberRef {
                        record_id: ObjectId::mint(),
                    }),
                )
            })
            .collect(),
    };
    planning
        .validate(&fixture.binding.project, &prepared, &fixture.intent)
        .unwrap();
    planning.source_changes = fixture
        .intent
        .candidate
        .changed
        .iter()
        .map(|id| (*id, None))
        .collect();
    assert!(
        planning
            .validate(&fixture.binding.project, &prepared, &fixture.intent)
            .is_err()
    );
}

#[test]
fn strict_delta_cuts_refuse_missing_origin_wrong_binding_feed_and_restore_occurrence() {
    let fixture = fixture();
    fixture
        .intent
        .local_cut
        .validate(&fixture.binding_id, &fixture.binding)
        .unwrap();
    let mut unenrolled = fixture.binding.clone();
    unenrolled.store.origin = None;
    let error = fixture
        .intent
        .local_cut
        .validate(&fixture.binding_id, &unenrolled)
        .unwrap_err();
    assert!(
        matches!(
            &error,
            crate::storage::StoreError::TaskSyncRefused {
                refusal: crate::task_sync::TaskSyncRefusal::IncrementalBasisUnavailable,
                ..
            }
        ),
        "{error:?}"
    );
    assert!(
        error.to_string().contains("explicitly enrolled origin"),
        "{error}"
    );
    for mutation in ["binding", "origin", "feed", "restore", "anchor"] {
        let mut cut = fixture.intent.local_cut.clone();
        match mutation {
            "binding" => cut.binding = ObjectId::mint(),
            "origin" => cut.origin = ObjectId::mint(),
            "feed" => cut.position.feed = FeedId::RootWork(WorkId::new()),
            "restore" => cut.restore_occurrence = Some(uuid::Uuid::new_v4()),
            "anchor" => cut.position.position = 1,
            _ => unreachable!(),
        }
        assert!(
            cut.validate(&fixture.binding_id, &fixture.binding).is_err(),
            "{mutation}"
        );
    }
    let mut encoded = serde_json::to_value(&fixture.intent.local_cut).unwrap();
    encoded["position"]["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<OriginQualifiedCut>(encoded).is_err());
    let mut encoded = serde_json::to_value(&fixture.intent.local_cut).unwrap();
    encoded["position"]["feed"]["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<OriginQualifiedCut>(encoded).is_err());
}

#[test]
fn outgoing_preparation_has_no_fabricated_commit_or_confirmed_publication() {
    let mut fixture = fixture();
    fixture.intent.direction = Direction::Outgoing;
    fixture.intent.basis = DeltaBasis::Outgoing {
        export_before: Box::new(PreparedExportBasis {
            anchor: PreparedExportAnchor::Bootstrap {
                checkpoint: ObjectId::mint(),
            },
            membership: OutputMembership {
                inventory_root: InventoryRoot::ZERO,
                profile: fixture.binding.profile.clone(),
            },
            cut: fixture.intent.local_cut.clone(),
        }),
        confirmed_before: None,
    };
    fixture
        .payload
        .documents
        .retain(|id, _| fixture.intent.candidate.changed.contains(id));
    fixture.payload.output.inventory_root = fixture
        .payload
        .documents
        .values()
        .filter_map(Option::as_ref)
        .try_fold(InventoryRoot::ZERO, InventoryRoot::including)
        .unwrap();
    fixture.intent.candidate.output = fixture.payload.output.clone();
    refresh(&mut fixture);
    fixture
        .payload
        .validate(&fixture.binding_id, &fixture.binding, &fixture.intent)
        .unwrap();
    let encoded = serde_json::to_value(&fixture.intent).unwrap();
    assert!(encoded["basis"]["confirmed_before"].is_null());
    let mut mixed = fixture.intent.clone();
    mixed.expected_head = Some(comparison("SHA-1 head").as_str()[..40].into());
    let mut common = commit(mixed.operation, "SHA-256 common commit");
    common.oid = comparison("SHA-256 common oid").to_string();
    common.tree = comparison("SHA-256 common tree").to_string();
    mixed.common_commit = Some(common);
    let error = mixed
        .validate(&fixture.binding_id, &fixture.binding)
        .unwrap_err();
    assert!(
        error.to_string().contains("mixed delta Git object formats"),
        "{error}"
    );
    assert!(
        !serde_json::to_string(&encoded["basis"])
            .unwrap()
            .contains("\"commit\"")
    );
    let prepared = ObjectId::mint();
    let mut selection = DeltaCommitSelection {
        prepared: prepared.clone(),
        commit: commit(fixture.intent.operation, "actual later selection"),
    };
    selection
        .validate(
            &fixture.binding_id,
            &fixture.binding,
            &fixture.intent,
            &prepared,
        )
        .unwrap();
    selection.prepared = ObjectId::mint();
    assert!(
        selection
            .validate(
                &fixture.binding_id,
                &fixture.binding,
                &fixture.intent,
                &prepared
            )
            .is_err()
    );
    fixture.intent.direction = Direction::Incoming;
    assert!(
        fixture
            .intent
            .validate(&fixture.binding_id, &fixture.binding)
            .unwrap_err()
            .to_string()
            .contains("direction and basis disagree")
    );
}

#[test]
fn selected_payload_checks_keys_shape_disclosure_and_bytes_without_claiming_graph_admission() {
    for mutation in ["key", "selection", "priority", "notes", "bytes"] {
        let mut fixture = fixture();
        let id = *fixture.payload.documents.keys().next().unwrap();
        match mutation {
            "key" => {
                fixture
                    .payload
                    .documents
                    .get_mut(&id)
                    .unwrap()
                    .as_mut()
                    .unwrap()
                    .id = WorkId::new();
            }
            "selection" => {
                fixture.payload.documents.remove(&id);
            }
            "priority" => {
                fixture
                    .payload
                    .documents
                    .get_mut(&id)
                    .unwrap()
                    .as_mut()
                    .unwrap()
                    .priority = 9;
            }
            "notes" => fixture
                .payload
                .documents
                .get_mut(&id)
                .unwrap()
                .as_mut()
                .unwrap()
                .notes
                .push(SharedNote {
                    id: ObjectId::mint(),
                    kind: SharedNoteKind::Generic,
                    body: "Eligible note".into(),
                    author: SharedActor {
                        actor_id: "writer".into(),
                        actor_kind: "test_agent".into(),
                    },
                    recorded_at: chrono::Utc::now(),
                    origin_machine: "machine".into(),
                }),
            "bytes" => fixture
                .payload
                .documents
                .get_mut(&id)
                .unwrap()
                .as_mut()
                .unwrap()
                .title
                .push_str(" changed"),
            _ => unreachable!(),
        }
        if mutation != "bytes" {
            refresh(&mut fixture);
        }
        let error = fixture
            .payload
            .validate(&fixture.binding_id, &fixture.binding, &fixture.intent)
            .unwrap_err();
        let expected = match mutation {
            "key" => "task key differs",
            "selection" => "selected keys differ",
            "priority" => "priority",
            "notes" => "notes_included",
            "bytes" => "content fingerprint",
            _ => unreachable!(),
        };
        assert!(error.to_string().contains(expected), "{mutation}: {error}");
    }
    // A valid selected document can reference a task outside this selection;
    // successful shape validation deliberately does not establish graph closure.
    let mut fixture = fixture();
    fixture
        .payload
        .documents
        .values_mut()
        .next()
        .unwrap()
        .as_mut()
        .unwrap()
        .parent_id = Some(WorkId::new());
    refresh(&mut fixture);
    fixture
        .payload
        .validate(&fixture.binding_id, &fixture.binding, &fixture.intent)
        .unwrap();
}

#[test]
fn managed_checkpoint_metadata_is_explicit_ordered_and_not_an_outcome() {
    let fixture = fixture();
    let operation = uuid::Uuid::new_v4();
    let mut checkpoint = ManagedCustodyCheckpoint {
        operation,
        key: operation.to_string(),
        request_fingerprint: comparison("custody audit request"),
        previous_checkpoint: None,
        local_cut: fixture.intent.local_cut.clone(),
        reason: CheckpointReason::Bootstrap,
        seed: None,
        artifacts: vec![CustodyArtifactRef {
            operation,
            kind: CustodyArtifactKind::PublicationWitness,
            content_fingerprint: comparison("unknown publication witness"),
            record: Some(ObjectId::mint()),
        }],
    };
    checkpoint
        .validate(&fixture.binding_id, &fixture.binding)
        .unwrap();
    let encoded = serde_json::to_value(&checkpoint).unwrap();
    assert!(encoded.get("completed").is_none());
    assert_eq!(
        serde_json::from_value::<ManagedCustodyCheckpoint>(encoded.clone()).unwrap(),
        checkpoint
    );
    let mut unknown = encoded;
    unknown["outcome"] = serde_json::json!("confirmed");
    assert!(serde_json::from_value::<ManagedCustodyCheckpoint>(unknown).is_err());
    checkpoint.artifacts.push(checkpoint.artifacts[0].clone());
    assert!(
        checkpoint
            .validate(&fixture.binding_id, &fixture.binding)
            .unwrap_err()
            .to_string()
            .contains("ordered and unique")
    );
}
