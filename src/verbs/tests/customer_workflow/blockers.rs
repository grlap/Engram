//! Several active blockers on one item: `show` gives each a selector, its
//! kind and detail and the exact command that clears it; that command clears
//! only its own blocker, history names what was cleared, and a selection
//! that does not name an active blocker of the item changes nothing.

use super::*;

fn block(verbs: &AgentVerbs, work: &str, detail: &str, now: i64) {
    verbs
        .update(
            UpdateInput {
                work_ref: Some(work.into()),
                action: UpdateAction::Blocked {
                    detail: detail.into(),
                },
            },
            at(now),
        )
        .expect("block");
}

fn block_as(verbs: &AgentVerbs, work: &str, kind: crate::WorkBlockerKind, detail: &str, now: i64) {
    verbs
        .service
        .work_update_on(
            Some(work),
            WorkUpdateInput::Block {
                blocker_kind: kind,
                detail: detail.into(),
                idempotency_key: String::new(),
            },
            at(now),
        )
        .expect("block with a kind");
}

fn unblock(
    verbs: &AgentVerbs,
    work: &str,
    blocker: Option<&str>,
    now: i64,
) -> Result<Receipt, crate::verbs::VerbError> {
    verbs.update(
        UpdateInput {
            work_ref: Some(work.into()),
            action: UpdateAction::Unblock {
                blocker: blocker.map(str::to_owned),
            },
        },
        at(now),
    )
}

/// The selectors `show` prints, in its order.
fn selectors(shown: &Receipt) -> Vec<String> {
    shown.value["blockers"]
        .as_array()
        .expect("blockers")
        .iter()
        .map(|blocker| blocker["blocker"].as_str().expect("selector").to_owned())
        .collect()
}

/// The item's state an invalid selection must leave alone: its active
/// selectors, its revision and its history length.
fn state(verbs: &AgentVerbs, work: &str, now: i64) -> (Vec<String>, i64, u64) {
    let shown = verbs.show(work, at(now)).expect("show");
    let revision = verbs
        .service
        .inspect_work(work, at(now))
        .expect("inspect")
        .status
        .work
        .revision;
    (
        selectors(&shown),
        revision,
        shown.value["history"]["total"]
            .as_u64()
            .expect("history total"),
    )
}

/// Runs a printed `engram work update REF --unblock --blocker SELECTOR`
/// command exactly as printed, through the word it names.
fn run_printed(verbs: &AgentVerbs, command: &str, now: i64) -> Receipt {
    let words = command.split(' ').collect::<Vec<_>>();
    let [
        "engram",
        "work",
        "update",
        work,
        "--unblock",
        "--blocker",
        selector,
    ] = words[..]
    else {
        panic!("not an exact unblock command: {command}");
    };
    unblock(verbs, work, Some(selector), now).expect("printed command")
}

fn history_summaries(shown: &Receipt, kind: &str) -> Vec<String> {
    shown.value["history"]["items"]
        .as_array()
        .expect("history")
        .iter()
        .filter(|item| item["kind"] == kind)
        .map(|item| item["summary"].as_str().expect("summary").to_owned())
        .collect()
}

