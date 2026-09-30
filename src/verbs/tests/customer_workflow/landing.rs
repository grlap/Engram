//! A completion's landing: named at `done`, sealed, and shown by `show` and
//! the completed item's JSON; absent landings read "no landing recorded".

use super::*;
use crate::domain::CompletionLanding;

fn landing(commit: &str) -> CompletionLanding {
    CompletionLanding {
        commit: commit.into(),
        remote: "origin".into(),
        branch: "master".into(),
        pushed_at: at(3),
        // A build fingerprint has the shape the fingerprint code produces.
        installed_build: Some(
            crate::ObjectId::from_canonical_bytes(b"installed build")
                .as_str()
                .to_owned(),
        ),
    }
}

fn claim(verbs: &AgentVerbs, work: &str, now: i64) {
    verbs
        .claim(
            ClaimInput {
                work_ref: work.into(),
                ttl_seconds: None,
                recover: None,
            },
            at(now),
        )
        .expect("claim");
}

fn done(
    verbs: &AgentVerbs,
    work: &str,
    landing: Option<CompletionLanding>,
    now: i64,
) -> Result<Receipt, crate::verbs::VerbError> {
    verbs.done(
        DoneInput {
            source_fingerprint: None,
            landing,
            links: Vec::new(),
            link_basis: None,
            work_ref: Some(work.into()),
            summary: Some("Delivered and landed".into()),
            note: None,
        },
        at(now),
    )
}

fn stored_seals(path: &std::path::Path) -> Vec<serde_json::Value> {
    let connection = rusqlite::Connection::open(path).expect("store");
    let mut statement = connection
        .prepare("SELECT seal_json FROM work_completion_seals ORDER BY rowid")
        .expect("seals");
    statement
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .expect("seal rows")
        .map(|bytes| serde_json::from_slice(&bytes.expect("seal bytes")).expect("seal JSON"))
        .collect()
}

#[test]
fn a_landing_named_at_done_is_sealed_and_shown() {
    let (_directory, verbs, path, _project) = fixture();
    let work = add(&verbs, "Land the change", None, false, 0);
    claim(&verbs, &work, 1);
    let commit = "0123456789abcdef0123456789abcdef01234567";
    let named = landing(commit);
    let receipt = done(&verbs, &work, Some(named.clone()), 4).expect("done with a landing");
    let text = receipt.text();
    assert!(
        text.contains(&format!("landing: {commit} on origin/master, pushed ")),
        "{text}"
    );
    assert_eq!(receipt.value["landing"]["commit"], commit);

    let shown = verbs.show(&work, at(5)).expect("show");
    let text = shown.text();
    assert!(
        text.contains(&format!(
            "landing: {commit} on origin/master, pushed {}, installed build {} (asserted, unchecked)",
            named.pushed_at.to_rfc3339(),
            named.installed_build.as_deref().unwrap()
        )),
        "{text}"
    );
    let mut shown_landing = shown.value["landing"].clone();
    assert_eq!(
        shown_landing["installed_build_assurance"],
        "asserted, unchecked"
    );
    shown_landing
        .as_object_mut()
        .expect("landing object")
        .remove("installed_build_assurance");
    assert_eq!(
        serde_json::from_value::<CompletionLanding>(shown_landing).expect("landing JSON"),
        named
    );

    let seals = stored_seals(&path);
    assert_eq!(seals.len(), 1);
    assert_eq!(
        serde_json::from_value::<CompletionLanding>(seals[0]["landing"].clone())
            .expect("sealed landing"),
        named
    );
    let store = SqliteStore::open(&path).expect("store");
    let recorded = store.recorded_landings().expect("recorded landings");
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].work_ref, work);
    assert_eq!(recorded[0].landing, named);

    // The same retry reads the frozen seal back, landing included.
    let replayed = done(&verbs, &work, Some(named.clone()), 6).expect("same retry");
    assert_eq!(replayed.value["seal"], receipt.value["seal"]);
    assert_eq!(replayed.value["landing"]["commit"], commit);
    assert!(
        replayed
            .text()
            .contains(&format!("landing: {commit} on origin/master"))
    );

    // The seal is frozen: a retry naming another landing is a late finding.
    let other = landing("89abcdef0123456789abcdef0123456789abcdef");
    let refused = done(&verbs, &work, Some(other), 7).expect_err("another landing is refused");
    assert!(
        matches!(&refused.error, StoreError::InvalidWork(reason)
            if reason == crate::work_service::COMPLETED_WORK_LATE_FINDING_REFUSAL),
        "{refused:?}"
    );
    let seals = stored_seals(&path);
    assert_eq!(seals.len(), 1);
    assert_eq!(seals[0]["landing"]["commit"], commit);
}

