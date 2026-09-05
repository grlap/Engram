use super::*;

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one deterministic focus-race scenario covers the exact-target agent words"
)]
fn explicit_agent_words_keep_their_resolved_target_after_focus_changes() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("agent-verb-explicit-targets".into());
    let session = SessionId("shared-agent-session".into());
    let service = Arc::new(LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        session.clone(),
        Some("agent-verb-target-test".into()),
    ));
    let create = |title: &str, key: &str| match service
        .work_propose(root_input(title, key), at(0))
        .expect("root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    let target = create("Exact target", "exact-target");
    let other = create("Concurrent focus", "concurrent-focus");
    let handoff_target = create("Exact handoff", "exact-handoff");
    let verbs = AgentVerbs::with_shared_service(service.clone(), "agent".into(), session.clone());
    let race_running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let race_barrier = Arc::new(std::sync::Barrier::new(2));
    let race_started = Arc::new(std::sync::Barrier::new(2));
    let focus_racer = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        session.clone(),
        Some("agent-verb-target-test".into()),
    );
    let raced_ref = other.short_ref.clone();
    let thread_running = race_running.clone();
    let thread_barrier = race_barrier.clone();
    let thread_started = race_started.clone();
    let focus_thread = std::thread::spawn(move || {
        thread_barrier.wait();
        focus_racer
            .select_work(&raced_ref, at(2))
            .expect("initial same-session focus race");
        let mut switches = 1_usize;
        thread_started.wait();
        while thread_running.load(std::sync::atomic::Ordering::Acquire) && switches < 10_000 {
            focus_racer
                .select_work(&raced_ref, at(2))
                .expect("same-session focus race");
            switches += 1;
            std::thread::yield_now();
        }
        switches
    });
    race_barrier.wait();
    race_started.wait();

    verbs
        .claim(
            ClaimInput {
                work_ref: target.short_ref.clone(),
                ttl_seconds: Some(300),
                recover: None,
            },
            at(1),
        )
        .expect("claim remains on exact target");
    assert_eq!(
        SqliteStore::open(&database)
            .expect("store")
            .current_work_claim(target.work_id)
            .expect("target claim")
            .expect("live target claim")
            .work_id,
        target.work_id
    );
    assert!(
        SqliteStore::open(&database)
            .expect("store")
            .current_work_claim(other.work_id)
            .expect("other claim")
            .is_none()
    );

    let note = NoteInput {
        work_ref: Some(target.short_ref.clone()),
        text: "one atomic note capture".into(),
        refs: vec!["test:exact-note".into()],
    };
    let first_note = verbs.note(&note, at(3)).expect("note exact target");
    let replayed_note = verbs.note(&note, at(4)).expect("replay exact note");
    assert_eq!(first_note.value["receipt"], replayed_note.value["receipt"]);
    assert_eq!(
        first_note.value["evidence"],
        replayed_note.value["evidence"]
    );
    let target_run = target.active_run_id.expect("target run");
    let store = SqliteStore::open(&database).expect("store");
    let evidence = store
        .work_run_evidence(target_run)
        .expect("target evidence");
    assert_eq!(evidence.len(), 1);
    let checkpoint_hash = ObjectHash::from_stored(
        first_note.value["receipt"]["result"]
            .as_str()
            .expect("checkpoint hash")
            .to_owned(),
    )
    .expect("valid checkpoint hash");
    let checkpoint = store
        .get::<crate::WorkCheckpoint>(&checkpoint_hash)
        .expect("checkpoint read")
        .expect("checkpoint");
    assert_eq!(checkpoint.work_id, target.work_id);
    assert_eq!(checkpoint.evidence, evidence);
    assert!(
        store
            .work_run_evidence(other.active_run_id.expect("other run"))
            .expect("other evidence")
            .is_empty()
    );

    verbs
        .done(
            DoneInput {
                work_ref: Some(target.short_ref.clone()),
                summary: Some("exact target completed".into()),
                note: None,
            },
            at(5),
        )
        .expect("completion remains on exact target");
    assert_eq!(
        service
            .inspect_work(&target.work_id.0.to_string(), at(6))
            .expect("target view")
            .status
            .work
            .lifecycle,
        WorkLifecycle::Completed
    );
    assert_eq!(
        service
            .inspect_work(&other.work_id.0.to_string(), at(6))
            .expect("other view")
            .status
            .work
            .lifecycle,
        WorkLifecycle::Open
    );

    verbs
        .claim(
            ClaimInput {
                work_ref: handoff_target.short_ref.clone(),
                ttl_seconds: Some(300),
                recover: None,
            },
            at(7),
        )
        .expect("claim handoff target");
    verbs
        .handoff(
            HandoffInput {
                work_ref: Some(handoff_target.short_ref.clone()),
                action: HandoffAction::Offer {
                    to: "peer-session".into(),
                    summary: Some("handoff the exact item".into()),
                    ttl_seconds: Some(300),
                },
            },
            at(8),
        )
        .expect("offer remains on exact target");
    race_running.store(false, std::sync::atomic::Ordering::Release);
    assert!(focus_thread.join().expect("focus race thread") > 0);
    let peer_session = SessionId("peer-session".into());
    let peer_service = Arc::new(LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        peer_session.clone(),
        Some("agent-verb-target-test".into()),
    ));
    let peer = AgentVerbs::with_shared_service(peer_service, "agent".into(), peer_session.clone());
    let peer_race_running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let peer_race_barrier = Arc::new(std::sync::Barrier::new(2));
    let peer_race_started = Arc::new(std::sync::Barrier::new(2));
    let peer_focus_racer = LocalWorkService::new(
        database.clone(),
        project,
        "agent".into(),
        peer_session.clone(),
        Some("agent-verb-target-test".into()),
    );
    let peer_raced_ref = other.short_ref.clone();
    let thread_running = peer_race_running.clone();
    let thread_barrier = peer_race_barrier.clone();
    let thread_started = peer_race_started.clone();
    let peer_focus_thread = std::thread::spawn(move || {
        thread_barrier.wait();
        peer_focus_racer
            .select_work(&peer_raced_ref, at(9))
            .expect("initial peer same-session focus race");
        let mut switches = 1_usize;
        thread_started.wait();
        while thread_running.load(std::sync::atomic::Ordering::Acquire) && switches < 10_000 {
            peer_focus_racer
                .select_work(&peer_raced_ref, at(9))
                .expect("peer same-session focus race");
            switches += 1;
            std::thread::yield_now();
        }
        switches
    });
    peer_race_barrier.wait();
    peer_race_started.wait();
    peer.handoff(
        HandoffInput {
            work_ref: Some(handoff_target.short_ref.clone()),
            action: HandoffAction::Accept,
        },
        at(10),
    )
    .expect("accept remains on exact target");
    peer_race_running.store(false, std::sync::atomic::Ordering::Release);
    assert!(peer_focus_thread.join().expect("peer focus race thread") > 0);
    let accepted = SqliteStore::open(&database)
        .expect("store")
        .current_work_claim(handoff_target.work_id)
        .expect("handoff claim")
        .expect("accepted claim");
    assert_eq!(accepted.work_id, handoff_target.work_id);
    assert_eq!(accepted.holder, peer_session);
    assert!(
        SqliteStore::open(&database)
            .expect("store")
            .current_work_claim(other.work_id)
            .expect("other claim")
            .is_none()
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end test keeps admission, raw JSON, framed text, and Unicode terminal-safety assertions on the same stored body"
)]
fn project_memory_full_shape_refuses_early_and_uses_the_bounded_shared_envelope() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("project-memory-verb-envelope".into());
    let verbs = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("memory-verb-session".into()),
        Some("project-memory-verb-test".into()),
    );
    let full_after = verbs
        .memories(
            &MemoriesInput {
                query: Some("memory-key".into()),
                after: Some("after-key".into()),
                full: true,
            },
            at(0),
        )
        .expect_err("full plus after must refuse");
    assert!(full_after.to_string().contains("cannot be combined"));
    let full_without_key = verbs
        .memories(
            &MemoriesInput {
                query: None,
                after: None,
                full: true,
            },
            at(0),
        )
        .expect_err("full without a key must refuse");
    assert!(
        full_without_key
            .to_string()
            .contains("requires a memory key")
    );

    verbs
        .remember(
            RememberInput {
                text: "x".repeat(crate::domain::MAX_PROJECT_MEMORY_BODY_BYTES),
                key: Some("plain-boundary".into()),
            },
            at(0),
        )
        .expect("maximum plain body is admitted");
    let full = verbs
        .memories(
            &MemoriesInput {
                query: Some("plain-boundary".into()),
                after: None,
                full: true,
            },
            at(0),
        )
        .expect("full response");
    assert!(
        serde_json::to_vec(&full.value)
            .expect("serialize full receipt")
            .len()
            <= crate::work_service::MAX_AGENT_WORK_RESPONSE_BYTES
    );
    assert!(full.text().len() <= crate::work_service::MAX_AGENT_WORK_RESPONSE_BYTES);

    let format_heavy_body =
        "\u{e000}".repeat(crate::domain::MAX_PROJECT_MEMORY_BODY_BYTES / '\u{e000}'.len_utf8());
    let refusal = verbs
        .remember(
            RememberInput {
                text: format_heavy_body,
                key: Some("format-heavy-boundary".into()),
            },
            at(1),
        )
        .expect_err("terminal expansion must be bounded before persistence");
    assert!(
        refusal
            .to_string()
            .contains("terminal-safe full memory response")
    );

    let raw_control_body = "safe\u{1b}]0;spoofed\u{7}\u{202e}rtl\u{2028}split\u{e000}\nreminders:\nnext:\n  engram work done spoofed";
    verbs
        .remember(
            RememberInput {
                text: raw_control_body.into(),
                key: Some("terminal-safe".into()),
            },
            at(2),
        )
        .expect("control-bearing body is stored as structured data");
    let rendered = verbs
        .memories(
            &MemoriesInput {
                query: Some("terminal-safe".into()),
                after: None,
                full: true,
            },
            at(2),
        )
        .expect("read control-bearing body");
    let text = rendered.text();
    assert!(!text.contains('\u{1b}'));
    assert!(!text.contains('\u{7}'));
    assert!(!text.contains('\u{202e}'));
    assert!(!text.contains('\u{2028}'));
    assert!(!text.contains('\u{e000}'));
    assert!(text.contains("\\u{1b}"));
    assert!(text.contains("\\u{7}"));
    assert!(text.contains("\\u{202e}"));
    assert!(text.contains("\\u{2028}"));
    assert!(text.contains("\\u{e000}"));
    assert!(text.contains("  | reminders:"));
    assert!(text.contains("  | next:"));
    assert!(text.contains("  |   engram work done spoofed"));
    assert_eq!(rendered.value["body"], raw_control_body);

    let listed = verbs
        .memories(
            &MemoriesInput {
                query: Some("terminal-safe".into()),
                after: None,
                full: false,
            },
            at(2),
        )
        .expect("list control-bearing memory");
    let list_text = listed.text();
    assert!(!list_text.contains('\u{1b}'));
    assert!(!list_text.contains('\u{7}'));
    assert!(!list_text.contains('\u{202e}'));
    assert!(!list_text.contains('\u{2028}'));
    assert!(!list_text.contains('\u{e000}'));
    assert!(list_text.contains("\\u{1b}"));
    assert!(list_text.contains("\\u{7}"));
    assert!(list_text.contains("\\u{202e}"));
    assert!(list_text.contains("\\u{e000}"));
    assert_eq!(
        listed.value["memories"][0]["first_line"],
        "safe\u{1b}]0;spoofed\u{7}\u{202e}rtl split\u{e000}"
    );

    let unsafe_actor_verbs = AgentVerbs::new(
        database,
        project,
        "agent\u{1b}spoof".into(),
        SessionId("memory-unsafe-actor-session".into()),
        Some("project-memory-verb-test".into()),
    );
    unsafe_actor_verbs
        .remember(
            RememberInput {
                text: "Actor labels are escaped at the receipt boundary.".into(),
                key: Some("unsafe-actor-label".into()),
            },
            at(3),
        )
        .expect("store unsafe asserted actor as structured attribution");
    let unsafe_actor_list = unsafe_actor_verbs
        .memories(
            &MemoriesInput {
                query: Some("unsafe-actor-label".into()),
                after: None,
                full: false,
            },
            at(3),
        )
        .expect("render unsafe asserted actor");
    let unsafe_actor_text = unsafe_actor_list.text();
    assert!(!unsafe_actor_text.contains('\u{1b}'));
    assert!(unsafe_actor_text.contains("agent\\u{1b}spoof"));
    assert_eq!(
        unsafe_actor_list.value["memories"][0]["actor_id"],
        "agent\u{1b}spoof"
    );
}

