use super::*;
use crate::task_sync::memories::{MergeInputs, SeedManifestEntry};
use chrono::{TimeZone, Utc};

fn actor() -> ActorContext {
    ActorContext {
        actor_id: "memory-sync-recorder".into(),
        actor_kind: "agent".into(),
        assurance: crate::domain::AssuranceLevel::Asserted,
        run_id: None,
        session_id: Some(SessionId("memory-sync-session".into())),
        source_tool: Some("task-sync".into()),
        source_skill: None,
        provenance_chain: Vec::new(),
        reason: "test explicit memory exchange".into(),
    }
}

#[test]
fn memory_sync_withholds_whole_mixed_sensitivity_history_without_body_echo() {
    for sensitivity in [
        Sensitivity::Public,
        Sensitivity::Restricted,
        Sensitivity::SecretRef,
    ] {
        let mut store = SqliteStore::open_in_memory().unwrap();
        remember(&mut store, "shareable earlier body", false);
        let history = history(&store);
        let request = RememberProjectMemoryRequest {
            project_id: crate::ProjectId("sync-project".into()),
            session_id: actor().session_id.unwrap(),
            key: Some("rule".into()),
            body: "withheld fixture body".into(),
            actor: actor(),
            created_at: Utc.timestamp_millis_opt(2000).unwrap(),
            revise: true,
            expected_revision: Some(1),
            retiring_target: crate::domain::ProjectMemoryRetiringTargetChange::Keep,
        };
        let mut prepared =
            prepare_project_memory(&request, "rule", history.last(), None, false).unwrap();
        prepared.version.sensitivity = sensitivity;
        prepared.version.source_snapshot = Some(crate::domain::SourceSnapshot {
            source_ref: crate::storage::graph_snapshot::RESTORED_MEMORY_SOURCE.into(),
            fingerprint: memories::body_sha256(&request.body),
            observed_at: request.created_at,
        });
        prepared.version.classification_reason = "restored project episode".into();
        prepared.version_object = CanonicalObject::mint(&prepared.version).unwrap();
        prepared.assertion.version = prepared.version_object.key().clone();
        prepared.assertion.policy_reason = "restored project episode is active immediately".into();
        prepared.assertion_object = CanonicalObject::mint(&prepared.assertion).unwrap();
        let transaction = store.connection.transaction().unwrap();
        SqliteStore::insert_project_memory_version_object(
            &transaction,
            &prepared.version_object,
            &request.project_id,
            "rule",
        )
        .unwrap();
        SqliteStore::insert_object(
            &transaction,
            "memory_assertion_event",
            &prepared.assertion_object,
        )
        .unwrap();
        SqliteStore::apply_memory_projection(
            &transaction,
            prepared.version_object.key(),
            prepared.assertion_object.key(),
            &prepared.version,
            &prepared.assertion,
            MemoryProjectionMode::Live,
        )
        .unwrap();
        advance_project_memory_state_on(&transaction, &request.project_id, 0).unwrap();
        transaction.commit().unwrap();
        let result = store
            .export_project_memory_sync(&request.project_id, &BTreeMap::new())
            .unwrap();
        assert!(result.keys.is_empty());
        assert_eq!(result.withheld.len(), 1);
        assert!(result.withheld[0].reason.contains("sensitivity"));
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("withheld fixture body")
        );
    }
}

#[test]
fn memory_sync_source_clear_on_fresh_host_does_not_invent_native_target() {
    let mut source = SqliteStore::open_in_memory().unwrap();
    remember(&mut source, "merged rule with no target", false);
    let mut selected = capture(&source, None);
    for value in selected.revisions.values_mut() {
        value.provenance.source_clear = true;
    }
    let mut destination = SqliteStore::open_in_memory().unwrap();
    import(&mut destination, &selected, false, None).unwrap();
    let stored = history(&destination);
    assert!(!stored[0].version.retiring_target_cleared);
    assert!(stored[0].version.retiring_target.is_none());
    assert_eq!(capture(&destination, Some(&selected)), selected);
}