#[test]
fn a_landing_named_after_a_completion_without_one_is_refused() {
    let (_directory, verbs, path, _project) = fixture();
    let work = add(&verbs, "Land after done", None, false, 0);
    claim(&verbs, &work, 1);
    done(&verbs, &work, None, 2).expect("done without a landing");
    let late = landing("0123456789abcdef0123456789abcdef01234567");
    let refused = done(&verbs, &work, Some(late), 3).expect_err("a late landing is refused");
    assert!(
        matches!(&refused.error, StoreError::InvalidWork(reason)
            if reason == crate::work_service::COMPLETED_WORK_LATE_FINDING_REFUSAL),
        "{refused:?}"
    );
    assert!(stored_seals(&path)[0].get("landing").is_none());
    let shown = verbs.show(&work, at(4)).expect("show");
    assert_eq!(shown.value["landing"], "no landing recorded");
}

#[test]
fn an_unreadable_seal_leaves_its_landing_unavailable() {
    let (_directory, verbs, path, _project) = fixture();
    let work = add(&verbs, "Seal read fails", None, false, 0);
    claim(&verbs, &work, 1);
    let named = landing("0123456789abcdef0123456789abcdef01234567");
    let core_input = WorkCompleteInput {
        source_fingerprint: None,
        landing: Some(named.clone()),
        links: Vec::new(),
        link_basis: None,
        capture: Some(crate::work_service::WorkCompletionCaptureInput {
            summary: "Delivered and landed".into(),
            refs: Vec::new(),
        }),
        evidence: Vec::new(),
        acceptance: None,
        note: None,
        idempotency_key: "landing-replay".into(),
    };
    let first = verbs
        .service
        .work_complete_on(Some(&work), core_input.clone(), at(2))
        .expect("complete");
    let WorkCompleteResult::Completed(completed) = first else {
        panic!("expected completion")
    };
    assert_eq!(completed.landing.as_ref(), Some(&named));
    // Seed the keyless post-completion attempt while the seal is readable, so
    // the retry below replays its stored result.
    let warm = done(&verbs, &work, Some(named), 3).expect("warm retry");
    assert_eq!(warm.value["seal"], completed.seal.as_str());
    rusqlite::Connection::open(&path)
        .expect("store")
        .execute(
            "UPDATE objects SET canonical_json = X'7B7D' WHERE object_id = ?1",
            [completed.seal.as_str()],
        )
        .expect("break the stored seal");

    // A replayed completion discloses the landing as unavailable, never drops it.
    verbs.service.select_work(&work, at(4)).expect("select");
    let replay = verbs
        .service
        .work_complete_on(Some(&work), core_input, at(4))
        .expect("replay");
    let WorkCompleteResult::Completed(replayed) = replay else {
        panic!("replay must retain success")
    };
    assert!(replayed.landing.is_none());
    assert!(replayed.landing_unavailable.is_some());
    let retried = done(
        &verbs,
        &work,
        Some(landing("0123456789abcdef0123456789abcdef01234567")),
        4,
    )
    .expect("keyless replay");
    assert!(
        retried.text().contains("landing: unavailable ("),
        "{}",
        retried.text()
    );
    assert!(
        retried.value["landing"]
            .as_str()
            .is_some_and(|value| value.starts_with("unavailable (")),
        "{}",
        retried.value["landing"]
    );

    let shown = verbs.show(&work, at(5)).expect("show");
    let text = shown.text();
    assert!(text.contains("landing: unavailable ("), "{text}");
    assert!(!text.contains("no landing recorded"), "{text}");
    assert!(
        shown.value["landing"]
            .as_str()
            .is_some_and(|value| value.starts_with("unavailable (")),
        "{}",
        shown.value["landing"]
    );
}

