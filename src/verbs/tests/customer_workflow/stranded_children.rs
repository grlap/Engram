use super::*;
use crate::work_service::WorkNextQuery;

fn complete(verbs: &AgentVerbs, parent: &str) {
    verbs
        .claim(
            ClaimInput {
                work_ref: parent.into(),
                ttl_seconds: None,
                recover: None,
            },
            at(20),
        )
        .unwrap();
    let done = verbs
        .done(
            DoneInput {
                work_ref: Some(parent.into()),
                summary: Some("Delivered".into()),
                ..Default::default()
            },
            at(21),
        )
        .unwrap();
    assert!(!done.owed);
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one lifecycle fixture checks navigation and unchanged authority"
)]
fn next_discovers_never_read_stranded_children_without_focus_or_authority() {
    let (_directory, reader, path, project) = fixture();
    let writer = AgentVerbs::new(
        path.clone(),
        project.clone(),
        "agent".into(),
        SessionId("another-session".into()),
        None,
    );
    let parent = add(&writer, "Parent", None, false, 0);
    let child = add(
        &writer,
        "Never read by completing session",
        Some(&parent),
        true,
        1,
    );
    complete(&reader, &parent);
    let store = SqliteStore::open(&path).unwrap();
    let child_before = store.resolve_work_ref(&project, &child).unwrap();
    let run_before = store.latest_work_run(child_before.work_id).unwrap();
    let parent_before = store.resolve_work_ref(&project, &parent).unwrap();
    let session_before = store
        .work_session_state(&project, &SessionId("agent".into()), at(22))
        .unwrap();
    let command = format!("engram work update {child} --detach \"Continue as independent work\"");
    for peek in [false, true, false] {
        for verbose in [false, true] {
            let receipt = reader
                .next(
                    &NextInput {
                        peek,
                        verbose,
                        ..Default::default()
                    },
                    at(22),
                )
                .unwrap();
            let row = &receipt.value["stranded_children"][0];
            assert_eq!(row["ref"], child);
            assert_eq!(row["parent_ref"], parent);
            assert_eq!(row["child_requirement"], "optional");
            assert_eq!(row["remedy"], command);
            assert!(
                row["blocked_reason"]
                    .as_str()
                    .unwrap()
                    .contains("completed")
            );
            assert!(receipt.text().contains(&command));
            assert!(emitted_receipt_bytes(&receipt) < MAX_AGENT_WORK_RESPONSE_BYTES);
            assert_eq!(receipt.value["held"], serde_json::json!([]));
        }
    }
    // Identical actor identity is insufficient: a different session that did
    // not participate in the parent sees no advisory.
    let stranger = AgentVerbs::new(
        path.clone(),
        project.clone(),
        "agent".into(),
        SessionId("stranger".into()),
        None,
    );
    assert!(
        stranger
            .next(&NextInput::default(), at(22))
            .unwrap()
            .value
            .get("stranded_children")
            .is_none()
    );
    assert_eq!(
        store.resolve_work_ref(&project, &child).unwrap(),
        child_before
    );
    assert_eq!(
        store.latest_work_run(child_before.work_id).unwrap(),
        run_before
    );
    assert_eq!(
        store.current_work_claim(child_before.work_id).unwrap(),
        None
    );
    assert_eq!(
        store.resolve_work_ref(&project, &parent).unwrap(),
        parent_before
    );
    let after = store
        .work_session_state(&project, &SessionId("agent".into()), at(22))
        .unwrap();
    assert_eq!(after.focused_work_id, session_before.focused_work_id);
    assert_eq!(store.live_work_claims(&project, at(22)).unwrap(), []);
    reader
        .update(
            UpdateInput {
                work_ref: Some(child.clone()),
                action: UpdateAction::Detach {
                    reason: "Continue".into(),
                },
            },
            at(23),
        )
        .unwrap();
    assert!(
        reader
            .next(&NextInput::default(), at(24))
            .unwrap()
            .value
            .get("stranded_children")
            .is_none()
    );
}