#[test]
fn the_printed_command_clears_its_own_blocker_and_history_names_it() {
    let (_directory, verbs, _path, _project) = fixture();
    let work = add(&verbs, "Wait on three things", None, false, 0);
    // The same text twice with one kind, and once more with another kind:
    // only the selector tells them apart.
    block(&verbs, &work, "Await the release", 1);
    block_as(
        &verbs,
        &work,
        crate::WorkBlockerKind::HumanDecision,
        "Await the release",
        2,
    );
    block(&verbs, &work, "Await the release", 3);

    let shown = verbs.show(&work, at(4)).expect("show");
    assert_eq!(shown.value["blockers_total"], 3);
    assert!(shown.value.get("blockers_omitted").is_none());
    let blockers = shown.value["blockers"]
        .as_array()
        .expect("blockers")
        .clone();
    assert_eq!(blockers.len(), 3);
    let text = shown.text();
    assert!(text.contains("blockers: 3 active"), "{text}");
    for blocker in &blockers {
        let selector = blocker["blocker"].as_str().expect("selector");
        assert!(selector.starts_with("b1-"), "{selector}");
        assert_eq!(blocker["detail"], "Await the release");
        let command = format!("engram work update {work} --unblock --blocker {selector}");
        assert_eq!(blocker["unblock"], command.as_str());
        assert!(text.contains(&format!("    clear: {command}")), "{text}");
    }
    assert_eq!(blockers[1]["kind"], "human_decision");
    // Several blockers: guidance reads them rather than guessing one.
    assert!(
        !shown
            .next
            .iter()
            .any(|command| command.contains("--unblock")),
        "{:?}",
        shown.next
    );

    // The second printed action clears the second blocker only.
    let second = blockers[1]["blocker"].as_str().expect("second").to_owned();
    let cleared = run_printed(&verbs, blockers[1]["unblock"].as_str().expect("command"), 5);
    assert_eq!(cleared.value["cleared_blocker"], second.as_str());
    assert_eq!(cleared.value["blockers_remaining"], 2);
    assert!(
        cleared.text().contains(&format!(
            "cleared blocker {second} (human_decision) \"Await the release\"; 2 active blocker(s) remain"
        )),
        "{}",
        cleared.text()
    );
    let first = blockers[0]["blocker"].as_str().expect("first").to_owned();
    let third = blockers[2]["blocker"].as_str().expect("third").to_owned();
    let after = verbs.show(&work, at(6)).expect("show after");
    assert_eq!(selectors(&after), [first.clone(), third.clone()]);
    assert_eq!(after.value["blockers_total"], 2);

    // The third of the identical manual blockers, by its selector alone.
    run_printed(
        &verbs,
        after.value["blockers"][1]["unblock"]
            .as_str()
            .expect("third command"),
        7,
    );
    let last = verbs.show(&work, at(8)).expect("show last");
    assert_eq!(selectors(&last), std::slice::from_ref(&first));
    // One blocker left: guidance offers the exact command that clears it.
    let only = format!("engram work update {work} --unblock --blocker {first}");
    assert!(last.next.contains(&only), "{:?}", last.next);

    // History names each cleared blocker by the same selector, with the kind
    // and detail it was raised with.
    let mut cleared_history = history_summaries(&last, "unblocked");
    cleared_history.sort();
    let mut expected = vec![
        format!("cleared blocker {second} (human_decision) \"Await the release\""),
        format!("cleared blocker {third} (manual) \"Await the release\""),
    ];
    expected.sort();
    assert_eq!(cleared_history, expected);
    // Raising a blocker is named the same way; the bounded history window
    // shows the latest ones.
    let blocked = history_summaries(&last, "blocked");
    assert!(
        blocked.contains(&format!(
            "blocker {second} (human_decision) \"Await the release\""
        )),
        "{blocked:?}"
    );
    assert!(
        blocked
            .iter()
            .all(|summary| summary.starts_with("blocker b1-")),
        "{blocked:?}"
    );
}

#[test]
fn a_selection_that_names_no_active_blocker_of_the_item_changes_nothing() {
    let (_directory, verbs, path, project) = fixture();
    let peer = AgentVerbs::new(path, project, "peer".into(), SessionId("peer".into()), None);
    let work = add(&verbs, "Wait on two things", None, false, 0);
    let other = add(&verbs, "Another blocked item", None, false, 1);
    block(&verbs, &work, "Await the first", 2);
    block(&verbs, &work, "Await the second", 3);
    block(&verbs, &other, "Await elsewhere", 4);
    let foreign = selectors(&verbs.show(&other, at(5)).expect("other"))[0].clone();
    let [first, second] = selectors(&verbs.show(&work, at(5)).expect("show"))
        .try_into()
        .expect("two");

    let refuse = |verbs: &AgentVerbs, selector: &str, words: &str, now: i64| {
        let before = state(verbs, &work, now);
        let error = unblock(verbs, &work, Some(selector), now).expect_err(selector);
        let message = verbs.error_message(&error);
        assert!(message.contains(words), "{selector:?}: {message}");
        if words == "unknown blocker" {
            assert!(message.contains("selector"), "{message}");
            assert_eq!(
                verbs.error_guidance(&error).next,
                [format!("engram work show {work}")]
            );
        }
        assert_eq!(state(verbs, &work, now), before, "{selector:?}");
    };
    refuse(&verbs, "   ", "empty", 6);
    refuse(&verbs, "b1-!!", "not a blocker selector", 7);
    refuse(&verbs, &format!("{first}="), "not a blocker selector", 8);
    refuse(
        &verbs,
        &first.replacen("b1-", "b2-", 1),
        "not a blocker selector",
        9,
    );
    refuse(&verbs, &foreign, "unknown blocker", 10);
    assert_eq!(
        selectors(&verbs.show(&other, at(11)).expect("other")),
        [foreign]
    );

    // A blocker a peer cleared, and one replaced by a new blocker with the
    // same text, are no longer active: their old selectors never clear the
    // remaining blocker.
    unblock(&peer, &work, Some(&first), 12).expect("peer clears the first");
    block(&verbs, &work, "Await the first", 13);
    let replaced = selectors(&verbs.show(&work, at(14)).expect("show"));
    assert_eq!(replaced.len(), 2);
    assert!(replaced.contains(&second));
    assert!(!replaced.contains(&first));
    refuse(&verbs, &first, "unknown blocker", 15);
    assert_eq!(
        selectors(&verbs.show(&work, at(16)).expect("show")),
        replaced
    );
}

