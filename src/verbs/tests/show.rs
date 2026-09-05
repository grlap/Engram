use super::*;

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end fixture proves child prioritization and both show representations"
)]
fn show_keeps_open_children_ahead_of_the_capped_terminal_remainder() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("show-open-children-first".into());
    let session = SessionId("show-open-children-session".into());
    let service = Arc::new(LocalWorkService::new(
        database,
        project,
        "agent".into(),
        session.clone(),
        Some("show-open-children-test".into()),
    ));
    let parent = match service
        .work_propose(
            root_input("Child ordering parent", "child-ordering-parent"),
            at(0),
        )
        .expect("parent")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    let decomposition = service
        .work_propose(
            WorkProposeInput::Decompose {
                children: (0..16)
                    .map(|index| WorkChildInput {
                        notes: Vec::new(),
                        key: format!("child-{index}"),
                        title: if index == 15 {
                            format!("Open required child {index} {}", "x".repeat(256))
                        } else {
                            format!("Completed child {index}")
                        },
                        outcome: format!("Child {index} outcome"),
                        acceptance: vec![format!("Child {index} accepted")],
                        requirement: Some(ChildRequirement::Required),
                        kind: Some(WorkItemKind::Task),
                        priority: None,
                        labels: Vec::new(),
                        assigned_to: None,
                        deferred_until: None,
                    })
                    .collect(),
                prerequisites: Vec::new(),
                idempotency_key: "child-ordering-decomposition".into(),
            },
            at(1),
        )
        .expect("decomposition");
    let WorkProposeResult::Decomposition(decomposition) = decomposition else {
        panic!("expected decomposition");
    };
    for (index, child) in decomposition.children.iter().take(15).enumerate() {
        let timestamp = 2 + i64::try_from(index).expect("small child index") * 3;
        service
            .work_focus(&child.short_ref, at(timestamp))
            .expect("focus child");
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: format!("claim-child-{index}"),
                },
                at(timestamp + 1),
            )
            .expect("claim child");
        assert!(matches!(
            service
                .work_complete(
                    WorkCompleteInput {
                        capture: Some(WorkCompletionCaptureInput {
                            summary: format!("Completed child {index}"),
                            refs: Vec::new(),
                        }),
                        evidence: Vec::new(),
                        acceptance: None,
                        note: None,
                        idempotency_key: format!("complete-child-{index}"),
                    },
                    at(timestamp + 2),
                )
                .expect("complete child"),
            WorkCompleteResult::Completed(_)
        ));
    }

    let open_children = &decomposition.children[15..];
    let mut fitted = service
        .work_focus(&parent.short_ref, at(50))
        .expect("focus parent");
    assert_eq!(fitted.child_count, 16);
    assert_eq!(fitted.children.len(), 8);
    assert!(fitted.omissions.iter().all(|omission| {
        omission.reason != WorkSectionOmissionReason::UnfinishedChildCountLimit
    }));
    assert!(fitted.omissions.iter().any(|omission| {
        omission.reason == WorkSectionOmissionReason::TerminalChildCountLimit
            && omission.omitted_count == 8
    }));
    assert_eq!(fitted.children[0].short_ref, open_children[0].short_ref);
    assert_eq!(fitted.children[0].lifecycle, WorkLifecycle::Open);
    assert!(
        fitted.children[1..]
            .iter()
            .all(|child| child.lifecycle == WorkLifecycle::Completed)
    );
    let verbs = AgentVerbs::with_shared_service(service, "agent".into(), session.clone());
    let receipt = verbs.show(&parent.short_ref, at(50)).expect("show parent");
    let children = receipt.value["children"].as_array().expect("children");
    assert_eq!(children.len(), 8);
    assert_eq!(children[0]["short_ref"], open_children[0].short_ref);
    assert_eq!(children[0]["lifecycle"], "open");
    assert_eq!(receipt.value["children_omitted"], 8);
    let text = receipt.text();
    let children_line = text
        .lines()
        .find(|line| line.starts_with("children:"))
        .expect("children line");
    assert!(children_line.contains(&open_children[0].short_ref));
    assert!(children_line.ends_with("(+8 more)"));

    fitted.children.clear();
    assert_eq!(
        show_lines(&fitted, Holder::Nobody, "agent", &session, at(50))
            .into_iter()
            .find(|line| line.starts_with("children:"))
            .as_deref(),
        Some("children: 16 not shown")
    );
}

