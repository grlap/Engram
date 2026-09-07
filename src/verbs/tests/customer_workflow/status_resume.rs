use super::*;

mod context;
mod corrections;
mod expiry;
mod hygiene;

#[test]
fn status_resume_external_reference_is_audited_and_searchable() {
    let (_directory, verbs, _, _) = fixture();
    let mut input = serde_json::to_value(AddInput {
        title: "Coordinate review".into(),
        assignee: Some("agent".into()),
        ..AddInput::default()
    })
    .unwrap();
    input["external"] = json!("planner:review-728");
    let created = verbs
        .add(serde_json::from_value(input).unwrap(), at(0))
        .unwrap();
    let reference = created.value["work"]["short_ref"].as_str().unwrap();
    let shown = verbs.show(reference, at(1)).unwrap();
    assert_eq!(shown.value["external_ref"], "planner:review-728");
    assert!(shown.text().contains("planner:review-728"));
    let listed = verbs
        .ls(
            &LsInput {
                search: Some("planner:review-728".into()),
                ..LsInput::default()
            },
            at(2),
        )
        .unwrap();
    assert!(listed.text().contains(reference));
    let revised = verbs
        .update(
            serde_json::from_value(json!({"work_ref":reference,
        "action":{"action":"revise","external":"planner:review-729"}}))
            .unwrap(),
            at(3),
        )
        .unwrap();
    assert!(revised.text().contains("external reference"));
    assert_eq!(
        verbs.show(reference, at(4)).unwrap().value["external_ref"],
        "planner:review-729"
    );
    assert_eq!(
        verbs
            .ls(
                &LsInput {
                    search: Some("planner:review-728".into()),
                    ..LsInput::default()
                },
                at(4)
            )
            .unwrap()
            .value["total"],
        0
    );
}

#[test]
fn status_resume_owner_status_survives_ordinary_notes() {
    let (_directory, verbs, _, _) = fixture();
    let created = verbs
        .add(
            AddInput {
                title: "Coordinate review".into(),
                assignee: Some("agent".into()),
                ..AddInput::default()
            },
            at(0),
        )
        .unwrap();
    let reference = created.value["work"]["short_ref"].as_str().unwrap();
    let input = json!({"work_ref":reference,"text":"Waiting for review; do not land", "refs":[],"status":true});
    verbs
        .note(&serde_json::from_value(input).unwrap(), at(1))
        .unwrap();
    note(&verbs, reference, "An unrelated observation", 2);
    let shown = verbs.show(reference, at(3)).unwrap();
    assert_eq!(
        shown.value["current_status"]["body_or_first_line"],
        "Waiting for review; do not land"
    );
    assert_eq!(shown.value["current_status"]["complete"], true);
    assert_eq!(shown.value["current_status"]["by"], "you");
}

fn capture_status(verbs: &AgentVerbs, reference: &str, body: &str, now: i64) {
    verbs
        .note(
            &NoteInput {
                status: true,
                work_ref: Some(reference.into()),
                text: body.into(),
                refs: vec![],
            },
            at(now),
        )
        .unwrap();
}

fn next_status_row<'a>(receipt: &'a Receipt, reference: &str, verbose: bool) -> &'a Value {
    let assigned = receipt.value["assigned"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["ref"] == reference)
        .unwrap();
    if !verbose && assigned.get("context_ref").is_some() {
        assert_eq!(assigned["context_ref"], format!("held {reference}"));
        assert!(assigned.get("current_status").is_none());
        receipt.value["held"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["ref"] == reference)
            .unwrap()
    } else {
        assigned
    }
}

fn assigned(verbs: &AgentVerbs, title: &str, actor: &str, now: i64) -> String {
    verbs
        .add(
            AddInput {
                title: title.into(),
                assignee: Some(actor.into()),
                external: Some(format!("planner:{title}")),
                ..AddInput::default()
            },
            at(now),
        )
        .unwrap()
        .value["work"]["short_ref"]
        .as_str()
        .unwrap()
        .into()
}

