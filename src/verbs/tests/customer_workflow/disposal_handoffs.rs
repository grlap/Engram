use super::*;

fn assert_disposal_guidance(verbs: &AgentVerbs, reference: &str, second: i64, admitted: bool) {
    let shown = verbs.show(reference, at(second)).unwrap();
    let focused = verbs.service.work_focus(reference, at(second)).unwrap();
    let inspected = verbs.service.work_inspect(reference, at(second)).unwrap();
    for action in ["work_update:cancel", "work_update:supersede"] {
        assert_eq!(
            shown.value["allowed_next"]
                .as_array()
                .unwrap()
                .contains(&json!(action)),
            admitted,
            "show: {action} at {second}"
        );
        assert_eq!(
            focused.allowed_next.contains(&action.to_owned()),
            admitted,
            "focus: {action} at {second}"
        );
        assert_eq!(
            inspected.view().allowed_next.contains(&action.to_owned()),
            admitted,
            "inspect: {action} at {second}"
        );
    }
}

fn offered_guidance_fixture() -> (
    crate::test_support::TempHome,
    AgentVerbs,
    AgentVerbs,
    String,
) {
    let (directory, owner, database, project) = fixture();
    let reference = add(&owner, "Transfer guidance", None, false, 0);
    owner
        .claim(
            ClaimInput {
                work_ref: reference.clone(),
                ttl_seconds: Some(300),
                recover: None,
            },
            at(1),
        )
        .unwrap();
    assert_disposal_guidance(&owner, &reference, 2, true);
    let before = owner.service.inspect_work(&reference, at(2)).unwrap();
    owner
        .handoff(
            HandoffInput {
                work_ref: Some(reference.clone()),
                action: HandoffAction::Offer {
                    to: "recipient".into(),
                    summary: Some("Transfer".into()),
                    ttl_seconds: Some(30),
                },
            },
            at(3),
        )
        .unwrap();
    assert_disposal_guidance(&owner, &reference, 4, false);
    let live = owner.service.inspect_work(&reference, at(4)).unwrap();
    let mut expected = before.allowed_next;
    expected.retain(|action| {
        !matches!(
            action.as_str(),
            "work_complete" | "work_update:cancel" | "work_update:supersede" | "work_handoff:offer"
        )
    });
    expected.push("work_handoff:cancel".into());
    expected.sort();
    assert_eq!(live.allowed_next, expected);
    let recipient = AgentVerbs::new(
        database,
        project,
        "recipient".into(),
        SessionId("recipient".into()),
        None,
    );
    assert_disposal_guidance(&recipient, &reference, 4, false);
    (directory, owner, recipient, reference)
}

#[test]
fn disposal_handoff_guidance_follows_cancel_accept_and_expiry() {
    for resolution in ["cancel", "accept", "expire"] {
        let (_directory, owner, recipient, reference) = offered_guidance_fixture();
        match resolution {
            "cancel" => {
                owner
                    .handoff(
                        HandoffInput {
                            work_ref: Some(reference.clone()),
                            action: HandoffAction::Cancel {
                                reason: "Retain ownership".into(),
                            },
                        },
                        at(5),
                    )
                    .unwrap();
                assert_disposal_guidance(&owner, &reference, 6, true);
            }
            "accept" => {
                recipient
                    .handoff(
                        HandoffInput {
                            work_ref: Some(reference.clone()),
                            action: HandoffAction::Accept,
                        },
                        at(5),
                    )
                    .unwrap();
                assert_disposal_guidance(&recipient, &reference, 6, true);
                assert_disposal_guidance(&owner, &reference, 6, false);
            }
            "expire" => {
                assert_disposal_guidance(&owner, &reference, 32, false);
                assert_disposal_guidance(&owner, &reference, 33, true);
                assert_disposal_guidance(&owner, &reference, 34, true);
            }
            _ => unreachable!(),
        }
        let holder = if resolution == "accept" {
            &recipient
        } else {
            &owner
        };
        let resolved = holder.service.inspect_work(&reference, at(34)).unwrap();
        assert!(resolved.allowed_next.contains(&"work_handoff:offer".into()));
        assert!(
            !resolved
                .allowed_next
                .contains(&"work_handoff:cancel".into())
        );
    }
}

