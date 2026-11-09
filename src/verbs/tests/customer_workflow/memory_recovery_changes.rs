//! What a session learns about project memories after a context refresh: the
//! keys changed since its recorded position, a compact listing that reaches
//! every key in a few reads, and the plain listing left as it was.

use super::*;

fn peek(verbs: &AgentVerbs, generation: Option<&str>, now: i64) -> Receipt {
    verbs
        .next(
            &NextInput {
                peek: true,
                context_generation: generation.map(str::to_owned),
                ..NextInput::default()
            },
            at(now),
        )
        .expect("peek")
}

fn remember(verbs: &AgentVerbs, key: &str, body: &str, now: i64) {
    verbs
        .service
        .remember_project_memory(body.into(), Some(key.into()), false, None, at(now))
        .expect("remember");
}

fn revise(verbs: &AgentVerbs, key: &str, revision: u64, now: i64) {
    verbs
        .service
        .remember_project_memory(
            format!("Rule {key}, revision {}.", revision + 1),
            Some(key.into()),
            true,
            Some(revision),
            at(now),
        )
        .expect("revise");
}

fn forget(verbs: &AgentVerbs, key: &str, now: i64) {
    verbs
        .service
        .forget_project_memory(key.into(), at(now))
        .expect("forget");
}

/// Reads the listing a peek directs, from its first page through every
/// continuation it names, returning the keys in order and the reads it took.
fn recover(verbs: &AgentVerbs, generation: &str, now: i64) -> (Vec<String>, usize) {
    let mut keys = Vec::new();
    let mut after = None;
    let mut reads = 0;
    loop {
        let receipt = verbs
            .memories(
                &MemoriesInput {
                    after: after.clone(),
                    context_generation: Some(generation.into()),
                    ..MemoriesInput::default()
                },
                at(now),
            )
            .expect("recovery listing");
        reads += 1;
        assert!(
            receipt.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES,
            "a page fits the response budget"
        );
        for row in receipt.value["memories"].as_array().expect("rows") {
            keys.push(row["key"].as_str().expect("key").to_owned());
        }
        let prefix = format!("engram work memories --context-generation {generation} --after ");
        let Some(next) = receipt
            .next
            .iter()
            .find_map(|command| command.strip_prefix(&prefix))
        else {
            assert_eq!(receipt.value["exhausted"], true);
            return (keys, reads);
        };
        after = Some(next.to_owned());
    }
}

fn changed_keys(receipt: &Receipt) -> Vec<(String, bool)> {
    receipt.value["memories"]["changes"]["keys"]
        .as_array()
        .expect("named changes")
        .iter()
        .map(|change| {
            (
                change["key"].as_str().unwrap().to_owned(),
                change["forgotten"].as_bool().unwrap_or(false),
            )
        })
        .collect()
}

fn memories_line(receipt: &Receipt) -> String {
    receipt
        .text()
        .lines()
        .find(|line| line.starts_with("memories: "))
        .expect("memories line")
        .to_owned()
}

#[test]
fn a_peek_names_the_keys_changed_since_the_sessions_listing_newest_first() {
    let (_home, reader, path, project) = fixture();
    for (time, key) in ["alpha", "beta", "gamma", "delta"].into_iter().enumerate() {
        remember(
            &reader,
            key,
            &format!("Rule {key}."),
            i64::try_from(time).unwrap(),
        );
    }
    recover(&reader, "termal-1", 10);
    let peer = AgentVerbs::new(path, project, "peer".into(), SessionId("peer".into()), None);
    revise(&peer, "beta", 1, 11);
    forget(&peer, "gamma", 12);
    remember(&peer, "epsilon", "Rule epsilon.", 13);
    revise(&peer, "beta", 2, 14);

    let announced = peek(&reader, Some("termal-1"), 15);
    let signal = &announced.value["memories"];
    assert_eq!(signal["count"], 4);
    assert_eq!(signal["changed"], true);
    assert_eq!(signal["changes"]["count"], 3);
    assert_eq!(signal["changes"]["forgotten"], 1);
    assert_eq!(
        changed_keys(&announced),
        [
            ("beta".to_owned(), false),
            ("epsilon".to_owned(), false),
            ("gamma".to_owned(), true)
        ]
    );
    let line = memories_line(&announced);
    assert!(line.contains("beta, epsilon, gamma (forgotten)"), "{line}");
    assert!(!line.contains("--after"), "{line}");

    // The recovery listing's first page names the same changes, then records.
    let first = reader
        .memories(
            &MemoriesInput {
                context_generation: Some("termal-1".into()),
                ..MemoriesInput::default()
            },
            at(16),
        )
        .expect("listing");
    assert_eq!(first.value["changes"]["count"], 3);
    assert_eq!(first.value["changes"]["keys"], signal["changes"]["keys"]);
    let settled = peek(&reader, Some("termal-1"), 17);
    assert_eq!(settled.value["memories"]["changed"], false);
    assert!(settled.value["memories"].get("changes").is_none());
}