#[test]
fn show_claim_guidance_uses_the_allowed_operation_as_its_source() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("show-claim-guidance".into());
    let first_session = SessionId("first-holder".into());
    let successor_session = SessionId("successor".into());
    let first = Arc::new(LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        first_session.clone(),
        Some("agent-verb-guidance-test".into()),
    ));
    let successor = Arc::new(LocalWorkService::new(
        database,
        project,
        "agent".into(),
        successor_session.clone(),
        Some("agent-verb-guidance-test".into()),
    ));
    let first_verbs = AgentVerbs::with_shared_service(first.clone(), "agent".into(), first_session);
    let successor_verbs =
        AgentVerbs::with_shared_service(successor, "agent".into(), successor_session);

    let released = match first
        .work_propose(
            root_input("Released claim guidance", "released-guidance"),
            at(0),
        )
        .expect("released root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    first_verbs
        .claim(
            ClaimInput {
                work_ref: released.short_ref.clone(),
                ttl_seconds: Some(60),
                recover: None,
            },
            at(1),
        )
        .expect("claim released root");
    first_verbs
        .note(
            &NoteInput {
                work_ref: Some(released.short_ref.clone()),
                text: "account the first holder before release".into(),
                refs: Vec::new(),
            },
            at(2),
        )
        .expect("record first-holder contribution");
    first_verbs
        .update(
            UpdateInput {
                work_ref: Some(released.short_ref.clone()),
                action: UpdateAction::Release {
                    reason: Some("make the item ordinarily claimable".into()),
                },
            },
            at(3),
        )
        .expect("release accounted claim");

    let ordinary = successor_verbs
        .show(&released.short_ref, at(4))
        .expect("show released claim");
    assert_ordinary_claim_guidance(&ordinary, &released.short_ref);

    let lapsed = match first
        .work_propose(
            root_input("Lapsed claim guidance", "lapsed-guidance"),
            at(10),
        )
        .expect("lapsed root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    first_verbs
        .claim(
            ClaimInput {
                work_ref: lapsed.short_ref.clone(),
                ttl_seconds: Some(1),
                recover: None,
            },
            at(11),
        )
        .expect("claim lapsed root");

    let recovery = successor_verbs
        .show(&lapsed.short_ref, at(13))
        .expect("show unaccounted lapsed claim");
    assert_recovery_claim_guidance(&recovery, &lapsed.short_ref);
}

#[test]
fn holder_note_never_shortens_an_explicit_long_claim() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("long-claim-renewal".into());
    let session = SessionId("long-claim-renewal-session".into());
    let service = Arc::new(LocalWorkService::new(
        database,
        project,
        "agent".into(),
        session.clone(),
        Some("protocol-test".into()),
    ));
    let work = match service
        .work_propose(
            root_input("Long claim renewal", "long-claim-renewal-root"),
            at(0),
        )
        .expect("root proposal")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    let verbs = AgentVerbs::with_shared_service(service, "agent".into(), session);
    let claimed = verbs
        .claim(
            ClaimInput {
                work_ref: work.short_ref.clone(),
                ttl_seconds: Some(14_400),
                recover: None,
            },
            at(1),
        )
        .expect("long claim");
    let claimed_expires_at = claimed.value["receipt"]["result"]["expires_at"]
        .as_str()
        .expect("claim expiry")
        .parse::<DateTime<Utc>>()
        .expect("RFC 3339 claim expiry");

    verbs
        .note(
            &NoteInput {
                work_ref: Some(work.short_ref.clone()),
                text: "keep the longer lease while recording progress".into(),
                refs: Vec::new(),
            },
            at(2),
        )
        .expect("record note");
    let shown = verbs.show(&work.short_ref, at(3)).expect("show claim");
    let shown_expires_at = shown.value["held_until"]
        .as_str()
        .expect("shown expiry")
        .parse::<DateTime<Utc>>()
        .expect("RFC 3339 shown expiry");

    assert!(shown_expires_at >= claimed_expires_at);
}

