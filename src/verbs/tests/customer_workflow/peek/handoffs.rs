use super::*;

fn coordinator(database: &std::path::Path, project: &ProjectId) -> AgentVerbs {
    AgentVerbs::new(
        database.into(),
        project.clone(),
        "Coordinator".into(),
        SessionId("coordinator-session".into()),
        None,
    )
}

fn offer(owner: &AgentVerbs, name: &str, now: i64) -> String {
    let reference = add(owner, name, None, false, now);
    owner
        .claim(
            ClaimInput {
                work_ref: reference.clone(),
                ttl_seconds: Some(3600),
                recover: None,
            },
            at(now + 1),
        )
        .unwrap();
    owner
        .handoff(
            HandoffInput {
                work_ref: Some(reference.clone()),
                action: HandoffAction::Offer {
                    to: "coordinator-session".into(),
                    summary: None,
                    ttl_seconds: Some(60),
                },
            },
            at(now + 2),
        )
        .unwrap();
    reference
}

#[test]
fn compact_peek_recovers_pending_handoff_without_focus_or_with_unrelated_held_focus() {
    for held_focus in [false, true] {
        let (_home, owner, database, project) = fixture();
        let recipient = coordinator(&database, &project);
        let offered = offer(&owner, "Pending incoming work", 0);
        let focus = if held_focus {
            let reference = add(&recipient, "Unrelated current work", None, false, 3);
            recipient
                .claim(
                    ClaimInput {
                        work_ref: reference.clone(),
                        ttl_seconds: Some(3600),
                        recover: None,
                    },
                    at(4),
                )
                .unwrap();
            Some(reference)
        } else {
            None
        };
        let peek = recipient.next(&peek_input(false), at(5)).unwrap();
        let incoming = &peek.value["incoming_handoffs"];
        assert_eq!(incoming["items"].as_array().unwrap().len(), 1);
        assert_eq!(incoming["items"][0]["ref"], offered);
        assert_eq!(incoming["omitted"], 0);
        let command = incoming["items"][0]["detail"].as_str().unwrap();
        assert_eq!(command, format!("engram work show {offered}"));
        assert!(peek.text().contains("incoming handoffs (1 shown)"));
        assert!(peek.text().contains(command));
        assert_eq!(peek.value["peek"]["delivery_advanced"], false);
        match &focus {
            Some(reference) => assert_eq!(peek.value["focus"]["ref"], *reference),
            None => assert!(peek.value["focus"].is_null()),
        }
        let shown = recipient.show(&offered, at(5)).unwrap();
        assert!(
            shown.value["allowed_next"]
                .as_array()
                .unwrap()
                .contains(&json!("work_handoff:accept"))
        );
        let again = recipient.next(&peek_input(false), at(5)).unwrap();
        assert_eq!(again.value["incoming_handoffs"], *incoming);
        assert_eq!(again.value["focus"], peek.value["focus"]);
    }
}

#[test]
fn compact_peek_counts_pending_offer_overflow_and_routes_to_catalog() {
    let (_home, owner, database, project) = fixture();
    let recipient = coordinator(&database, &project);
    let refs = (0..6)
        .map(|index| offer(&owner, &format!("Incoming {index}"), index * 3))
        .collect::<Vec<_>>();
    let peek = recipient.next(&peek_input(false), at(20)).unwrap();
    let incoming = &peek.value["incoming_handoffs"];
    assert_eq!(incoming["items"].as_array().unwrap().len(), 5);
    assert_eq!(incoming["omitted"], 1);
    assert_eq!(incoming["items"][0]["ref"], refs[0]);
    assert_eq!(
        incoming["catalog_detail"],
        "engram work ls --all --limit 20"
    );
    let catalog = recipient
        .ls(
            &LsInput {
                all: true,
                ..Default::default()
            },
            at(20),
        )
        .unwrap();
    assert!(catalog.text().contains(&refs[5]));
    let omitted = recipient.show(&refs[5], at(20)).unwrap();
    assert!(
        omitted.value["allowed_next"]
            .as_array()
            .unwrap()
            .contains(&json!("work_handoff:accept"))
    );
}

#[test]
fn compact_peek_does_not_count_cancelled_accepted_or_expired_offers_as_pending() {
    let (_home, owner, database, project) = fixture();
    let recipient = coordinator(&database, &project);
    let cancelled = offer(&owner, "Cancelled incoming", 0);
    let accepted = offer(&owner, "Accepted incoming", 3);
    let expired = offer(&owner, "Expired incoming", 6);
    owner
        .handoff(
            HandoffInput {
                work_ref: Some(cancelled),
                action: HandoffAction::Cancel {
                    reason: "Keep ownership".into(),
                },
            },
            at(10),
        )
        .unwrap();
    recipient
        .handoff(
            HandoffInput {
                work_ref: Some(accepted),
                action: HandoffAction::Accept,
            },
            at(11),
        )
        .unwrap();
    let early = recipient.next(&peek_input(false), at(12)).unwrap();
    assert_eq!(
        early.value["incoming_handoffs"]["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(early.value["incoming_handoffs"]["items"][0]["ref"], expired);
    let late = recipient.next(&peek_input(false), at(69)).unwrap();
    assert!(late.value.get("incoming_handoffs").is_none());
}