fn remember(store: &mut SqliteStore, body: &str, revise: bool) {
    store
        .remember_project_memory(
            &RememberProjectMemoryRequest {
                project_id: crate::ProjectId("sync-project".into()),
                session_id: actor().session_id.unwrap(),
                key: Some("rule".into()),
                body: body.into(),
                actor: actor(),
                created_at: Utc
                    .timestamp_millis_opt(history(store).last().map_or(1000, |entry| {
                        entry.version.created_at.timestamp_millis() + 1000
                    }))
                    .unwrap(),
                revise,
                expected_revision: None,
                retiring_target: crate::domain::ProjectMemoryRetiringTargetChange::Keep,
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
}

fn capture(store: &SqliteStore, selected: Option<&KeyHistory>) -> KeyHistory {
    let basis = selected
        .map(|s| BTreeMap::from([(s.key.clone(), s.clone())]))
        .unwrap_or_default();
    let result = store
        .export_project_memory_sync(&crate::ProjectId("sync-project".into()), &basis)
        .unwrap();
    assert!(result.withheld.is_empty(), "{:?}", result.withheld);
    result.keys["rule"].clone()
}

fn history(store: &SqliteStore) -> Vec<StoredProjectMemory> {
    project_memory_history_on(
        &store.connection,
        &crate::ProjectId("sync-project".into()),
        "rule",
    )
    .unwrap()
}

fn import(
    store: &mut SqliteStore,
    selected: &KeyHistory,
    dry_run: bool,
    merge: Option<&MergeInputs>,
) -> Result<ImportOutcome, StoreError> {
    let local = history(store);
    let actor = actor();
    store.import_project_memory_sync(&ImportRequest {
        selected,
        expected_local_head: local.last().map(|e| &e.version_id),
        actor: &actor,
        session_id: actor.session_id.as_ref().unwrap(),
        now: Utc
            .timestamp_millis_opt(local.last().map_or(10000, |entry| {
                entry.version.created_at.timestamp_millis() + 1000
            }))
            .unwrap(),
        dry_run,
        merge,
        seed: None,
        mark_untagged_seed: false,
    })
}

#[test]
fn memory_sync_prefix_import_and_three_host_relay_do_not_bounce() {
    let mut a = SqliteStore::open_in_memory().unwrap();
    let mut b = SqliteStore::open_in_memory().unwrap();
    let mut c = SqliteStore::open_in_memory().unwrap();
    remember(&mut a, "first", false);
    let prefix = capture(&a, None);
    assert_eq!(
        import(&mut b, &prefix, false, None)
            .unwrap()
            .appended_revisions,
        1
    );
    remember(&mut a, "second", true);
    remember(&mut a, "third", true);
    let shared = capture(&a, Some(&prefix));
    assert_eq!(
        import(&mut b, &shared, false, None)
            .unwrap()
            .appended_revisions,
        2
    );
    assert_eq!(
        history(&b)
            .iter()
            .map(|e| e.version.body.as_str())
            .collect::<Vec<_>>(),
        ["first", "second", "third"]
    );
    let relay = capture(&b, Some(&shared));
    assert_eq!(relay, shared);
    assert_eq!(
        import(&mut c, &relay, false, None)
            .unwrap()
            .appended_revisions,
        3
    );
    assert_eq!(capture(&c, Some(&shared)), shared);
    let changes = a.connection.total_changes();
    assert!(import(&mut a, &shared, false, None).unwrap().duplicate);
    assert_eq!(a.connection.total_changes(), changes);
    assert!(import(&mut b, &shared, false, None).unwrap().duplicate);
    assert_eq!(capture(&b, Some(&shared)), shared);
}

#[test]
fn memory_sync_dry_run_and_stale_head_preserve_the_store() {
    let mut a = SqliteStore::open_in_memory().unwrap();
    remember(&mut a, "first", false);
    remember(&mut a, "second", true);
    let shared = capture(&a, None);
    let mut b = SqliteStore::open_in_memory().unwrap();
    let before = crate::storage::test_database_shape_snapshot(&b.connection);
    assert_eq!(
        import(&mut b, &shared, true, None)
            .unwrap()
            .appended_revisions,
        2
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&b.connection),
        before
    );
    import(&mut b, &shared, false, None).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&b.connection);
    let actor = actor();
    assert!(
        b.import_project_memory_sync(&ImportRequest {
            selected: &shared,
            expected_local_head: None,
            actor: &actor,
            session_id: actor.session_id.as_ref().unwrap(),
            now: Utc.timestamp_millis_opt(2000).unwrap(),
            dry_run: false,
            merge: None,
            seed: None,
            mark_untagged_seed: false
        })
        .is_err()
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&b.connection),
        before
    );
}

