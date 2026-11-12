use super::*;

#[test]
fn abandoned_fetched_pin_cannot_bootstrap_membership_or_omit_its_binding() {
    let fixture = fixture();
    let selected = commit(fixture.intent.operation, "abandoned data");
    let mut request = CustodyAuditRequest {
        binding: fixture.binding_id.clone(),
        operation: fixture.intent.operation,
        key: "explicit abandonment".into(),
        reason: CheckpointReason::NamedAudit,
        previous_checkpoint: None,
        expected_tail: ObjectId::mint(),
        expected_local_cut: fixture.intent.local_cut.clone(),
        seed: CustodySeedSelection::AbandonFetchedCommit {
            binding: fixture.binding_id.clone(),
            commit: selected.clone(),
        },
    };
    request.validate(&fixture.binding).unwrap();
    request.reason = CheckpointReason::Bootstrap;
    assert!(request.validate(&fixture.binding).is_err());
    let mut original = checkpoint(&fixture, None);
    original.operation = fixture.intent.operation;
    original.artifacts.push(CustodyArtifactRef {
        operation: original.operation,
        kind: CustodyArtifactKind::AbandonedFetchedCommit,
        content_fingerprint: CanonicalObject::freeze(&selected).unwrap().key().clone(),
        record: Some(fixture.binding_id.clone()),
    });
    original
        .validate(&fixture.binding_id, &fixture.binding)
        .unwrap();
    assert!(original.abandoned_commit().is_some());
    assert!(original.data_commit().is_none());
    let mut seeded = original.clone();
    seeded.seed = Some(incoming(&fixture));
    assert!(
        seeded
            .validate(&fixture.binding_id, &fixture.binding)
            .is_err()
    );
    original.artifacts[0].record = None;
    assert!(
        original
            .validate(&fixture.binding_id, &fixture.binding)
            .is_err()
    );
}

#[test]
fn data_bootstrap_pin_stays_with_its_seed_across_later_custody_checkpoints() {
    let fixture = fixture();
    let mut original = checkpoint(&fixture, Some(incoming(&fixture)));
    original.operation = fixture.intent.operation;
    original.key = original.operation.to_string();
    let Some(CheckpointSeed::Incoming { snapshot, .. }) = &original.seed else {
        unreachable!()
    };
    let selected = snapshot.commit.clone();
    original.artifacts.push(CustodyArtifactRef {
        operation: original.operation,
        kind: CustodyArtifactKind::IncomingDataCommit,
        content_fingerprint: CanonicalObject::freeze(&selected).unwrap().key().clone(),
        record: None,
    });
    original
        .validate(&fixture.binding_id, &fixture.binding)
        .unwrap();
    assert_eq!(original.data_commit(), Some(&selected));
    let mut continuation = checkpoint(&fixture, None);
    continuation.reason = CheckpointReason::NamedAudit;
    continuation.artifacts.clone_from(&original.artifacts);
    continuation
        .validate(&fixture.binding_id, &fixture.binding)
        .unwrap();
    assert!(continuation.data_commit().is_none());
    for mutation in ["comparison", "canonical_owner", "seed"] {
        let mut changed = original.clone();
        match mutation {
            "comparison" => changed.artifacts[0].content_fingerprint = comparison("another pin"),
            "canonical_owner" => changed.artifacts[0].record = Some(ObjectId::mint()),
            "seed" => changed.seed = None,
            _ => unreachable!(),
        }
        assert!(
            changed
                .validate(&fixture.binding_id, &fixture.binding)
                .is_err(),
            "{mutation}"
        );
    }
}

#[test]
fn incoming_data_audit_cannot_reuse_another_operations_pin() {
    let fixture = fixture();
    let mut request = CustodyAuditRequest {
        binding: fixture.binding_id.clone(),
        operation: fixture.intent.operation,
        key: "incoming-data".into(),
        reason: CheckpointReason::Bootstrap,
        previous_checkpoint: None,
        expected_tail: ObjectId::mint(),
        expected_local_cut: fixture.intent.local_cut.clone(),
        seed: CustodySeedSelection::IncomingDataCommit {
            commit: commit(fixture.intent.operation, "selected data"),
        },
    };
    request.validate(&fixture.binding).unwrap();
    request.operation = uuid::Uuid::new_v4();
    assert!(
        request
            .validate(&fixture.binding)
            .unwrap_err()
            .to_string()
            .contains("another operation")
    );
}

