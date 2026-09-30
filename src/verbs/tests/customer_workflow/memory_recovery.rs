//! A peek that carries a context generation no recorded memories listing of
//! the session carries tells the session to list its memories before acting,
//! until the start of an unfiltered listing carries that generation.

use super::*;

const DIRECTION: &str = "the host reports a new context for this session: before acting, list project memories through the continuation and read the relevant current entries in full";

fn peek(verbs: &AgentVerbs, generation: Option<&str>, verbose: bool, now: i64) -> Receipt {
    verbs
        .next(
            &NextInput {
                peek: true,
                verbose,
                context_generation: generation.map(str::to_owned),
                ..NextInput::default()
            },
            at(now),
        )
        .expect("peek")
}

fn list(verbs: &AgentVerbs, generation: Option<&str>, now: i64) -> Receipt {
    verbs
        .memories(
            &MemoriesInput {
                context_generation: generation.map(str::to_owned),
                ..MemoriesInput::default()
            },
            at(now),
        )
        .expect("list memories")
}

fn remember(verbs: &AgentVerbs, key: &str, now: i64) {
    verbs
        .service
        .remember_project_memory(
            format!("Rule {key}"),
            Some(key.into()),
            false,
            None,
            at(now),
        )
        .expect("remember");
}

/// The direction is the first reminder and the first text line with its
/// command under it; its command is the first next command and the memory
/// detail, so every command the peek names for the listing settles it.
fn assert_directed(receipt: &Receipt, generation: &str) {
    let command = format!("engram work memories --context-generation {generation}");
    assert_eq!(
        receipt.reminders.first().map(String::as_str),
        Some(DIRECTION)
    );
    assert_eq!(receipt.value["reminders"][0], DIRECTION);
    assert_eq!(receipt.next.first(), Some(&command));
    assert_eq!(receipt.value["next"][0], command);
    assert_eq!(receipt.value["memories_detail"], command);
    assert_eq!(receipt.value["peek"]["memory_listing_due"], true);
    let text = receipt.text();
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some(DIRECTION));
    assert_eq!(lines.next(), Some(format!("  {command}").as_str()));
    assert!(lines.next().is_some_and(|line| line.starts_with("focus: ")));
    assert!(text.contains(&format!("memory detail: {command};")));
    assert!(!text.contains("memory detail: engram work memories;"));
}

fn assert_not_directed(receipt: &Receipt) {
    assert!(
        receipt
            .reminders
            .iter()
            .all(|reminder| reminder != DIRECTION),
        "{:?}",
        receipt.reminders
    );
    assert!(receipt.value["peek"].get("memory_listing_due").is_none());
    assert_eq!(
        receipt.next.first().map(String::as_str),
        Some("engram work memories")
    );
    assert_eq!(receipt.value["memories_detail"], "engram work memories");
    let text = receipt.text();
    assert!(text.starts_with("focus: "));
    assert!(text.contains("memory detail: engram work memories;"));
}

#[test]
fn a_session_with_no_record_is_directed_by_the_hosts_generation_until_it_lists() {
    let (_home, reader, path, _) = fixture();
    remember(&reader, "first", 0);
    let inspect = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&inspect).unwrap();
    for (time, verbose) in [(1, false), (2, true), (3, false)] {
        assert_directed(&peek(&reader, Some("termal-1"), verbose, time), "termal-1");
    }
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&inspect).unwrap(),
        before,
        "a peek records nothing"
    );

    list(&reader, Some("termal-1"), 4);

    // The generation is now the recorded one: nothing to do.
    for verbose in [false, true] {
        assert_not_directed(&peek(&reader, Some("termal-1"), verbose, 5));
    }
}

#[test]
fn a_changed_generation_is_directed_until_a_listing_carries_it() {
    let (_home, reader, _, _) = fixture();
    remember(&reader, "first", 0);
    list(&reader, Some("termal-1"), 1);
    assert_not_directed(&peek(&reader, Some("termal-1"), false, 2));

    for (time, verbose) in [(3, false), (4, true)] {
        assert_directed(&peek(&reader, Some("termal-2"), verbose, time), "termal-2");
    }

    list(&reader, Some("termal-2"), 5);

    for verbose in [false, true] {
        assert_not_directed(&peek(&reader, Some("termal-2"), verbose, 6));
    }
    // The earlier generation is no longer the recorded one.
    assert_directed(&peek(&reader, Some("termal-1"), false, 7), "termal-1");
}

#[test]
fn a_peek_without_a_generation_gives_no_direction() {
    let (_home, reader, _, _) = fixture();
    remember(&reader, "first", 0);
    // A session the store has never seen, and one with a recorded listing.
    assert_not_directed(&peek(&reader, None, false, 1));
    list(&reader, Some("termal-1"), 2);
    for verbose in [false, true] {
        assert_not_directed(&peek(&reader, None, verbose, 3));
    }
}