#[test]
fn memory_sync_unknown_tombstone_reserves_key_and_relays_without_bodies() {
    let mut a = SqliteStore::open_in_memory().unwrap();
    remember(&mut a, "old body", false);
    let recorder = actor();
    a.forget_project_memory(
        &ForgetProjectMemoryRequest {
            project_id: crate::ProjectId("sync-project".into()),
            session_id: recorder.session_id.clone().unwrap(),
            key: "rule".into(),
            actor: recorder,
            created_at: Utc.timestamp_millis_opt(2000).unwrap(),
        },
        &DevelopmentNoopRedactor,
    )
    .unwrap();
    let tombstone = capture(&a, None);
    assert!(tombstone.revisions.is_empty());
    let mut b = SqliteStore::open_in_memory().unwrap();
    assert!(import(&mut b, &tombstone, false, None).unwrap().retired);
    assert_eq!(
        project_memory_state_on(&b.connection, &tombstone.project_id).unwrap(),
        (0, 1)
    );
    assert_eq!(
        derived_project_memory_state_on(&b.connection, &tombstone.project_id).unwrap(),
        (0, 1)
    );
    assert_eq!(capture(&b, Some(&tombstone)), tombstone);
    assert!(import(&mut b, &tombstone, false, None).unwrap().duplicate);
    assert_eq!(
        project_memory_state_on(&b.connection, &tombstone.project_id).unwrap(),
        (0, 1)
    );
    let raw = history(&b);
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0].assertion.status, MemoryStatus::Tombstoned);
    assert_eq!(raw[0].version.body, "[retired project memory]");
    assert!(matches!(
        b.remember_project_memory(
            &RememberProjectMemoryRequest {
                project_id: crate::ProjectId("sync-project".into()),
                session_id: actor().session_id.unwrap(),
                key: Some("rule".into()),
                body: "revive".into(),
                actor: actor(),
                created_at: Utc.timestamp_millis_opt(3000).unwrap(),
                revise: false,
                expected_revision: None,
                retiring_target: crate::domain::ProjectMemoryRetiringTargetChange::Keep
            },
            &DevelopmentNoopRedactor
        ),
        Err(StoreError::ProjectMemoryRetired(_))
    ));
}

#[test]
fn memory_sync_unknown_retirement_updates_empty_and_active_project_state_atomically() {
    let mut source = SqliteStore::open_in_memory().unwrap();
    remember(&mut source, "source", false);
    let mut tombstone = capture(&source, None);
    tombstone.head = Head::Tombstone {
        source: Source::from_actor(&actor(), Utc.timestamp_millis_opt(2000).unwrap()),
    };
    tombstone.revisions.clear();
    for active in [false, true] {
        let mut store = SqliteStore::open_in_memory().unwrap();
        if active {
            remember(&mut store, "existing", false);
            tombstone.key = "retired-other".into();
        } else {
            tombstone.key = "rule".into();
        }
        let before = crate::storage::test_database_shape_snapshot(&store.connection);
        // Inject a late state-write failure after canonical/projection writes.
        store.connection.execute_batch("CREATE TEMP TRIGGER refuse_terminal_state BEFORE INSERT ON project_memory_state BEGIN SELECT RAISE(ABORT, 'terminal state test refusal'); END;").unwrap();
        let with_trigger = crate::storage::test_database_shape_snapshot(&store.connection);
        let recorder = actor();
        let request = ImportRequest {
            selected: &tombstone,
            expected_local_head: None,
            actor: &recorder,
            session_id: recorder.session_id.as_ref().unwrap(),
            now: Utc.timestamp_millis_opt(3000).unwrap(),
            dry_run: false,
            merge: None,
            seed: None,
            mark_untagged_seed: false,
        };
        assert!(store.import_project_memory_sync(&request).is_err());
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&store.connection),
            with_trigger
        );
        store
            .connection
            .execute_batch("DROP TRIGGER refuse_terminal_state")
            .unwrap();
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&store.connection),
            before
        );
        store.import_project_memory_sync(&request).unwrap();
        let expected = (i64::from(active), if active { 2 } else { 1 });
        assert_eq!(
            derived_project_memory_state_on(&store.connection, &tombstone.project_id).unwrap(),
            expected
        );
        assert_eq!(
            project_memory_state_on(&store.connection, &tombstone.project_id).unwrap(),
            (expected.0 as usize, expected.1)
        );
        let mut another = tombstone.clone();
        another.key = "another-retirement".into();
        store
            .import_project_memory_sync(&ImportRequest {
                selected: &another,
                ..request
            })
            .unwrap();
        assert_eq!(
            derived_project_memory_state_on(&store.connection, &tombstone.project_id).unwrap(),
            (expected.0, expected.1 + 1)
        );
    }
}

