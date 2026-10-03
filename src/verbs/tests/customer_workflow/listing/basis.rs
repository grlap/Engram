use super::*;
use sha2::{Digest, Sha256};

// Parse the emitted ready navigation, not a separately constructed query.
fn ready_navigation(command: &str) -> LsInput {
    let mut words = command.split_whitespace();
    assert_eq!(words.next(), Some("engram"));
    assert_eq!(words.next(), Some("work"));
    assert_eq!(words.next(), Some("ls"));
    let mut input = LsInput::default();
    while let Some(word) = words.next() {
        match word {
            "--ready" => input.ready = true,
            "--limit" => input.limit = Some(words.next().unwrap().parse().unwrap()),
            "--after" => input.after = Some(words.next().unwrap().into()),
            other => panic!("unexpected navigation argument {other}"),
        }
    }
    assert!(input.ready);
    input
}

fn peek(verbs: &AgentVerbs, now: DateTime<Utc>) -> Receipt {
    verbs
        .next(
            &NextInput {
                peek: true,
                ..NextInput::default()
            },
            now,
        )
        .unwrap()
}

fn ready_next(receipt: &Receipt) -> LsInput {
    let input = ready_navigation(receipt.value["ready_next"].as_str().expect("ready_next"));
    assert!(input.after.is_some(), "{}", receipt.text());
    input
}

fn refused(result: Result<Receipt, crate::verbs::VerbError>) -> bool {
    matches!(
        result.map(|_| ()).unwrap_err().error,
        StoreError::WorkCatalogCursorInvalid { .. }
    )
}

fn candidates(verbs: &AgentVerbs) -> Vec<String> {
    (0..MAX_NEXT_READY_CANDIDATES + 2)
        .map(|index| {
            add(
                verbs,
                &format!("Candidate {index}"),
                None,
                false,
                i64::from(index),
            )
        })
        .collect()
}

#[test]
fn next_minted_ready_continuation_is_refused_by_an_unrelated_note_unlike_ls() {
    let (_directory, verbs, _path, _project) = fixture();
    let candidates = candidates(&verbs);
    let unrelated = candidates.last().unwrap().clone();
    let receipt = peek(&verbs, at(100));
    let navigation = ready_next(&receipt);
    let token = listing_token_value(navigation.after.as_deref().unwrap());
    assert_eq!(token["basis"]["kind"], "project_cut");
    assert!(token["basis"]["cut"]["project_position"].is_i64());
    assert!(verbs.ls(&navigation, at(101)).is_ok());
    let listed = verbs
        .ls(
            &LsInput {
                ready: true,
                limit: Some(1),
                ..LsInput::default()
            },
            at(101),
        )
        .unwrap();
    let continued = LsInput {
        ready: true,
        limit: Some(1),
        after: Some(listed.value["after"].as_str().unwrap().into()),
        ..LsInput::default()
    };
    let token = listing_token_value(continued.after.as_deref().unwrap());
    assert_eq!(token["basis"]["kind"], "membership");
    verbs
        .note(
            &NoteInput {
                status: false,
                work_ref: Some(unrelated),
                text: "an observation that changes no listing".into(),
                refs: Vec::new(),
            },
            at(102),
        )
        .unwrap();
    // The project cut moved, so compact next's navigation is refused; the
    // listing's own sequence did not, so its continuation survives.
    assert!(refused(verbs.ls(&navigation, at(103))));
    assert!(verbs.ls(&continued, at(103)).is_ok());
    assert!(
        verbs
            .ls(&ready_next(&peek(&verbs, at(104))), at(104))
            .is_ok()
    );
}

#[test]
fn next_minted_ready_continuation_is_refused_inside_a_sub_millisecond_expiry() {
    let (_directory, verbs, _path, _project) = fixture();
    candidates(&verbs);
    let held = add_ready(&verbs, "Held until the boundary", 0, 20);
    verbs
        .claim(
            ClaimInput {
                work_ref: held,
                ttl_seconds: Some(10),
                recover: None,
            },
            at(30) + chrono::Duration::microseconds(123_900),
        )
        .unwrap();
    // The claim column truncates to .123 while the holder lives until .123900:
    // a cut minted inside that millisecond cannot be continued in it.
    let minted = at(40) + chrono::Duration::microseconds(123_100);
    let receipt = peek(&verbs, minted);
    let navigation = ready_next(&receipt);
    let token = listing_token_value(navigation.after.as_deref().unwrap());
    assert_eq!(
        token["basis"]["cut"]["valid_until_ms"],
        (at(40) + chrono::Duration::milliseconds(123)).timestamp_millis()
    );
    assert!(refused(verbs.ls(
        &navigation,
        at(40) + chrono::Duration::microseconds(123_200)
    )));
    let after = at(40) + chrono::Duration::milliseconds(124);
    let fresh = ready_next(&peek(&verbs, after));
    assert!(
        listing_token_value(fresh.after.as_deref().unwrap())["basis"]["cut"]["valid_until_ms"]
            .is_null()
    );
    assert!(verbs.ls(&fresh, after).is_ok());
}

