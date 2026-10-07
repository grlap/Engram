use super::super::test_support::*;
use super::super::*;
use super::*;

mod creation;
mod shared_guards;

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
    let observed = store
        .verify_all()
        .expect("metadata integrity")
        .invalid_work_records;
    assert!(observed.is_empty(), "{observed:?}");
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
fn create_work_refuses_an_oversized_actor_session_before_effects() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let before = test_database_shape_snapshot(&store.connection).expect("initial shape");
    let mut request = root_request("session-admission-create", "oversized-actor-session", 0);
    let giant = SessionId("s".repeat(65));
    request.actor.session_id = Some(giant.clone());
    let error = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect_err("oversized actor session");
    assert!(matches!(
        error,
        StoreError::InvalidWork(ref reason) if reason == crate::SessionIdAdmissionError::TooLong.as_str()
    ));
    assert!(!error.to_string().contains(&giant.0));
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("shape after refusal"),
        before,
        "oversized planning session must not mutate the store"
    );
}

#[test]
fn create_work_preserves_an_exact_64_byte_actor_session() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let session = "s".repeat(crate::MAX_SESSION_ID_BYTES);
    let mut request = root_request("session-admission-create", "max-actor-session", 0);
    request.actor.session_id = Some(SessionId(session.clone()));
    let item = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("admitted actor session");
    assert_eq!(
        item.created_by.session_id.as_ref().map(|id| id.0.as_str()),
        Some(session.as_str())
    );
}

#[test]
fn decompose_work_refuses_an_oversized_actor_session_before_effects() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("session-admission-plan", "plan-root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let before = test_database_shape_snapshot(&store.connection).expect("after root");
    let giant = SessionId("p".repeat(65));
    let mut actor = actor("peer");
    actor.session_id = Some(giant.clone());
    let request = DecomposeWorkRequest {
        parent_id: root.work_id,
        expected_parent_revision: root.revision,
        children: vec![child(
            "proposal",
            ChildRequirement::Required,
            "Peer proposal",
        )],
        prerequisites: Vec::new(),
        authority: WorkPlanningAuthority::Project,
        actor,
        idempotency_key: "oversized-plan".into(),
        created_at: at(3),
    };
    let error = store
        .decompose_work(&request, &DevelopmentNoopRedactor)
        .expect_err("oversized planning session");
    assert!(matches!(
        error,
        StoreError::InvalidWork(ref reason) if reason == crate::SessionIdAdmissionError::TooLong.as_str()
    ));
    assert!(!error.to_string().contains(&giant.0));
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("shape after refusal"),
        before,
        "oversized decompose session must not add children or events"
    );
}

#[test]
fn assert_actor_session_compares_an_oversized_expected_without_length_error() {
    let expected = SessionId("h".repeat(65));
    let error =
        assert_actor_session(&actor("caller"), &expected).expect_err("historical holder mismatch");
    assert!(
        matches!(
            error,
            StoreError::InvalidWork(ref reason)
                if reason.contains("does not match lifecycle holder")
        ),
        "{error}"
    );
    assert!(
        !error
            .to_string()
            .contains(crate::SessionIdAdmissionError::TooLong.as_str()),
        "persisted expected must not be length-admitted: {error}"
    );
}

#[test]
fn decompose_work_preserves_an_exact_64_byte_actor_session() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("session-admission-plan", "plan-root-64", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let session = "p".repeat(crate::MAX_SESSION_ID_BYTES);
    let mut actor = actor("peer");
    actor.session_id = Some(SessionId(session.clone()));
    let request = DecomposeWorkRequest {
        parent_id: root.work_id,
        expected_parent_revision: root.revision,
        children: vec![child(
            "proposal",
            ChildRequirement::Required,
            "Peer proposal",
        )],
        prerequisites: Vec::new(),
        authority: WorkPlanningAuthority::Project,
        actor,
        idempotency_key: "max-plan-session".into(),
        created_at: at(3),
    };
    let decomposition = store
        .decompose_work(&request, &DevelopmentNoopRedactor)
        .expect("admitted planning session");
    assert_eq!(
        decomposition.children[0]
            .created_by
            .session_id
            .as_ref()
            .map(|id| id.0.as_str()),
        Some(session.as_str())
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
        .key()
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
    request.source_snapshot_id = Some(valid_object.key().clone());
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
    request.source_snapshot_id = Some(wrong_kind.key().clone());
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
    request.source_snapshot_id = Some(malformed.key().clone());
    assert!(matches!(
        store.create_work(&request, &DevelopmentNoopRedactor),
        Err(StoreError::InvalidWork(_))
    ));

    let corrupt = CanonicalObject::freeze(&snapshot("tracker:ENG-3", "etag-corrupt", at(4)))
        .expect("corrupt snapshot identity");
    store
        .connection
        .execute(
            "INSERT INTO objects (object_id, object_kind, canonical_json)
             VALUES (?1, 'work_source_snapshot', CAST('{}' AS BLOB))",
            [corrupt.key().as_str()],
        )
        .expect("store corrupt snapshot bytes");
    let mut request = root_request("project-import", "corrupt", 4);
    request.origin = WorkOrigin::Imported;
    request.source_snapshot_id = Some(corrupt.key().clone());
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
    request.source_snapshot_id = Some(invalid_object.key().clone());
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
    let limit = usize::try_from(MAX_OPEN_WORK_DESCENDANTS).expect("limit");
    for (batch, start) in (0..limit)
        .step_by(MAX_CHILDREN_PER_DECOMPOSITION)
        .enumerate()
    {
        take_descendant_scan_count();
        take_descendant_scan_vm_steps();
        parent = store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: parent.work_id,
                    expected_parent_revision: parent.revision,
                    children: (start..limit.min(start + MAX_CHILDREN_PER_DECOMPOSITION))
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
                    created_at: at(3 + i64::try_from(batch).expect("batch")),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("fill the default root-wide open-descendant budget")
            .parent;
        assert_eq!(take_descendant_scan_count(), 1);
        let steps = take_descendant_scan_vm_steps();
        // Set before measuring: the lifecycle-index count must stay linear
        // in this all-open fixture, not re-scan the subtree for every child.
        assert!(
            steps > 0 && steps <= 100 * i32::try_from(start + 1).expect("size") + 256,
            "descendants={start}, SQLite VM steps={steps}"
        );
        eprintln!("ordinary descendant count: prior={start} scans=1 sqlite_vm_steps={steps}");
    }
    let before = crate::storage::test_database_shape_snapshot(&store.connection).expect("before");
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
    assert!(matches!(over_budget, Err(StoreError::InvalidWork(reason))
        if reason == format!("decomposition exceeds the root open-descendant budget: at most {MAX_OPEN_WORK_DESCENDANTS} open descendants per root ({} tasks including the root)", MAX_OPEN_WORK_DESCENDANTS + 1)));
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).expect("after"),
        before
    );
}