#[test]
fn a_restored_completion_leaves_its_landing_unavailable() {
    let (directory, verbs, path, project) = fixture();
    let work = add(&verbs, "Landed before the save", None, false, 0);
    claim(&verbs, &work, 1);
    done(
        &verbs,
        &work,
        Some(landing("0123456789abcdef0123456789abcdef01234567")),
        2,
    )
    .expect("done with a landing");
    let mut store = SqliteStore::open(&path).expect("store");
    let actor = store
        .resolve_work_ref(&project, &work)
        .expect("work")
        .created_by;
    let document = store
        .save_work_graph_snapshot(
            &project,
            &actor,
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &crate::DevelopmentNoopRedactor,
        )
        .expect("save")
        .document;
    let restored_path = directory.path().join("restored.db");
    SqliteStore::open(&restored_path)
        .expect("restored store")
        .load_work_graph_snapshot(
            &project,
            &actor,
            &serde_json::to_vec(&document).expect("snapshot bytes"),
            false,
            at(4),
            &crate::DevelopmentNoopRedactor,
        )
        .expect("load");
    let reader = AgentVerbs::new(
        restored_path,
        project,
        "agent".into(),
        SessionId("reader".into()),
        None,
    );
    // Restored history, not a native seal, completed the item here, so its
    // landing cannot be read and is never reported as absent.
    let shown = reader.show(&work, at(5)).expect("show restored");
    assert_eq!(shown.value["status"]["work"]["lifecycle"], "completed");
    assert!(
        shown
            .text()
            .contains("landing: unavailable (restored completion)"),
        "{}",
        shown.text()
    );
    assert_eq!(shown.value["landing"], "unavailable (restored completion)");
}

#[test]
fn landing_rendering_distinguishes_none_from_unavailable() {
    use crate::verbs::show::{landing_line, landing_value};
    // A seal written around validation never reaches the terminal raw.
    let smuggled = CompletionLanding {
        commit: "\u{1b}]0;title\u{7}".into(),
        installed_build: Some("\u{1b}[2J".into()),
        ..landing("0123456789abcdef0123456789abcdef01234567")
    };
    let line = landing_line(Some(&smuggled), None);
    assert!(
        !line.contains('\u{1b}') && !line.contains('\u{7}'),
        "{line:?}"
    );
    assert_eq!(landing_line(None, None), "landing: no landing recorded");
    assert_eq!(
        landing_line(None, Some("restored completion")),
        "landing: unavailable (restored completion)"
    );
    assert_eq!(landing_value(None, None), "no landing recorded");
    assert_eq!(
        landing_value(None, Some("restored completion")),
        "unavailable (restored completion)"
    );
}

#[test]
fn a_completion_without_a_landing_reads_no_landing_recorded() {
    let (_directory, verbs, path, _project) = fixture();
    let work = add(&verbs, "Complete without landing", None, false, 0);
    let open = verbs.show(&work, at(1)).expect("show open");
    assert!(open.value.get("landing").is_none());
    assert!(!open.text().contains("landing:"));
    claim(&verbs, &work, 2);
    let receipt = done(&verbs, &work, None, 3).expect("done");
    assert!(receipt.value.get("landing").is_none());
    assert!(!receipt.text().contains("landing:"));

    let shown = verbs.show(&work, at(4)).expect("show completed");
    assert!(
        shown.text().contains("landing: no landing recorded"),
        "{}",
        shown.text()
    );
    assert_eq!(shown.value["landing"], "no landing recorded");

    // A seal without the field is stored without it, and one read back
    // without it decodes with no landing.
    let seals = stored_seals(&path);
    assert!(seals[0].get("landing").is_none());
    let seal: crate::domain::CompletionSeal =
        serde_json::from_value(seals[0].clone()).expect("seal decodes");
    assert!(seal.landing.is_none());
    assert_eq!(serde_json::to_value(&seal).expect("seal JSON"), seals[0]);
    let store = SqliteStore::open(&path).expect("store");
    assert!(store.recorded_landings().expect("recorded").is_empty());
}

