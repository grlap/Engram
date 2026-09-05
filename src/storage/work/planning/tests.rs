use super::super::test_support::*;
use super::super::*;
use super::*;

mod creation;

#[test]
fn revision_kind_and_label_deltas_preserve_unmentioned_labels() {
    let project = "revision-metadata";
    let mut store = SqliteStore::open_in_memory().expect("metadata fixture");
    let mut request = root_request(project, "metadata-root", 1);
    request.labels = (0..12).map(|index| format!("label-{index:02}")).collect();
    request.labels.push("Straße".into());
    let root = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("metadata root");

    let revised = store
        .revise_work(
            &ReviseWorkRequest {
                work_id: root.work_id,
                expected_revision: root.revision,
                patch: WorkRevisionPatch {
                    kind: Some(WorkItemKind::Bug),
                    add_labels: vec!["phoenix".into()],
                    remove_labels: vec!["LABEL-00".into(), "STRASSE".into()],
                    ..WorkRevisionPatch::default()
                },
                authority: delegated(project, "planner"),
                actor: actor("planner"),
                idempotency_key: "revise-metadata".into(),
                updated_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("revise metadata");

    assert_eq!(revised.kind, WorkItemKind::Bug);
    assert_eq!(revised.labels.len(), 12);
    assert!(!revised.labels.iter().any(|label| label == "label-00"));
    assert!(!revised.labels.iter().any(|label| label == "Straße"));
    assert!(revised.labels.iter().any(|label| label == "label-11"));
    assert!(revised.labels.iter().any(|label| label == "phoenix"));
    let latest = latest_canonical_work_event_for_item(&store.connection, root.work_id)
        .expect("latest revised event");
    assert_eq!(latest.work.kind, WorkItemKind::Bug);
    assert_eq!(latest.work.labels, revised.labels);
    assert!(
        store
            .verify_all()
            .expect("metadata integrity")
            .invalid_work_records
            .is_empty()
    );
}

#[test]
fn work_request_actor_context_refusal_is_typed_and_non_mutating() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let before = test_database_shape_snapshot(&store.connection).expect("initial shape");
    let mut request = root_request("invalid-work-context", "invalid context", 0);
    request.actor.provenance_chain.extend([
        ProvenanceLink {
            relation: ProvenanceRelation::DerivedFrom,
            source: "model=first".into(),
            reference: Some(crate::domain::ACTOR_CONTEXT_PROVENANCE_REFERENCE.into()),
        },
        ProvenanceLink {
            relation: ProvenanceRelation::DerivedFrom,
            source: "model=second".into(),
            reference: Some(crate::domain::ACTOR_CONTEXT_PROVENANCE_REFERENCE.into()),
        },
    ]);

    assert!(matches!(
        store.create_work(&request, &DevelopmentNoopRedactor),
        Err(StoreError::InvalidWork(detail)) if detail.contains("at most one value")
    ));
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("shape after refusal"),
        before,
        "invalid work attribution must not mutate the store"
    );
}

