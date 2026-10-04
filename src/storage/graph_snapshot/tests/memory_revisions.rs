use std::collections::HashMap;

use super::*;

#[test]
fn snapshot_memory_history_import_decodes_linearly() {
    let project = ProjectId("decode-snapshot".into());
    let mut source = SqliteStore::open_in_memory().unwrap();
    for revision in 1..=12 {
        source
            .remember_project_memory(
                &RememberProjectMemoryRequest {
                    project_id: project.clone(),
                    session_id: actor("author").session_id.unwrap(),
                    key: Some("rule".into()),
                    body: format!("body {revision}"),
                    actor: actor("author"),
                    created_at: at(revision),
                    revise: revision > 1,
                    expected_revision: None,
                    retiring_target: crate::domain::ProjectMemoryRetiringTargetChange::Keep,
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
    }
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("save"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(20),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut destination = SqliteStore::open_in_memory().unwrap();
    crate::canonical::reset_canonical_decode_count();
    destination
        .load_work_graph_snapshot(
            &project,
            &actor("load"),
            &snapshot_bytes(&saved.document),
            false,
            at(21),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let decodes = crate::canonical::canonical_decode_count();
    eprintln!("snapshot 12-version import decodes={decodes}");
    assert!(decodes <= 8 * 12 + 32, "import decoded {decodes}");
    assert!(destination.verify_all().unwrap().is_healthy());
}

#[test]
fn snapshot_retains_live_memory_revisions_but_never_carries_retired_bodies() {
    let directory = crate::test_support::temp_home().unwrap();
    let project = ProjectId("revision-snapshot".into());
    let mut source = SqliteStore::open(directory.path().join("source.db")).unwrap();
    let retiring_work = create_root(&mut source, &project, "Retiring fix", "retiring-fix");
    let bodies = [
        "first attributed belief",
        "second corrected belief",
        "third current belief",
    ];
    let authors = [actor("first"), actor("second"), actor("third")];
    for (index, body) in bodies.iter().enumerate() {
        source
            .remember_project_memory(
                &RememberProjectMemoryRequest {
                    project_id: project.clone(),
                    session_id: authors[index].session_id.clone().unwrap(),
                    key: Some("belief".into()),
                    body: (*body).into(),
                    actor: authors[index].clone(),
                    created_at: at(i64::try_from(index).unwrap()),
                    revise: index > 0,
                    expected_revision: None,
                    retiring_target: match index {
                        0 => crate::domain::ProjectMemoryRetiringTargetChange::Set {
                            target: crate::domain::ProjectMemoryRetiringTargetInput::Local {
                                work_ref: retiring_work.short_ref.clone(),
                            },
                        },
                        2 => crate::domain::ProjectMemoryRetiringTargetChange::Clear,
                        _ => crate::domain::ProjectMemoryRetiringTargetChange::Keep,
                    },
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
    }
    let memory_state = |store: &SqliteStore| -> (i64, i64) {
        store.connection.query_row(
            "SELECT active_count, change_position FROM project_memory_state WHERE project_id = ?1",
            [&project.0], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap()
    };
    let before_save = memory_state(&source);
    assert_eq!(before_save.0, 1);
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("save"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let memory = &saved.document.body.memories[0];
    assert_eq!(memory_state(&source), before_save);
    assert_eq!(memory.history.len(), 2);
    assert_eq!(memory.history[0].revision, 1);
    assert_eq!(memory.history[1].revision, 2);
    assert!(
        memory
            .history
            .iter()
            .all(|revision| revision.retiring_target.is_some())
    );
    assert!(
        memory
            .history
            .iter()
            .all(|revision| !revision.retiring_target_cleared)
    );
    assert!(matches!(
        &memory.state,
        WorkGraphSnapshotMemoryState::Active {
            retiring_target: None,
            retiring_target_cleared: true,
            ..
        }
    ));
    let mut destination = SqliteStore::open(directory.path().join("destination.db")).unwrap();
    destination
        .load_work_graph_snapshot(
            &project,
            &actor("load"),
            &snapshot_bytes(&saved.document),
            false,
            at(5),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    for (index, body) in bodies.iter().enumerate() {
        let full = destination
            .project_memory_full(
                &project,
                &crate::SessionId("reader".into()),
                &actor("reader"),
                "belief",
                Some(index as u64 + 1),
            )
            .unwrap();
        assert_eq!(&full.body, body);
        assert_eq!(full.actor_id, authors[index].actor_id);
        assert_eq!(full.session_id, authors[index].session_id);
        assert_eq!(full.revision, index as u64 + 1);
        assert_eq!(full.current_revision, 3);
        assert_eq!(full.retiring_target.is_some(), index < 2);
        assert!(
            full.retiring_target_dropped.is_none(),
            "the loaded clear is still a clear, not a drop"
        );
    }
    let resaved = destination
        .save_work_graph_snapshot(
            &project,
            &actor("save"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(6),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    assert_eq!(resaved.document.body.memories, saved.document.body.memories);
    let observed = destination.verify_all().unwrap().invalid_objects;
    assert!(observed.is_empty(), "{observed:?}");
    let mut corrupt_store = SqliteStore::open(directory.path().join("corrupt.db")).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&corrupt_store.connection);
    for mutate in 0..6 {
        let mut document = saved.document.clone();
        let memory = &mut document.body.memories[0];
        match mutate {
            0 => memory.history.swap(0, 1),
            1 => memory.history[1].revision = 4,
            2 => memory.history[0].remembered_at = at(20),
            // A clear on the first revision follows nothing.
            3 => memory.history[0].retiring_target_cleared = true,
            // A clear never carries a target.
            4 => memory.history[1].retiring_target_cleared = true,
            // A clear must follow a target still in force; here none ever was.
            5 => {
                memory.history[0].retiring_target = None;
                memory.history[1].retiring_target = None;
            }
            _ => unreachable!(),
        }
        rebind_snapshot_body(&mut document);
        assert!(matches!(
            corrupt_store.load_work_graph_snapshot(
                &project,
                &actor("load"),
                &snapshot_bytes(&document),
                false,
                at(7),
                &DevelopmentNoopRedactor
            ),
            Err(StoreError::InvalidGraphSnapshot(_))
        ));
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&corrupt_store.connection),
            before
        );
    }
    source
        .forget_project_memory(
            &crate::ForgetProjectMemoryRequest {
                project_id: project.clone(),
                session_id: crate::SessionId("retirer".into()),
                key: "belief".into(),
                actor: actor("retirer"),
                created_at: at(8),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let after_forget = memory_state(&source);
    assert_eq!(after_forget.0, 0);
    assert!(after_forget.1 > before_save.1);
    let forgotten = source
        .save_work_graph_snapshot(
            &project,
            &actor("save"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(9),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let observed = &forgotten.document.body.memories[0].history;
    assert!(observed.is_empty(), "{observed:?}");
    assert_eq!(memory_state(&source), after_forget);
    assert_eq!(forgotten.document.body.summary.secret_ref_bodies, 0);
    assert!(matches!(
        forgotten.document.body.memories[0].state,
        WorkGraphSnapshotMemoryState::Tombstone { .. }
    ));
    let bytes = snapshot_bytes(&forgotten.document);
    let text = String::from_utf8(bytes.clone()).unwrap();
    for body in bodies {
        assert!(!text.contains(body));
    }
    let mut retired = SqliteStore::open(directory.path().join("retired.db")).unwrap();
    retired
        .load_work_graph_snapshot(
            &project,
            &actor("load"),
            &bytes,
            false,
            at(10),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    for store in [&source, &retired] {
        assert!(matches!(
            store.project_memory_full(
                &project,
                &crate::SessionId("reader".into()),
                &actor("reader"),
                "belief",
                Some(1)
            ),
            Err(StoreError::ProjectMemoryRetired(_))
        ));
    }
    let origin = super::super::super::project_memory::project_memory_history_on(
        &source.connection,
        &project,
        "belief",
    )
    .unwrap();
    assert_eq!(origin.len(), 3);
    let restored = super::super::super::project_memory::project_memory_history_on(
        &retired.connection,
        &project,
        "belief",
    )
    .unwrap();
    assert_eq!(restored.len(), 1);
    for body in bodies {
        assert!(restored.iter().all(|entry| entry.version.body != body));
    }
}

/// The memory id, version id and assertion id of every restored memory
/// object, by project key; assertions are matched to their version.
fn restored_chains(store: &SqliteStore) -> HashMap<String, Vec<(String, String)>> {
    let mut statement = store
        .connection
        .prepare(
            "SELECT json_extract(version.canonical_json, '$.project_key'),
                    json_extract(version.canonical_json, '$.memory_id'),
                    json_extract(assertion.canonical_json, '$.memory_id')
             FROM objects AS version
             JOIN objects AS assertion
               ON assertion.object_kind = 'memory_assertion_event'
              AND json_extract(assertion.canonical_json, '$.version') = version.object_id
             WHERE version.object_kind = 'memory_version'",
        )
        .unwrap();
    let mut chains: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for row in statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
    {
        let (key, version_id, assertion_id): (String, String, String) = row.unwrap();
        chains
            .entry(key)
            .or_default()
            .push((version_id, assertion_id));
    }
    chains
}

// The snapshot carries no memory id, so each load mints one per restored
// chain: every version and assertion of a chain, tombstone included, share
// it, and loading the same bytes again mints a different one.
#[test]
fn each_load_mints_one_fresh_memory_id_per_restored_chain() {
    let project = ProjectId("restored-memory-id".into());
    let mut source = SqliteStore::open_in_memory().unwrap();
    let remember = |source: &mut SqliteStore, key: &str, body: &str, second: i64, revise: bool| {
        source
            .remember_project_memory(
                &RememberProjectMemoryRequest {
                    project_id: project.clone(),
                    session_id: actor("author").session_id.unwrap(),
                    key: Some(key.into()),
                    body: body.into(),
                    actor: actor("author"),
                    created_at: at(second),
                    revise,
                    expected_revision: None,
                    retiring_target: crate::domain::ProjectMemoryRetiringTargetChange::Keep,
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
    };
    for (revision, body) in ["first", "second", "third"].into_iter().enumerate() {
        let revision = i64::try_from(revision).unwrap();
        remember(&mut source, "chain", body, revision, revision > 0);
    }
    remember(&mut source, "gone", "retired soon", 5, false);
    source
        .forget_project_memory(
            &crate::domain::ForgetProjectMemoryRequest {
                project_id: project.clone(),
                session_id: actor("author").session_id.unwrap(),
                key: "gone".into(),
                actor: actor("author"),
                created_at: at(6),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("save"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(10),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let bytes = snapshot_bytes(&saved.document);
    // The snapshot names no memory id, so load has none to keep.
    assert!(!String::from_utf8_lossy(&bytes).contains("memory_id"));

    let mut loads = Vec::new();
    for _ in 0..2 {
        let mut destination = SqliteStore::open_in_memory().unwrap();
        destination
            .load_work_graph_snapshot(
                &project,
                &actor("load"),
                &bytes,
                false,
                at(11),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        assert!(destination.verify_all().unwrap().is_healthy());
        let chains = restored_chains(&destination);
        let mut ids = HashMap::new();
        for (key, versions) in &chains {
            let id = &versions[0].0;
            assert!(
                versions
                    .iter()
                    .all(|(version, assertion)| version == id && assertion == id),
                "{key}: {versions:?}"
            );
            assert_eq!(uuid::Uuid::parse_str(id).unwrap().get_version_num(), 7);
            ids.insert(key.clone(), id.clone());
        }
        assert_eq!(chains["chain"].len(), 3, "every revision is restored");
        // A retired memory travels as its tombstone alone, never its bodies.
        assert_eq!(chains["gone"].len(), 1, "the tombstone is restored");
        assert_ne!(ids["chain"], ids["gone"]);
        let head: String = destination
            .connection
            .query_row(
                "SELECT memory_id FROM memory_heads WHERE version_id IN (
                     SELECT object_id FROM objects
                     WHERE json_extract(canonical_json, '$.project_key') = 'chain')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(head, ids["chain"]);
        loads.push(ids);
    }
    for key in ["chain", "gone"] {
        assert_ne!(loads[0][key], loads[1][key], "{key} is minted per load");
    }
}