#[test]
fn the_same_selected_command_repeated_clears_once_and_answers_the_same() {
    let (_directory, verbs, _path, _project) = fixture();
    let work = add(&verbs, "Wait on two things", None, false, 0);
    block(&verbs, &work, "Await the first", 1);
    block(&verbs, &work, "Await the second", 2);
    let [first, second] = selectors(&verbs.show(&work, at(3)).expect("show"))
        .try_into()
        .expect("two");
    let answered = unblock(&verbs, &work, Some(&first), 4).expect("clear");
    let history = state(&verbs, &work, 5).2;
    // The answer was lost; the same command is sent again after another
    // blocker was raised.
    block(&verbs, &work, "Await the third", 6);
    let repeated = unblock(&verbs, &work, Some(&first), 7).expect("repeat");
    assert_eq!(
        repeated.value["cleared_blocker"],
        answered.value["cleared_blocker"]
    );
    assert_eq!(repeated.value["revision"], answered.value["revision"]);
    // The repeated answer still names the cleared blocker's kind and detail:
    // they come from the committed clear, not from the active list.
    for receipt in [&answered, &repeated] {
        assert!(
            receipt.text().contains(&format!(
                "cleared blocker {first} (manual) \"Await the first\""
            )),
            "{}",
            receipt.text()
        );
    }
    let (active, _, total) = state(&verbs, &work, 8);
    assert_eq!(total, history + 1, "one more event: the third block only");
    assert_eq!(active.len(), 2);
    assert!(active.contains(&second));
    assert!(!active.contains(&first));
    assert_eq!(
        history_summaries(&verbs.show(&work, at(9)).expect("show"), "unblocked").len(),
        1
    );
}

#[test]
fn a_clear_recorded_before_selectors_reads_with_its_blocker_and_its_bytes_unchanged() {
    let (_directory, verbs, path, _project) = fixture();
    let work = add(&verbs, "Wait once", None, false, 0);
    block(&verbs, &work, "Await the vendor", 1);
    let selector = selectors(&verbs.show(&work, at(2)).expect("show"))[0].clone();
    // A bare unblock writes the clear event every earlier build wrote: it
    // names only the blocker's id.
    unblock(&verbs, &work, None, 3).expect("bare unblock");
    let stored = || -> Vec<Vec<u8>> {
        let connection = rusqlite::Connection::open(&path).expect("store");
        let mut statement = connection
            .prepare(
                "SELECT canonical_json FROM objects WHERE object_kind = 'work_event' ORDER BY object_id",
            )
            .expect("events");
        statement
            .query_map([], |row| row.get(0))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("bytes")
    };
    let before = stored();
    let shown = verbs.show(&work, at(4)).expect("show");
    assert_eq!(
        history_summaries(&shown, "unblocked"),
        [format!(
            "cleared blocker {selector} (manual) \"Await the vendor\""
        )]
    );
    assert_eq!(stored(), before);
    assert!(shown.value.get("blockers_total").is_none());
    assert_eq!(shown.value["blockers"], serde_json::json!([]));
}