fn checkpoint(fixture: &Fixture, seed: Option<CheckpointSeed>) -> ManagedCustodyCheckpoint {
    let operation = uuid::Uuid::new_v4();
    ManagedCustodyCheckpoint {
        operation,
        key: operation.to_string(),
        request_fingerprint: comparison("custody audit request"),
        previous_checkpoint: None,
        local_cut: fixture.intent.local_cut.clone(),
        reason: CheckpointReason::Bootstrap,
        seed,
        artifacts: Vec::new(),
    }
}

fn incoming(fixture: &Fixture) -> CheckpointSeed {
    CheckpointSeed::Incoming {
        snapshot: Box::new(MembershipSnapshot {
            commit: commit(fixture.intent.operation, "audited source"),
            membership: fixture.payload.output.clone(),
        }),
        members: BootstrapMembers::Captured {
            members: fixture
                .payload
                .documents
                .iter()
                .map(|(id, task)| {
                    (
                        *id,
                        BootstrapMember::Document {
                            task: Box::new(task.clone().unwrap()),
                        },
                    )
                })
                .collect(),
        },
    }
}

#[test]
fn checkpoint_seed_preserves_linked_and_previously_unstored_source_members() {
    let fixture = fixture();
    let mut seed = incoming(&fixture);
    checkpoint(&fixture, Some(seed.clone()))
        .validate(&fixture.binding_id, &fixture.binding)
        .unwrap();
    let CheckpointSeed::Incoming { snapshot, members } = &mut seed else {
        unreachable!()
    };
    let DeltaBasis::Incoming { local_before, .. } = &fixture.intent.basis else {
        unreachable!()
    };
    assert_ne!(
        snapshot.membership.inventory_root,
        local_before.membership.inventory_root
    );
    let BootstrapMembers::Captured { members } = members else {
        unreachable!()
    };
    let id = *members.keys().next().unwrap();
    let original_owner = ObjectId::mint();
    members.insert(
        id,
        BootstrapMember::Canonical {
            member: SourceMemberRef {
                record_id: original_owner.clone(),
            },
        },
    );
    let checkpoint = checkpoint(&fixture, Some(seed));
    checkpoint
        .validate(&fixture.binding_id, &fixture.binding)
        .unwrap();
    let encoded = serde_json::to_value(&checkpoint).unwrap();
    assert_eq!(
        encoded["seed"]["members"]["members"][id.0.to_string()]["member"]["record_id"],
        original_owner.as_str()
    );
    assert_eq!(
        serde_json::from_value::<ManagedCustodyCheckpoint>(encoded).unwrap(),
        checkpoint
    );
    // Validation here has no store or Git handle: it deliberately cannot
    // resolve the minted fixture reference or establish complete membership.
}

#[test]
fn checkpoint_embedded_members_refuse_wrong_keys_fields_disclosure_and_root() {
    for mutation in ["key", "priority", "notes", "root"] {
        let fixture = fixture();
        let mut seed = incoming(&fixture);
        let CheckpointSeed::Incoming { snapshot, members } = &mut seed else {
            unreachable!()
        };
        let BootstrapMembers::Captured { members } = members else {
            unreachable!()
        };
        let BootstrapMember::Document { task } = members.values_mut().next().unwrap() else {
            unreachable!()
        };
        match mutation {
            "key" => task.id = WorkId::new(),
            "priority" => task.priority = 9,
            "notes" => task.notes.push(SharedNote {
                id: ObjectId::mint(),
                kind: SharedNoteKind::Generic,
                body: "Eligible but excluded by the notes-off profile".into(),
                author: SharedActor {
                    actor_id: "writer".into(),
                    actor_kind: "test_agent".into(),
                },
                recorded_at: chrono::Utc::now(),
                origin_machine: "machine".into(),
            }),
            "root" => snapshot.membership.inventory_root = InventoryRoot::ZERO,
            _ => unreachable!(),
        }
        let error = checkpoint(&fixture, Some(seed))
            .validate(&fixture.binding_id, &fixture.binding)
            .unwrap_err();
        let expected = match mutation {
            "key" => "checkpoint task key differs",
            "priority" => "priority",
            "notes" => "notes_included",
            "root" => "embedded membership root differs",
            _ => unreachable!(),
        };
        assert!(error.to_string().contains(expected), "{mutation}: {error}");
    }
}

