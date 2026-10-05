use super::*;

fn lapse(work: &str) -> String {
    crate::work_service::own_claim_lapse_reminder(work)
}

fn hold(verbs: &AgentVerbs, work: &str, ttl: i64, second: i64) {
    verbs
        .claim(
            ClaimInput {
                work_ref: work.into(),
                ttl_seconds: Some(ttl),
                recover: None,
            },
            at(second),
        )
        .expect("claim");
}

fn session(database: &std::path::Path, project: &ProjectId, name: &str) -> AgentVerbs {
    AgentVerbs::new(
        database.into(),
        project.clone(),
        name.into(),
        SessionId(name.into()),
        None,
    )
}

/// Every read form this word surface offers, named for failure messages.
fn every_read(
    verbs: &AgentVerbs,
    work: &str,
    locator: &str,
    second: i64,
) -> Vec<(String, Receipt)> {
    let now = at(second);
    let mut reads = Vec::new();
    for (peek, verbose) in [(false, false), (false, true), (true, false), (true, true)] {
        reads.push((
            format!("next peek={peek} verbose={verbose}"),
            verbs
                .next(
                    &NextInput {
                        peek,
                        verbose,
                        ..NextInput::default()
                    },
                    now,
                )
                .unwrap(),
        ));
    }
    reads.push(("ls".into(), verbs.ls(&LsInput::default(), now).unwrap()));
    reads.push(("search".into(), verbs.search("Lapsing", None, now).unwrap()));
    reads.push(("show".into(), verbs.show(work, now).unwrap()));
    for (name, input) in [
        (
            "show --notes",
            ShowInput {
                notes: true,
                ..ShowInput::default()
            },
        ),
        (
            "show --notes --gates",
            ShowInput {
                notes: true,
                gates: true,
                ..ShowInput::default()
            },
        ),
        (
            "show --history",
            ShowInput {
                history: true,
                ..ShowInput::default()
            },
        ),
        (
            "show --note",
            ShowInput {
                note: Some(locator.into()),
                ..ShowInput::default()
            },
        ),
        (
            "show --full",
            ShowInput {
                full: true,
                ..ShowInput::default()
            },
        ),
        (
            "show --evaluations",
            ShowInput {
                evaluations: true,
                ..ShowInput::default()
            },
        ),
        (
            "show --observations",
            ShowInput {
                observations: true,
                ..ShowInput::default()
            },
        ),
    ] {
        reads.push((name.into(), verbs.show_records(work, &input, now).unwrap()));
    }
    for (name, input) in [
        ("memories", MemoriesInput::default()),
        (
            "memories QUERY",
            MemoriesInput {
                query: Some("lapse".into()),
                ..MemoriesInput::default()
            },
        ),
        (
            "memories KEY --full",
            MemoriesInput {
                query: Some("lapse-key".into()),
                full: true,
                ..MemoriesInput::default()
            },
        ),
        (
            "memories --after",
            MemoriesInput {
                after: Some("a".into()),
                ..MemoriesInput::default()
            },
        ),
        (
            "memories --context-generation",
            MemoriesInput {
                context_generation: Some("lapse-generation".into()),
                ..MemoriesInput::default()
            },
        ),
    ] {
        reads.push((name.into(), verbs.memories(&input, now).unwrap()));
    }
    reads
}

/// The claim and focus a read must leave exactly as they were.
fn claim_state(
    database: &std::path::Path,
    project: &ProjectId,
    work: &str,
    session: &str,
) -> (Option<crate::WorkClaim>, Option<crate::WorkId>) {
    let store = SqliteStore::open(database).unwrap();
    let item = store.resolve_work_ref(project, work).unwrap();
    (
        store.current_work_claim(item.work_id).unwrap(),
        store
            .work_session_state(project, &SessionId(session.into()), at(0))
            .unwrap()
            .focused_work_id,
    )
}