#[test]
fn ls_pages_run_the_classified_catalog_twice_and_never_the_expiry_query() {
    let (_directory, verbs, _path, _project) = fixture();
    candidates(&verbs);
    let held = add_ready(&verbs, "Held", 0, 20);
    verbs
        .claim(
            ClaimInput {
                work_ref: held,
                ttl_seconds: Some(600),
                recover: None,
            },
            at(30),
        )
        .unwrap();
    for ready in [false, true] {
        let input = LsInput {
            ready,
            limit: Some(2),
            ..LsInput::default()
        };
        crate::storage::reset_work_catalog_count_queries();
        let first = verbs.ls(&input, at(40)).unwrap();
        assert_eq!(crate::storage::work_catalog_classified_queries(), 2);
        assert_eq!(crate::storage::work_catalog_count_queries(), 1);
        assert_eq!(crate::storage::work_catalog_expiry_queries(), 0);
        let continued = LsInput {
            after: Some(first.value["after"].as_str().unwrap().into()),
            ..input
        };
        crate::storage::reset_work_catalog_count_queries();
        verbs.ls(&continued, at(41)).unwrap();
        assert_eq!(crate::storage::work_catalog_classified_queries(), 2);
        assert_eq!(crate::storage::work_catalog_expiry_queries(), 0);
    }
    // Only compact next mints a project cut, and reads its expiry once.
    crate::storage::reset_work_catalog_count_queries();
    let navigation = ready_next(&peek(&verbs, at(42)));
    assert_eq!(crate::storage::work_catalog_expiry_queries(), 1);
    crate::storage::reset_work_catalog_count_queries();
    verbs.ls(&navigation, at(42)).unwrap();
    assert_eq!(crate::storage::work_catalog_classified_queries(), 2);
    assert_eq!(crate::storage::work_catalog_expiry_queries(), 0);
}

#[test]
fn membership_fingerprint_hashes_the_ordered_sequence_with_ready_priorities() {
    let (_directory, verbs, path, project) = fixture();
    let titled = [("Tie one", 1), ("Tie two", 1), ("Urgent", 0), ("Later", 2)];
    let refs: Vec<_> = titled
        .iter()
        .zip(0..)
        .map(|((title, priority), second)| add_ready(&verbs, title, *priority, second))
        .collect();
    let store = crate::SqliteStore::open(&path).unwrap();
    let mut members: Vec<(i32, String)> = refs
        .iter()
        .zip(titled)
        .map(|(work_ref, (_, priority))| {
            let id = store.resolve_work_ref(&project, work_ref).unwrap().work_id;
            (priority, id.0.to_string())
        })
        .collect();
    drop(store);
    for ready in [true, false] {
        if ready {
            members.sort();
        } else {
            members.sort_by(|left, right| left.1.cmp(&right.1));
        }
        let mut bytes = Vec::new();
        for (priority, work) in &members {
            bytes.extend_from_slice(work.as_bytes());
            if ready {
                bytes.extend_from_slice(format!(":{priority}").as_bytes());
            }
            bytes.push(b'\n');
        }
        let page = verbs
            .ls(
                &LsInput {
                    ready,
                    limit: Some(1),
                    ..LsInput::default()
                },
                at(10),
            )
            .unwrap();
        let token = listing_token_value(page.value["after"].as_str().unwrap());
        assert_eq!(
            token["basis"]["fingerprint"],
            format!("{:x}", Sha256::digest(&bytes)),
            "ready order {ready}"
        );
        assert_eq!(
            token["basis"]["observed_at"],
            serde_json::to_value(at(10)).unwrap()
        );
    }
}