#[test]
fn memory_sync_concurrent_forgets_keep_selected_terminal_without_native_rewrite() {
    let mut a = SqliteStore::open_in_memory().unwrap();
    let mut b = SqliteStore::open_in_memory().unwrap();
    remember(&mut a, "shared", false);
    let active = capture(&a, None);
    import(&mut b, &active, false, None).unwrap();
    for (store, millis, reason) in [
        (&mut a, 20000, "retire on A"),
        (&mut b, 21000, "retire on B"),
    ] {
        let mut recorder = actor();
        recorder.reason = reason.into();
        store
            .forget_project_memory(
                &ForgetProjectMemoryRequest {
                    project_id: active.project_id.clone(),
                    session_id: recorder.session_id.clone().unwrap(),
                    key: "rule".into(),
                    actor: recorder,
                    created_at: Utc.timestamp_millis_opt(millis).unwrap(),
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
    }
    let selected = capture(&a, None);
    assert_ne!(capture(&b, None), selected);
    let before = crate::storage::test_database_shape_snapshot(&b.connection);
    for _ in 0..2 {
        assert!(import(&mut b, &selected, false, None).unwrap().duplicate);
        assert_eq!(capture(&b, Some(&selected)), selected);
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&b.connection),
            before
        );
    }
}

#[test]
fn memory_sync_known_retirement_blocks_active_import_and_export() {
    let mut a = SqliteStore::open_in_memory().unwrap();
    remember(&mut a, "live", false);
    let active = capture(&a, None);
    let mut b = SqliteStore::open_in_memory().unwrap();
    import(&mut b, &active, false, None).unwrap();
    let mut tombstone = active.clone();
    tombstone.head = Head::Tombstone {
        source: Source::from_actor(&actor(), Utc.timestamp_millis_opt(2000).unwrap()),
    };
    tombstone.revisions.clear();
    assert!(import(&mut b, &tombstone, false, None).unwrap().retired);
    assert!(import(&mut b, &active, false, None).is_err());
    let exported = a
        .export_project_memory_sync(
            &active.project_id,
            &BTreeMap::from([("rule".into(), tombstone)]),
        )
        .unwrap();
    assert!(exported.keys.is_empty());
    assert_eq!(exported.withheld.len(), 1);
    assert!(exported.withheld[0].reason.contains("tombstone"));
}

#[test]
fn memory_sync_missing_historical_target_refuses_before_any_revision() {
    let mut a = SqliteStore::open_in_memory().unwrap();
    remember(&mut a, "targeted", false);
    remember(&mut a, "cleared", true);
    let mut selected = capture(&a, None);
    let ids = match &selected.head {
        Head::Active { chain } => chain.clone(),
        _ => unreachable!(),
    };
    selected
        .revisions
        .get_mut(&ids[0])
        .unwrap()
        .provenance
        .retiring_target = Some(crate::domain::ProjectMemoryRetiringTarget::Local {
        work_id: crate::WorkId::new(),
        work_ref: "w-000000000001".into(),
    });
    selected
        .revisions
        .get_mut(&ids[1])
        .unwrap()
        .provenance
        .source_clear = true;
    let mut b = SqliteStore::open_in_memory().unwrap();
    let before = crate::storage::test_database_shape_snapshot(&b.connection);
    assert!(import(&mut b, &selected, false, None).is_err());
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&b.connection),
        before
    );
}