#[test]
fn supersession_ref_and_replacement_matrix_is_enforced_in_storage() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let source = store
        .create_work(
            &root_request("project-supersession-matrix", "matrix-source", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("source");
    let live_replacement = store
        .create_work(
            &root_request("project-supersession-matrix", "matrix-live", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("live replacement");
    let cross_project = store
        .create_work(
            &root_request("other-supersession-project", "matrix-cross", 2),
            &DevelopmentNoopRedactor,
        )
        .expect("cross-project replacement");

    let supersede =
        |work: &WorkItem, replacement_id: WorkId, key: &str, second: i64| -> DisposeWorkRequest {
            DisposeWorkRequest {
                work_id: work.work_id,
                expected_work_revision: work.revision,
                disposition: WorkDisposition::Superseded,
                replacement_id: Some(replacement_id),
                reason: "matrix validation".into(),
                actor: actor("planner"),
                idempotency_key: key.into(),
                disposed_at: at(second),
            }
        };

    assert!(matches!(
        store.dispose_work(
            &supersede(&source, source.work_id, "matrix-self", 3),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidWork(_))
    ));
    assert!(matches!(
        store.dispose_work(
            &supersede(&source, cross_project.work_id, "matrix-cross", 4),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidWork(_))
    ));

    let cancelled_replacement = store
        .create_work(
            &root_request("project-supersession-matrix", "matrix-cancelled", 5),
            &DevelopmentNoopRedactor,
        )
        .expect("cancelled replacement");
    let cancelled_replacement = store
        .dispose_work(
            &DisposeWorkRequest {
                work_id: cancelled_replacement.work_id,
                expected_work_revision: cancelled_replacement.revision,
                disposition: WorkDisposition::Cancelled,
                replacement_id: None,
                reason: "cancel replacement".into(),
                actor: actor("planner"),
                idempotency_key: "matrix-cancel-replacement".into(),
                disposed_at: at(6),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("cancel replacement");
    assert!(matches!(
        store.dispose_work(
            &supersede(
                &source,
                cancelled_replacement.work_id,
                "matrix-cancelled-target",
                7,
            ),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidWork(_))
    ));

    let obsolete_replacement = store
        .create_work(
            &root_request("project-supersession-matrix", "matrix-obsolete", 8),
            &DevelopmentNoopRedactor,
        )
        .expect("obsolete replacement");
    let obsolete_replacement = store
        .dispose_work(
            &supersede(
                &obsolete_replacement,
                live_replacement.work_id,
                "matrix-obsolete-dispose",
                9,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("supersede obsolete replacement");
    assert!(matches!(
        store.dispose_work(
            &supersede(
                &source,
                obsolete_replacement.work_id,
                "matrix-superseded-target",
                10,
            ),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidWork(_))
    ));

    let disposed = store
        .dispose_work(
            &supersede(&source, live_replacement.work_id, "matrix-valid", 11),
            &DevelopmentNoopRedactor,
        )
        .expect("open replacement is admitted");
    assert_eq!(disposed.lifecycle, WorkLifecycle::Superseded);
    assert_eq!(disposed.superseded_by, Some(live_replacement.work_id));
    assert!(matches!(
        store.dispose_work(
            &supersede(
                &disposed,
                live_replacement.work_id,
                "matrix-closed-source",
                12,
            ),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::WorkNotOpen(work_id)) if work_id == disposed.work_id
    ));

    let direct_source = store
        .create_work(
            &root_request("project-supersession-matrix", "matrix-direct-source", 13),
            &DevelopmentNoopRedactor,
        )
        .expect("direct cycle source");
    let direct_replacement = store
        .create_work(
            &root_request(
                "project-supersession-matrix",
                "matrix-direct-replacement",
                14,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("direct cycle replacement");
    store
        .add_work_prerequisite(
            &ChangeWorkPrerequisiteRequest {
                work_id: direct_replacement.work_id,
                prerequisite_id: direct_source.work_id,
                expected_revision: direct_replacement.revision,
                authority: delegated("project-supersession-matrix", "planner"),
                actor: actor("planner"),
                idempotency_key: "matrix-direct-prerequisite".into(),
                changed_at: at(15),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("direct prerequisite");
    assert!(matches!(
        store.dispose_work(
            &supersede(
                &direct_source,
                direct_replacement.work_id,
                "matrix-direct-cycle",
                16,
            ),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::WorkDependencyCycle)
    ));

    let transitive_source = store
        .create_work(
            &root_request(
                "project-supersession-matrix",
                "matrix-transitive-source",
                17,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("transitive cycle source");
    let transitive_middle = store
        .create_work(
            &root_request(
                "project-supersession-matrix",
                "matrix-transitive-middle",
                18,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("transitive cycle middle");
    let transitive_replacement = store
        .create_work(
            &root_request(
                "project-supersession-matrix",
                "matrix-transitive-replacement",
                19,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("transitive cycle replacement");
    store
        .add_work_prerequisite(
            &ChangeWorkPrerequisiteRequest {
                work_id: transitive_middle.work_id,
                prerequisite_id: transitive_source.work_id,
                expected_revision: transitive_middle.revision,
                authority: delegated("project-supersession-matrix", "planner"),
                actor: actor("planner"),
                idempotency_key: "matrix-transitive-first".into(),
                changed_at: at(20),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("first transitive prerequisite");
    store
        .add_work_prerequisite(
            &ChangeWorkPrerequisiteRequest {
                work_id: transitive_replacement.work_id,
                prerequisite_id: transitive_middle.work_id,
                expected_revision: transitive_replacement.revision,
                authority: delegated("project-supersession-matrix", "planner"),
                actor: actor("planner"),
                idempotency_key: "matrix-transitive-second".into(),
                changed_at: at(21),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("second transitive prerequisite");
    assert!(matches!(
        store.dispose_work(
            &supersede(
                &transitive_source,
                transitive_replacement.work_id,
                "matrix-transitive-cycle",
                22,
            ),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::WorkDependencyCycle)
    ));
}

#[test]
fn local_decomposition_is_atomic_cycle_safe_and_uses_dense_named_feeds() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-a", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("create root");
    let replay = store
        .create_work(
            &root_request("project-a", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("idempotent create");
    assert_eq!(replay, root);
    assert_eq!(
        store
            .inspect_work(root.work_id, at(0))
            .expect("inspect")
            .availability,
        WorkAvailability::Ready
    );

    let decomposition = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![
                    child("required", ChildRequirement::Required, "Required child"),
                    child("optional", ChildRequirement::Optional, "Optional child"),
                ],
                prerequisites: vec![ChildWorkPrerequisite {
                    work_key: "optional".into(),
                    prerequisite: WorkDependencyRef::Proposed("required".into()),
                }],
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "decompose".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("decompose");
    let required = &decomposition.children[0];
    let optional = &decomposition.children[1];
    assert!(required.labels.contains(&"local-work".into()));
    assert_eq!(
        store
            .inspect_work(required.work_id, at(2))
            .expect("required")
            .availability,
        WorkAvailability::Ready
    );
    let optional_view = store
        .inspect_work(optional.work_id, at(2))
        .expect("optional");
    assert_eq!(optional_view.availability, WorkAvailability::Blocked);
    assert_eq!(optional_view.blocked_by, vec![required.work_id]);

    let optional_parent_cycle = store.add_work_prerequisite(
        &ChangeWorkPrerequisiteRequest {
            work_id: optional.work_id,
            prerequisite_id: root.work_id,
            expected_revision: optional.revision,
            authority: delegated(&root.project_id.0, "planner"),
            actor: actor("planner"),
            idempotency_key: "optional-parent-cycle".into(),
            changed_at: at(3),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(
        optional_parent_cycle,
        Err(StoreError::WorkDependencyCycle)
    ));
    let nested_ancestor_cycle = store.decompose_work(
        &DecomposeWorkRequest {
            parent_id: optional.work_id,
            expected_parent_revision: optional.revision,
            children: vec![child(
                "nested-optional",
                ChildRequirement::Optional,
                "Nested optional child",
            )],
            prerequisites: vec![ChildWorkPrerequisite {
                work_key: "nested-optional".into(),
                prerequisite: WorkDependencyRef::Existing(root.work_id),
            }],
            authority: delegated(&root.project_id.0, "planner"),
            actor: actor("planner"),
            idempotency_key: "nested-ancestor-cycle".into(),
            created_at: at(3),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(
        nested_ancestor_cycle,
        Err(StoreError::WorkDependencyCycle)
    ));

    let cycle = store.add_work_prerequisite(
        &ChangeWorkPrerequisiteRequest {
            work_id: required.work_id,
            prerequisite_id: root.work_id,
            expected_revision: required.revision,
            authority: delegated(&root.project_id.0, "planner"),
            actor: actor("planner"),
            idempotency_key: "union-cycle".into(),
            changed_at: at(3),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(cycle, Err(StoreError::WorkDependencyCycle)));
    assert_eq!(
        store
            .get_work_item(required.work_id)
            .expect("required remains")
            .revision,
        1
    );

    let completed_prerequisite = store
        .create_work(
            &root_request("project-a", "completed-prerequisite", 4),
            &DevelopmentNoopRedactor,
        )
        .expect("create completed prerequisite");
    let completed_claim = claim(
        &mut store,
        &completed_prerequisite,
        "planner",
        "completed-prerequisite-claim",
        5,
        300,
    );
    let completed_evidence = evidence(
        &mut store,
        &completed_prerequisite,
        &completed_claim,
        "planner",
        "completed-prerequisite-evidence",
        6,
    );
    checkpoint(
        &mut store,
        &completed_prerequisite,
        &completed_claim,
        "planner",
        "completed-prerequisite-checkpoint",
        7,
        std::slice::from_ref(&completed_evidence),
    );
    complete(
        &mut store,
        &completed_prerequisite,
        &completed_claim,
        "planner",
        &completed_evidence,
        "completed-prerequisite-complete",
        8,
    )
    .expect("complete prerequisite");
    let completed_target = store.add_work_prerequisite(
        &ChangeWorkPrerequisiteRequest {
            work_id: required.work_id,
            prerequisite_id: completed_prerequisite.work_id,
            expected_revision: required.revision,
            authority: delegated(&root.project_id.0, "planner"),
            actor: actor("planner"),
            idempotency_key: "completed-prerequisite-refusal".into(),
            changed_at: at(9),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(
        completed_target,
        Err(StoreError::WorkPrerequisiteAlreadySatisfied(work_id))
            if work_id == completed_prerequisite.work_id
    ));
    let completed_decomposition = store.decompose_work(
        &DecomposeWorkRequest {
            parent_id: root.work_id,
            expected_parent_revision: decomposition.parent.revision,
            children: vec![child(
                "completed-existing",
                ChildRequirement::Optional,
                "Completed existing prerequisite",
            )],
            prerequisites: vec![ChildWorkPrerequisite {
                work_key: "completed-existing".into(),
                prerequisite: WorkDependencyRef::Existing(completed_prerequisite.work_id),
            }],
            authority: delegated(&root.project_id.0, "planner"),
            actor: actor("planner"),
            idempotency_key: "completed-existing-decompose".into(),
            created_at: at(9),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(
        completed_decomposition,
        Err(StoreError::WorkPrerequisiteAlreadySatisfied(work_id))
            if work_id == completed_prerequisite.work_id
    ));

    let terminal_prerequisite = store
        .create_work(
            &root_request("project-a", "terminal-prerequisite", 4),
            &DevelopmentNoopRedactor,
        )
        .expect("create terminal prerequisite");
    let terminal_prerequisite = store
        .dispose_work(
            &DisposeWorkRequest {
                work_id: terminal_prerequisite.work_id,
                expected_work_revision: terminal_prerequisite.revision,
                disposition: WorkDisposition::Cancelled,
                replacement_id: None,
                reason: "terminal prerequisites are refused".into(),
                actor: actor("planner"),
                idempotency_key: "cancel-terminal-prerequisite".into(),
                disposed_at: at(5),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("cancel terminal prerequisite");
    let closed_target = store.add_work_prerequisite(
        &ChangeWorkPrerequisiteRequest {
            work_id: required.work_id,
            prerequisite_id: terminal_prerequisite.work_id,
            expected_revision: required.revision,
            authority: delegated(&root.project_id.0, "planner"),
            actor: actor("planner"),
            idempotency_key: "terminal-prerequisite-refusal".into(),
            changed_at: at(6),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(
        closed_target,
        Err(StoreError::WorkNotOpen(work_id)) if work_id == terminal_prerequisite.work_id
    ));

    let before = store
        .connection
        .query_row("SELECT COUNT(*) FROM work_items", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count before");
    let closed_decomposition = store.decompose_work(
        &DecomposeWorkRequest {
            parent_id: root.work_id,
            expected_parent_revision: decomposition.parent.revision,
            children: vec![child(
                "closed-existing",
                ChildRequirement::Required,
                "Closed existing prerequisite",
            )],
            prerequisites: vec![ChildWorkPrerequisite {
                work_key: "closed-existing".into(),
                prerequisite: WorkDependencyRef::Existing(terminal_prerequisite.work_id),
            }],
            authority: delegated(&root.project_id.0, "planner"),
            actor: actor("planner"),
            idempotency_key: "closed-existing-decompose".into(),
            created_at: at(7),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(
        closed_decomposition,
        Err(StoreError::WorkNotOpen(work_id)) if work_id == terminal_prerequisite.work_id
    ));
    assert_eq!(
        before,
        store
            .connection
            .query_row("SELECT COUNT(*) FROM work_items", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("count after closed prerequisite refusal")
    );
    let bad = store.decompose_work(
        &DecomposeWorkRequest {
            parent_id: root.work_id,
            expected_parent_revision: decomposition.parent.revision,
            children: vec![child("new", ChildRequirement::Required, "New child")],
            prerequisites: vec![ChildWorkPrerequisite {
                work_key: "new".into(),
                prerequisite: WorkDependencyRef::Proposed("missing".into()),
            }],
            authority: delegated(&root.project_id.0, "planner"),
            actor: actor("planner"),
            idempotency_key: "bad-decompose".into(),
            created_at: at(4),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(bad.is_err());
    let after = store
        .connection
        .query_row("SELECT COUNT(*) FROM work_items", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count after");
    assert_eq!(before, after);

    let entries = store
        .work_feed_after(&FeedId::Project(root.project_id.clone()), 0, 100)
        .expect("project feed");
    assert!(entries.len() >= 4);
    for (index, entry) in entries.iter().enumerate() {
        assert_eq!(entry.position.position, i64::try_from(index).unwrap() + 1);
    }
}

#[test]
fn redaction_direct_child_creation_and_unverified_drain_fail_closed() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let rejected = store.create_work(
        &root_request("project-policy", "redacted-root", 0),
        &RejectingRedactor,
    );
    assert!(matches!(rejected, Err(StoreError::RedactionRefused(_))));
    let root = store
        .create_work(
            &root_request("project-policy", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let mut direct_child = root_request("project-policy", "direct-child", 2);
    direct_child.parent_id = Some(root.work_id);
    let direct = store.create_work(&direct_child, &DevelopmentNoopRedactor);
    assert!(matches!(direct, Err(StoreError::InvalidWork(_))));

    let claim = claim(&mut store, &root, "root-agent", "root-claim", 3, 100);
    let evidence = evidence(&mut store, &root, &claim, "root-agent", "root-evidence", 4);
    checkpoint(
        &mut store,
        &root,
        &claim,
        "root-agent",
        "root-checkpoint",
        5,
        std::slice::from_ref(&evidence),
    );
    let mut unverified_drain = completion_request(
        &root,
        &claim,
        "root-agent",
        &evidence,
        "unverified-drain",
        6,
    );
    unverified_drain
        .drain
        .released_resource_leases
        .push("agent-supplied-lease".into());
    let unverified_drain = store.complete_work(&unverified_drain, &DevelopmentNoopRedactor);
    assert!(matches!(
        unverified_drain,
        Err(StoreError::WorkCompletionRefused { .. })
    ));

    assert_eq!(
        store
            .get_work_item(root.work_id)
            .expect("root remains")
            .lifecycle,
        WorkLifecycle::Open
    );
}

#[test]
fn imported_work_requires_a_hash_verified_typed_source_snapshot() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let snapshot = |reference: &str, fingerprint: &str, captured_at| WorkSourceSnapshot {
        schema_version: SCHEMA_VERSION,
        adapter_kind: "beads".into(),
        canonical_ref: reference.into(),
        projected: crate::domain::WorkSourceProjection {
            title: Some("Imported work".into()),
            body: None,
            status: Some("open".into()),
            owner: None,
        },
        captured_at,
        source_revision: Some(fingerprint.into()),
        fingerprint: fingerprint.into(),
        canonical_url: Some(format!("https://tracker.invalid/{reference}")),
        payload_hash: CanonicalObject::freeze(&serde_json::json!({
            "reference": reference,
            "fingerprint": fingerprint
        }))
        .expect("payload")
        .hash()
        .clone(),
        raw: std::collections::BTreeMap::default(),
    };

    let valid = snapshot("tracker:ENG-1", "etag-valid", at(0));
    let valid_object = CanonicalObject::freeze(&valid).expect("valid snapshot");
    let transaction = store
        .connection
        .transaction()
        .expect("snapshot transaction");
    SqliteStore::insert_object(&transaction, "work_source_snapshot", &valid_object)
        .expect("store valid snapshot");
    transaction.commit().expect("commit valid snapshot");
    let mut request = root_request("project-import", "valid-import", 1);
    request.origin = WorkOrigin::Imported;
    request.source_snapshot_id = Some(valid_object.hash().clone());
    store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("verified imported work");

    let wrong_kind = CanonicalObject::freeze(&snapshot("tracker:ENG-2", "etag-kind", at(2)))
        .expect("wrong-kind snapshot");
    let transaction = store
        .connection
        .transaction()
        .expect("snapshot transaction");
    SqliteStore::insert_object(&transaction, "not_source_snapshot", &wrong_kind)
        .expect("store wrong kind");
    transaction.commit().expect("commit wrong kind");
    let mut request = root_request("project-import", "wrong-kind", 2);
    request.origin = WorkOrigin::Imported;
    request.source_snapshot_id = Some(wrong_kind.hash().clone());
    assert!(matches!(
        store.create_work(&request, &DevelopmentNoopRedactor),
        Err(StoreError::InvalidWork(_))
    ));

    let malformed = CanonicalObject::freeze(&serde_json::json!({"unexpected": true}))
        .expect("malformed typed object");
    let transaction = store
        .connection
        .transaction()
        .expect("snapshot transaction");
    SqliteStore::insert_object(&transaction, "work_source_snapshot", &malformed)
        .expect("store malformed source snapshot");
    transaction.commit().expect("commit malformed snapshot");
    let mut request = root_request("project-import", "malformed", 3);
    request.origin = WorkOrigin::Imported;
    request.source_snapshot_id = Some(malformed.hash().clone());
    assert!(matches!(
        store.create_work(&request, &DevelopmentNoopRedactor),
        Err(StoreError::InvalidWork(_))
    ));

    let corrupt = CanonicalObject::freeze(&snapshot("tracker:ENG-3", "etag-corrupt", at(4)))
        .expect("corrupt snapshot identity");
    store
        .connection
        .execute(
            "INSERT INTO objects (object_hash, object_kind, canonical_json)
             VALUES (?1, 'work_source_snapshot', CAST('{}' AS BLOB))",
            [corrupt.hash().as_str()],
        )
        .expect("store corrupt snapshot bytes");
    let mut request = root_request("project-import", "corrupt", 4);
    request.origin = WorkOrigin::Imported;
    request.source_snapshot_id = Some(corrupt.hash().clone());
    assert!(matches!(
        store.create_work(&request, &DevelopmentNoopRedactor),
        Err(StoreError::InvalidWork(_))
    ));

    let invalid = snapshot("", "etag-future", at(10));
    let invalid_object = CanonicalObject::freeze(&invalid).expect("invalid snapshot object");
    let transaction = store
        .connection
        .transaction()
        .expect("snapshot transaction");
    SqliteStore::insert_object(&transaction, "work_source_snapshot", &invalid_object)
        .expect("store invalid source snapshot");
    transaction.commit().expect("commit invalid snapshot");
    let mut request = root_request("project-import", "invalid-shape", 5);
    request.origin = WorkOrigin::Imported;
    request.source_snapshot_id = Some(invalid_object.hash().clone());
    assert!(matches!(
        store.create_work(&request, &DevelopmentNoopRedactor),
        Err(StoreError::InvalidWork(_))
    ));
}

#[test]
fn decomposition_enforces_default_fanout_and_open_descendant_budget() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-budget", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let no_children = store.decompose_work(
        &DecomposeWorkRequest {
            parent_id: root.work_id,
            expected_parent_revision: root.revision,
            children: Vec::new(),
            prerequisites: Vec::new(),
            authority: WorkPlanningAuthority::Project,
            actor: actor("planner"),
            idempotency_key: "no-children".into(),
            created_at: at(1),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(no_children, Err(StoreError::InvalidWork(_))));

    let too_many = store.decompose_work(
        &DecomposeWorkRequest {
            parent_id: root.work_id,
            expected_parent_revision: root.revision,
            children: (0..17)
                .map(|index| {
                    child(
                        &format!("fanout-{index}"),
                        ChildRequirement::Required,
                        &format!("Fanout {index}"),
                    )
                })
                .collect(),
            prerequisites: Vec::new(),
            authority: WorkPlanningAuthority::Project,
            actor: actor("planner"),
            idempotency_key: "over-fanout".into(),
            created_at: at(2),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(too_many, Err(StoreError::InvalidWork(_))));

    let mut parent = root;
    for batch in 0..8 {
        parent = store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: parent.work_id,
                    expected_parent_revision: parent.revision,
                    children: (0..16)
                        .map(|index| {
                            child(
                                &format!("batch-{batch}-{index}"),
                                ChildRequirement::Required,
                                &format!("Batch {batch} child {index}"),
                            )
                        })
                        .collect(),
                    prerequisites: Vec::new(),
                    authority: WorkPlanningAuthority::Project,
                    actor: actor("planner"),
                    idempotency_key: format!("budget-batch-{batch}"),
                    created_at: at(3 + batch),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("fill the default root-wide open-descendant budget")
            .parent;
    }
    let over_budget = store.decompose_work(
        &DecomposeWorkRequest {
            parent_id: parent.work_id,
            expected_parent_revision: parent.revision,
            children: vec![child(
                "over-budget",
                ChildRequirement::Required,
                "Over budget",
            )],
            prerequisites: Vec::new(),
            authority: WorkPlanningAuthority::Project,
            actor: actor("planner"),
            idempotency_key: "over-open-budget".into(),
            created_at: at(20),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(over_budget, Err(StoreError::InvalidWork(_))));
}

#[test]
fn decomposition_enforces_default_depth_budget() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut parent = store
        .create_work(
            &root_request("project-depth-budget", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    for depth in 1..=MAX_WORK_DEPTH {
        parent = store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: parent.work_id,
                    expected_parent_revision: parent.revision,
                    children: vec![child(
                        &format!("depth-{depth}"),
                        ChildRequirement::Required,
                        &format!("Depth {depth}"),
                    )],
                    prerequisites: Vec::new(),
                    authority: WorkPlanningAuthority::Project,
                    actor: actor("planner"),
                    idempotency_key: format!("depth-{depth}"),
                    created_at: at(i64::from(depth)),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("decomposition through the maximum depth")
            .children
            .into_iter()
            .next()
            .expect("one child");
    }
    let over_depth = store.decompose_work(
        &DecomposeWorkRequest {
            parent_id: parent.work_id,
            expected_parent_revision: parent.revision,
            children: vec![child(
                "over-depth",
                ChildRequirement::Required,
                "Over depth",
            )],
            prerequisites: Vec::new(),
            authority: WorkPlanningAuthority::Project,
            actor: actor("planner"),
            idempotency_key: "over-depth".into(),
            created_at: at(10),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(
        over_depth,
        Err(StoreError::InvalidWork(message)) if message.contains("hierarchy depth")
    ));
}