#[test]
fn memories_without_a_generation_is_a_read_that_records_nothing() {
    let (_home, reader, path, _) = fixture();
    for (index, key) in ["alpha", "beta", "gamma"].into_iter().enumerate() {
        remember(&reader, key, i64::try_from(index).unwrap());
    }
    reader
        .service
        .remember_project_memory(
            "Rule alpha, revised".into(),
            Some("alpha".into()),
            true,
            Some(1),
            at(4),
        )
        .expect("revise");
    let inspect = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&inspect).unwrap();
    let requests = [
        MemoriesInput::default(),
        MemoriesInput {
            query: Some("Rule".into()),
            ..MemoriesInput::default()
        },
        MemoriesInput {
            after: Some("alpha".into()),
            ..MemoriesInput::default()
        },
        MemoriesInput {
            query: Some("alpha".into()),
            full: true,
            ..MemoriesInput::default()
        },
        MemoriesInput {
            query: Some("alpha".into()),
            full: true,
            revision: Some(1),
            ..MemoriesInput::default()
        },
    ];
    for request in &requests {
        reader.memories(request, at(10)).expect("memories request");
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&inspect).unwrap(),
            before,
            "{request:?} must record nothing"
        );
    }
    assert_directed(&peek(&reader, Some("termal-1"), false, 11), "termal-1");
}

#[test]
fn only_the_start_of_an_unfiltered_listing_records_the_generation() {
    let (_home, reader, path, _) = fixture();
    for (index, key) in ["alpha", "beta", "gamma"].into_iter().enumerate() {
        remember(&reader, key, i64::try_from(index).unwrap());
    }
    reader
        .service
        .remember_project_memory(
            "Rule alpha, revised".into(),
            Some("alpha".into()),
            true,
            Some(1),
            at(4),
        )
        .expect("revise");
    let inspect = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&inspect).unwrap();
    let generation = Some("termal-2".to_owned());
    let requests = [
        // A search.
        MemoriesInput {
            query: Some("Rule".into()),
            context_generation: generation.clone(),
            ..MemoriesInput::default()
        },
        // A full read.
        MemoriesInput {
            query: Some("alpha".into()),
            full: true,
            context_generation: generation.clone(),
            ..MemoriesInput::default()
        },
        // A history read.
        MemoriesInput {
            query: Some("alpha".into()),
            full: true,
            revision: Some(1),
            context_generation: generation.clone(),
            ..MemoriesInput::default()
        },
        // A continuation page.
        MemoriesInput {
            after: Some("alpha".into()),
            context_generation: generation.clone(),
            ..MemoriesInput::default()
        },
    ];
    for request in &requests {
        reader.memories(request, at(10)).expect("memories request");
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&inspect).unwrap(),
            before,
            "{request:?} must record nothing"
        );
        assert_directed(&peek(&reader, Some("termal-2"), false, 11), "termal-2");
    }

    list(&reader, Some("termal-2"), 12);
    assert_not_directed(&peek(&reader, Some("termal-2"), false, 13));
}

#[test]
fn an_advancing_next_settles_the_memory_signal_and_never_the_generation() {
    let (_home, reader, _, _) = fixture();
    remember(&reader, "first", 0);
    // Without a generation an advancing next settles `changed`, as before.
    let first = reader.next(&NextInput::default(), at(1)).expect("next");
    assert_eq!(first.value["memories"]["changed"], true);
    let settled = reader.next(&NextInput::default(), at(2)).expect("next");
    assert_eq!(settled.value["memories"]["changed"], false);
    assert_eq!(
        peek(&reader, None, false, 3).value["memories"]["changed"],
        false
    );

    // With one it never records it: the signal and the direction stay.
    let with_generation = NextInput {
        context_generation: Some("termal-1".into()),
        ..NextInput::default()
    };
    for time in [4, 5] {
        let advanced = reader.next(&with_generation, at(time)).expect("next");
        assert_eq!(advanced.value["memories"]["changed"], true);
        assert!(
            advanced
                .reminders
                .iter()
                .all(|reminder| reminder != DIRECTION),
            "only a peek carries the direction"
        );
    }
    assert_directed(&peek(&reader, Some("termal-1"), false, 6), "termal-1");

    list(&reader, Some("termal-1"), 7);
    assert_not_directed(&peek(&reader, Some("termal-1"), false, 8));
    let listed = reader.next(&with_generation, at(9)).expect("next");
    assert_eq!(listed.value["memories"]["changed"], false);
}