#[test]
fn memory_sync_divergent_merge_keeps_native_histories_and_later_suffix() {
    let mut a = SqliteStore::open_in_memory().unwrap();
    let mut b = SqliteStore::open_in_memory().unwrap();
    remember(&mut a, "original", false);
    let common = capture(&a, None);
    import(&mut b, &common, false, None).unwrap();
    remember(&mut a, "side A", true);
    remember(&mut b, "side B", true);
    let side_a = capture(&a, Some(&common));
    let side_b = capture(&b, Some(&common));
    assert!(import(&mut b, &side_a, false, None).is_err());
    // The author captures an ordinary new revision before materializing the
    // agent's selected shared common-prefix-plus-result tree.
    remember(&mut a, "merged A and B", true);
    let native = history(&a);
    let common_id = match &common.head {
        Head::Active { chain } => chain[0].clone(),
        _ => unreachable!(),
    };
    let merged = revision(
        native.last().unwrap(),
        &common.project_id,
        "rule",
        Some(common_id.clone()),
    )
    .unwrap();
    let merged_id = merged.provenance.revision_id.clone();
    let selected = KeyHistory {
        format: memories::FORMAT.into(),
        project_id: common.project_id.clone(),
        key: "rule".into(),
        head: Head::Active {
            chain: vec![common_id, merged_id.clone()],
        },
        revisions: common
            .revisions
            .clone()
            .into_iter()
            .chain([(merged_id, merged)])
            .collect(),
    };
    let proof = MergeInputs {
        common: Some(common),
        local: side_b,
        incoming: side_a,
    };
    let author_proof = MergeInputs {
        common: proof.common.clone(),
        local: proof.incoming.clone(),
        incoming: proof.local.clone(),
    };
    let author_export = a
        .export_project_memory_sync_with_resolutions(
            &selected.project_id,
            &BTreeMap::from([("rule".into(), author_proof.local.clone())]),
            &BTreeMap::from([(
                "rule".into(),
                memories::ImportOptions {
                    merge: Some(author_proof.clone()),
                    ..Default::default()
                },
            )]),
        )
        .unwrap();
    assert!(author_export.withheld.is_empty());
    assert_eq!(author_export.keys["rule"], selected);
    // A retry may see the already-published result, but a new incoming edit
    // was not reviewed and must never be replaced by the stale merge.
    let options = BTreeMap::from([(
        "rule".into(),
        memories::ImportOptions {
            merge: Some(author_proof.clone()),
            ..Default::default()
        },
    )]);
    let retry = a
        .export_project_memory_sync_with_resolutions(
            &selected.project_id,
            &BTreeMap::from([("rule".into(), selected.clone())]),
            &options,
        )
        .unwrap();
    assert_eq!(retry.keys["rule"], selected);
    let mut incoming_editor = SqliteStore::open_in_memory().unwrap();
    import(&mut incoming_editor, &author_proof.incoming, false, None).unwrap();
    remember(&mut incoming_editor, "unreviewed incoming suffix", true);
    let changed_incoming = capture(&incoming_editor, Some(&author_proof.incoming));
    let refused = a
        .export_project_memory_sync_with_resolutions(
            &selected.project_id,
            &BTreeMap::from([("rule".into(), changed_incoming)]),
            &options,
        )
        .unwrap();
    assert!(refused.keys.is_empty());
    assert!(
        refused.withheld[0]
            .reason
            .contains("selected input changed")
    );
    assert_eq!(
        import(&mut b, &selected, false, Some(&proof))
            .unwrap()
            .appended_revisions,
        1
    );
    assert_eq!(
        history(&b)
            .iter()
            .map(|e| e.version.body.as_str())
            .collect::<Vec<_>>(),
        ["original", "side B", "merged A and B"]
    );
    assert_eq!(capture(&b, Some(&selected)), selected);
    assert_eq!(capture(&a, Some(&selected)), selected);
    remember(&mut b, "later", true);
    let later = capture(&b, Some(&selected));
    assert!(import(&mut b, &later, false, None).unwrap().duplicate);
    assert_eq!(capture(&b, Some(&later)), later);
}

#[test]
fn memory_sync_untagged_seed_is_marked_only_on_explicit_request() {
    let mut a = SqliteStore::open_in_memory().unwrap();
    remember(&mut a, "earlier", false);
    remember(&mut a, "seed", true);
    let selected = capture(&a, None);
    let mut b = SqliteStore::open_in_memory().unwrap();
    remember(&mut b, "seed", false);
    let local = history(&b);
    let shared_id = match &selected.head {
        Head::Active { chain } => chain.last().unwrap().clone(),
        _ => unreachable!(),
    };
    let manifest = SeedManifestEntry {
        project_id: selected.project_id.clone(),
        key: "rule".into(),
        local_version_id: local[0].version_id.clone(),
        shared_revision_id: shared_id,
        body_sha256: memories::body_sha256("seed"),
    };
    let actor = actor();
    let mut request = ImportRequest {
        selected: &selected,
        expected_local_head: Some(&local[0].version_id),
        actor: &actor,
        session_id: actor.session_id.as_ref().unwrap(),
        now: Utc.timestamp_millis_opt(2000).unwrap(),
        dry_run: false,
        merge: None,
        seed: Some(&manifest),
        mark_untagged_seed: false,
    };
    assert!(b.import_project_memory_sync(&request).is_err());
    assert_eq!(history(&b).len(), 1);
    request.mark_untagged_seed = true;
    assert_eq!(
        b.import_project_memory_sync(&request)
            .unwrap()
            .appended_revisions,
        1
    );
    assert_eq!(
        history(&b)
            .iter()
            .map(|e| e.version.body.as_str())
            .collect::<Vec<_>>(),
        ["seed", "seed"]
    );
    assert_eq!(capture(&b, Some(&selected)), selected);
    let current = history(&b);
    request.expected_local_head = Some(&current.last().unwrap().version_id);
    assert!(b.import_project_memory_sync(&request).unwrap().duplicate);
    assert_eq!(history(&b).len(), 2);
}