#[test]
fn disposal_handoff_historical_offers_are_not_current_on_any_agent_view() {
    for disposition in [
        crate::WorkDisposition::Cancelled,
        crate::WorkDisposition::Superseded,
    ] {
        let (_directory, owner, database, project) = fixture();
        let reference = add(&owner, "Historical transfer", None, false, 0);
        let replacement_ref = add(&owner, "Replacement", None, false, 1);
        owner
            .claim(
                ClaimInput {
                    work_ref: reference.clone(),
                    ttl_seconds: Some(300),
                    recover: None,
                },
                at(2),
            )
            .unwrap();
        owner
            .handoff(
                HandoffInput {
                    work_ref: Some(reference.clone()),
                    action: HandoffAction::Offer {
                        to: "recipient".into(),
                        summary: Some("Transfer".into()),
                        ttl_seconds: Some(30),
                    },
                },
                at(3),
            )
            .unwrap();
        let recipient = AgentVerbs::new(
            database.clone(),
            project.clone(),
            "recipient".into(),
            SessionId("recipient".into()),
            None,
        );
        let live = recipient.show(&reference, at(4)).unwrap();
        assert_eq!(live.value["handoffs"].as_array().unwrap().len(), 1);
        assert!(
            live.value["allowed_next"]
                .as_array()
                .unwrap()
                .contains(&json!("work_handoff:accept"))
        );
        let mut store = SqliteStore::open(&database).unwrap();
        let work = store.resolve_work_ref(&project, &reference).unwrap();
        let replacement = store.resolve_work_ref(&project, &replacement_ref).unwrap();
        store.test_dispose_with_historical_offer(&crate::DisposeWorkRequest {
            work_id: work.work_id,
            expected_work_revision: work.revision,
            disposition,
            replacement_id: (disposition == crate::WorkDisposition::Superseded)
                .then_some(replacement.work_id),
            reason: "Legacy disposal".into(),
            actor: crate::ActorContext {
                actor_id: "agent".into(),
                actor_kind: "test_agent".into(),
                assurance: crate::domain::AssuranceLevel::Asserted,
                run_id: None,
                session_id: Some(SessionId("agent".into())),
                source_tool: None,
                source_skill: None,
                provenance_chain: vec![],
                reason: "Historical disposal".into(),
            },
            idempotency_key: "historical-dispose".into(),
            disposed_at: at(5),
        });
        let historical = store.work_handoff_offers(work.work_id).unwrap();
        let before = crate::storage::test_database_shape_snapshot(
            &rusqlite::Connection::open(&database).unwrap(),
        )
        .unwrap();
        let shown = recipient.show(&reference, at(6)).unwrap();
        assert_eq!(shown.value["handoffs"].as_array().unwrap().len(), 0);
        assert!(!shown.text().contains("handoff: offered"));
        assert!(
            !shown.value["allowed_next"]
                .as_array()
                .unwrap()
                .contains(&json!("work_handoff:accept"))
        );
        let raw = recipient.service.inspect_work(&reference, at(6)).unwrap();
        assert!(raw.handoffs.is_empty());
        let peek = recipient
            .next(
                &NextInput {
                    peek: true,
                    verbose: true,
                    ..Default::default()
                },
                at(6),
            )
            .unwrap();
        assert!(
            !serde_json::to_string(&peek.value["incoming_handoffs"])
                .unwrap()
                .contains(&reference)
        );
        assert_eq!(
            crate::storage::test_database_shape_snapshot(
                &rusqlite::Connection::open(&database).unwrap()
            )
            .unwrap(),
            before
        );
        for second in [6, 34] {
            for (verbs, action) in [
                (&recipient, HandoffAction::Accept),
                (
                    &owner,
                    HandoffAction::Cancel {
                        reason: "Historic".into(),
                    },
                ),
            ] {
                let error = verbs
                    .handoff(
                        HandoffInput {
                            work_ref: Some(reference.clone()),
                            action,
                        },
                        at(second),
                    )
                    .unwrap_err();
                assert!(error.to_string().contains(&reference));
                assert!(error.to_string().contains(
                    if disposition == crate::WorkDisposition::Cancelled {
                        "cancelled"
                    } else {
                        "superseded"
                    }
                ));
                assert!(
                    error
                        .to_string()
                        .contains(&format!("engram work show {reference}"))
                );
            }
            assert_eq!(store.work_handoff_offers(work.work_id).unwrap(), historical);
        }
    }
}