#[test]
fn project_memory_listing_sheds_escape_heavy_rows_without_skipping_a_blank_query_page() {
    let directory = tempdir().expect("temporary directory");
    let verbs = AgentVerbs::new(
        directory.path().join("engram.sqlite3"),
        ProjectId("project-memory-list-budget".into()),
        "agent".into(),
        SessionId("memory-list-budget-session".into()),
        Some("project-memory-list-budget-test".into()),
    );
    for index in 0..20 {
        verbs
            .remember(
                RememberInput {
                    text: "\u{7}".repeat(160),
                    key: Some(format!("escape-heavy-{index:02}")),
                },
                at(i64::from(index)),
            )
            .expect("store escape-heavy preview");
    }

    let mut receipt = verbs
        .memories(
            &MemoriesInput {
                query: Some(" \t ".into()),
                ..MemoriesInput::default()
            },
            at(20),
        )
        .expect("fit project-memory listing");
    assert!(
        receipt.value["memories"]
            .as_array()
            .is_some_and(|rows| rows.len() < 20)
    );
    assert_eq!(receipt.value["omitted_count"], 0);
    assert!(receipt.value["next_after"].is_string());
    let mut seen = Vec::new();
    loop {
        assert!(
            serde_json::to_vec(&receipt.value)
                .expect("serialize fitted list")
                .len()
                <= crate::work_service::MAX_AGENT_WORK_RESPONSE_BYTES
        );
        assert!(receipt.text().len() <= crate::work_service::MAX_AGENT_WORK_RESPONSE_BYTES);
        seen.extend(
            receipt.value["memories"]
                .as_array()
                .expect("memory rows")
                .iter()
                .map(|row| row["key"].as_str().expect("memory key").to_owned()),
        );
        let Some(after) = receipt.value["next_after"].as_str() else {
            break;
        };
        receipt = verbs
            .memories(
                &MemoriesInput {
                    after: Some(after.to_owned()),
                    ..MemoriesInput::default()
                },
                at(20),
            )
            .expect("continue shed listing");
    }
    seen.sort();
    seen.dedup();
    assert_eq!(
        seen,
        (0..20)
            .map(|index| format!("escape-heavy-{index:02}"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn completion_recovery_reminder_names_each_disposed_child_lifecycle() {
    let child = WorkId(uuid::Uuid::from_u128(1));
    for (lifecycle, word) in [
        (WorkLifecycle::Cancelled, "cancelled"),
        (WorkLifecycle::Superseded, "superseded"),
    ] {
        let recovery = crate::WorkCompletionRecovery {
            cause: crate::WorkCompletionRecoveryCause::RequiredChildUnsealed { child },
            item: crate::WorkReferenceCandidate {
                work_id: child,
                short_ref: "w-000000000001".into(),
                title: "Disposed child".into(),
                lifecycle,
            },
            command: "engram work update w-000000000002 --waive w-000000000001 --reason \"why\""
                .into(),
        };
        assert_eq!(
            completion_recovery_reminder(&recovery),
            format!(
                "required child w-000000000001 \"Disposed child\" is {word} without a completion seal or waiver"
            )
        );
    }
}

#[test]
fn readiness_reasons_become_words() {
    let session = SessionId("peer".into());
    let now = Utc::now();
    assert_eq!(
        reminder_for_reason(
            "live claim has not checkpointed progress",
            Holder::You(now),
            &[],
            false,
        )
        .as_deref(),
        Some("you hold this item but have not noted progress yet")
    );
    assert_eq!(
        reminder_for_reason(
            "live claim has not checkpointed progress",
            Holder::Other(&session, now),
            &[],
            false,
        )
        .as_deref(),
        Some("held by another session; no progress noted yet")
    );
    assert_eq!(
        reminder_for_reason(
            "one or more typed blockers remain active",
            Holder::Nobody,
            &["waiting on review".into()],
            false,
        )
        .as_deref(),
        Some("blocked: waiting on review")
    );
    assert_eq!(
        reminder_for_reason(
            "one or more prerequisites are dead and must be removed",
            Holder::Nobody,
            &[],
            false,
        )
        .as_deref(),
        Some("waiting: a dead prerequisite must be removed")
    );
    assert_eq!(
        reminder_for_reason("lifecycle is Completed", Holder::Nobody, &[], false),
        None
    );
    assert_eq!(
        reminder_for_reason(
            "open, admitted, unblocked, and unclaimed",
            Holder::Nobody,
            &[],
            false,
        )
        .as_deref(),
        Some("unclaimed: claim it before execution")
    );
    assert_eq!(
        reminder_for_reason("prior claim is recoverable", Holder::Nobody, &[], false,),
        None
    );
    assert_eq!(
        reminder_for_reason("prior claim is recoverable", Holder::Nobody, &[], true,).as_deref(),
        Some("a previous holder's claim lapsed; claiming needs a recovery reason")
    );
}

#[test]
fn catalog_claim_guidance_routes_through_exact_show() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("catalog-claim-guidance".into());
    let first_session = SessionId("first-holder".into());
    let reader_session = SessionId("catalog-reader".into());
    let first = Arc::new(LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        first_session.clone(),
        Some("catalog-guidance-test".into()),
    ));
    let reader = Arc::new(LocalWorkService::new(
        database,
        project,
        "agent".into(),
        reader_session.clone(),
        Some("catalog-guidance-test".into()),
    ));
    let first_verbs = AgentVerbs::with_shared_service(first.clone(), "agent".into(), first_session);
    let reader_verbs = AgentVerbs::with_shared_service(reader, "agent".into(), reader_session);
    let work = match first
        .work_propose(root_input("Catalog recovery", "catalog-recovery"), at(0))
        .expect("root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    first_verbs
        .claim(
            ClaimInput {
                work_ref: work.short_ref.clone(),
                ttl_seconds: Some(1),
                recover: None,
            },
            at(1),
        )
        .expect("claim root");

    let expected = format!("engram work show {}", work.short_ref);
    let next = reader_verbs
        .next(&NextInput::default(), at(3))
        .expect("next catalog guidance");
    assert_eq!(next.next, vec![expected.clone()]);
    let list = reader_verbs
        .ls(&LsInput::default(), at(3))
        .expect("list catalog guidance");
    assert_eq!(list.next, vec![expected]);

    let mismatch = VerbError::at(
        StoreError::WorkClaimMismatch { work: work.work_id },
        &work.short_ref,
    )
    .guidance();
    assert_eq!(mismatch.next, list.next);
}

#[test]
fn open_test_obligation_becomes_the_test_reminder() {
    let reminders = obligation_reminders(&page(VerificationKind::Test, WorkObligationState::Open));
    assert_eq!(
            reminders,
            vec![
                "tests have not run since your last source change — run them; the host records the result"
                    .to_owned()
            ]
        );
    assert!(
        obligation_reminders(&page(
            VerificationKind::Test,
            WorkObligationState::Satisfied
        ))
        .is_empty()
    );
}

#[test]
fn allowed_next_tags_become_commands_and_host_only_entries_vanish() {
    let tags = [
        "work_focus",
        "work_update:claim",
        "work_update:reopen",
        "work_update:supersede",
        "work_update:waive_required_child",
        "work_update:add_prerequisite",
        "work_propose:decompose",
    ]
    .map(String::from);
    assert_eq!(
        next_commands(&tags, "w-0123456789ab", "add", false, true, &[]),
        vec![
            "engram work claim w-0123456789ab",
            "engram work show w-0123456789ab",
        ]
    );
    // Planning edits the holder could make (release, offer, block,
    // decompose, revise, cancel) stay behind `show`; only the moves that
    // change who holds the item or whether it is finished are suggested.
    let held = [
        "work_focus",
        "work_update:checkpoint",
        "work_update:evidence",
        "work_update:release",
        "work_complete",
        "work_handoff:offer",
        "work_update:block",
        "work_update:unblock",
        "work_propose:decompose",
        "work_update:revise",
        "work_update:cancel",
    ]
    .map(String::from);
    assert_eq!(
        next_commands(&held, "w-0123456789ab", "show", false, true, &[]),
        vec![
            "engram work note w-0123456789ab \"…\"",
            "engram work done w-0123456789ab \"…\"",
        ]
    );
    assert_eq!(
        next_commands(&held, "w-0123456789ab", "claim", true, true, &[]),
        vec![
            "engram work note w-0123456789ab \"…\"",
            "engram work done w-0123456789ab \"…\"",
            "engram work update w-0123456789ab --unblock",
            "engram work show w-0123456789ab",
        ]
    );
    // Lifecycle moves are capped at three in priority order; `show` is
    // still the one trailing entry, so no receipt lists more than four.
    let crowded = [
        "work_focus",
        "work_handoff:accept",
        "work_update:claim",
        "work_update:checkpoint",
        "work_complete",
        "work_update:unblock",
    ]
    .map(String::from);
    assert_eq!(
        next_commands(&crowded, "w-0123456789ab", "next", true, true, &[]),
        vec![
            "engram work handoff w-0123456789ab --accept",
            "engram work claim w-0123456789ab",
            "engram work note w-0123456789ab \"…\"",
            "engram work show w-0123456789ab",
        ]
    );
    assert_eq!(
        next_commands(
            &["work_focus".into()],
            "w-0123456789ab",
            "done",
            false,
            true,
            &[],
        ),
        vec!["engram work show w-0123456789ab", "engram work next",]
    );
    // A closed item is not worth showing again, and `next` never
    // suggests itself.
    assert_eq!(
        next_commands(
            &["work_focus".into()],
            "w-0123456789ab",
            "done",
            false,
            false,
            &[],
        ),
        vec!["engram work next"]
    );
    assert!(
        next_commands(
            &["work_focus".into()],
            "w-0123456789ab",
            "next",
            false,
            false,
            &[],
        )
        .is_empty()
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one table test covers authority, pending, dead, and bounded command priority"
)]
fn drop_prerequisite_guidance_requires_plan_authority_and_a_dead_target() {
    let summary = |index: u128, lifecycle| crate::work_service::WorkItemSummary {
        work_id: WorkId(uuid::Uuid::from_u128(index)),
        short_ref: format!("w-{index:012x}"),
        root_id: WorkId(uuid::Uuid::from_u128(index)),
        parent_id: None,
        child_requirement: ChildRequirement::Required,
        title: "Prerequisite".into(),
        outcome: "Prerequisite".into(),
        acceptance: vec!["Prerequisite is done".into()],
        acceptance_count: 1,
        kind: WorkItemKind::Task,
        priority: 2,
        labels: Vec::new(),
        assigned_to: None,
        lifecycle,
        restored: false,
        revision: 1,
        active_run_id: None,
        superseded_by: None,
        prerequisite_state: Some(match lifecycle {
            WorkLifecycle::Cancelled => WorkPrerequisiteState::Dead,
            WorkLifecycle::Completed => WorkPrerequisiteState::Satisfied,
            _ => WorkPrerequisiteState::Pending,
        }),
        updated_at: DateTime::<Utc>::from_timestamp(0, 0).expect("epoch"),
    };
    let cancelled = summary(0, WorkLifecycle::Cancelled);
    let open = summary(0, WorkLifecycle::Open);
    let mut superseded_dead = summary(0, WorkLifecycle::Superseded);
    superseded_dead.prerequisite_state = Some(WorkPrerequisiteState::Dead);
    let command = "engram work update w-111111111111 --drop-after w-000000000000".to_owned();

    assert!(
        !next_commands(
            &["work_focus".into()],
            "w-111111111111",
            "show",
            false,
            true,
            std::slice::from_ref(&cancelled),
        )
        .contains(&command)
    );
    assert!(
        !next_commands(
            &[
                "work_focus".into(),
                "work_update:remove_prerequisite".into()
            ],
            "w-111111111111",
            "show",
            false,
            true,
            &[open],
        )
        .contains(&command)
    );
    assert!(
        next_commands(
            &[
                "work_focus".into(),
                "work_update:remove_prerequisite".into()
            ],
            "w-111111111111",
            "show",
            false,
            true,
            &[cancelled],
        )
        .contains(&command)
    );
    assert!(
        next_commands(
            &[
                "work_focus".into(),
                "work_update:remove_prerequisite".into()
            ],
            "w-111111111111",
            "show",
            false,
            true,
            &[superseded_dead],
        )
        .contains(&command)
    );

    let cancelled = (0..4)
        .map(|index| summary(index, WorkLifecycle::Cancelled))
        .collect::<Vec<_>>();
    let crowded = [
        "work_focus",
        "work_update:remove_prerequisite",
        "work_handoff:accept",
        "work_update:claim",
        "work_update:checkpoint",
        "work_complete",
    ]
    .map(String::from);
    assert_eq!(
        next_commands(&crowded, "w-111111111111", "show", true, true, &cancelled,),
        [
            "engram work update w-111111111111 --drop-after w-000000000000",
            "engram work handoff w-111111111111 --accept",
            "engram work claim w-111111111111",
        ]
    );
}

#[test]
fn lapsed_holder_guidance_names_the_expiry_and_plain_retake_command() {
    let expired_at = Utc::now();
    let error = VerbError::at(
        StoreError::WorkClaimLapsed {
            work: crate::domain::WorkId::new(),
            expired_at,
        },
        "w-0123456789ab",
    );
    let guidance = error.guidance();
    assert_eq!(
        guidance.reminders,
        vec![format!("claim lapsed at {}", clock(expired_at, Utc::now()))]
    );
    assert_eq!(
        guidance.next,
        vec![String::from("engram work claim w-0123456789ab")]
    );
}

#[test]
fn not_ready_guidance_names_the_inspection_command() {
    let error = VerbError::at(
        StoreError::InvalidWork("work is not ready: Blocked".into()),
        "w-0123456789ab",
    );
    let guidance = error.guidance();
    assert_eq!(
        guidance.reminders,
        vec!["this item is not ready; inspect its blockers or deferral"]
    );
    assert_eq!(guidance.next, vec!["engram work show w-0123456789ab"]);
}

#[test]
fn explicit_claim_recovery_refusal_supplies_the_required_command() {
    let reason = "claim recovery requires an explicit attributed reason";
    let error = VerbError::at(StoreError::InvalidWork(reason.into()), "w-0123456789ab");
    let guidance = error.guidance();
    assert_eq!(guidance.reminders, vec![reason]);
    assert_eq!(
        guidance.next,
        vec!["engram work claim w-0123456789ab --recover \"…\""]
    );
}

#[test]
fn completed_holder_word_refusal_supplies_only_the_late_note_command() {
    let work_ref = "w-0123456789ab";
    let guidance = VerbError::at(
        StoreError::InvalidWork(COMPLETED_WORK_LATE_FINDING_REFUSAL.into()),
        work_ref,
    )
    .guidance();
    assert_eq!(
        guidance.reminders,
        vec![COMPLETED_WORK_LATE_FINDING_REFUSAL]
    );
    assert_eq!(
        guidance.next,
        vec![format!("engram work note {work_ref} \"…\"")]
    );
    assert!(
        guidance
            .next
            .iter()
            .all(|command| !command.contains("reopen"))
    );
}

#[test]
fn invalid_context_generation_guidance_retries_next_without_the_bad_advisory() {
    let reason = "context_generation must be at most 256 bytes without control characters";
    let guidance = VerbError::from(StoreError::InvalidProjectMemory(reason.into())).guidance();
    assert_eq!(guidance.reminders, vec![reason]);
    assert_eq!(guidance.next, vec!["engram work next"]);
}

#[test]
fn ambiguous_reference_guidance_names_candidates_and_uses_full_ids() {
    let first = crate::WorkId::new();
    let second = crate::WorkId::new();
    let error = VerbError::at(
        StoreError::WorkReferenceAmbiguous {
            reference: "w-collision".into(),
            candidates: vec![
                crate::WorkReferenceCandidate {
                    work_id: first,
                    short_ref: "w-collision".into(),
                    title: "First candidate".into(),
                    lifecycle: WorkLifecycle::Open,
                },
                crate::WorkReferenceCandidate {
                    work_id: second,
                    short_ref: "w-collision".into(),
                    title: "Second candidate".into(),
                    lifecycle: WorkLifecycle::Completed,
                },
            ],
            more: 3,
        },
        "w-collision",
    );
    let guidance = error.guidance();
    assert_eq!(guidance.reminders.len(), 3);
    assert!(guidance.reminders[0].contains("First candidate\" is open"));
    assert!(guidance.reminders[1].contains("Second candidate\" is completed"));
    assert_eq!(
        guidance.reminders[2],
        "3 additional ambiguous candidates were omitted"
    );
    assert!(
        error
            .error
            .to_string()
            .contains("3 additional candidates omitted")
    );
    assert_eq!(
        guidance.next,
        vec![
            format!("engram work show {}", first.0),
            format!("engram work show {}", second.0),
        ]
    );
}

#[test]
fn invalid_waiver_child_reference_is_attributed_to_the_child() {
    let directory = tempdir().expect("temporary store");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("waiver-child-attribution".into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("waiver-child-session".into()),
        Some("protocol-test".into()),
    );
    let parent = match service
        .work_propose(root_input("Waiver parent", "waiver-parent"), at(0))
        .expect("create parent")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    let verbs = AgentVerbs::new(
        database,
        project,
        "agent".into(),
        SessionId("waiver-child-session".into()),
        Some("protocol-test".into()),
    );
    let child_ref = "w-ffffffffffff";
    let error = verbs
        .update(
            UpdateInput {
                work_ref: Some(parent.short_ref),
                action: UpdateAction::WaiveRequiredChild {
                    child: child_ref.into(),
                    reason: "account for disposed child".into(),
                },
            },
            at(1),
        )
        .expect_err("unknown child is refused");
    assert_eq!(error.work_ref.as_deref(), Some(child_ref));
}

#[test]
fn gate_input_normalizes_identity_and_deduplicates_failures() {
    let normalized = normalize_gate_input(&GateInput {
        work_ref: None,
        name: "  CARGO-TEST  ".into(),
        failed: vec![" test_b ".into(), "test_a".into(), "test_a".into()],
        evidence_ref: Some(" target/cafe\u{301}.log ".into()),
    })
    .expect("normalize gate input");

    assert_eq!(normalized.name, "cargo-test");
    assert_eq!(normalized.failed, ["test_a", "test_b"]);
    assert_eq!(normalized.evidence_ref.as_deref(), Some("target/café.log"));
    assert_eq!(
        normalize_gate_input(&GateInput {
            work_ref: None,
            name: "cargo-test".into(),
            failed: vec!["same".into(); MAX_GATE_FAILURES + 1],
            evidence_ref: None,
        })
        .expect("the distinct-failure bound is applied after deduplication")
        .failed,
        ["same"]
    );
}

#[test]
fn gate_evidence_preserves_exact_failure_boundaries() {
    let left = GateInput {
        work_ref: None,
        name: "cargo-test".into(),
        failed: vec!["a | b".into(), "c".into()],
        evidence_ref: None,
    };
    let right = GateInput {
        work_ref: None,
        name: "cargo-test".into(),
        failed: vec!["a".into(), "b | c".into()],
        evidence_ref: None,
    };

    let left_summary = serde_json::to_string(&crate::GateEvidenceRecord {
        schema_version: crate::domain::SCHEMA_VERSION,
        name: left.name,
        passed: false,
        failed: left.failed,
        previous: None,
    })
    .expect("left summary");
    let right_summary = serde_json::to_string(&crate::GateEvidenceRecord {
        schema_version: crate::domain::SCHEMA_VERSION,
        name: right.name,
        passed: false,
        failed: right.failed,
        previous: None,
    })
    .expect("right summary");
    assert_ne!(left_summary, right_summary);
    assert_eq!(
        serde_json::from_str::<Value>(&left_summary).expect("structured summary"),
        json!({
            "schema_version": crate::domain::SCHEMA_VERSION,
            "name": "cargo-test",
            "passed": false,
            "failed": ["a | b", "c"],
        })
    );
}

#[test]
fn gate_input_enforces_every_normalized_bound() {
    let oversized_total = (0..=MAX_GATE_FAILURE_TOTAL_BYTES / MAX_GATE_FAILURE_BYTES)
        .map(|index| format!("{index:02}{}", "x".repeat(MAX_GATE_FAILURE_BYTES - 2)))
        .collect();
    for (input, expected) in [
        (
            GateInput {
                work_ref: None,
                name: "x".repeat(MAX_GATE_NAME_BYTES + 1),
                failed: Vec::new(),
                evidence_ref: None,
            },
            format!(
                "local work input is invalid: gate_input_too_large: gate name exceeds {MAX_GATE_NAME_BYTES} UTF-8 bytes; rerun with one aggregate --failed entry and --ref OPAQUE_REFERENCE"
            ),
        ),
        (
            GateInput {
                work_ref: None,
                name: "gate".into(),
                failed: vec!["x".repeat(MAX_GATE_FAILURE_BYTES + 1)],
                evidence_ref: None,
            },
            format!(
                "local work input is invalid: gate_input_too_large: one gate failure label exceeds {MAX_GATE_FAILURE_BYTES} UTF-8 bytes; rerun with one aggregate --failed entry and --ref OPAQUE_REFERENCE"
            ),
        ),
        (
            GateInput {
                work_ref: None,
                name: "gate".into(),
                failed: (0..=MAX_GATE_FAILURES)
                    .map(|index| format!("test-{index}"))
                    .collect(),
                evidence_ref: None,
            },
            format!(
                "local work input is invalid: gate_input_too_large: more than {MAX_GATE_FAILURES} distinct gate failure labels were supplied; rerun with one aggregate --failed entry and --ref OPAQUE_REFERENCE"
            ),
        ),
        (
            GateInput {
                work_ref: None,
                name: "gate".into(),
                failed: oversized_total,
                evidence_ref: None,
            },
            format!(
                "local work input is invalid: gate_input_too_large: the normalized gate failure-label list exceeds {MAX_GATE_FAILURE_TOTAL_BYTES} UTF-8 bytes; rerun with one aggregate --failed entry and --ref OPAQUE_REFERENCE"
            ),
        ),
        (
            GateInput {
                work_ref: None,
                name: "gate".into(),
                failed: vec!["same".into(); MAX_GATE_FAILURE_INPUTS + 1],
                evidence_ref: None,
            },
            format!(
                "local work input is invalid: gate_input_too_large: more than {MAX_GATE_FAILURE_INPUTS} gate failure labels were supplied; rerun with one aggregate --failed entry and --ref OPAQUE_REFERENCE"
            ),
        ),
    ] {
        assert_eq!(
            normalize_gate_input(&input)
                .expect_err("oversize gate input")
                .to_string(),
            expected
        );
    }
}

#[test]
fn gate_input_enforces_reference_bound_and_shape() {
    let oversized_ref = normalize_gate_input(&GateInput {
        work_ref: None,
        name: "gate".into(),
        failed: Vec::new(),
        evidence_ref: Some("x".repeat(MAX_GATE_REF_BYTES + 1)),
    })
    .expect_err("oversize gate reference");
    assert_eq!(
        oversized_ref.to_string(),
        format!(
            "local work input is invalid: gate --ref must be a control- and format-free opaque reference of at most {MAX_GATE_REF_BYTES} UTF-8 bytes"
        )
    );

    let unsafe_ref = normalize_gate_input(&GateInput {
        work_ref: None,
        name: "gate".into(),
        failed: Vec::new(),
        evidence_ref: Some("bad\nref".into()),
    })
    .expect_err("unsafe gate reference");
    assert!(
        unsafe_ref
            .to_string()
            .contains("control- and format-free opaque reference")
    );
}

#[test]
fn gate_input_refuses_control_and_format_characters() {
    for input in [
        GateInput {
            work_ref: None,
            name: "bad\ngate".into(),
            failed: Vec::new(),
            evidence_ref: None,
        },
        GateInput {
            work_ref: None,
            name: "gate".into(),
            failed: vec!["bad\u{1b}test".into()],
            evidence_ref: None,
        },
        GateInput {
            work_ref: None,
            name: "gate".into(),
            failed: vec!["bad\u{202e}test".into()],
            evidence_ref: None,
        },
        GateInput {
            work_ref: None,
            name: "gate".into(),
            failed: vec!["bad\u{e0020}test".into()],
            evidence_ref: None,
        },
    ] {
        let error = normalize_gate_input(&input).expect_err("unsafe gate text");
        assert!(error.to_string().contains("control or format characters"));
    }
}

#[test]
fn gate_input_rejects_oversized_raw_strings_before_normalization() {
    for input in [
        GateInput {
            work_ref: None,
            name: "x".repeat(MAX_GATE_NAME_BYTES * 4 + 1),
            failed: Vec::new(),
            evidence_ref: None,
        },
        GateInput {
            work_ref: None,
            name: "gate".into(),
            failed: vec!["x".repeat(MAX_GATE_FAILURE_BYTES * 4 + 1)],
            evidence_ref: None,
        },
        GateInput {
            work_ref: None,
            name: "gate".into(),
            failed: Vec::new(),
            evidence_ref: Some("x".repeat(MAX_GATE_REF_BYTES * 4 + 1)),
        },
    ] {
        assert!(
            normalize_gate_input(&input)
                .expect_err("raw oversize must be refused before normalization")
                .to_string()
                .contains("normalization input ceiling")
        );
    }
}