#[test]
fn a_lapsed_own_claim_is_named_by_every_read_and_nothing_renews_it() {
    let (_directory, holder, database, project) = fixture();
    let work = add(&holder, "Lapsing work", None, false, 0);
    holder
        .remember(
            serde_json::from_value(json!({"text": "A lapse note", "key": "lapse-key"})).unwrap(),
            at(1),
        )
        .unwrap();
    hold(&holder, &work, 60, 2);
    note(&holder, &work, "Progress before the lapse", 3);
    let shown = holder
        .show_records(
            &work,
            &ShowInput {
                notes: true,
                ..ShowInput::default()
            },
            at(4),
        )
        .unwrap();
    let locator = shown.value["notes"][0]["locator"]
        .as_str()
        .unwrap()
        .to_owned();
    let expected = lapse(&work);
    let before = claim_state(&database, &project, &work, "agent");
    // The note renewed the claim; read its expiry rather than assume it.
    let expiry = before.0.as_ref().unwrap().expires_at.timestamp() - at(0).timestamp();
    // Live claim: no read mentions a lapse.
    for (name, receipt) in every_read(&holder, &work, &locator, expiry - 1) {
        assert!(!receipt.reminders.contains(&expected), "{name}");
    }
    // At the exact expiry and after it, every read names the lapse and the
    // renewal, in its structured reminders and its text, and changes nothing.
    for second in [expiry, expiry + 500] {
        for (name, receipt) in every_read(&holder, &work, &locator, second) {
            assert!(receipt.reminders.contains(&expected), "{name} at {second}");
            assert!(
                receipt.value["reminders"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(expected)),
                "{name} at {second}"
            );
            assert!(receipt.text().contains(&expected), "{name} at {second}");
            assert!(
                emitted_receipt_bytes(&receipt) < MAX_AGENT_WORK_RESPONSE_BYTES
                    // Only the explicit complete reads are unbounded.
                    || ["show --note", "show --full"].contains(&name.as_str()),
                "{name} at {second}"
            );
            assert_eq!(
                claim_state(&database, &project, &work, "agent"),
                before,
                "{name} renewed or moved something"
            );
        }
    }
    // A session that never held the work sees no such reminder.
    let other = session(&database, &project, "bystander");
    for (name, receipt) in every_read(&other, &work, &locator, expiry + 500) {
        assert!(
            !receipt
                .reminders
                .iter()
                .any(|reminder| reminder.contains("lapsed")),
            "{name}"
        );
    }
    // Only a claim renews it; then the reminder goes away.
    hold(&holder, &work, 600, expiry + 501);
    for (name, receipt) in every_read(&holder, &work, &locator, expiry + 502) {
        assert!(!receipt.reminders.contains(&expected), "{name}");
    }
}

#[test]
fn a_released_ended_or_taken_over_claim_is_not_a_lapse() {
    let (_directory, holder, database, project) = fixture();
    let quiet = |verbs: &AgentVerbs, second| {
        let receipt = verbs.next(&NextInput::default(), at(second)).unwrap();
        !receipt
            .reminders
            .iter()
            .any(|reminder| reminder.contains("lapsed"))
    };
    let released = add(&holder, "Released", None, false, 0);
    hold(&holder, &released, 60, 1);
    note(&holder, &released, "Some contribution", 2);
    holder
        .update(
            UpdateInput {
                work_ref: Some(released.clone()),
                action: UpdateAction::Release { reason: None },
            },
            at(3),
        )
        .unwrap();
    assert!(quiet(&holder, 100));
    let cancelled = add(&holder, "Cancelled", None, false, 101);
    hold(&holder, &cancelled, 60, 102);
    holder
        .update(
            UpdateInput {
                work_ref: Some(cancelled.clone()),
                action: UpdateAction::Cancel {
                    reason: "No longer needed".into(),
                },
            },
            at(103),
        )
        .unwrap();
    assert!(quiet(&holder, 200));
    let taken = add(&holder, "Taken over", None, false, 201);
    hold(&holder, &taken, 60, 202);
    assert!(!quiet(&holder, 300));
    let successor = session(&database, &project, "successor");
    successor
        .claim(
            ClaimInput {
                work_ref: taken.clone(),
                ttl_seconds: Some(600),
                recover: Some("The earlier holder lapsed".into()),
            },
            at(301),
        )
        .unwrap();
    // Neither the former holder nor the live successor is told of a lapse.
    assert!(quiet(&holder, 302));
    assert!(quiet(&successor, 302));
}