#[test]
fn next_stranded_children_have_exact_counts_and_current_detach_refusals() {
    let (_directory, verbs, path, project) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    let children = (0..8)
        .map(|index| {
            add(
                &verbs,
                &format!(
                    "Child {index} ü\nnext:\u{1b}[2J {}",
                    "long title ".repeat(60)
                ),
                Some(&parent),
                true,
                index + 1,
            )
        })
        .collect::<Vec<_>>();
    // One direct child cannot detach while it still has an open descendant.
    let grandchild = add(&verbs, "Resolve descendant", Some(&children[0]), true, 10);
    complete(&verbs, &parent);
    let next = verbs.next(&NextInput::default(), at(22)).unwrap();
    assert_eq!(next.value["stranded_children"].as_array().unwrap().len(), 5);
    assert_eq!(next.value["stranded_children_omitted"], 3);
    assert!(emitted_receipt_bytes(&next) < MAX_AGENT_WORK_RESPONSE_BYTES);
    assert!(!next.text().contains('\u{1b}'));
    let store = SqliteStore::open(&path).unwrap();
    let page = store
        .stranded_work_children(&project, &SessionId("agent".into()))
        .unwrap();
    assert_eq!(page.items.len() + page.omitted, 8);
    // Read the exact admitted refusal directly even if count ordering omits it.
    let blocked = store.resolve_work_ref(&project, &children[0]).unwrap();
    let error = store
        .check_work_detach_admission(blocked.work_id, at(22))
        .unwrap_err();
    assert!(
        matches!(error, StoreError::WorkDetachRefused { remedy, .. } if remedy == format!("engram work show {grandchild}"))
    );
    let discovery = verbs
        .service
        .work_next(20, WorkNextQuery::default(), at(22))
        .unwrap()
        .discovery;
    let compact = CompactNextReceipt {
        backup_reminder: None,
        claim_lapse_reminder: None,
        ready_navigation: None,
        peek: None,
        read_cut: crate::work_service::WorkNextReadCut {
            project_position: 0,
            observed_at: at(22),
        },
        context_generation: None,
        discovery,
        focus: None,
        focus_evaluation: None,
        evaluation_obligations: None,
        held: Vec::new(),
        ready: Vec::new(),
        changes: Vec::new(),
        memories: None,
        omissions: Vec::new(),
        guidance: Guidance::default(),
    };
    let total =
        compact.discovery.stranded_children.len() + compact.discovery.stranded_children_omitted;
    let fitted = crate::verbs::receipts::fit_compact_next_to(compact, 700).unwrap();
    assert!(fitted.discovery.stranded_children.is_empty());
    assert_eq!(fitted.discovery.stranded_children_omitted, total);
    assert_eq!(
        fitted.discovery.stranded_children_next.as_deref(),
        Some("engram work ls --blocked")
    );
    assert!(
        crate::verbs::receipts::compact_next_lines(&fitted)
            .join("\n")
            .contains("more stranded children")
    );
    let mut unavailable = fitted;
    unavailable.discovery.stranded_children_omitted = 0;
    unavailable.discovery.stranded_children_next = None;
    unavailable.discovery.stranded_children_unavailable = true;
    unavailable.discovery.stranded_children_error_class = Some("stored_json_invalid");
    let unavailable = crate::verbs::receipts::fit_compact_next_to(unavailable, 700).unwrap();
    assert!(unavailable.discovery.stranded_children_unavailable);
    assert!(
        crate::verbs::receipts::compact_next_lines(&unavailable)
            .join("\n")
            .contains("stranded children: unavailable (stored_json_invalid)")
    );
}