/// The most SQLite VM work the open-descendant count may do per proposed or
/// open row of the project, plus a constant for the statement itself. The
/// count's cost is stated in these terms: rows the lifecycle index range
/// visits, never a root's closed history.
const DESCENDANT_COUNT_VM_STEPS_PER_LIVE_ROW: i32 = 16;
const DESCENDANT_COUNT_VM_STEPS_CONSTANT: i32 = 256;

/// Decomposes `count` optional children directly under `parent` in batches
/// of at most sixteen, cancelling each one when `cancel` is set, and returns
/// them. The parent is re-read before each batch, since its revision moves.
fn grow_root(
    store: &mut SqliteStore,
    parent: WorkId,
    prefix: &str,
    count: usize,
    cancel: bool,
    seconds: &mut i64,
) -> Vec<WorkItem> {
    let mut created = Vec::with_capacity(count);
    for (batch, start) in (0..count)
        .step_by(MAX_CHILDREN_PER_DECOMPOSITION)
        .enumerate()
    {
        let current = store.get_work_item(parent).expect("parent");
        *seconds += 1;
        let planned = store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: current.work_id,
                    expected_parent_revision: current.revision,
                    children: (start..count.min(start + MAX_CHILDREN_PER_DECOMPOSITION))
                        .map(|index| {
                            child(
                                &format!("{prefix}-{index}"),
                                ChildRequirement::Optional,
                                &format!("{prefix} {index}"),
                            )
                        })
                        .collect(),
                    prerequisites: Vec::new(),
                    authority: WorkPlanningAuthority::Project,
                    actor: actor("planner"),
                    idempotency_key: format!("{prefix}-batch-{batch}"),
                    created_at: at(*seconds),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("decomposition within the budget");
        for item in planned.children {
            if cancel {
                *seconds += 1;
                store
                    .dispose_work(
                        &crate::DisposeWorkRequest {
                            work_id: item.work_id,
                            expected_work_revision: item.revision,
                            replacement_id: None,
                            disposition: crate::WorkDisposition::Cancelled,
                            reason: "closed history".into(),
                            actor: actor("planner"),
                            idempotency_key: format!("{prefix}-close-{}", item.work_id.0),
                            disposed_at: at(*seconds),
                        },
                        &DevelopmentNoopRedactor,
                    )
                    .expect("cancel the child");
            }
            created.push(item);
        }
    }
    created
}

/// One open-descendant count of `root`, with the SQLite VM steps it took.
fn measured_descendant_count(store: &SqliteStore, project: &str, root: WorkId) -> (i64, i32) {
    take_descendant_scan_count();
    take_descendant_scan_vm_steps();
    let count = root_open_descendant_count(&store.connection, project, root).expect("count");
    assert_eq!(take_descendant_scan_count(), 1);
    (count, take_descendant_scan_vm_steps())
}

fn descendant_count_vm_step_bound(live_rows: i32) -> i32 {
    DESCENDANT_COUNT_VM_STEPS_PER_LIVE_ROW * live_rows + DESCENDANT_COUNT_VM_STEPS_CONSTANT
}

