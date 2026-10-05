use super::*;

#[test]
fn waiver_guidance_preserves_specific_invalid_work_remedies() {
    for (reason, command) in [
        ("work does not exist", "engram work ls"),
        (
            crate::storage::PENDING_HANDOFF_REFUSAL,
            "engram work handoff w-parent --cancel \"…\"",
        ),
        (
            crate::work_service::COMPLETED_WORK_LATE_FINDING_REFUSAL,
            "engram work note w-parent \"…\"",
        ),
        ("an ancestor is not open", "engram work show w-parent"),
    ] {
        let error = VerbError::for_waiver(StoreError::InvalidWork(reason.into()), "w-parent");
        assert_eq!(error.guidance().next, [command]);
    }
}

#[test]
fn completed_parent_waiver_keeps_late_finding_note_guidance() {
    let (_directory, verbs, _database, _project) = fixture();
    let parent = add(&verbs, "Completed parent", None, false, 0);
    let child = add(&verbs, "Optional child", Some(&parent), true, 1);
    verbs
        .claim(
            ClaimInput {
                work_ref: parent.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(2),
        )
        .unwrap();
    verbs
        .done(
            DoneInput {
                work_ref: Some(parent.clone()),
                summary: Some("Delivered".into()),
                ..DoneInput::default()
            },
            at(3),
        )
        .unwrap();
    let error = verbs
        .update(
            UpdateInput {
                work_ref: Some(parent.clone()),
                action: UpdateAction::WaiveRequiredChild {
                    child,
                    reason: "Late finding".into(),
                },
            },
            at(4),
        )
        .unwrap_err();
    assert_eq!(
        error.guidance().next,
        [format!("engram work note {parent} \"…\"")]
    );
    let payload = crate::store_error_value(&error.error);
    assert!(
        payload["error"]["details"]["remedy"]
            .as_str()
            .unwrap()
            .contains("note")
    );
}

#[test]
fn direct_waiver_under_terminal_ancestors_refuses_without_writing() {
    for intermediate in [false, true] {
        for superseded in [false, true] {
            let (_directory, verbs, database, project) = fixture();
            let root = add(&verbs, "Root", None, false, 0);
            let ancestor = if intermediate {
                add(&verbs, "Ancestor", Some(&root), true, 1)
            } else {
                root.clone()
            };
            let parent = add(&verbs, "Parent", Some(&ancestor), true, 2);
            let child = add(&verbs, "Child", Some(&parent), false, 3);
            let optional = add(&verbs, "Optional child", Some(&parent), true, 4);
            let live = add(&verbs, "Live child", Some(&parent), false, 5);
            let replacement = add(&verbs, "Independent replacement", None, false, 6);
            verbs
                .update(
                    UpdateInput {
                        work_ref: Some(child.clone()),
                        action: if superseded {
                            UpdateAction::Supersede {
                                replacement: replacement.clone(),
                                reason: "Continue separately".into(),
                            }
                        } else {
                            UpdateAction::Cancel {
                                reason: "Not needed".into(),
                            }
                        },
                    },
                    at(7),
                )
                .unwrap();
            verbs
                .claim(
                    ClaimInput {
                        work_ref: ancestor.clone(),
                        ttl_seconds: None,
                        recover: None,
                    },
                    at(8),
                )
                .unwrap();
            verbs
                .done(
                    DoneInput {
                        work_ref: Some(ancestor),
                        summary: Some("Delivered".into()),
                        ..DoneInput::default()
                    },
                    at(9),
                )
                .unwrap();
            let error = verbs
                .update(
                    UpdateInput {
                        work_ref: Some(parent.clone()),
                        action: UpdateAction::WaiveRequiredChild {
                            child: child.clone(),
                            reason: "Account for omission".into(),
                        },
                    },
                    at(10),
                )
                .expect_err("terminal ancestry cannot admit a new waiver");
            let value = crate::store_error_value(&error.error);
            assert_eq!(value["error"]["code"], "work_invalid");
            let expected = format!(
                "Cannot waive {child} from {parent} because an ancestor is not open. Run engram work show {parent} and follow its admitted detach or resolve-first guidance. For work beneath a completed, cancelled, or superseded ancestor, continue through an admitted detach or file an independent root follow-up."
            );
            assert_eq!(value["error"]["details"]["reason"], expected);
            assert_eq!(
                error.guidance().next,
                [format!("engram work show {parent}")]
            );

            let mut store = SqliteStore::open(&database).unwrap();
            let parent_item = store.resolve_work_ref(&project, &parent).unwrap();
            let child_item = store.resolve_work_ref(&project, &child).unwrap();
            let request = crate::domain::WaiveRequiredChildRequest {
                parent_id: parent_item.work_id,
                child_id: child_item.work_id,
                expected_parent_revision: parent_item.revision,
                reason: "Account for omission".into(),
                actor: crate::domain::ActorContext {
                    actor_id: "agent".into(),
                    actor_kind: "test_agent".into(),
                    assurance: crate::domain::AssuranceLevel::Asserted,
                    run_id: None,
                    session_id: Some(SessionId("agent".into())),
                    source_tool: Some("work_test".into()),
                    source_skill: None,
                    provenance_chain: Vec::new(),
                    reason: "exercise waiver refusal".into(),
                },
                idempotency_key: "explicit-waiver".into(),
                waived_at: at(11),
            };
            let connection = rusqlite::Connection::open(&database).unwrap();
            let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
            assert!(
                matches!(store.waive_required_child(&request, &crate::memory::DevelopmentNoopRedactor), Err(StoreError::InvalidWork(reason)) if reason == expected)
            );
            let mut stale = request.clone();
            stale.expected_parent_revision -= 1;
            assert!(matches!(
                store.waive_required_child(&stale, &crate::memory::DevelopmentNoopRedactor),
                Err(StoreError::WorkRevisionConflict { .. })
            ));
            for invalid in [&optional, &live, &parent] {
                let mut invalid_request = request.clone();
                invalid_request.child_id =
                    store.resolve_work_ref(&project, invalid).unwrap().work_id;
                assert!(
                    matches!(store.waive_required_child(&invalid_request, &crate::memory::DevelopmentNoopRedactor), Err(StoreError::InvalidWork(reason)) if reason == "completion waiver requires a directly required cancelled or superseded child")
                );
            }
            assert_eq!(
                crate::storage::test_database_shape_snapshot(&connection).unwrap(),
                before
            );
            assert!(store.verify_all().unwrap().is_healthy());
        }
    }
}

#[test]
fn direct_waiver_under_open_ancestors_still_succeeds() {
    let (_directory, verbs, _database, _project) = fixture();
    let root = add(&verbs, "Open root", None, false, 0);
    let parent = add(&verbs, "Open parent", Some(&root), true, 1);
    let child = add(&verbs, "Required child", Some(&parent), false, 2);
    verbs
        .update(
            UpdateInput {
                work_ref: Some(child.clone()),
                action: UpdateAction::Cancel {
                    reason: "Not needed".into(),
                },
            },
            at(3),
        )
        .unwrap();
    let waiver = verbs
        .update(
            UpdateInput {
                work_ref: Some(parent),
                action: UpdateAction::WaiveRequiredChild {
                    child,
                    reason: "Account for omission".into(),
                },
            },
            at(4),
        )
        .unwrap();
    assert_eq!(waiver.value["operation"], "waive_required_child");
}