#[test]
fn checkpoint_seed_roles_do_not_infer_empty_source_or_confirmed_export() {
    let fixture = fixture();
    let custody_only = checkpoint(&fixture, None);
    custody_only
        .validate(&fixture.binding_id, &fixture.binding)
        .unwrap();
    assert!(serde_json::to_value(&custody_only).unwrap()["seed"].is_null());

    let outcome = ObjectId::mint();
    let mut seed = incoming(&fixture);
    let CheckpointSeed::Incoming { members, snapshot } = &mut seed else {
        unreachable!()
    };
    *members = BootstrapMembers::InitialImport {
        outcome: outcome.clone(),
    };
    let membership = snapshot.membership.clone();
    checkpoint(&fixture, Some(seed))
        .validate(&fixture.binding_id, &fixture.binding)
        .unwrap();
    // Only storage can distinguish an actual complete initial receipt from
    // a reconciled receipt; the pure contract never treats a root as that link.
    let error = checkpoint(
        &fixture,
        Some(CheckpointSeed::PreparedExport {
            membership: membership.clone(),
            members: BootstrapMembers::InitialImport { outcome },
        }),
    )
    .validate(&fixture.binding_id, &fixture.binding)
    .unwrap_err();
    assert!(error.to_string().contains("requires captured membership"));

    let mut empty = checkpoint(
        &fixture,
        Some(CheckpointSeed::PreparedExport {
            membership: OutputMembership {
                inventory_root: InventoryRoot::ZERO,
                profile: fixture.binding.profile.clone(),
            },
            members: BootstrapMembers::Captured {
                members: BTreeMap::new(),
            },
        }),
    );
    empty
        .validate(&fixture.binding_id, &fixture.binding)
        .unwrap();
    let encoded = serde_json::to_value(&empty).unwrap();
    assert_eq!(encoded["seed"]["kind"], "prepared_export");
    assert!(encoded["seed"].get("confirmed").is_none());
    let Some(CheckpointSeed::PreparedExport {
        membership: output, ..
    }) = &mut empty.seed
    else {
        unreachable!()
    };
    output.inventory_root = membership.inventory_root;
    assert!(
        empty
            .validate(&fixture.binding_id, &fixture.binding)
            .unwrap_err()
            .to_string()
            .contains("embedded membership root differs")
    );
}

#[test]
fn checkpoint_seed_wire_refuses_tombstones_unknown_fields_and_wrong_profiles() {
    let fixture = fixture();
    let encoded = serde_json::to_value(checkpoint(&fixture, Some(incoming(&fixture)))).unwrap();
    let id = fixture
        .payload
        .documents
        .keys()
        .next()
        .unwrap()
        .0
        .to_string();
    for location in ["seed", "members", "member", "tombstone"] {
        let mut value = encoded.clone();
        match location {
            "seed" => value["seed"]["unexpected"] = true.into(),
            "members" => value["seed"]["members"]["unexpected"] = true.into(),
            "member" => value["seed"]["members"]["members"][&id]["unexpected"] = true.into(),
            "tombstone" => value["seed"]["members"]["members"][&id] = serde_json::Value::Null,
            _ => unreachable!(),
        }
        assert!(
            serde_json::from_value::<ManagedCustodyCheckpoint>(value).is_err(),
            "{location}"
        );
    }
    let mut seed = incoming(&fixture);
    let CheckpointSeed::Incoming { snapshot, .. } = &mut seed else {
        unreachable!()
    };
    snapshot.membership.profile.include_notes = true;
    snapshot.membership.profile.origin_machine = Some("source".into());
    let error = checkpoint(&fixture, Some(seed))
        .validate(&fixture.binding_id, &fixture.binding)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("incoming checkpoint note profile differs"),
        "{error}"
    );
    let error = checkpoint(
        &fixture,
        Some(CheckpointSeed::PreparedExport {
            membership: OutputMembership {
                inventory_root: InventoryRoot::ZERO,
                profile: ExportProfile {
                    origin_machine: Some("other".into()),
                    ..fixture.binding.profile.clone()
                },
            },
            members: BootstrapMembers::Captured {
                members: BTreeMap::new(),
            },
        }),
    )
    .validate(&fixture.binding_id, &fixture.binding)
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("export checkpoint profile differs"),
        "{error}"
    );
}
