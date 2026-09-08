use super::*;

#[test]
fn show_child_summary_preserves_historical_execution_after_root_reopen() {
    for with_waiver in [false, true] {
        let (_directory, verbs, path, project) = fixture();
        let root = add(&verbs, "Root", None, false, 0);
        let parent = add(&verbs, "Historical parent", Some(&root), false, 1);
        let child = add(&verbs, "Delivered child", Some(&parent), false, 2);
        if with_waiver {
            let disposed = add(&verbs, "Waived child", Some(&parent), false, 3);
            cancel(&verbs, &disposed, 4);
            verbs
                .update(
                    UpdateInput {
                        work_ref: Some(parent.clone()),
                        action: UpdateAction::WaiveRequiredChild {
                            child: disposed,
                            reason: "Approved historical omission".into(),
                        },
                    },
                    at(5),
                )
                .unwrap();
        }
        finish(&verbs, &child, 6);
        finish(&verbs, &parent, 8);
        finish(&verbs, &root, 10);
        let before = verbs.show(&parent, at(12)).unwrap();
        let service = LocalWorkService::new(
            path.clone(),
            project.clone(),
            "agent".into(),
            SessionId("agent".into()),
            None,
        );
        service.work_focus(&root, at(13)).unwrap();
        service
            .work_update(
                crate::work_service::WorkUpdateInput::Reopen {
                    reason: "A new root generation".into(),
                    idempotency_key: "reopen-root".into(),
                },
                at(14),
            )
            .unwrap();
        let store = SqliteStore::open(&path).unwrap();
        assert_eq!(
            store.resolve_work_ref(&project, &root).unwrap().lifecycle,
            WorkLifecycle::Open
        );
        assert_eq!(
            store.resolve_work_ref(&project, &parent).unwrap().lifecycle,
            WorkLifecycle::Completed
        );
        assert!(store.verify_all().unwrap().is_healthy());
        for notes in [false, true] {
            let receipt = verbs
                .show_with_notes(&parent, notes, at(15))
                .expect("completed child parents remain inspectable after root reopen");
            assert_eq!(
                receipt.value["child_obligations"],
                before.value["child_obligations"]
            );
            assert_eq!(
                receipt.value["child_obligations"]["required_owed"]["count"],
                0
            );
            assert_eq!(
                receipt.value["child_obligations"]["open_optional"]["count"],
                0
            );
            assert!(
                receipt
                    .text()
                    .contains("required children still owed (0 of 0 shown)")
            );
            assert!(receipt.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
            assert!(
                serde_json::to_vec_pretty(&receipt.value).unwrap().len()
                    < MAX_AGENT_WORK_RESPONSE_BYTES
            );
        }
        let connection = rusqlite::Connection::open(&path).unwrap();
        let selected = crate::storage::test_database_shape_snapshot(&connection).unwrap();
        verbs.show_with_notes(&parent, true, at(15)).unwrap();
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&connection).unwrap(),
            selected
        );
        if with_waiver {
            let parent_id = store.resolve_work_ref(&project, &parent).unwrap().work_id;
            assert_historical_binding_refuses_corruption(&verbs, &connection, &parent, parent_id);
        }
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

fn assert_historical_binding_refuses_corruption(
    verbs: &AgentVerbs,
    connection: &rusqlite::Connection,
    parent: &str,
    parent_id: crate::WorkId,
) {
    let original: Vec<u8> = connection
        .query_row(
            "SELECT header_json FROM work_root_executions
         WHERE root_execution_id = (
             SELECT root_execution_id FROM work_runs WHERE work_id = ?1
             ORDER BY generation DESC LIMIT 1
         )",
            [parent_id.0.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    let execution: crate::domain::RootExecutionHeader = serde_json::from_slice(&original).unwrap();
    let original_waivers = connection.prepare(
        "SELECT member_hash, member_json FROM work_root_members WHERE root_execution_id = ?1 AND json_extract(member_json, '$.collection') = 'child_waiver'"
    ).unwrap().query_map([execution.root_execution_id.0.to_string()], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)))
        .unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    let waiver_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM work_root_members WHERE root_execution_id = ?1 AND json_extract(member_json, '$.collection') = 'child_waiver'",
        [execution.root_execution_id.0.to_string()], |row| row.get(0)).unwrap();
    assert!(waiver_count > 0);
    let current: i64 = connection
        .query_row(
            "SELECT generation FROM work_root_executions WHERE root_id = ?1 AND state = 'active'",
            [execution.root_id.0.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(current > execution.generation);
    for fault in ["scalar", "canonical_waivers", "generation"] {
        // The reader owns another connection: the deliberate corruption must
        // be committed before that reader can observe it under WAL.
        let mut damaged = execution.clone();
        let mut generation = execution.generation;
        match fault {
            "scalar" => generation += 10,
            "canonical_waivers" => {
                let changed = connection.execute(
                    "DELETE FROM work_root_members WHERE root_execution_id = ?1 AND json_extract(member_json, '$.collection') = 'child_waiver'",
                    [execution.root_execution_id.0.to_string()]).unwrap();
                assert_eq!(i64::try_from(changed).unwrap(), waiver_count);
            }
            "generation" => {
                generation += 10;
                damaged.generation = generation;
            }
            _ => unreachable!(),
        }
        connection
            .execute(
                "UPDATE work_root_executions SET generation = ?1, header_json = ?2
             WHERE root_execution_id = ?3",
                rusqlite::params![
                    generation,
                    serde_json::to_vec(&damaged).unwrap(),
                    execution.root_execution_id.0.to_string()
                ],
            )
            .unwrap();
        let error = verbs.show(parent, at(15)).unwrap_err();
        assert!(
            matches!(error.error, StoreError::InvalidWorkProjection(_)),
            "{fault}: {error:?}"
        );
        connection.execute("UPDATE work_root_executions SET generation = ?1, header_json = ?2 WHERE root_execution_id = ?3",
            rusqlite::params![execution.generation, original, execution.root_execution_id.0.to_string()]).unwrap();
        if fault == "canonical_waivers" {
            for (hash, bytes) in &original_waivers {
                connection
                    .execute(
                        "INSERT INTO work_root_members VALUES (?1, ?2, ?3)",
                        rusqlite::params![execution.root_execution_id.0.to_string(), hash, bytes],
                    )
                    .unwrap();
            }
        }
        let receipt = verbs.show(parent, at(15)).unwrap();
        assert_eq!(
            receipt.value["child_obligations"]["required_owed"]["count"],
            0
        );
    }
}