#[test]
fn a_memory_written_after_a_listing_is_announced_without_the_direction() {
    let (_home, reader, path, project) = fixture();
    remember(&reader, "first", 0);
    list(&reader, Some("termal-1"), 1);
    let peer = AgentVerbs::new(path, project, "peer".into(), SessionId("peer".into()), None);
    remember(&peer, "second", 2);

    let announced = peek(&reader, Some("termal-1"), false, 3);
    assert_eq!(announced.value["memories"]["changed"], true);
    assert_not_directed(&announced);
    // The record is the reader's alone: the peer is directed for the same
    // generation.
    assert_directed(&peek(&peer, Some("termal-1"), false, 4), "termal-1");
}

#[test]
fn a_listing_that_cannot_be_recorded_still_lists_and_leaves_the_direction() {
    let (_home, reader, path, _) = fixture();
    remember(&reader, "first", 0);
    let inspect = rusqlite::Connection::open(&path).unwrap();
    inspect
        .execute_batch(
            "CREATE TRIGGER refuse_listing_record BEFORE INSERT ON project_memory_advertisements
             BEGIN SELECT RAISE(ABORT, 'the record is refused'); END;",
        )
        .unwrap();

    let listed = list(&reader, Some("termal-1"), 1);
    assert_eq!(listed.value["memories"][0]["key"], "first");

    inspect
        .execute_batch("DROP TRIGGER refuse_listing_record;")
        .unwrap();
    assert_directed(&peek(&reader, Some("termal-1"), false, 2), "termal-1");
}

#[test]
fn a_generation_that_is_not_a_plain_token_is_refused_by_next_and_memories() {
    let (_home, reader, path, _) = fixture();
    remember(&reader, "first", 0);
    let inspect = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&inspect).unwrap();
    // A private-use character would be printed escaped and a space would
    // need quoting: neither could be run back as the value supplied.
    for generation in ["g\u{e000}", "two words", "", "-leading"] {
        for peek in [true, false] {
            let error = reader
                .next(
                    &NextInput {
                        peek,
                        context_generation: Some(generation.into()),
                        ..NextInput::default()
                    },
                    at(1),
                )
                .expect_err("next must refuse the generation");
            assert!(error.to_string().contains("context_generation"), "{error}");
        }
        for request in [
            MemoriesInput {
                context_generation: Some(generation.into()),
                ..MemoriesInput::default()
            },
            MemoriesInput {
                query: Some("first".into()),
                full: true,
                context_generation: Some(generation.into()),
                ..MemoriesInput::default()
            },
        ] {
            let error = reader
                .memories(&request, at(1))
                .expect_err("memories must refuse the generation");
            assert!(error.to_string().contains("context_generation"), "{error}");
        }
    }
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&inspect).unwrap(),
        before
    );
}

#[test]
fn the_printed_command_reaches_the_listing_as_the_generation_supplied() {
    // The emitted text, split as a shell splits plain words, is the argument
    // list that records the generation the peek was given.
    let (_home, reader, _, _) = fixture();
    remember(&reader, "first", 0);
    let generation = format!("A.b_c-{}", "9".repeat(200));
    let receipt = peek(&reader, Some(&generation), false, 1);
    let text = receipt.text();
    let printed = text.lines().nth(1).expect("the command line").trim();
    let words = printed.split(' ').collect::<Vec<_>>();
    assert_eq!(
        words,
        [
            "engram",
            "work",
            "memories",
            "--context-generation",
            generation.as_str()
        ]
    );
    list(&reader, Some(words[4]), 2);
    assert_not_directed(&peek(&reader, Some(&generation), false, 3));
}

#[test]
fn the_direction_and_its_command_survive_every_budget_in_both_renderers() {
    let (_home, reader, service, root) = super::budgets::rich_focus(8);
    service.select_work(&root, at(100)).unwrap();
    let input = NextInput {
        peek: true,
        context_generation: Some("termal-9".into()),
        ..NextInput::default()
    };
    let command = "engram work memories --context-generation termal-9";
    for verbose in [false, true] {
        let input = NextInput {
            verbose,
            ..input.clone()
        };
        for budget in [MAX_AGENT_WORK_RESPONSE_BYTES, 4_096, 1] {
            let receipt = reader
                .next_with_verbose_budget(&input, at(101), budget)
                .expect("peek");
            assert_eq!(
                receipt.reminders.first().map(String::as_str),
                Some(DIRECTION),
                "verbose {verbose}, budget {budget}"
            );
            assert_eq!(receipt.next.first().map(String::as_str), Some(command));
            assert_eq!(receipt.value["memories_detail"], command);
            let text = receipt.text();
            let mut lines = text.lines();
            assert_eq!(lines.next(), Some(DIRECTION));
            assert_eq!(lines.next(), Some(format!("  {command}").as_str()));
        }
    }
}