// The open-descendant count is bounded by the project's proposed and open
// rows, not by a root's history: a root carrying twice as many cancelled
// descendants as live ones is counted with the same work as an all-open root
// of the same live size, and its budget still refuses the next child.
#[test]
fn root_descendant_count_cost_does_not_grow_with_closed_history() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let live = usize::try_from(MAX_OPEN_WORK_DESCENDANTS).expect("limit");
    let closed = 2 * live + 2;
    let mut seconds = 0;

    let all_open = "project-all-open";
    let open_root = store
        .create_work(&root_request(all_open, "root", 0), &DevelopmentNoopRedactor)
        .expect("all-open root");
    grow_root(
        &mut store,
        open_root.work_id,
        "open",
        live,
        false,
        &mut seconds,
    );
    let (open_count, open_steps) = measured_descendant_count(&store, all_open, open_root.work_id);
    assert_eq!(open_count, i64::try_from(live).expect("live"));

    let with_history = "project-closed-history";
    let history_root = store
        .create_work(
            &root_request(with_history, "history-root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("closed-history root");
    grow_root(
        &mut store,
        history_root.work_id,
        "closed",
        closed,
        true,
        &mut seconds,
    );
    grow_root(
        &mut store,
        history_root.work_id,
        "live",
        live,
        false,
        &mut seconds,
    );
    let (history_count, history_steps) =
        measured_descendant_count(&store, with_history, history_root.work_id);
    assert_eq!(history_count, i64::try_from(live).expect("live"));
    eprintln!(
        "descendant count: live={live} closed=0 sqlite_vm_steps={open_steps}; live={live} closed={closed} sqlite_vm_steps={history_steps}"
    );
    // Set before measuring: both projects hold the root plus `live` open rows,
    // so the closed history must not show in the work, within a quarter.
    assert!(
        history_steps * 4 <= open_steps * 5 + DESCENDANT_COUNT_VM_STEPS_CONSTANT,
        "closed history grew the count: all-open {open_steps} steps, with history {history_steps}"
    );
    let live_rows = i32::try_from(live + 1).expect("rows");
    assert!(
        open_steps > 0 && open_steps <= descendant_count_vm_step_bound(live_rows),
        "all-open steps {open_steps} exceed the stated bound"
    );
    assert!(
        history_steps <= descendant_count_vm_step_bound(live_rows),
        "closed-history steps {history_steps} exceed the stated bound"
    );
    // The other project's rows are not visited: they are out of the index
    // range, so the all-open count costs what it cost before they existed,
    // give or take the statement's own setup.
    let (recount, recount_steps) = measured_descendant_count(&store, all_open, open_root.work_id);
    assert_eq!(recount, open_count);
    assert!(
        (recount_steps - open_steps).abs() <= 8,
        "another project's rows changed the count's work: {open_steps} then {recount_steps}"
    );

    // The budget is still enforced on the root with history.
    let current = store.get_work_item(history_root.work_id).expect("root");
    let before = crate::storage::test_database_shape_snapshot(&store.connection).expect("before");
    let over_budget = store.decompose_work(
        &DecomposeWorkRequest {
            parent_id: current.work_id,
            expected_parent_revision: current.revision,
            children: vec![child(
                "over-budget",
                ChildRequirement::Optional,
                "Over budget",
            )],
            prerequisites: Vec::new(),
            authority: WorkPlanningAuthority::Project,
            actor: actor("planner"),
            idempotency_key: "over-open-budget-with-history".into(),
            created_at: at(seconds + 1),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(over_budget, Err(StoreError::InvalidWork(reason))
        if reason.starts_with("decomposition exceeds the root open-descendant budget")));
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).expect("after"),
        before
    );
}