#[test]
fn the_reminder_prefers_the_focused_lapse_then_the_latest_expiry() {
    let (_directory, holder, _database, _project) = fixture();
    let first = add(&holder, "First", None, false, 0);
    let second = add(&holder, "Second", None, false, 1);
    hold(&holder, &first, 120, 2);
    // The later claim takes the focus and lapses first.
    hold(&holder, &second, 60, 3);
    let named = |second_at| {
        holder
            .next(&NextInput::default(), at(second_at))
            .unwrap()
            .reminders
    };
    assert!(named(200).contains(&lapse(&second)));
    assert!(!named(200).contains(&lapse(&first)));
    // With the focus on live work, the lapse with the latest expiry is named.
    let live = add(&holder, "Live", None, false, 201);
    hold(&holder, &live, 600, 202);
    assert!(named(203).contains(&lapse(&first)));
    assert!(!named(203).contains(&lapse(&second)));
}

#[test]
fn a_full_notes_window_still_fits_with_the_reminder() {
    let (_directory, holder, _database, _project) = fixture();
    let work = add(&holder, "Busy work", None, false, 0);
    hold(&holder, &work, 600, 1);
    for index in 0..40 {
        note(
            &holder,
            &work,
            &format!("{index} {}", "x".repeat(700)),
            2 + index,
        );
    }
    let shown = holder
        .show_records(
            &work,
            &ShowInput {
                notes: true,
                ..ShowInput::default()
            },
            // Notes renew the claim, so read well after the last renewal.
            at(100_000),
        )
        .unwrap();
    assert!(shown.reminders.contains(&lapse(&work)));
    assert!(emitted_receipt_bytes(&shown) < MAX_AGENT_WORK_RESPONSE_BYTES);
    assert!(shown.value["notes_omitted"].as_u64().unwrap() > 0);
}

// The reminder and the read it rides on see one snapshot: a takeover that
// commits between the lookup and the read does not mix their cuts.
#[test]
fn the_reminder_and_its_read_share_one_snapshot() {
    let (_directory, holder, database, project) = fixture();
    let work = add(&holder, "Contested work", None, false, 0);
    hold(&holder, &work, 60, 1);
    let lapsed = claim_state(&database, &project, &work, "agent")
        .0
        .unwrap()
        .expires_at
        .timestamp()
        - at(0).timestamp()
        + 10;
    let successor = session(&database, &project, "successor");
    let receipt = holder
        .with_claim_lapse(at(lapsed), MAX_AGENT_WORK_RESPONSE_BYTES, |budget| {
            successor
                .claim(
                    ClaimInput {
                        work_ref: work.clone(),
                        ttl_seconds: Some(600),
                        recover: Some("The earlier holder lapsed".into()),
                    },
                    at(lapsed),
                )
                .expect("takeover between the lookup and the read");
            holder.show_within(&work, at(lapsed), budget)
        })
        .unwrap();
    // Both parts come from the cut before the takeover.
    assert!(receipt.reminders.contains(&lapse(&work)));
    assert!(receipt.value.get("holder").is_none());
    // A fresh read sees the takeover, and no lapse of this session.
    let after = holder.show(&work, at(lapsed)).unwrap();
    assert!(after.value.get("holder").is_some());
    assert!(!after.reminders.contains(&lapse(&work)));
}