#[test]
fn a_bare_unblock_receipt_names_the_blocker_its_own_clear_removed() {
    let (_directory, verbs, path, project) = fixture();
    let work = add(&verbs, "Wait in turn", None, false, 0);
    block(&verbs, &work, "Await the first", 1);
    let first = selectors(&verbs.show(&work, at(2)).expect("show"))[0].clone();
    let cleared_first = unblock(&verbs, &work, None, 3).expect("clear the first");
    assert_eq!(cleared_first.value["cleared_blocker"], first.as_str());
    block(&verbs, &work, "Await the second", 4);
    let second = selectors(&verbs.show(&work, at(5)).expect("show"))[0].clone();
    let cleared_second = unblock(&verbs, &work, None, 6).expect("clear the second");
    assert_eq!(cleared_second.value["cleared_blocker"], second.as_str());
    assert!(
        cleared_second.text().contains(&format!(
            "cleared blocker {second} (manual) \"Await the second\"; 0 active blocker(s) remain"
        )),
        "{}",
        cleared_second.text()
    );
    // Each clear is found by the revision it produced, not by what was
    // active before it: the receipt cannot name a blocker another clear
    // removed.
    let store = SqliteStore::open(&path).expect("store");
    let item = store.resolve_work_ref(&project, &work).expect("item");
    for (receipt, selector) in [(&cleared_first, &first), (&cleared_second, &second)] {
        let revision = receipt.value["receipt"]["revision"]
            .as_i64()
            .or_else(|| receipt.value["revision"].as_i64())
            .expect("revision");
        let cleared = store
            .blocker_cleared_at(item.work_id, revision)
            .expect("read")
            .expect("a clear at that revision");
        assert_eq!(
            &crate::work_service::blocker_selector::encode(&cleared.blocker_id),
            selector
        );
        assert_eq!(
            store
                .blocker_cleared_at(item.work_id, revision - 1)
                .expect("read"),
            None
        );
    }
}

#[test]
fn more_blockers_than_show_lists_are_counted_and_each_listed_row_is_whole() {
    let (_directory, verbs, _path, _project) = fixture();
    let work = add(&verbs, "Wait on many things", None, false, 0);
    for index in 0..10 {
        block(&verbs, &work, &format!("Await part {index}"), 1 + index);
    }
    let shown = verbs.show(&work, at(20)).expect("show");
    let listed = shown.value["blockers"].as_array().expect("blockers");
    assert_eq!(listed.len(), crate::work_service::MAX_FOCUS_RELATIONS);
    assert_eq!(shown.value["blockers_total"], 10);
    assert_eq!(
        shown.value["blockers_omitted"],
        10 - crate::work_service::MAX_FOCUS_RELATIONS
    );
    let text = shown.text();
    assert!(
        text.contains(&format!(
            "blockers: 10 active, {} not shown",
            10 - crate::work_service::MAX_FOCUS_RELATIONS
        )),
        "{text}"
    );
    for row in listed {
        let selector = row["blocker"].as_str().expect("selector");
        assert!(
            crate::work_service::blocker_selector::decode(selector).is_some(),
            "{selector}"
        );
        let command = format!("engram work update {work} --unblock --blocker {selector}");
        assert_eq!(row["unblock"], command.as_str());
        assert!(text.contains(&format!("    clear: {command}")), "{text}");
    }
}

#[test]
fn a_reader_who_cannot_unblock_still_sees_each_exact_command() {
    let (_directory, holder, path, project) = fixture();
    let reader = AgentVerbs::new(
        path,
        project,
        "reader".into(),
        SessionId("reader".into()),
        None,
    );
    let work = add(&holder, "Held and blocked", None, false, 0);
    holder
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: Some(3600),
                recover: None,
            },
            at(1),
        )
        .expect("claim");
    block(&holder, &work, "Await the first", 2);
    block(&holder, &work, "Await the second", 3);
    // Another session's live claim leaves the reader no planning authority,
    // yet show still prints each blocker's exact command; running it is what
    // admits or refuses the clear.
    let shown = reader.show(&work, at(4)).expect("show");
    let text = shown.text();
    let blockers = shown.value["blockers"].as_array().expect("blockers");
    assert_eq!(blockers.len(), 2);
    for blocker in blockers {
        let selector = blocker["blocker"].as_str().expect("selector");
        let command = format!("engram work update {work} --unblock --blocker {selector}");
        assert_eq!(blocker["unblock"], command.as_str());
        assert!(text.contains(&format!("    clear: {command}")), "{text}");
    }
    let first = blockers[0]["blocker"].as_str().expect("first").to_owned();
    let before = state(&reader, &work, 5);
    unblock(&reader, &work, Some(&first), 6).expect_err("the holder's claim refuses the reader");
    assert_eq!(state(&reader, &work, 7), before);
    unblock(&holder, &work, Some(&first), 8).expect("the holder clears it");
}
