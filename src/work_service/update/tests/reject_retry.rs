use super::*;

fn assert_safe_retry_refusal(error: &StoreError, child_ref: &str, parent_ref: &str, key: &str) {
    let envelope = crate::mcp::store_error_value(error);
    let value = &envelope["error"];
    assert_eq!(value["code"], "work_reject_refused");
    assert_eq!(value["details"]["child_ref"], child_ref);
    assert_eq!(value["details"]["parent_ref"], parent_ref);
    let text = format!("{error} {value}");
    assert!(text.contains(&format!("engram work show {child_ref}")));
    assert!(!text.contains(key));
    assert!(!text.contains("auto:"));
    assert!(!text.contains("idempotency"));
    assert!(
        !text
            .as_bytes()
            .windows(64)
            .any(|part| part.iter().all(u8::is_ascii_hexdigit))
    );
}

#[test]
fn hygiene_reject_pending_and_committed_retries_are_guarded() {
    for stage in 0..3 {
        let dir = crate::test_support::temp_home().unwrap();
        let path = dir.path().join("retry.db");
        let project = ProjectId("reject-retry".into());
        let session = SessionId("agent".into());
        let service = LocalWorkService::new(
            path.clone(),
            project.clone(),
            "agent".into(),
            session.clone(),
            None,
        );
        let parent = proposed_root(
            service
                .work_propose(root_input("Parent", "parent"), at(0))
                .unwrap(),
        );
        let decomposition = service.work_propose_on(Some(&parent.short_ref), serde_json::from_value(serde_json::json!({
            "kind":"decompose", "children":[{"key":"child", "title":"Finding", "outcome":"Finding assessed", "acceptance":["Finding assessed"]}]
        })).unwrap(), at(1)).unwrap();
        let WorkProposeResult::Decomposition(decomposition) = decomposition else {
            panic!("decomposition")
        };
        let child_ref = decomposition.children[0].short_ref.clone();
        let mut store = SqliteStore::open(&path).unwrap();
        let target = service
            .bind_target(&mut store, Some(&child_ref), at(2))
            .unwrap();
        let basis = service
            .protocol_basis(&store, true, false, target, at(2))
            .unwrap();
        let input = WorkUpdateInput::Reject {
            reason: "Disproved finding".into(),
            idempotency_key: String::new(),
        };
        let intent = service.protocol_intent(&input);
        let key = service
            .effective_idempotency_key("", "work_update:reject", &basis, &intent, at(2))
            .unwrap();
        store
            .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                project_id: &project,
                session_id: &session,
                operation: "work_update:reject",
                idempotency_key: &key,
                intent: &intent,
                basis: &basis,
                now: at(2),
            })
            .unwrap();
        let child = basis.focused_work.as_ref().unwrap();
        if stage == 0 {
            store
                .revise_work(
                    &ReviseWorkRequest {
                        work_id: child.work_id,
                        expected_revision: child.revision,
                        patch: WorkRevisionPatch {
                            title: Some("Changed finding".into()),
                            ..Default::default()
                        },
                        authority: WorkPlanningAuthority::Project,
                        actor: service.actor("test", "change pending child"),
                        idempotency_key: "change-child".into(),
                        updated_at: at(3),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
            // Explicit targeting refreshes session focus even on refusal.
            // Establish that same timestamp before measuring mutation effects.
            service
                .bind_target(&mut store, Some(&child_ref), at(4))
                .unwrap();
            let connection = rusqlite::Connection::open(&path).unwrap();
            let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
            let error = service
                .work_update_on(Some(&child_ref), input, at(4))
                .unwrap_err();
            assert_safe_retry_refusal(&error, &child_ref, &parent.short_ref, &key);
            assert!(!error.to_string().contains("its recorded rejection"));
            assert!(
                error
                    .to_string()
                    .contains(&format!("update {child_ref} --cancel"))
            );
            assert!(
                error
                    .to_string()
                    .contains(&format!("update {} --waive {child_ref}", parent.short_ref))
            );
            assert!(matches!(
                error,
                StoreError::WorkRejectRefused {
                    reason: "the child changed since the original rejection attempt",
                    ..
                }
            ));
            assert_eq!(
                crate::storage::test_database_shape_snapshot(&connection).unwrap(),
                before
            );
        } else {
            let current_parent = store.get_work_item(parent.work_id).unwrap();
            let committed = store
                .reject_required_child(
                    &crate::RejectRequiredChildRequest {
                        work_id: child.work_id,
                        expected_work_revision: child.revision,
                        expected_parent_revision: Some(current_parent.revision),
                        reason: "Disproved finding".into(),
                        actor: service
                            .actor("work_update", "reject required child and waive its barrier"),
                        idempotency_key: service
                            .core_operation_key("work_update:reject", &key, "reject_required_child")
                            .unwrap(),
                        rejected_at: at(3),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
            let recovered = service
                .work_update_on(Some(&child_ref), input.clone(), at(4))
                .unwrap();
            assert_eq!(
                recovered.receipt.result["parent_ref"],
                current_parent.short_ref
            );
            assert_eq!(recovered.receipt.result["required_child_waived"], true);
            assert_eq!(store.get_work_item(child.work_id).unwrap(), committed.child);
            service
                .bind_target(&mut store, Some(&child_ref), at(5))
                .unwrap();
            let connection = rusqlite::Connection::open(&path).unwrap();
            let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
            let replay = service
                .work_update_on(Some(&child_ref), input.clone(), at(5))
                .unwrap();
            assert_eq!(
                serde_json::to_value(recovered).unwrap(),
                serde_json::to_value(replay).unwrap()
            );
            assert_eq!(
                crate::storage::test_database_shape_snapshot(&connection).unwrap(),
                before
            );
            // Invalid/corrupt post-commit child drift must never replay success.
            // Use a stale basis clone to exercise the same production guard,
            // without manufacturing unsupported state in the canonical store.
            let mut changed = basis.clone();
            changed.focused_work = Some(committed.child.clone());
            if stage == 1 {
                changed.focused_work.as_mut().unwrap().revision += 1;
            } else {
                changed.focused_work.as_mut().unwrap().lifecycle = WorkLifecycle::Open;
            }
            let error = super::super::reject_retry::guard_committed(
                &store,
                &changed,
                &serde_json::to_value(&committed).unwrap(),
            )
            .unwrap_err();
            assert_safe_retry_refusal(&error, &child_ref, &parent.short_ref, &key);
            assert!(matches!(
                error,
                StoreError::WorkRejectRefused {
                    reason: "the child changed after the original rejection committed",
                    ..
                }
            ));
            assert_eq!(
                crate::storage::test_database_shape_snapshot(&connection).unwrap(),
                before
            );
        }
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn hygiene_pending_reject_after_peer_release_names_execution_basis() {
    for explicit_key in [false, true] {
        let dir = crate::test_support::temp_home().unwrap();
        let path = dir.path().join("released-claim.db");
        let project = ProjectId("released-claim".into());
        let service = LocalWorkService::new(
            path.clone(),
            project.clone(),
            "agent".into(),
            SessionId("agent".into()),
            None,
        );
        let peer = LocalWorkService::new(
            path.clone(),
            project.clone(),
            "peer".into(),
            SessionId("peer".into()),
            None,
        );
        let parent = proposed_root(
            service
                .work_propose(root_input("Parent", "parent"), at(0))
                .unwrap(),
        );
        let decomposition = service.work_propose_on(Some(&parent.short_ref), serde_json::from_value(serde_json::json!({
            "kind":"decompose", "children":[{"key":"child", "title":"Finding", "outcome":"Finding assessed", "acceptance":["Finding assessed"]}]
        })).unwrap(), at(1)).unwrap();
        let WorkProposeResult::Decomposition(decomposition) = decomposition else {
            panic!("decomposition")
        };
        let child_ref = &decomposition.children[0].short_ref;
        peer.work_update_on(
            Some(child_ref),
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "peer-claim".into(),
            },
            at(2),
        )
        .unwrap();
        let input = WorkUpdateInput::Reject {
            reason: "Evidence refutes finding".into(),
            idempotency_key: String::new(),
        };
        let mut store = SqliteStore::open(&path).unwrap();
        let child = store.resolve_work_ref(&project, child_ref).unwrap();
        assert!(matches!(
            service
                .work_update_on(Some(child_ref), input.clone(), at(3))
                .unwrap_err(),
            StoreError::InvalidWork(_)
        ));
        let target = service
            .bind_target(&mut store, Some(child_ref), at(3))
            .unwrap();
        let original_basis = service
            .protocol_basis(&store, true, false, target, at(3))
            .unwrap();
        let key = service
            .effective_idempotency_key(
                "",
                "work_update:reject",
                &original_basis,
                &service.protocol_intent(&input),
                at(3),
            )
            .unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        let pending: (bool, bool) = connection.query_row(
            "SELECT basis_json IS NOT NULL, result_json IS NULL FROM work_protocol_attempts WHERE operation = ?1 AND idempotency_key = ?2",
            rusqlite::params!["work_update:reject", key], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(pending, (true, true));
        peer.work_update_on(
            Some(child_ref),
            WorkUpdateInput::Release {
                reason: "Allow finding assessment".into(),
                waiver_reason: Some("No contribution owed by the departing peer".into()),
                idempotency_key: "peer-release".into(),
            },
            at(4),
        )
        .unwrap();
        assert_eq!(store.get_work_item(child.work_id).unwrap(), child);
        // Normalize explicit-target focus time before comparing all stored rows.
        let target = service
            .bind_target(&mut store, Some(child_ref), at(5))
            .unwrap();
        let live_basis = service
            .protocol_basis(&store, true, false, target, at(5))
            .unwrap();
        assert_eq!(original_basis.focused_work, live_basis.focused_work);
        assert!(original_basis.retry_stable() != live_basis.retry_stable());
        let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
        let error = service
            .work_update_on(Some(child_ref), input, at(5))
            .unwrap_err();
        assert_safe_retry_refusal(&error, child_ref, &parent.short_ref, &key);
        assert!(matches!(
            error,
            StoreError::WorkRejectRefused {
                reason: "the recorded claim or execution basis changed; the child is unchanged",
                ..
            }
        ));
        let message = error.to_string();
        assert!(message.contains("different reason text or an explicit key"));
        assert!(!message.contains("--cancel"));
        assert!(!message.contains("--waive"));
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&connection).unwrap(),
            before
        );
        // A new intent is a fresh admission, not a refresh of the pending row.
        let result = service
            .work_update_on(
                Some(child_ref),
                WorkUpdateInput::Reject {
                    reason: if explicit_key {
                        "Evidence refutes finding"
                    } else {
                        "Reassessed evidence refutes finding"
                    }
                    .into(),
                    idempotency_key: if explicit_key {
                        "fresh-explicit-rejection"
                    } else {
                        ""
                    }
                    .into(),
                },
                at(6),
            )
            .unwrap();
        assert_eq!(result.receipt.result["required_child_waived"], true);
        assert_eq!(
            store.get_work_item(child.work_id).unwrap().lifecycle,
            WorkLifecycle::Cancelled
        );
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn hygiene_correction_reject_service_guards_seeded_core_receipts() {
    for stage in 0..3 {
        let dir = crate::test_support::temp_home().unwrap();
        let path = dir.path().join("guard-wiring.db");
        let project = ProjectId("guard-wiring".into());
        let session = SessionId("agent".into());
        let service = LocalWorkService::new(
            path.clone(),
            project.clone(),
            "agent".into(),
            session.clone(),
            None,
        );
        let parent = proposed_root(
            service
                .work_propose(root_input("Parent", "parent"), at(0))
                .unwrap(),
        );
        let decomposition = service.work_propose_on(Some(&parent.short_ref), serde_json::from_value(serde_json::json!({
            "kind":"decompose", "children":[{"key":"child", "title":"Finding", "outcome":"Finding assessed", "acceptance":["Finding assessed"]}]
        })).unwrap(), at(1)).unwrap();
        let WorkProposeResult::Decomposition(decomposition) = decomposition else {
            panic!("decomposition")
        };
        let child_ref = &decomposition.children[0].short_ref;
        let mut store = SqliteStore::open(&path).unwrap();
        let target = service
            .bind_target(&mut store, Some(child_ref), at(2))
            .unwrap();
        let original_basis = service
            .protocol_basis(&store, true, false, target, at(2))
            .unwrap();
        let input = WorkUpdateInput::Reject {
            reason: "Disproved finding".into(),
            idempotency_key: String::new(),
        };
        let key = service
            .effective_idempotency_key(
                "",
                "work_update:reject",
                &original_basis,
                &service.protocol_intent(&input),
                at(2),
            )
            .unwrap();
        service
            .work_update_on(Some(child_ref), input.clone(), at(3))
            .unwrap();
        let scoped_key = service
            .core_operation_key("work_update:reject", &key, "reject_required_child")
            .unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        if stage == 2 {
            // Seed a completed protocol whose core receipt is missing. This is
            // deliberately inconsistent fixture state, not a supported writer.
            assert_eq!(connection.execute("DELETE FROM work_operation_results WHERE operation = ?1 AND idempotency_key = ?2", rusqlite::params!["reject_required_child", scoped_key]).unwrap(), 1);
        } else {
            let value = store
                .work_operation_result_value("reject_required_child", &scoped_key)
                .unwrap()
                .unwrap();
            let mut divergent: crate::RejectRequiredChildReceipt =
                serde_json::from_value(value).unwrap();
            // Keep the receipt's cancellation/waiver binding internally valid
            // but distinct from the live child. This isolates the wiring guard.
            divergent.child.title = "Different committed child title".into();
            let encoded = CanonicalObject::freeze(&divergent).unwrap();
            assert_eq!(connection.execute("UPDATE work_operation_results SET result_json = ?1 WHERE operation = ?2 AND idempotency_key = ?3", rusqlite::params![encoded.bytes(), "reject_required_child", scoped_key]).unwrap(), 1);
            if stage == 0 {
                let encoded_basis = CanonicalObject::freeze(&original_basis).unwrap();
                assert_eq!(connection.execute("UPDATE work_protocol_attempts SET result_hash = NULL, result_json = NULL, basis_hash = ?1, basis_json = ?2 WHERE project_id = ?3 AND session_id = ?4 AND operation = ?5 AND idempotency_key = ?6", rusqlite::params![encoded_basis.hash().as_str(), encoded_basis.bytes(), project.0, session.0, "work_update:reject", key]).unwrap(), 1);
            }
        }
        service
            .bind_target(&mut store, Some(child_ref), at(4))
            .unwrap();
        let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
        let error = service
            .work_update_on(Some(child_ref), input, at(4))
            .unwrap_err();
        if stage == 2 {
            assert!(
                matches!(error, StoreError::InvalidWorkProjection(ref reason) if reason == "completed rejection attempt has no committed core result")
            );
        } else {
            assert_safe_retry_refusal(&error, child_ref, &parent.short_ref, &key);
            assert!(matches!(
                error,
                StoreError::WorkRejectRefused {
                    reason: "the child changed after the original rejection committed",
                    ..
                }
            ));
        }
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&connection).unwrap(),
            before
        );
    }
}