#[test]
fn more_changes_than_are_named_give_their_count_and_the_listing_command() {
    let (_home, reader, path, project) = fixture();
    let keys = (0..12)
        .map(|index| format!("key-{index:02}"))
        .collect::<Vec<_>>();
    for (time, key) in keys.iter().enumerate() {
        remember(&reader, key, "Rule.", i64::try_from(time).unwrap());
    }
    recover(&reader, "termal-1", 20);
    let peer = AgentVerbs::new(path, project, "peer".into(), SessionId("peer".into()), None);
    for (time, key) in keys.iter().enumerate() {
        let now = 30 + i64::try_from(time).unwrap();
        if time < 3 {
            forget(&peer, key, now);
        } else {
            revise(&peer, key, 1, now);
        }
    }

    let announced = peek(&reader, Some("termal-1"), 50);
    let changes = &announced.value["memories"]["changes"];
    assert_eq!(changes["count"], 12);
    assert_eq!(changes["forgotten"], 3);
    let named = changed_keys(&announced);
    assert_eq!(named.len(), crate::domain::MAX_NAMED_PROJECT_MEMORY_CHANGES);
    assert_eq!(named[0].0, "key-11", "newest first");
    let line = memories_line(&announced);
    assert!(
        line.contains("engram work memories --context-generation termal-1"),
        "{line}"
    );
}

#[test]
fn a_session_with_no_recorded_listing_is_told_so_and_directed_to_list_everything() {
    let (_home, reader, _, _) = fixture();
    remember(&reader, "alpha", "Rule alpha.", 0);
    let first = peek(&reader, Some("termal-1"), 1);
    assert_eq!(first.value["memories"]["changed"], true);
    assert!(first.value["memories"].get("changes").is_none());
    assert_eq!(first.value["peek"]["memory_listing_due"], true);
}

#[test]
fn a_new_context_is_directed_to_list_every_key_even_when_nothing_changed() {
    let (_home, reader, _, _) = fixture();
    remember(&reader, "alpha", "Rule alpha.", 0);
    recover(&reader, "termal-1", 1);

    let refreshed = peek(&reader, Some("termal-2"), 2);
    assert_eq!(refreshed.value["peek"]["memory_listing_due"], true);
    assert_eq!(refreshed.value["memories"]["changes"]["count"], 0);
    assert_eq!(
        refreshed.next.first().map(String::as_str),
        Some("engram work memories --context-generation termal-2")
    );
    let (keys, _) = recover(&reader, "termal-2", 3);
    assert_eq!(keys, ["alpha"]);
    assert_eq!(
        peek(&reader, Some("termal-2"), 4).value["peek"].get("memory_listing_due"),
        None
    );
}

#[test]
fn an_advancing_next_names_the_changes_once() {
    let (_home, reader, path, project) = fixture();
    remember(&reader, "alpha", "Rule alpha.", 0);
    recover(&reader, "termal-1", 1);
    let peer = AgentVerbs::new(path, project, "peer".into(), SessionId("peer".into()), None);
    remember(&peer, "beta", "Rule beta.", 2);

    let shown = reader.next(&NextInput::default(), at(3)).expect("next");
    assert_eq!(changed_keys(&shown), [("beta".to_owned(), false)]);
    let after = reader.next(&NextInput::default(), at(4)).expect("next");
    assert_eq!(after.value["memories"]["changed"], false);
}