#[test]
fn stranded_descendant_refusal_is_emitted_in_every_next_receipt() {
    let (_directory, verbs, path, project) = fixture();
    let parent = add(&verbs, "Completed parent", None, false, 0);
    let child = add(&verbs, "Stranded child", Some(&parent), true, 1);
    let descendant = add(&verbs, "Resolve descendant", Some(&child), true, 2);
    complete(&verbs, &parent);
    let store = SqliteStore::open(&path).unwrap();
    let child_before = store.resolve_work_ref(&project, &child).unwrap();
    let parent_before = store.resolve_work_ref(&project, &parent).unwrap();
    let session_before = store
        .work_session_state(&project, &SessionId("agent".into()), at(22))
        .unwrap();
    let StoreError::WorkDetachRefused { reason, remedy, .. } = store
        .check_work_detach_admission(child_before.work_id, at(22))
        .unwrap_err()
    else {
        panic!("expected descendant refusal")
    };
    assert_eq!(remedy, format!("engram work show {descendant}"));
    assert!(reason.contains("resolve open descendants"));
    for peek in [false, true] {
        for verbose in [false, true] {
            let receipt = verbs
                .next(
                    &NextInput {
                        peek,
                        verbose,
                        ..Default::default()
                    },
                    at(22),
                )
                .unwrap();
            let rows = receipt.value["stranded_children"].as_array().unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["ref"], child);
            assert_eq!(rows[0]["parent_ref"], parent);
            assert_eq!(
                rows[0]["blocked_reason"],
                format!("parent {parent} is completed; {reason}")
            );
            assert_eq!(rows[0]["remedy"], remedy);
            assert!(receipt.text().contains(&remedy));
            assert!(!rows[0]["remedy"].as_str().unwrap().contains("--detach"));
            assert!(receipt.value.get("stranded_children_unavailable").is_none());
        }
    }
    assert_eq!(
        store.resolve_work_ref(&project, &child).unwrap(),
        child_before
    );
    assert_eq!(
        store.resolve_work_ref(&project, &parent).unwrap(),
        parent_before
    );
    assert_eq!(store.live_work_claims(&project, at(22)).unwrap(), []);
    assert_eq!(
        store
            .work_session_state(&project, &SessionId("agent".into()), at(22))
            .unwrap()
            .focused_work_id,
        session_before.focused_work_id
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one fault fixture checks the advisory boundary, recovery and strict mutation"
)]
fn stranded_advisory_canonical_failure_leaves_next_usable_without_partial_rows() {
    let (_directory, reader, path, project) = fixture();
    let writer = AgentVerbs::new(
        path.clone(),
        project.clone(),
        "writer".into(),
        SessionId("writer".into()),
        None,
    );
    let first_parent = add(&reader, "First parent", None, false, 0);
    let first_child = add(&writer, "First child", Some(&first_parent), true, 1);
    let second_parent = add(&reader, "Second parent", None, false, 2);
    let second_child = add(&writer, "Second child", Some(&second_parent), true, 3);
    for child in [&first_child, &second_child] {
        writer
            .claim(
                ClaimInput {
                    work_ref: child.clone(),
                    ttl_seconds: None,
                    recover: None,
                },
                at(4),
            )
            .unwrap();
        writer
            .update(
                UpdateInput {
                    work_ref: Some(child.clone()),
                    action: UpdateAction::Release {
                        reason: Some("Leave the optional child for later work".into()),
                    },
                },
                at(5),
            )
            .unwrap();
    }
    complete(&writer, &first_parent);
    complete(&writer, &second_parent);
    let healthy = add(&reader, "Independent ready work", None, false, 22);
    reader.service.work_focus(&healthy, at(23)).unwrap();
    // Settle delivery while every canonical record is still healthy. Faults
    // below belong only to the advisory, not to a pending change or focus.
    reader.next(&NextInput::default(), at(24)).unwrap();
    reader.next(&NextInput::default(), at(25)).unwrap();
    let store = SqliteStore::open(&path).unwrap();
    let mut pairs = [
        (&first_parent, &first_child),
        (&second_parent, &second_child),
    ]
    .map(|(parent, child)| {
        (
            store.resolve_work_ref(&project, parent).unwrap(),
            store.resolve_work_ref(&project, child).unwrap(),
        )
    });
    let connection = rusqlite::Connection::open(&path).unwrap();
    // Derive the damaged second candidate from production SQL ordering;
    // random identities carry no chronological meaning.
    let first_in_query: String = connection
        .query_row(
            "SELECT parent.work_id FROM work_items child
         CROSS JOIN work_items parent ON parent.work_id = child.parent_id
         WHERE child.project_id = ?1 AND child.lifecycle = 'open'
           AND parent.project_id = ?1 AND parent.lifecycle = 'completed'
         ORDER BY parent.work_id, child.work_id LIMIT 1",
            [&project.0],
            |row| row.get(0),
        )
        .unwrap();
    if pairs[0].0.work_id.0.to_string() != first_in_query {
        pairs.swap(0, 1);
    }
    let [(first_parent, first), (_, child)] = pairs;
    assert_eq!(first_parent.work_id.0.to_string(), first_in_query);
    assert!(child.active_run_id.is_some());
    let fault_rows = [(false, "stored_json_invalid"), (true, "sqlite_error")];
    for (claim_fault, class) in fault_rows {
        let work_id = child.work_id;
        let select = if claim_fault {
            "SELECT run_id, claim_json FROM work_claims WHERE work_id = ?1"
        } else {
            "SELECT object_id, canonical_json FROM work_feed_entries JOIN objects USING(object_id)
                WHERE feed_kind = 'project' AND work_id = ?1 AND work_feed_entries.object_kind = 'work_event'
                ORDER BY position DESC LIMIT 1"
        };
        let update = if claim_fault {
            "UPDATE work_claims SET claim_json = ?1 WHERE run_id = ?2"
        } else {
            "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2"
        };
        let (id, original): (String, Vec<u8>) = connection
            .query_row(select, [work_id.0.to_string()], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        connection
            .execute(update, rusqlite::params![b"{}".as_slice(), id])
            .unwrap();
        if claim_fault {
            // Candidate selection succeeds, and the earlier child's remedy is
            // valid. The later claim read fails only during remedy derivation.
            let page = store
                .stranded_work_children(&project, &SessionId("agent".into()))
                .unwrap();
            assert_eq!(page.items.len(), 2);
            assert!(
                store
                    .check_work_detach_admission(first.work_id, at(26))
                    .is_ok()
            );
        }
        for peek in [false, true] {
            for verbose in [false, true] {
                let receipt = reader
                    .next(
                        &NextInput {
                            peek,
                            verbose,
                            ..Default::default()
                        },
                        at(26),
                    )
                    .unwrap();
                assert_eq!(receipt.value["stranded_children_unavailable"], true);
                assert_eq!(receipt.value["stranded_children_error_class"], class);
                for key in [
                    "stranded_children",
                    "stranded_children_omitted",
                    "stranded_children_next",
                ] {
                    assert!(receipt.value.get(key).is_none(), "no partial group: {key}");
                }
                assert!(
                    receipt
                        .text()
                        .contains(&format!("stranded children: unavailable ({class})"))
                );
                if !id.is_empty() {
                    assert!(!receipt.text().contains(&id));
                }
                assert!(
                    receipt.value["ready"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|row| row["ref"] == healthy || row["work"]["short_ref"] == healthy)
                );
                assert!(emitted_receipt_bytes(&receipt) < MAX_AGENT_WORK_RESPONSE_BYTES);
            }
        }
        if work_id == child.work_id {
            assert!(
                store
                    .check_work_detach_admission(child.work_id, at(26))
                    .is_err()
            );
            assert!(reader.show(&child.short_ref, at(26)).is_err());
        }
        connection
            .execute(update, rusqlite::params![original, id])
            .unwrap();
        assert!(
            reader
                .next(
                    &NextInput {
                        peek: true,
                        ..Default::default()
                    },
                    at(27)
                )
                .unwrap()
                .value
                .get("stranded_children_unavailable")
                .is_none()
        );
    }
    assert_eq!(
        store.resolve_work_ref(&project, &first.short_ref).unwrap(),
        first
    );
    assert_eq!(
        store.resolve_work_ref(&project, &child.short_ref).unwrap(),
        child
    );
    assert!(store.verify_all().unwrap().is_healthy());
    connection.close().unwrap();
}

#[test]
fn next_stranded_restored_required_child_uses_historical_session_not_loader() {
    let (directory, historical, path, project) = fixture();
    let parent = add(&historical, "Delivered parent", None, false, 0);
    let child = add(
        &historical,
        "Retained required follow-up",
        Some(&parent),
        true,
        1,
    );
    complete(&historical, &parent);
    let mut document = super::review::snapshot(&path, &project, &parent);
    // A recovery graph can carry an open required child under completed
    // historical work. Its completion is inert proof, never a native seal or
    // authority to complete an open required child in the active store.
    document
        .body
        .items
        .iter_mut()
        .find(|item| item.short_ref == child)
        .unwrap()
        .child_requirement = crate::ChildRequirement::Required;
    document.manifest.body_sha256 = crate::CanonicalObject::freeze(&document.body)
        .unwrap()
        .key()
        .clone();
    let restored_path = directory.path().join("required-restored.db");
    let mut store = SqliteStore::open(&restored_path).unwrap();
    let mut actor = document
        .body
        .records
        .iter()
        .find_map(|record| match &record.payload {
            crate::WorkGraphSnapshotRecordPayload::Native { history } => {
                history.events.first().map(|event| event.actor.clone())
            }
            crate::WorkGraphSnapshotRecordPayload::Restored { .. } => None,
        })
        .unwrap();
    actor.session_id = Some(SessionId("loader".into()));
    store
        .load_work_graph_snapshot(
            &project,
            &actor,
            &serde_json::to_vec(&document).unwrap(),
            false,
            at(101),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
    let reader = AgentVerbs::new(
        restored_path.clone(),
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let loader = AgentVerbs::new(
        restored_path,
        project.clone(),
        "agent".into(),
        SessionId("loader".into()),
        None,
    );
    let before = store.resolve_work_ref(&project, &parent).unwrap();
    let next = reader.next(&NextInput::default(), at(102)).unwrap();
    assert_eq!(next.value["stranded_children"][0]["ref"], child);
    assert_eq!(
        next.value["stranded_children"][0]["child_requirement"],
        "required"
    );
    assert_eq!(
        next.value["stranded_children"][0]["remedy"],
        format!("engram work update {child} --detach \"Continue as independent work\"")
    );
    assert!(next.value["focus"].is_null());
    assert_eq!(next.value["held"], serde_json::json!([]));
    assert!(
        loader
            .next(&NextInput::default(), at(102))
            .unwrap()
            .value
            .get("stranded_children")
            .is_none()
    );
    assert_eq!(store.resolve_work_ref(&project, &parent).unwrap(), before);
    assert!(store.latest_work_run(before.work_id).unwrap().is_none());
    reader
        .update(
            UpdateInput {
                work_ref: Some(child),
                action: UpdateAction::Detach {
                    reason: "Continue after recovery".into(),
                },
            },
            at(103),
        )
        .unwrap();
    assert!(
        reader
            .next(&NextInput::default(), at(104))
            .unwrap()
            .value
            .get("stranded_children")
            .is_none()
    );
}

#[test]
fn next_stranded_remedies_use_current_obstacles_and_parent_observation_participation() {
    let (_directory, reader, path, project) = fixture();
    let owner = AgentVerbs::new(
        path.clone(),
        project.clone(),
        "owner".into(),
        SessionId("owner".into()),
        None,
    );
    let parent = add(&owner, "Parent", None, false, 0);
    let child = add(&owner, "Follow-up", Some(&parent), true, 1);
    let prerequisite = add(&owner, "Resolve prerequisite", None, false, 2);
    note(&reader, &parent, "Participate only in the parent", 3);
    owner
        .update(
            UpdateInput {
                work_ref: Some(child.clone()),
                action: UpdateAction::After {
                    prerequisite: prerequisite.clone(),
                },
            },
            at(4),
        )
        .unwrap();
    complete(&owner, &parent);
    let read = || {
        reader
            .next(
                &NextInput {
                    peek: true,
                    ..Default::default()
                },
                at(22),
            )
            .unwrap()
    };
    assert_eq!(
        read().value["stranded_children"][0]["remedy"],
        format!("engram work update {child} --drop-after {prerequisite}")
    );
    let mut store = SqliteStore::open(&path).unwrap();
    let item = store.resolve_work_ref(&project, &child).unwrap();
    let blocker = store
        .add_work_blocker(
            &crate::AddWorkBlockerRequest {
                work_id: item.work_id,
                expected_work_revision: item.revision,
                kind: crate::WorkBlockerKind::Manual,
                detail: "External input".into(),
                authority: crate::WorkPlanningAuthority::Project,
                actor: item.created_by.clone(),
                idempotency_key: "stranded-block".into(),
                blocked_at: at(22),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let selector = crate::work_service::blocker_selector::encode(&blocker.blocker_id);
    let next = read();
    assert_eq!(
        next.value["stranded_children"][0]["remedy"],
        format!("engram work update {child} --unblock --blocker {selector}")
    );
    assert!(
        next.value["stranded_children"][0]["blocked_reason"]
            .as_str()
            .unwrap()
            .contains("independent active blocker")
    );
    let shown = owner.show(&child, at(22)).unwrap();
    assert_eq!(shown.value["blockers"][0]["blocker"], selector);
    owner
        .update(
            UpdateInput {
                work_ref: Some(child.clone()),
                action: UpdateAction::Unblock {
                    blocker: Some(selector),
                },
            },
            at(23),
        )
        .unwrap();
    owner
        .update(
            UpdateInput {
                work_ref: Some(child.clone()),
                action: UpdateAction::DropAfter { prerequisite },
            },
            at(24),
        )
        .unwrap();
    owner.update(UpdateInput { work_ref: Some(child.clone()), action: serde_json::from_value(serde_json::json!({"action":"revise", "defer":at(100), "title":null,"outcome":null,"acceptance":null,"assignee":null,"priority":null,"kind":null})).unwrap() }, at(25)).unwrap();
    assert_eq!(
        read().value["stranded_children"][0]["remedy"],
        format!("engram work show {child}")
    );
    assert!(
        read().value["stranded_children"][0]["blocked_reason"]
            .as_str()
            .unwrap()
            .contains("deferred wake time")
    );
}

#[test]
fn stranded_candidates_and_remedies_remain_on_the_pinned_snapshot() {
    let (_directory, reader, path, project) = fixture();
    let parent = add(&reader, "Parent", None, false, 0);
    let child = add(&reader, "Child", Some(&parent), true, 1);
    complete(&reader, &parent);
    let store = SqliteStore::open(&path).unwrap();
    store
        .work_read_snapshot(|snapshot| {
            let cut = snapshot.work_feed_head(&crate::FeedId::Project(project.clone()))?;
            reader
                .update(
                    UpdateInput {
                        work_ref: Some(child.clone()),
                        action: UpdateAction::Cancel {
                            reason: "Concurrent disposition".into(),
                        },
                    },
                    at(22),
                )
                .unwrap();
            let page = snapshot.stranded_work_children(&project, &SessionId("agent".into()))?;
            assert_eq!(page.items.len(), 1);
            assert_eq!(page.items[0].0.short_ref, child);
            snapshot.check_work_detach_admission(page.items[0].0.work_id, at(22))?;
            assert_eq!(
                snapshot.work_feed_head(&crate::FeedId::Project(project.clone()))?,
                cut
            );
            Ok(())
        })
        .unwrap();
    assert!(
        reader
            .next(&NextInput::default(), at(23))
            .unwrap()
            .value
            .get("stranded_children")
            .is_none()
    );
}