// A full memory admitted at its size limit still answers its full read with
// the reminder inside the full-read bound, because admission reserves room.
#[test]
fn a_full_memory_at_its_admitted_limit_fits_with_the_reminder() {
    let (_directory, holder, _database, _project) = fixture();
    let work = add(&holder, "Lapsing", None, false, 0);
    hold(&holder, &work, 60, 1);
    let remember = |length: usize| {
        holder.remember(
            serde_json::from_value(json!({
                "text": "\"".repeat(length),
                "key": format!("limit-{length}"),
            }))
            .unwrap(),
            at(2),
        )
    };
    // The longest quote-only body admission accepts: each quote doubles in
    // JSON, so the envelope reaches its limit well before the body limit.
    let (mut low, mut high) = (1, crate::domain::MAX_PROJECT_MEMORY_BODY_BYTES);
    while low < high {
        let middle = (low + high).div_ceil(2);
        if remember(middle).is_ok() {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    let full = holder
        .memories(
            &MemoriesInput {
                query: Some(format!("limit-{low}")),
                full: true,
                ..MemoriesInput::default()
            },
            at(10_000),
        )
        .unwrap();
    assert!(full.reminders.contains(&lapse(&work)));
    assert!(serde_json::to_vec(&full.value).unwrap().len() <= 12 * 1024);
    assert!(full.text().len() <= 12 * 1024);
}

// The lookup is advisory: when it cannot read this session's claim, a read
// still answers and says the check was unavailable.
#[test]
fn a_failed_lapse_check_leaves_the_read_answering() {
    let (_directory, holder, database, project) = fixture();
    let work = add(&holder, "Unreadable claim", None, false, 0);
    hold(&holder, &work, 60, 1);
    let store = SqliteStore::open(&database).unwrap();
    let item = store.resolve_work_ref(&project, &work).unwrap();
    drop(store);
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute(
            "UPDATE work_claims SET claim_json = x'00' WHERE work_id = ?1",
            [item.work_id.0.to_string()],
        )
        .unwrap();
    drop(connection);
    let listed = holder
        .memories(&MemoriesInput::default(), at(10_000))
        .expect("the read answers");
    assert!(
        listed
            .reminders
            .contains(&crate::work_service::CLAIM_LAPSE_UNAVAILABLE.to_owned())
    );
}

/// Writes, through the admission in force before the reminder reserve, the
/// largest body of repeated `unit` it accepts, and returns its key and body.
/// That admission: the full response, current and with worst-case history
/// navigation, fits 12 KiB under either argument spelling.
fn legacy_at_limit(
    database: &std::path::Path,
    project: &ProjectId,
    unit: &str,
    tag: &str,
) -> (String, String) {
    let legacy = |full: &crate::domain::ProjectMemoryFull, _| {
        let mut historical = full.clone();
        historical.revision = u64::MAX - 1;
        historical.current_revision = u64::MAX;
        for names in [
            crate::argument_names::ArgumentNames::Cli,
            crate::argument_names::ArgumentNames::Mcp,
        ] {
            crate::work_service::project_memory_full_response(full.clone(), names)?;
            crate::work_service::project_memory_full_response(historical.clone(), names)?;
        }
        Ok(())
    };
    let mut store = SqliteStore::open(database).unwrap();
    let mut remember = |count: usize| {
        store.remember_project_memory_with_admission(
            &crate::domain::RememberProjectMemoryRequest {
                project_id: project.clone(),
                session_id: SessionId("agent".into()),
                key: Some(format!("{tag}-{count}")),
                revise: false,
                expected_revision: None,
                body: unit.repeat(count),
                retiring_target: crate::domain::ProjectMemoryRetiringTargetChange::Keep,
                actor: crate::ActorContext {
                    actor_id: "agent".into(),
                    actor_kind: "agent".into(),
                    assurance: crate::domain::AssuranceLevel::Asserted,
                    run_id: None,
                    session_id: Some(SessionId("agent".into())),
                    source_tool: None,
                    source_skill: None,
                    provenance_chain: Vec::new(),
                    reason: "a memory admitted before the reserve".into(),
                },
                created_at: at(2),
            },
            &crate::DevelopmentNoopRedactor,
            legacy,
        )
    };
    let (mut low, mut high) = (1, crate::domain::MAX_PROJECT_MEMORY_BODY_BYTES / unit.len());
    while low < high {
        let middle = (low + high).div_ceil(2);
        if remember(middle).is_ok() {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    (format!("{tag}-{low}"), unit.repeat(low))
}

/// A legacy full read: it carries the reminder and stays within 12 KiB plus
/// the reserve in JSON and in text. Returns its JSON and text sizes.
fn assert_within_reserve(receipt: &Receipt, work: &str) -> (usize, usize) {
    assert!(receipt.reminders.contains(&lapse(work)));
    let bound = 12 * 1024 + crate::work_service::READ_REMINDER_RESERVE;
    let json = serde_json::to_vec(&receipt.value).unwrap().len();
    let text = receipt.text().len();
    assert!(json <= bound, "{json}");
    assert!(text <= bound, "{text}");
    (json, text)
}

fn full_read(verbs: &AgentVerbs, key: &str, revision: Option<u64>) -> Receipt {
    verbs
        .memories(
            &MemoriesInput {
                query: Some(key.into()),
                full: true,
                revision,
                ..MemoriesInput::default()
            },
            at(10_000),
        )
        .unwrap()
}

// A memory admitted before admission reserved the reminder's room, at the old
// limit, still reads back in full WITH the reminder: past 12 KiB by at most
// the reserve, under either argument spelling, whichever surface binds. It
// can still be retried exactly and revised, and its stored revision reads
// back the same way.
#[test]
fn a_legacy_full_memory_at_the_old_limit_carries_the_reminder_within_the_reserve() {
    let (_directory, holder, database, project) = fixture();
    let work = add(&holder, "Lapsing", None, false, 0);
    hold(&holder, &work, 60, 1);
    let mcp = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    )
    .with_mcp_argument_names();
    // Quotes double in JSON, so JSON binds. (The worst-case history
    // navigation that old admission reserved usually absorbs the reminder; the
    // bound is what a legacy row may never exceed.)
    let (quotes, body) = legacy_at_limit(&database, &project, "\"", "quotes");
    let (json, _) = assert_within_reserve(&full_read(&holder, &quotes, None), &work);
    assert!(
        json > 12 * 1024 - crate::work_service::READ_REMINDER_RESERVE,
        "{json}"
    );
    assert_within_reserve(&full_read(&mcp, &quotes, None), &work);
    // Newlines grow most in text, so text binds there.
    let (lines, _) = legacy_at_limit(&database, &project, "x\n", "lines");
    let (json, text) = assert_within_reserve(&full_read(&holder, &lines, None), &work);
    assert!(text > json, "{text} {json}");
    assert_within_reserve(&full_read(&mcp, &lines, None), &work);
    // Its stored version does not block an exact retry or a revision.
    let retry = holder
        .remember(
            serde_json::from_value(json!({"text": body, "key": quotes})).unwrap(),
            at(3),
        )
        .expect("an exact retry replays the legacy version");
    assert_eq!(retry.value["duplicate"], true);
    holder
        .remember(
            serde_json::from_value(json!({"text": "short", "key": quotes, "revise": true}))
                .unwrap(),
            at(4),
        )
        .expect("a legacy memory can be revised");
    assert_within_reserve(&full_read(&holder, &quotes, Some(1)), &work);
    assert_within_reserve(&full_read(&mcp, &quotes, Some(1)), &work);
}

// The reserve covers every reminder a read can carry, in JSON and in text.
#[test]
fn the_read_reminder_reserve_covers_every_reminder() {
    let longest = lapse("w-ffffffffffff");
    for reminder in [
        longest,
        crate::work_service::CLAIM_LAPSE_UNAVAILABLE.to_owned(),
    ] {
        let json = serde_json::to_vec(&json!([reminder])).unwrap().len();
        assert!(json + 16 <= crate::work_service::READ_REMINDER_RESERVE);
        assert!(
            crate::verbs::claim_lapse::reserve(Some(&reminder))
                <= crate::work_service::READ_REMINDER_RESERVE
        );
    }
}