#[test]
fn a_malformed_landing_is_refused_before_anything_is_recorded() {
    let (_directory, verbs, path, _project) = fixture();
    let work = add(&verbs, "Refuse a malformed landing", None, false, 0);
    claim(&verbs, &work, 1);
    let valid = landing("0123456789abcdef0123456789abcdef01234567");
    let mut cases = Vec::new();
    for commit in [
        "0123456789abcdef0123456789abcdef0123456",
        "0123456789ABCDEF0123456789abcdef01234567",
        "0123456789abcdef0123456789abcdef0123456g",
    ] {
        cases.push((
            "commit",
            CompletionLanding {
                commit: commit.into(),
                ..valid.clone()
            },
        ));
    }
    for name in [
        "",
        "with space",
        "-option",
        "a..b",
        "trailing/",
        "/leading",
        "a//b",
        "master~5",
        "master^",
        "master@{1}",
        "@",
        "refs:heads",
        "wild*",
        "what?",
        "[class",
        "back\\slash",
        ".hidden",
        "part/.hidden",
        "ends.",
        "branch.lock",
    ] {
        cases.push((
            "remote",
            CompletionLanding {
                remote: name.into(),
                ..valid.clone()
            },
        ));
        cases.push((
            "branch",
            CompletionLanding {
                branch: name.into(),
                ..valid.clone()
            },
        ));
    }
    cases.push((
        "installed build",
        CompletionLanding {
            installed_build: Some("abc123".into()),
            ..valid.clone()
        },
    ));
    let connection = rusqlite::Connection::open(&path).expect("store");
    let objects = object_count(&connection);
    for (field, malformed) in cases {
        let error = done(&verbs, &work, Some(malformed.clone()), 2)
            .expect_err("a malformed landing is refused");
        assert!(
            error.to_string().contains(&format!("landing {field}")),
            "{field}: {error}"
        );
        assert_eq!(object_count(&connection), objects, "{malformed:?}");
    }
    let shown = verbs.show(&work, at(3)).expect("show");
    assert_eq!(shown.value["status"]["work"]["lifecycle"], "open");
    done(&verbs, &work, Some(valid), 4).expect("a valid landing completes");
}

/// An installed build is the completing agent's word: read back in full and
/// exactly as stored, beside "asserted, unchecked", whether it is a build's
/// full fingerprint or one with the right prefix and a wrong remainder; an
/// absent one reads "no installed build recorded". The words are derived
/// when read: the stored seal holds only what the agent asserted.
#[test]
fn an_installed_build_reads_back_as_stored_and_as_asserted() {
    let (_directory, verbs, path, _project) = fixture();
    // A build fingerprint of this very executable, derived when the test runs.
    let full = crate::build_identity::current()
        .build_fingerprint
        .as_ref()
        .map_or_else(
            || crate::ObjectId::from_canonical_bytes(b"unmeasured build"),
            Clone::clone,
        )
        .as_str()
        .to_owned();
    // The same first twelve characters, then a remainder of another build.
    let other = crate::ObjectId::from_canonical_bytes(format!("not {full}").as_bytes());
    let wrong_remainder = format!("{}{}", &full[..12], &other.as_str()[12..]);
    assert_ne!(wrong_remainder, full);
    let cases = [
        ("full", Some(full.clone())),
        ("wrong remainder", Some(wrong_remainder)),
        ("absent", None),
    ];
    for (index, (label, installed_build)) in cases.iter().enumerate() {
        let now = i64::try_from(index).expect("small") * 10;
        let work = add(&verbs, &format!("Land {label}"), None, false, now);
        claim(&verbs, &work, now + 1);
        let named = CompletionLanding {
            installed_build: installed_build.clone(),
            ..landing("0123456789abcdef0123456789abcdef01234567")
        };
        let receipt = done(&verbs, &work, Some(named.clone()), now + 2).expect(label);
        let shown = verbs.show(&work, at(now + 3)).expect("show");
        for value in [&receipt.value["landing"], &shown.value["landing"]] {
            if let Some(build) = installed_build {
                assert_eq!(value["installed_build"], build.as_str(), "{label}");
                assert_eq!(
                    value["installed_build_assurance"], "asserted, unchecked",
                    "{label}"
                );
            } else {
                assert!(value.get("installed_build").is_none(), "{label}: {value}");
                assert_eq!(
                    value["installed_build_assurance"], "no installed build recorded",
                    "{label}"
                );
            }
        }
        let text = shown.text();
        let expected = installed_build.as_deref().map_or_else(
            || ", no installed build recorded".to_owned(),
            |build| format!(", installed build {build} (asserted, unchecked)"),
        );
        assert!(text.contains(&expected), "{label}: {text}");
    }
    // The seals hold the landings as asserted, and no derived words.
    for seal in stored_seals(&path) {
        assert!(
            seal["landing"].get("installed_build_assurance").is_none(),
            "{seal}"
        );
    }
}