// The count's work follows the project's live rows, including other live
// roots, and it still counts open work below a terminal ancestor and proposed
// rows, while a cancelled row stops counting.
#[test]
fn root_descendant_count_is_project_wide_and_keeps_the_walk_s_members() {
    let project = "project-live-roots";
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut seconds = 0;
    let first = store
        .create_work(&root_request(project, "first", 0), &DevelopmentNoopRedactor)
        .expect("first root");
    let first_live: i64 = 120;
    grow_root(
        &mut store,
        first.work_id,
        "first",
        usize::try_from(first_live).expect("count"),
        false,
        &mut seconds,
    );
    let (first_count, alone_steps) = measured_descendant_count(&store, project, first.work_id);
    assert_eq!(first_count, first_live);

    let second = store
        .create_work(
            &root_request(project, "second", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("second root");
    let second_live: i64 = 80;
    let second_children = grow_root(
        &mut store,
        second.work_id,
        "second",
        usize::try_from(second_live).expect("count"),
        false,
        &mut seconds,
    );
    let (first_again, shared_steps) = measured_descendant_count(&store, project, first.work_id);
    assert_eq!(
        first_again, first_live,
        "another root's rows are not counted"
    );
    let (second_count, _) = measured_descendant_count(&store, project, second.work_id);
    assert_eq!(second_count, second_live);
    eprintln!(
        "descendant count: project live rows {} -> {alone_steps} steps; {} -> {shared_steps} steps",
        first_live + 1,
        first_live + second_live + 2
    );
    // The bound is project-wide: the second root's live rows are visited.
    let live_rows = i32::try_from(first_live + second_live + 2).expect("rows");
    assert!(
        shared_steps <= descendant_count_vm_step_bound(live_rows),
        "steps {shared_steps} exceed the stated bound for {live_rows} live rows"
    );

    // A proposed row counts like an open one; a cancelled row no longer does.
    let probe = &second_children[0];
    let relabel = |store: &SqliteStore, lifecycle: &str| {
        store
            .connection
            .execute(
                "UPDATE work_items SET lifecycle = ?1 WHERE work_id = ?2",
                rusqlite::params![lifecycle, probe.work_id.0.to_string()],
            )
            .expect("relabel the probe row");
    };
    relabel(&store, "proposed");
    assert_eq!(
        measured_descendant_count(&store, project, second.work_id).0,
        second_live
    );
    relabel(&store, "cancelled");
    assert_eq!(
        measured_descendant_count(&store, project, second.work_id).0,
        second_live - 1
    );
    relabel(&store, "open");

    // Open work below a completed parent is still a live descendant of the root.
    let third = store
        .create_work(&root_request(project, "third", 2), &DevelopmentNoopRedactor)
        .expect("third root");
    let planned = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: third.work_id,
                expected_parent_revision: third.revision,
                children: vec![child("middle", ChildRequirement::Required, "Middle")],
                prerequisites: Vec::new(),
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "third-middle".into(),
                created_at: at(9_000),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("middle child");
    let middle = planned.children[0].clone();
    let planned = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: middle.work_id,
                expected_parent_revision: middle.revision,
                children: vec![child("leaf", ChildRequirement::Optional, "Leaf")],
                prerequisites: Vec::new(),
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "third-leaf".into(),
                created_at: at(9_001),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("leaf child");
    let middle = planned.parent;
    let owned = claim(&mut store, &middle, "middle", "claim-middle", 9_002, 60);
    let proof = evidence(
        &mut store,
        &middle,
        &owned,
        "middle",
        "middle-evidence",
        9_003,
    );
    checkpoint(
        &mut store,
        &middle,
        &owned,
        "middle",
        "middle-checkpoint",
        9_004,
        std::slice::from_ref(&proof),
    );
    complete(
        &mut store,
        &middle,
        &owned,
        "middle",
        &proof,
        "middle-done",
        9_005,
    )
    .expect("complete the middle item over its open optional child");
    assert_eq!(
        store
            .get_work_item(middle.work_id)
            .expect("middle")
            .lifecycle,
        WorkLifecycle::Completed
    );
    assert_eq!(
        measured_descendant_count(&store, project, third.work_id).0,
        1,
        "the open leaf below the completed middle item still counts"
    );
    assert!(store.verify_all().expect("doctor").is_healthy());
}

/// Creates `count` open roots in `project` through atomic plans of at most
/// 256 tasks each, returning each root's id and short reference.
fn existing_roots(
    store: &mut SqliteStore,
    project: &str,
    count: usize,
    seconds: &mut i64,
) -> Vec<(WorkId, String)> {
    let mut roots = Vec::with_capacity(count);
    for (batch, start) in (0..count)
        .step_by(crate::domain::MAX_WORK_PLAN_TASKS)
        .enumerate()
    {
        *seconds += 1;
        let receipt = store
            .propose_work_plan(
                &crate::domain::ProposeWorkPlanRequest {
                    project_id: crate::domain::ProjectId(project.into()),
                    actor: actor("planner"),
                    created_at: at(*seconds),
                    plan: crate::domain::WorkPlanInput {
                        tasks: (start..count.min(start + crate::domain::MAX_WORK_PLAN_TASKS))
                            .map(|index| plan_task(&format!("existing-{index}")))
                            .collect(),
                        prerequisites: Vec::new(),
                        idempotency_key: format!("existing-batch-{batch}"),
                    },
                },
                &DevelopmentNoopRedactor,
            )
            .expect("a plan of independent roots");
        roots.extend(
            receipt
                .tasks
                .into_iter()
                .map(|task| (task.work_id, task.short_ref)),
        );
    }
    roots
}

fn plan_task(key: &str) -> crate::domain::WorkPlanTask {
    crate::domain::WorkPlanTask {
        bindings: Vec::new(),
        key: key.into(),
        parent_key: None,
        title: format!("Task {key}"),
        outcome: format!("Deliver {key}"),
        acceptance: vec![format!("{key} delivered")],
        requirement: None,
        kind: None,
        priority: None,
        labels: Vec::new(),
        assigned_to: None,
        deferred_until: None,
        external_ref: None,
        notes: Vec::new(),
    }
}

fn in_degree_refusal() -> String {
    format!(
        "prerequisite in-degree exceeds the per-item bound: at most {MAX_WORK_PREREQUISITES_PER_ITEM} prerequisites per item"
    )
}

/// Canonical decodes an ordinary add makes beyond its item's in-degree: the
/// add verifies each retained edge's event binding once (one decode per
/// edge) and a constant number of records besides.
const PREREQUISITE_ADD_DECODE_SLACK: usize = 8;

// Each ordinary add decodes one record per retained edge, so the per-add cost
// grows with the in-degree and the total with its square; the per-item bound
// caps that at the bound. This records the shape so the cap's purpose stays
// measured, not asserted.
#[test]
fn prerequisite_adds_decode_in_proportion_to_the_in_degree() {
    let project = "project-in-degree-cost";
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut seconds = 0;
    let adds = 128;
    let dependent = store
        .create_work(
            &root_request(project, "dependent", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("dependent");
    let existing = existing_roots(&mut store, project, adds, &mut seconds);
    let mut decodes_per_add = Vec::with_capacity(adds);
    for (index, (prerequisite, _)) in existing.iter().enumerate() {
        let current = store.get_work_item(dependent.work_id).expect("dependent");
        seconds += 1;
        crate::canonical::reset_canonical_decode_count();
        store
            .add_work_prerequisite(
                &ChangeWorkPrerequisiteRequest {
                    work_id: current.work_id,
                    prerequisite_id: *prerequisite,
                    expected_revision: current.revision,
                    authority: delegated(project, "planner"),
                    actor: actor("planner"),
                    idempotency_key: format!("add-{index}"),
                    changed_at: at(seconds),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("an add within the bound");
        decodes_per_add.push(crate::canonical::canonical_decode_count());
    }
    let first = decodes_per_add[0];
    let last = decodes_per_add[adds - 1];
    eprintln!("prerequisite adds: n={adds} canonical_decodes first={first} last={last}");
    for (index, decodes) in decodes_per_add.iter().enumerate() {
        assert!(
            *decodes <= index + first + PREREQUISITE_ADD_DECODE_SLACK,
            "add {index} decoded {decodes} records with {index} edges retained"
        );
    }
    // The growth is real: the last add decodes at least one record per edge
    // beyond the constant part. The per-item bound is what keeps it finite.
    assert!(last >= first + adds - 1 - PREREQUISITE_ADD_DECODE_SLACK);
    assert!(store.verify_all().expect("doctor").is_healthy());
}

// One item takes prerequisites one at a time through the ordinary word all
// the way to the declared bound: the 1024th add is admitted, the 1025th is
// refused by name and writes nothing. The climb costs about a minute because
// each add decodes one record per retained edge, so it runs in the gate's
// `planning_scale_` phase (scripts/test-rust.ps1 and test-rust.sh).
#[test]
#[ignore = "1024 sequential adds belong to the separate scale phase"]
fn planning_scale_prerequisite_adds_one_at_a_time_reach_the_bound() {
    let project = "project-in-degree-climb";
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut seconds = 0;
    let cap = MAX_WORK_PREREQUISITES_PER_ITEM;
    let dependent = store
        .create_work(
            &root_request(project, "dependent", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("dependent");
    let existing = existing_roots(&mut store, project, cap + 1, &mut seconds);
    let add = |store: &mut SqliteStore, prerequisite: WorkId, key: &str, second: i64| {
        let current = store.get_work_item(dependent.work_id).expect("dependent");
        store.add_work_prerequisite(
            &ChangeWorkPrerequisiteRequest {
                work_id: current.work_id,
                prerequisite_id: prerequisite,
                expected_revision: current.revision,
                authority: delegated(project, "planner"),
                actor: actor("planner"),
                idempotency_key: key.into(),
                changed_at: at(second),
            },
            &DevelopmentNoopRedactor,
        )
    };
    for (index, (prerequisite, _)) in existing.iter().take(cap).enumerate() {
        seconds += 1;
        add(
            &mut store,
            *prerequisite,
            &format!("climb-{index}"),
            seconds,
        )
        .expect("an add within the bound");
    }
    assert_eq!(
        store
            .work_prerequisites(dependent.work_id)
            .expect("edges")
            .len(),
        cap
    );
    let before = crate::storage::test_database_shape_snapshot(&store.connection).expect("full");
    seconds += 1;
    let refused = add(&mut store, existing[cap].0, "climb-over", seconds);
    assert!(
        matches!(refused, Err(StoreError::InvalidWork(reason)) if reason == in_degree_refusal())
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).expect("after refusal"),
        before
    );
    assert!(store.verify_all().expect("doctor").is_healthy());
}

// Decomposition counts a proposed child's deduplicated prerequisites before
// any child is written: exactly the bound is admitted, one more is refused
// without writes. The full child then refuses an ordinary add by name and
// writes nothing, a repeated add at the bound stays a no-op, and a removal
// is never refused and makes room for the next add.
#[test]
fn decomposition_refuses_a_child_over_the_prerequisite_bound() {
    let project = "project-in-degree-decomposition";
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut seconds = 0;
    let cap = MAX_WORK_PREREQUISITES_PER_ITEM;
    let existing = existing_roots(&mut store, project, cap + 1, &mut seconds);
    let root = store
        .create_work(
            &root_request(project, "parent", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("parent root");
    let edges = |count: usize| {
        existing
            .iter()
            .take(count)
            .map(|(prerequisite, _)| crate::domain::ChildWorkPrerequisite {
                work_key: "fan-in".into(),
                prerequisite: WorkDependencyRef::Existing(*prerequisite),
            })
            .collect::<Vec<_>>()
    };
    let decompose = |store: &mut SqliteStore, count: usize, key: &str, second: i64| {
        let parent = store.get_work_item(root.work_id).expect("parent");
        store.decompose_work(
            &DecomposeWorkRequest {
                parent_id: parent.work_id,
                expected_parent_revision: parent.revision,
                children: vec![child("fan-in", ChildRequirement::Optional, "Fan-in")],
                prerequisites: edges(count),
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: key.into(),
                created_at: at(second),
            },
            &DevelopmentNoopRedactor,
        )
    };

    let before = crate::storage::test_database_shape_snapshot(&store.connection).expect("before");
    seconds += 1;
    let refused = decompose(&mut store, cap + 1, "over-the-bound", seconds);
    assert!(
        matches!(refused, Err(StoreError::InvalidWork(reason)) if reason == in_degree_refusal())
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).expect("after refusal"),
        before
    );

    seconds += 1;
    let planned = decompose(&mut store, cap, "at-the-bound", seconds).expect("exactly the bound");
    let fan_in = planned.children.into_iter().next().expect("the child");
    assert_eq!(
        store
            .work_prerequisites(fan_in.work_id)
            .expect("edges")
            .len(),
        cap
    );
    let change =
        |store: &mut SqliteStore, prerequisite: WorkId, add: bool, key: &str, second: i64| {
            let current = store.get_work_item(fan_in.work_id).expect("the child");
            let request = ChangeWorkPrerequisiteRequest {
                work_id: current.work_id,
                prerequisite_id: prerequisite,
                expected_revision: current.revision,
                authority: delegated(project, "planner"),
                actor: actor("planner"),
                idempotency_key: key.into(),
                changed_at: at(second),
            };
            if add {
                store.add_work_prerequisite(&request, &DevelopmentNoopRedactor)
            } else {
                store.remove_work_prerequisite(&request, &DevelopmentNoopRedactor)
            }
        };
    let before = crate::storage::test_database_shape_snapshot(&store.connection).expect("full");
    seconds += 1;
    let refused = change(&mut store, existing[cap].0, true, "one-more", seconds);
    assert!(
        matches!(refused, Err(StoreError::InvalidWork(reason)) if reason == in_degree_refusal())
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).expect("after refusal"),
        before
    );

    // A repeated add at the bound is the documented no-op, not a refusal.
    let revision = store.get_work_item(fan_in.work_id).expect("child").revision;
    seconds += 1;
    let repeated = change(&mut store, existing[0].0, true, "add-again", seconds)
        .expect("a repeated add is a no-op");
    assert_eq!(repeated.revision, revision);

    // A completed prerequisite's kept edge still counts: completing one of
    // the 1024 frees no capacity, so the add is refused as before.
    let completed = store
        .get_work_item(existing[1].0)
        .expect("a prerequisite root");
    let owned = claim(
        &mut store,
        &completed,
        "closer",
        "claim-prerequisite",
        20_000,
        60,
    );
    let proof = evidence(
        &mut store,
        &completed,
        &owned,
        "closer",
        "prerequisite-evidence",
        20_001,
    );
    checkpoint(
        &mut store,
        &completed,
        &owned,
        "closer",
        "prerequisite-checkpoint",
        20_002,
        std::slice::from_ref(&proof),
    );
    complete(
        &mut store,
        &completed,
        &owned,
        "closer",
        &proof,
        "prerequisite-done",
        20_003,
    )
    .expect("complete one prerequisite");
    assert_eq!(
        store
            .get_work_item(completed.work_id)
            .expect("completed")
            .lifecycle,
        WorkLifecycle::Completed
    );
    seconds += 1;
    let refused = change(
        &mut store,
        existing[cap].0,
        true,
        "one-more-after-completion",
        seconds,
    );
    assert!(
        matches!(refused, Err(StoreError::InvalidWork(reason)) if reason == in_degree_refusal())
    );

    // Removal is never refused, and the next add then fits.
    seconds += 1;
    change(&mut store, existing[0].0, false, "remove-first", seconds).expect("removal");
    seconds += 1;
    change(
        &mut store,
        existing[cap].0,
        true,
        "add-after-removal",
        seconds,
    )
    .expect("an add that fits after the removal");
    assert_eq!(
        store
            .work_prerequisites(fan_in.work_id)
            .expect("edges")
            .len(),
        cap
    );
    assert!(store.verify_all().expect("doctor").is_healthy());
}

/// Proposes a forest of `roots` independent tasks in `project` and returns the
/// descendant-count scans and SQLite VM steps its admission took.
fn forest_plan_count_work(
    store: &mut SqliteStore,
    project: &str,
    roots: usize,
    key: &str,
    second: i64,
) -> (usize, i32) {
    take_descendant_scan_count();
    take_descendant_scan_vm_steps();
    store
        .propose_work_plan(
            &crate::domain::ProposeWorkPlanRequest {
                project_id: crate::domain::ProjectId(project.into()),
                actor: actor("planner"),
                created_at: at(second),
                plan: crate::domain::WorkPlanInput {
                    tasks: (0..roots)
                        .map(|index| plan_task(&format!("{key}-{index}")))
                        .collect(),
                    prerequisites: Vec::new(),
                    idempotency_key: key.into(),
                },
            },
            &DevelopmentNoopRedactor,
        )
        .expect("a forest plan within every bound");
    (
        take_descendant_scan_count(),
        take_descendant_scan_vm_steps(),
    )
}

// A root a plan creates is counted through its own new subtree, so a wide
// forest plan does the same descendant-count work in a project full of live
// rows as in an empty one: one scan per new root, each over that root alone.
#[test]
fn a_forest_plan_counts_each_new_root_through_its_own_subtree() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let roots = crate::domain::MAX_WORK_PLAN_TASKS;
    let (empty_scans, empty_steps) =
        forest_plan_count_work(&mut store, "project-empty-forest", roots, "forest", 1);
    assert_eq!(empty_scans, roots);

    let busy = "project-busy-forest";
    let mut seconds = 10;
    let background = existing_roots(&mut store, busy, 4 * roots, &mut seconds);
    assert_eq!(background.len(), 4 * roots);
    let (busy_scans, busy_steps) =
        forest_plan_count_work(&mut store, busy, roots, "forest", seconds + 1);
    assert_eq!(busy_scans, roots);
    eprintln!(
        "forest plan of {roots} roots: empty project {empty_steps} steps; over {} live rows {busy_steps} steps",
        4 * roots
    );
    // Set before measuring: the background live rows must not show in the
    // plan's count work, within a quarter.
    assert!(
        busy_steps * 4 <= empty_steps * 5 + 256,
        "live rows grew the plan's count work: empty {empty_steps}, busy {busy_steps}"
    );
    assert!(store.verify_all().expect("doctor").is_healthy());
}

// A plan may give one new task as many existing prerequisites as it may
// declare edges, which is the bound, so every legal plan's fan-in is admitted
// through the shared guard; the task is then full for an ordinary add.
#[test]
fn an_atomic_plan_fan_in_at_the_bound_is_admitted() {
    let project = "project-in-degree-plan";
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut seconds = 0;
    let cap = MAX_WORK_PREREQUISITES_PER_ITEM;
    assert_eq!(cap, crate::domain::MAX_WORK_PLAN_EDGES);
    let existing = existing_roots(&mut store, project, cap + 1, &mut seconds);
    seconds += 1;
    let receipt = store
        .propose_work_plan(
            &crate::domain::ProposeWorkPlanRequest {
                project_id: crate::domain::ProjectId(project.into()),
                actor: actor("planner"),
                created_at: at(seconds),
                plan: crate::domain::WorkPlanInput {
                    tasks: vec![plan_task("verify")],
                    prerequisites: existing
                        .iter()
                        .take(cap)
                        .map(|(_, short_ref)| crate::domain::WorkPlanPrerequisite {
                            work_key: "verify".into(),
                            prerequisite: crate::domain::WorkPlanDependency::Existing(
                                short_ref.clone(),
                            ),
                        })
                        .collect(),
                    idempotency_key: "fan-in-plan".into(),
                },
            },
            &DevelopmentNoopRedactor,
        )
        .expect("a fan-in task at the bound");
    let verify = store
        .get_work_item(receipt.tasks[0].work_id)
        .expect("the fan-in task");
    assert_eq!(
        store
            .work_prerequisites(verify.work_id)
            .expect("edges")
            .len(),
        cap
    );
    seconds += 1;
    let refused = store.add_work_prerequisite(
        &ChangeWorkPrerequisiteRequest {
            work_id: verify.work_id,
            prerequisite_id: existing[cap].0,
            expected_revision: verify.revision,
            authority: delegated(project, "planner"),
            actor: actor("planner"),
            idempotency_key: "one-more".into(),
            changed_at: at(seconds),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(
        matches!(refused, Err(StoreError::InvalidWork(reason)) if reason == in_degree_refusal())
    );
    assert!(store.verify_all().expect("doctor").is_healthy());
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

// An ordinary prerequisite change validates the item's prior relations once
// and hands that basis to its own append. The fingerprint the event records
// is the one an independent reading of the resulting edges gives. A no-op
// change validates once and appends nothing.
#[test]
fn an_ordinary_prerequisite_change_validates_its_relations_once() {
    let project = "project-prerequisite-once";
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut create = |key: &str, second: i64| {
        store
            .create_work(
                &root_request(project, key, second),
                &DevelopmentNoopRedactor,
            )
            .expect(key)
    };
    let dependent = create("dependent", 0);
    let prerequisites = [create("first", 1), create("second", 2), create("third", 3)];
    let change =
        |store: &mut SqliteStore, prerequisite: &WorkItem, add: bool, key: &str, second: i64| {
            let current = store.get_work_item(dependent.work_id).expect("dependent");
            let request = ChangeWorkPrerequisiteRequest {
                work_id: current.work_id,
                prerequisite_id: prerequisite.work_id,
                expected_revision: current.revision,
                authority: delegated(project, "planner"),
                actor: actor("planner"),
                idempotency_key: key.into(),
                changed_at: at(second),
            };
            let before = super::relation_basis_validations();
            let changed = if add {
                store.add_work_prerequisite(&request, &DevelopmentNoopRedactor)
            } else {
                store.remove_work_prerequisite(&request, &DevelopmentNoopRedactor)
            }
            .expect("prerequisite change");
            (changed, super::relation_basis_validations() - before)
        };
    // The fingerprint built here from the expected edges alone.
    let expected_fingerprint = |edges: &[&WorkItem]| {
        let mut prerequisite_ids = edges.iter().map(|item| item.work_id).collect::<Vec<_>>();
        prerequisite_ids.sort_by_key(|id| id.0);
        CanonicalObject::freeze(&WorkRelationBasis {
            schema_version: SCHEMA_VERSION,
            prerequisite_ids,
            active_blockers: Vec::new(),
        })
        .expect("expected fingerprint")
        .key()
        .clone()
    };
    let recorded_fingerprint = |store: &SqliteStore| {
        super::super::query::latest_canonical_work_event_for_item_optional(
            &store.connection,
            dependent.work_id,
        )
        .expect("latest event")
        .expect("an event")
        .relation_fingerprint
    };

    for (count, prerequisite) in prerequisites.iter().enumerate() {
        let (_, validations) = change(
            &mut store,
            prerequisite,
            true,
            &format!("add-{count}"),
            10 + i64::try_from(count).unwrap(),
        );
        assert_eq!(
            validations, 1,
            "add {count} validates once with {count} prior edges"
        );
        let edges = prerequisites.iter().take(count + 1).collect::<Vec<_>>();
        assert_eq!(recorded_fingerprint(&store), expected_fingerprint(&edges));
    }

    // Adding an edge that already exists validates once and appends nothing.
    let revision = store
        .get_work_item(dependent.work_id)
        .expect("dependent")
        .revision;
    let (unchanged, validations) = change(&mut store, &prerequisites[0], true, "add-again", 20);
    assert_eq!(validations, 1);
    assert_eq!(
        unchanged.revision, revision,
        "a no-op change appends nothing"
    );

    let (_, validations) = change(&mut store, &prerequisites[1], false, "remove", 21);
    assert_eq!(validations, 1, "a removal validates once");
    assert_eq!(
        recorded_fingerprint(&store),
        expected_fingerprint(&[&prerequisites[0], &prerequisites[2]])
    );
    assert!(store.verify_all().expect("doctor").is_healthy());
}

// The single validation still comes first: an ordinary add or removal on an
// item whose projected edges disagree with its canonical history is refused
// before anything is written, whether an edge was dropped or one was added
// behind the history.
#[test]
fn an_ordinary_prerequisite_change_refuses_a_corrupt_edge_set() {
    let project = "project-prerequisite-corrupt";
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut create = |key: &str, second: i64| {
        store
            .create_work(
                &root_request(project, key, second),
                &DevelopmentNoopRedactor,
            )
            .expect(key)
    };
    let dependent = create("dependent", 0);
    let first = create("first", 1);
    let second = create("second", 2);
    let third = create("third", 3);
    let request = |store: &SqliteStore, prerequisite: &WorkItem, key: &str, second: i64| {
        let current = store.get_work_item(dependent.work_id).expect("dependent");
        ChangeWorkPrerequisiteRequest {
            work_id: current.work_id,
            prerequisite_id: prerequisite.work_id,
            expected_revision: current.revision,
            authority: delegated(project, "planner"),
            actor: actor("planner"),
            idempotency_key: key.into(),
            changed_at: at(second),
        }
    };
    let added = request(&store, &first, "add-first", 10);
    store
        .add_work_prerequisite(&added, &DevelopmentNoopRedactor)
        .expect("add first");
    let feed = FeedId::Project(dependent.project_id.clone());
    let edges = |store: &SqliteStore| {
        store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM work_prerequisites WHERE work_id = ?1",
                [dependent.work_id.0.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .expect("edge count")
    };
    let refuses_every_change = |store: &mut SqliteStore, label: &str| {
        let head = store.work_feed_head(&feed).expect("feed head");
        let revision = store
            .get_work_item(dependent.work_id)
            .expect("item")
            .revision;
        let edge_count = edges(store);
        for (prerequisite, add) in [(&second, true), (&first, false), (&third, false)] {
            let change = request(store, prerequisite, &format!("{label}-{add}"), 20);
            let refused = if add {
                store.add_work_prerequisite(&change, &DevelopmentNoopRedactor)
            } else {
                store.remove_work_prerequisite(&change, &DevelopmentNoopRedactor)
            };
            assert!(
                matches!(refused, Err(StoreError::InvalidWorkProjection(_))),
                "{label}: change ({add}) must be refused, got {refused:?}"
            );
            assert_eq!(store.work_feed_head(&feed).expect("feed"), head, "{label}");
            assert_eq!(
                store
                    .get_work_item(dependent.work_id)
                    .expect("item")
                    .revision,
                revision,
                "{label}"
            );
            assert_eq!(edges(store), edge_count, "{label}: no edge written");
        }
    };

    // An edge added behind the canonical history.
    let event_id = store
        .connection
        .query_row(
            "SELECT latest_event_id FROM work_items WHERE work_id = ?1",
            [dependent.work_id.0.to_string()],
            |row| row.get::<_, String>(0),
        )
        .expect("latest event");
    store
        .connection
        .execute(
            "INSERT INTO work_prerequisites (work_id, prerequisite_id, event_id)
             VALUES (?1, ?2, ?3)",
            params![
                dependent.work_id.0.to_string(),
                third.work_id.0.to_string(),
                event_id
            ],
        )
        .expect("insert an edge behind history");
    refuses_every_change(&mut store, "extra edge");

    // An edge the canonical history has, dropped from the projection.
    store
        .connection
        .execute(
            "DELETE FROM work_prerequisites WHERE work_id = ?1",
            [dependent.work_id.0.to_string()],
        )
        .expect("drop every projected edge");
    refuses_every_change(&mut store, "omitted edge");
}