#[test]
fn the_recovery_listing_reaches_hundreds_of_memories_in_a_few_reads() {
    let (_home, reader, _, _) = fixture();
    let preview = "Greg, relayed by the assistant with its mailbox and message, verbatim: a rule \
                   that agents follow when they recover context and plan their next work items";
    let mut expected = (0..300)
        .map(|index| format!("rules-greg-topic-{index:03}-2026-11-09"))
        .collect::<Vec<_>>();
    for (time, key) in expected.iter().enumerate() {
        remember(
            &reader,
            key,
            &format!("{preview} ({key})\nMore detail follows."),
            i64::try_from(time).unwrap(),
        );
    }
    expected.sort();

    let (keys, reads) = recover(&reader, "termal-1", 400);
    assert_eq!(keys, expected, "every key once, in key order");
    assert!(reads <= 6, "{reads} reads for 300 memories");
    assert_eq!(
        peek(&reader, Some("termal-1"), 401).value["peek"].get("memory_listing_due"),
        None,
        "the first page recorded the generation"
    );
}

#[test]
fn the_plain_listing_keeps_its_rows_and_page_size() {
    let (_home, reader, _, _) = fixture();
    for index in 0..25 {
        remember(&reader, &format!("key-{index:02}"), "Rule.", index);
    }
    let plain = reader
        .memories(&MemoriesInput::default(), at(30))
        .expect("plain listing");
    let rows = plain.value["memories"].as_array().expect("rows");
    assert_eq!(rows.len(), 20);
    for row in rows {
        assert!(row["remembered_at"].is_string(), "{row}");
        assert_eq!(row["actor_id"], "agent");
    }
    assert!(plain.value.get("changes").is_none());
    assert_eq!(plain.next, ["engram work memories --after key-19"]);

    // The recovery listing's rows leave author and time to the full read.
    let recovery = reader
        .memories(
            &MemoriesInput {
                context_generation: Some("termal-1".into()),
                ..MemoriesInput::default()
            },
            at(31),
        )
        .expect("recovery listing");
    let row = &recovery.value["memories"][0];
    assert_eq!(row["key"], "key-00");
    assert!(row.get("remembered_at").is_none());
    assert!(row.get("actor_id").is_none());
}

#[test]
fn previews_keep_numbered_lines_and_cut_long_ones_at_a_character_boundary() {
    let (_home, reader, _, _) = fixture();
    let numbered = "1. Never commit without the owner's word and a passing gate.";
    let multibyte = "ż".repeat(100);
    let long_sentence = format!("{}.", "word ".repeat(40).trim_end());
    for (key, body) in [
        ("a-numbered", numbered.to_owned()),
        ("b-multibyte", multibyte.clone()),
        ("c-long-sentence", long_sentence.clone()),
    ] {
        remember(&reader, key, &body, 0);
    }
    let listing = reader
        .memories(
            &MemoriesInput {
                context_generation: Some("termal-1".into()),
                ..MemoriesInput::default()
            },
            at(1),
        )
        .expect("recovery listing");
    let previews = listing.value["memories"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["first_line"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(previews[0], numbered, "a list number is not a sentence end");
    for (preview, original) in [(&previews[1], &multibyte), (&previews[2], &long_sentence)] {
        let kept = preview.strip_suffix('…').expect("a cut preview is marked");
        assert!(original.starts_with(kept), "{preview}");
        assert!((101..=120).contains(&kept.len()), "{preview}");
    }
}

#[test]
fn pages_that_cannot_hold_every_row_read_continue_without_skipping_a_key() {
    let (_home, reader, _, _) = fixture();
    let body = "ą".repeat(150);
    let mut expected = (0..70)
        .map(|index| format!("{index:03}-{}", "k".repeat(60)))
        .collect::<Vec<_>>();
    for (time, key) in expected.iter().enumerate() {
        remember(&reader, key, &body, i64::try_from(time).unwrap());
    }
    expected.sort();
    let (keys, reads) = recover(&reader, "termal-1", 100);
    assert_eq!(keys, expected);
    assert!(
        reads > 1,
        "64-byte keys and long previews need several pages"
    );
}