#[test]
fn show_reports_the_true_note_total_and_latest_feed_entry() {
    const GATE_TRANSITIONS: usize = 128;

    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("show-bounded-evidence".into());
    let session = SessionId("show-bounded-evidence-session".into());
    let service = Arc::new(LocalWorkService::new(
        database,
        project,
        "agent".into(),
        session.clone(),
        Some("protocol-test".into()),
    ));
    let work = match service
        .work_propose(
            root_input("Show bounded evidence", "show-bounded-evidence-root"),
            at(0),
        )
        .expect("root proposal")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    service
        .work_update_on(
            Some(&work.short_ref),
            WorkUpdateInput::Claim {
                ttl_seconds: Some(3_600),
                recovery_reason: None,
                idempotency_key: "show-bounded-evidence-claim".into(),
            },
            at(1),
        )
        .expect("claim work");
    for index in 0..GATE_TRANSITIONS {
        let failed = if index % 2 == 0 {
            Vec::new()
        } else {
            vec!["alternating failure".to_owned()]
        };
        service
            .work_gate_on(
                Some(&work.short_ref),
                "bounded-show",
                &failed,
                None,
                at(2 + i64::try_from(index).expect("gate timestamp")),
            )
            .expect("record gate transition");
    }
    service
        .work_note_on(
            Some(&work.short_ref),
            "earlier append with newer asserted timestamp",
            &[],
            at(130),
        )
        .expect("record latest note");
    service
        .work_note_on(
            Some(&work.short_ref),
            "latest append with older asserted timestamp",
            &[],
            at(129),
        )
        .expect("record older note after latest note");

    let verbs = AgentVerbs::with_shared_service(service, "agent".into(), session);
    let receipt = verbs.show(&work.short_ref, at(131)).expect("show work");
    let notes = receipt.value["notes"].as_array().expect("notes");
    assert_eq!(notes.len(), crate::work_service::MAX_FOCUS_RELATIONS);
    assert_eq!(
        notes.last().expect("latest note")["summary"],
        "latest append with older asserted timestamp"
    );
    let evidence_count = GATE_TRANSITIONS + 2;
    let omitted = evidence_count - crate::work_service::MAX_FOCUS_RELATIONS;
    assert_eq!(receipt.value["notes_omitted"], omitted);
    assert!(
        receipt.value["omissions"]
            .as_array()
            .expect("omissions")
            .iter()
            .any(|omission| {
                omission["reason"] == "evidence_count_limit" && omission["omitted_count"] == omitted
            })
    );
    assert!(receipt.text().contains(
        "notes: 130 recorded; latest note by you: \"latest append with older asserted timestamp\""
    ));
}

#[test]
fn completed_show_advertises_late_note_without_hijacking_done_navigation() {
    let tags = [
        "work_focus".into(),
        "work_update:gate".into(),
        "work_update:note".into(),
        "work_update:reopen".into(),
    ];
    assert_eq!(
        next_commands(&tags, "w-0123456789ab", "show", false, false, &[],),
        vec!["engram work note w-0123456789ab \"…\""]
    );
    assert_eq!(
        next_commands(&tags, "w-0123456789ab", "done", false, false, &[],),
        vec!["engram work next"]
    );
}