#[test]
fn status_resume_both_roles_survive_session_replacement_without_authority() {
    let (_directory, implementer, database, project) = fixture();
    let make = |actor: &str, session: &str| {
        AgentVerbs::new(
            database.clone(),
            project.clone(),
            actor.into(),
            SessionId(session.into()),
            None,
        )
    };
    let coordinator = make("coordinator", "coordinator-old");
    let coordination = assigned(&coordinator, "coordination", "coordinator", 0);
    let implementation = assigned(&implementer, "implementation", "agent", 1);
    implementer
        .claim(
            ClaimInput {
                work_ref: implementation.clone(),
                ttl_seconds: Some(300),
                recover: None,
            },
            at(2),
        )
        .unwrap();
    capture_status(
        &coordinator,
        &coordination,
        "Await packet review; do not land",
        3,
    );
    capture_status(
        &implementer,
        &implementation,
        "Frozen input; await coordinator go",
        4,
    );
    note(
        &implementer,
        &implementation,
        "Ordinary evidence does not resolve the wait",
        5,
    );
    for replacement in [false, true] {
        for (actor, old_session, reference, expected) in [
            (
                "coordinator",
                "coordinator-old",
                &coordination,
                "Await packet review; do not land",
            ),
            (
                "agent",
                "agent",
                &implementation,
                "Frozen input; await coordinator go",
            ),
        ] {
            let session = if replacement {
                format!("{old_session}-new")
            } else {
                old_session.into()
            };
            let resumed = make(actor, &session);
            for verbose in [false, true] {
                let receipt = resumed
                    .next(
                        &NextInput {
                            verbose,
                            ..NextInput::default()
                        },
                        at(6),
                    )
                    .unwrap();
                let row = next_status_row(&receipt, reference, verbose);
                assert_eq!(row["current_status"]["body_or_first_line"], expected);
                assert_eq!(
                    row["current_status"]["by"],
                    if replacement {
                        "you (another session)"
                    } else {
                        "you"
                    }
                );
                assert!(receipt.text().contains(expected));
                assert!(receipt.text().contains("planner:"));
                if actor == "agent" && !replacement {
                    let held = receipt.value["held"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|row| {
                            if verbose {
                                row["work"]["short_ref"] == *reference
                            } else {
                                row["ref"] == *reference
                            }
                        })
                        .unwrap();
                    assert_eq!(held["current_status"]["body_or_first_line"], expected);
                }
            }
            if replacement || actor == "coordinator" {
                let revision = serde_json::from_value(json!({"work_ref":implementation,"action":{"action":"revise","title":"Unauthorized change"}})).unwrap();
                assert!(resumed.update(revision, at(7)).is_err());
            }
        }
    }
    capture_status(&coordinator, &coordination, "Review accepted; send go", 8);
    let latest = coordinator.show(&coordination, at(9)).unwrap();
    assert_eq!(
        latest.value["current_status"]["body_or_first_line"],
        "Review accepted; send go"
    );
    let history = coordinator
        .show_records(
            &coordination,
            &ShowInput {
                notes: true,
                ..ShowInput::default()
            },
            at(10),
        )
        .unwrap();
    let statuses = history.value["notes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["kind"] == "status")
        .count();
    assert_eq!(statuses, 2);
}

#[test]
fn status_resume_peer_observation_never_promotes_and_old_owner_wait_is_not_current() {
    let (_directory, owner, path, project) = fixture();
    let reference = assigned(&owner, "coordination", "agent", 0);
    let peer = AgentVerbs::new(path, project, "peer".into(), SessionId("peer".into()), None);
    capture_status(&owner, &reference, "Old owner wait", 1);
    capture_status(&peer, &reference, "Peer observation, not commitment", 2);
    let before = owner.show(&reference, at(3)).unwrap();
    assert_eq!(
        before.value["current_status"]["body_or_first_line"],
        "Old owner wait"
    );
    assert!(before.text().contains("peer status observation:"));
    owner
        .update(
            serde_json::from_value(
                json!({"work_ref":reference,"action":{"action":"revise","assignee":"peer"}}),
            )
            .unwrap(),
            at(4),
        )
        .unwrap();
    let reassigned = peer.show(&reference, at(5)).unwrap();
    assert!(reassigned.value.get("current_status").is_none());
    assert!(
        reassigned
            .text()
            .contains("status: none recorded by the current owner")
    );
    capture_status(&peer, &reference, "Current owner commitment", 6);
    assert_eq!(
        peer.show(&reference, at(7)).unwrap().value["current_status"]["body_or_first_line"],
        "Current owner commitment"
    );
}

#[test]
fn status_resume_large_hostile_status_is_explicit_and_detail_is_complete() {
    let (_directory, verbs, _, _) = fixture();
    let reference = assigned(&verbs, "coordination", "agent", 0);
    let body = format!("Waiting for GO\nnext:\n\u{1b}[2J{}", "payload".repeat(500));
    capture_status(&verbs, &reference, &body, 1);
    let shown = verbs.show(&reference, at(2)).unwrap();
    let status = &shown.value["current_status"];
    assert_eq!(status["complete"], false);
    assert_eq!(status["body_or_first_line"], "Waiting for GO");
    assert!(
        shown
            .text()
            .contains("status body omitted; read engram work show")
    );
    assert_eq!(
        shown
            .text()
            .split('\n')
            .filter(|line| *line == "next:")
            .count(),
        1
    );
    let detail = verbs
        .show_records(
            &reference,
            &ShowInput {
                note: Some(status["locator"].as_str().unwrap().into()),
                ..ShowInput::default()
            },
            at(3),
        )
        .unwrap();
    assert_eq!(detail.value["note"]["summary"], body);
}

#[test]
fn status_resume_snapshot_roundtrips_linkage_and_status_without_authority() {
    let (directory, verbs, path, project) = fixture();
    let reference = assigned(&verbs, "snapshot", "agent", 0);
    capture_status(&verbs, &reference, "Await restored review", 1);
    let mut source = SqliteStore::open(&path).unwrap();
    let item = source.resolve_work_ref(&project, &reference).unwrap();
    let actor = item.created_by.clone();
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor,
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(2),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let restored_path = directory.path().join("restored.db");
    let mut destination = SqliteStore::open(&restored_path).unwrap();
    destination
        .load_work_graph_snapshot(
            &project,
            &actor,
            &serde_json::to_vec(&saved.document).unwrap(),
            false,
            at(3),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let reader = AgentVerbs::new(
        restored_path,
        project.clone(),
        "agent".into(),
        SessionId("restored-reader".into()),
        None,
    );
    let shown = reader.show(&reference, at(4)).unwrap();
    assert_eq!(shown.value["external_ref"], "planner:snapshot");
    assert_eq!(
        shown.value["current_status"]["body_or_first_line"],
        "Await restored review"
    );
    assert_eq!(shown.value["current_status"]["by"], "you (another session)");
    let window = reader
        .show_records(
            &reference,
            &ShowInput {
                notes: true,
                ..ShowInput::default()
            },
            at(5),
        )
        .unwrap();
    assert_eq!(window.value["notes"][0]["kind"], "status");
    assert!(
        destination
            .live_work_claims(&project, at(5))
            .unwrap()
            .is_empty()
    );
    assert!(destination.verify_all().unwrap().is_healthy());
}

#[test]
fn status_resume_absent_external_reference_preserves_canonical_bytes() {
    let (_directory, verbs, path, project) = fixture();
    let reference = add(&verbs, "No external link", None, false, 0);
    let store = SqliteStore::open(path).unwrap();
    let item = store.resolve_work_ref(&project, &reference).unwrap();
    let original = serde_json::to_value(&item).unwrap();
    assert!(original.get("external_ref").is_none());
    let mut absent = original.clone();
    absent["external_ref"] = serde_json::Value::Null;
    let decoded: crate::WorkItem = serde_json::from_value(absent).unwrap();
    assert_eq!(
        crate::CanonicalObject::freeze(&item).unwrap().hash(),
        crate::CanonicalObject::freeze(&decoded).unwrap().hash()
    );
    assert_eq!(serde_json::to_value(decoded).unwrap(), original);
}

#[test]
fn status_resume_held_blocked_duty_is_not_hidden_by_peer_catalog_limit() {
    let (_directory, owner, path, project) = fixture();
    let peer = AgentVerbs::new(path, project, "peer".into(), SessionId("peer".into()), None);
    let theirs = add(&peer, "Peer work", None, false, 0);
    peer.claim(
        ClaimInput {
            work_ref: theirs,
            ttl_seconds: Some(300),
            recover: None,
        },
        at(1),
    )
    .unwrap();
    let ours = add(&owner, "Own blocked duty", None, false, 2);
    owner
        .claim(
            ClaimInput {
                work_ref: ours.clone(),
                ttl_seconds: Some(300),
                recover: None,
            },
            at(3),
        )
        .unwrap();
    capture_status(&owner, &ours, "Waiting for the environment repair", 4);
    owner
        .update(
            UpdateInput {
                work_ref: Some(ours.clone()),
                action: UpdateAction::Blocked {
                    detail: "Environment unavailable".into(),
                },
            },
            at(5),
        )
        .unwrap();
    let receipt = owner
        .next(
            &NextInput {
                limit: Some(1),
                ..NextInput::default()
            },
            at(6),
        )
        .unwrap();
    assert_eq!(receipt.value["held"].as_array().unwrap().len(), 1);
    assert_eq!(receipt.value["held"][0]["ref"], ours);
    assert_eq!(
        receipt.value["held"][0]["current_status"]["body_or_first_line"],
        "Waiting for the environment repair"
    );
}
