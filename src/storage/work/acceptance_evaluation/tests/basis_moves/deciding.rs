//! The source observation that decided a move is named beside the move, on
//! the refusal and on the stale status, and nothing is named for a move no
//! observation decided.

use super::*;
use crate::storage::DecidingObservation;

fn status_observation(store: &SqliteStore, work: &WorkItem) -> Option<DecidingObservation> {
    store
        .acceptance_evaluation_status(work.work_id, None)
        .expect("status read")
        .expect("an evaluation")
        .stale_observation
}

/// The refusal's move, its observation, and its message.
fn refused(
    result: Result<AcceptanceEvaluationReceipt, StoreError>,
) -> (
    EvaluationBasisMove,
    Option<DecidingObservation>,
    String,
    serde_json::Value,
) {
    let Err(error) = result else {
        panic!("expected a refusal, got {result:?}");
    };
    let value = crate::store_error_value(&error);
    let message = error.to_string();
    let StoreError::AcceptanceEvaluationBasisMoved {
        moved,
        observation,
        reason,
        ..
    } = error
    else {
        panic!("expected a moved basis, got {error:?}");
    };
    // The existing reason is kept byte for byte, as the prefix of the words
    // after the refusal's opening.
    assert!(
        message.contains(&format!("was refused: {reason}")),
        "{message}"
    );
    (
        moved,
        observation.map(|observation| *observation),
        message,
        value,
    )
}

/// The observation's position is its own run-feed entry.
fn assert_positioned(store: &SqliteStore, observation: &DecidingObservation) {
    let (position, kind): (i64, String) = store
        .connection
        .query_row(
            "SELECT position, object_kind FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND object_id = ?1",
            [observation.observation.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("the observation's feed entry");
    assert_eq!(kind, "execution_observation");
    assert_eq!(position, observation.position);
}

fn setup(name: &str) -> (Fixture, WorkItem, ObjectId, HostSession) {
    let mut fixture = fixture(name);
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable",
        5,
    );
    disable_obligation_rules(store, 6);
    let (work, note) = (fixture.work.clone(), fixture.evidence.clone());
    let mut host = HostSession::bind(store, &work, &fixture.claim, 10);
    host.checkpoint(store, true, None, 20);
    (fixture, work, note, host)
}

// A sighting at an undeclared revision with source_changed false: it is the
// observation that decides the move, on the status and on the refusal.
#[test]
fn a_quiet_sighting_at_an_undeclared_revision_is_named() {
    let (mut fixture, work, note, mut host) = setup("project-deciding-quiet");
    let store = &mut fixture.store;
    let read = cut(store, &work);
    let declared = judged_again(
        &work,
        &note,
        read,
        Some(revision("content-revision-2", None)),
        "declared",
        25,
    );
    record(store, &declared).expect("the declared evaluation records");
    // An earlier mismatch, then the newest: the newest decides.
    host.report(store, &[(false, Some("content-revision-4"))], 30);
    host.report(store, &[(false, Some("content-revision-3"))], 40);

    let named = status_observation(store, &work).expect("the deciding observation");
    assert_positioned(store, &named);
    assert!(!named.source_changed);
    assert_eq!(named.revision.as_deref(), Some("content-revision-3"));
    assert_eq!(
        named.workspace.as_deref(),
        Some(host.basis.workspace_id.as_str())
    );
    assert_eq!(named.reporting_session, host.session_id);
    assert_eq!(named.observed_at, Some(at(41)));
    assert_eq!(
        named.evaluated_revision.as_deref(),
        Some("content-revision-2")
    );
    assert!(named.evaluated_revision_declared);
    assert_eq!(
        freshness(store, &work).and_then(|(_, stale)| stale),
        Some(AcceptanceStaleReason::Mutation),
        "the cause stays the move"
    );

    // The same judgment submitted again at its cut is refused, naming the
    // same observation in the structured details and in one sentence.
    let (moved, observation, message, value) = refused(record(
        store,
        &judged_again(
            &work,
            &note,
            read,
            Some(revision("content-revision-2", None)),
            "declared-again",
            50,
        ),
    ));
    assert_eq!(moved, EvaluationBasisMove::SourceChanged);
    assert_eq!(observation.as_ref(), Some(&named));
    assert_eq!(
        value["error"]["details"]["deciding_observation"]["position"],
        named.position
    );
    assert_eq!(
        value["error"]["details"]["remedy"],
        EvaluationBasisMove::SourceChanged.remedy()
    );
    assert!(
        message.ends_with(&named.sentence()),
        "the sentence follows the unchanged refusal: {message}"
    );
    assert!(!message.contains('\n'));
    assert!(message.contains(&format!(
        "at run-feed position {}: workspace {}, revision content-revision-3",
        named.position, host.basis.workspace_id
    )));
    assert!(message.contains("the evaluation declared revision content-revision-2."));
}

// A change to the declared revision is exempt: it is never named, and it
// clears an earlier quiet mismatch.
#[test]
fn a_change_to_the_declared_revision_is_not_named() {
    let (mut fixture, work, note, mut host) = setup("project-deciding-declared");
    let store = &mut fixture.store;
    let read = cut(store, &work);
    let recorded = record(
        store,
        &judged_again(
            &work,
            &note,
            read,
            Some(revision("content-revision-2", None)),
            "declared",
            25,
        ),
    )
    .expect("the declared evaluation records");
    host.report(store, &[(true, Some("content-revision-2"))], 30);
    assert_eq!(
        freshness(store, &work),
        Some((recorded.evaluation.clone(), None))
    );
    assert_eq!(status_observation(store, &work), None);

    // A quiet mismatch, then a change back to the declared revision.
    host.report(store, &[(false, Some("content-revision-3"))], 40);
    assert!(status_observation(store, &work).is_some());
    host.report(store, &[(true, Some("content-revision-2"))], 50);
    assert_eq!(freshness(store, &work), Some((recorded.evaluation, None)));
    assert_eq!(status_observation(store, &work), None);
}

// A reported change to an undeclared revision decides at once: the first
// such change is named, even when a later sighting returns.
#[test]
fn the_first_undeclared_change_is_named() {
    let (mut fixture, work, note, mut host) = setup("project-deciding-change");
    let store = &mut fixture.store;
    let read = cut(store, &work);
    record(
        store,
        &judged_again(
            &work,
            &note,
            read,
            Some(revision("content-revision-2", None)),
            "declared",
            25,
        ),
    )
    .expect("the declared evaluation records");
    host.report(store, &[(true, Some("content-revision-5"))], 30);
    host.report(store, &[(false, Some("content-revision-2"))], 40);
    let named = status_observation(store, &work).expect("the deciding observation");
    assert_positioned(store, &named);
    assert!(named.source_changed);
    assert_eq!(named.revision.as_deref(), Some("content-revision-5"));
}

// Without a declaration the evaluation judged the revision last seen at its
// cut, and says so.
#[test]
fn an_undeclared_evaluation_names_the_revision_judged_at_its_cut() {
    let (mut fixture, work, note, mut host) = setup("project-deciding-undeclared");
    let store = &mut fixture.store;
    host.report(store, &[(false, Some("content-revision-2"))], 22);
    let read = cut(store, &work);
    record(
        store,
        &judged_again(&work, &note, read, None, "undeclared", 25),
    )
    .expect("the evaluation records");
    host.report(store, &[(false, Some("content-revision-3"))], 30);
    let named = status_observation(store, &work).expect("the deciding observation");
    assert_eq!(
        named.evaluated_revision.as_deref(),
        Some("content-revision-2")
    );
    assert!(!named.evaluated_revision_declared);
    assert!(
        named
            .sentence()
            .ends_with("the evaluation judged revision content-revision-2 at its cut.")
    );
}

// A move no observation decided names none: a host check after the cut.
#[test]
fn a_check_after_the_cut_names_no_observation() {
    let (mut fixture, work, note, mut host) = setup("project-deciding-check");
    let store = &mut fixture.store;
    let read = cut(store, &work);
    record(store, &judged(&work, &note, read, None, 25)).expect("the evaluation records");
    host.capture_environment(store, "content-revision-3", 30);
    let (moved, observation, message, value) = refused(record(
        store,
        &judged_again(&work, &note, read, None, "after-the-check", 40),
    ));
    assert_eq!(moved, EvaluationBasisMove::CheckRecorded);
    assert_eq!(observation, None);
    assert!(
        value["error"]["details"]
            .get("deciding_observation")
            .is_none()
    );
    assert!(
        !message.contains("deciding source observation"),
        "{message}"
    );
}

// Every stored field is shown on one line, control characters escaped, and
// a field the observation did not record says so.
#[test]
fn the_sentence_is_one_line_and_says_what_was_not_recorded() {
    let observation = DecidingObservation {
        observation: ObjectId::from_canonical_bytes(b"observation"),
        position: 7,
        source_changed: false,
        admitted: true,
        workspace: Some("work\nspace".into()),
        revision: None,
        root_generation: None,
        reporting_session: crate::SessionId("host-session".into()),
        observed_at: None,
        recorded_at: at(3),
        evaluated_revision: None,
        evaluated_revision_declared: false,
    };
    let sentence = observation.sentence();
    assert!(!sentence.contains('\n'), "{sentence}");
    assert!(sentence.contains("workspace work\\nspace"), "{sentence}");
    assert!(sentence.contains("revision not recorded"), "{sentence}");
    assert!(
        sentence.contains("observed at a time not recorded"),
        "{sentence}"
    );
    assert!(
        sentence.ends_with("judged revision not known at its cut."),
        "{sentence}"
    );
}

// Every show surface names the deciding observation alike, and the
// observations window lists the run's observations, other workspaces
// included, as a read that records nothing.
#[test]
fn every_show_surface_names_the_deciding_observation() {
    use crate::verbs::{AgentVerbs, ShowInput};
    let (mut fixture, work, note, mut host) = setup("project-deciding-show");
    let database = fixture.directory.path().join("engram.sqlite3");
    let store = &mut fixture.store;
    let read = cut(store, &work);
    let recorded = record(
        store,
        &judged_again(
            &work,
            &note,
            read,
            Some(revision("content-revision-2", None)),
            "declared",
            25,
        ),
    )
    .expect("the declared evaluation records");
    let own_workspace = host.basis.workspace_id.clone();
    host.basis.workspace_id = "another-workspace".into();
    host.report(store, &[(false, Some("content-revision-2"))], 30);
    host.basis.workspace_id = own_workspace;
    host.report(store, &[(false, Some("content-revision-3"))], 40);
    let named = status_observation(store, &work).expect("the deciding observation");

    let viewer = AgentVerbs::new(
        database.clone(),
        work.project_id.clone(),
        "viewer".into(),
        crate::SessionId("viewer".into()),
        None,
    );
    let show = |input: ShowInput| {
        viewer
            .show_records(&work.short_ref, &input, at(100))
            .expect("show")
    };
    let line = format!(
        "source moved at run-feed position {}: a sighting",
        named.position
    );
    let before = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();

    let plain = show(ShowInput::default());
    assert_eq!(
        plain.value["acceptance_evaluation"]["stale_observation"]["position"], named.position,
        "{}",
        plain.value
    );
    assert!(plain.text().contains(&line), "{}", plain.text());
    let full = show(ShowInput {
        full: true,
        ..ShowInput::default()
    });
    assert_eq!(
        full.value["work"]["evaluation"]["stale_observation"]["position"], named.position,
        "{}",
        full.value
    );
    assert!(full.text().contains(&line), "{}", full.text());
    let window = show(ShowInput {
        evaluations: true,
        ..ShowInput::default()
    });
    assert_eq!(
        window.value["evaluations"][0]["stale_observation"]["position"],
        named.position
    );
    assert!(window.text().contains(&line), "{}", window.text());
    let detail = show(ShowInput {
        evaluation: Some(recorded.evaluation.as_str().to_owned()),
        ..ShowInput::default()
    });
    assert_eq!(
        detail.value["evaluation"]["stale_observation"]["position"],
        named.position
    );
    assert_eq!(
        detail.value["evaluation"]["stale_observation"]["revision"],
        "content-revision-3"
    );
    assert_eq!(
        detail.value["evaluation"]["stale_observation"]["evaluated_revision"],
        "content-revision-2"
    );
    assert!(detail.text().contains(&line), "{}", detail.text());

    // The window: every observation of the run, the foreign one included.
    let observations = show(ShowInput {
        observations: true,
        ..ShowInput::default()
    });
    let rows = observations.value["observations"].as_array().expect("rows");
    assert!(
        rows.iter()
            .any(|row| row["workspace"] == "another-workspace"),
        "{}",
        observations.value
    );
    let last = rows.last().expect("the newest row");
    assert_eq!(last["run_position"], named.position);
    assert_eq!(last["revision"], "content-revision-3");
    assert_eq!(last["source_changed"], false);
    assert_eq!(
        observations.value["observations_window"]["total"],
        rows.len()
    );
    assert!(observations.text().contains(&format!(
        "run position {}: {} sighting; workspace",
        named.position, named.observation
    )));
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).unwrap(),
        before,
        "the reads record nothing"
    );
}

// The window pages every observation exactly once, oldest to newest, and a
// cursor from another window kind is refused.
#[test]
fn the_observations_window_pages_every_observation_once() {
    use crate::verbs::{AgentVerbs, ShowInput};
    let (mut fixture, work, _note, mut host) = setup("project-deciding-paging");
    let database = fixture.directory.path().join("engram.sqlite3");
    let store = &mut fixture.store;
    for index in 0..40 {
        host.report(
            store,
            &[(false, Some(&format!("content-revision-{index}")))],
            30 + index * 3,
        );
    }
    let viewer = AgentVerbs::new(
        database,
        work.project_id.clone(),
        "viewer".into(),
        crate::SessionId("viewer".into()),
        None,
    );
    let page = |after: Option<String>| {
        viewer
            .show_records(
                &work.short_ref,
                &ShowInput {
                    observations: true,
                    after,
                    ..ShowInput::default()
                },
                at(1_000),
            )
            .expect("observations window")
    };
    let first = page(None);
    let total = first.value["observations_window"]["total"]
        .as_u64()
        .expect("total");
    // The fixture's own change, then the 40 reports.
    assert_eq!(total, 41);
    let mut seen = Vec::new();
    let mut receipt = first;
    loop {
        let rows = receipt.value["observations"].as_array().expect("rows");
        assert!(!rows.is_empty(), "{rows:?}");
        let positions = rows
            .iter()
            .map(|row| row["run_position"].as_i64().expect("position"))
            .collect::<Vec<_>>();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        // Pages go back in time: prepend each older page.
        seen.splice(0..0, positions);
        match receipt.value["observations_window"]["after"].as_str() {
            Some(after) => receipt = page(Some(after.to_owned())),
            None => break,
        }
    }
    assert_eq!(seen.len(), 41);
    assert!(seen.windows(2).all(|pair| pair[0] < pair[1]), "{seen:?}");

    // A write elsewhere in the project leaves the run's source records, and
    // so the window, as they were: the continuation still reads its page.
    let token = page(None).value["observations_window"]["after"]
        .as_str()
        .expect("a continuation")
        .to_owned();
    viewer
        .add(
            crate::AddInput {
                title: "Unrelated item".into(),
                ..crate::AddInput::default()
            },
            at(1_000),
        )
        .expect("an unrelated item");
    assert!(
        page(Some(token)).value["observations"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
    );

    let foreign = viewer
        .show_records(
            &work.short_ref,
            &ShowInput {
                evaluations: true,
                after: Some(
                    page(None).value["observations_window"]["after"]
                        .as_str()
                        .expect("a continuation")
                        .to_owned(),
                ),
                ..ShowInput::default()
            },
            at(1_000),
        )
        .expect_err("an observations cursor is no evaluations cursor");
    assert!(matches!(
        foreign.error,
        StoreError::WorkShowCursorInvalid { .. }
    ));
    for conflicting in [
        ShowInput {
            observations: true,
            notes: true,
            ..ShowInput::default()
        },
        ShowInput {
            observations: true,
            evaluations: true,
            ..ShowInput::default()
        },
        ShowInput {
            observations: true,
            full: true,
            ..ShowInput::default()
        },
    ] {
        assert!(matches!(
            viewer
                .show_records(&work.short_ref, &conflicting, at(1_000))
                .expect_err("observations is exclusive")
                .error,
            StoreError::InvalidWork(_)
        ));
    }
}

// The longest stored fields, full of characters that escape, still leave
// the refusal one line well within what a host relays, and every show
// surface within the agent budget.
#[test]
fn the_longest_fields_fit_the_refusal_and_every_show_surface() {
    use crate::verbs::{AgentVerbs, ShowInput};
    let (mut fixture, work, note, mut host) = setup("project-deciding-budget");
    let database = fixture.directory.path().join("engram.sqlite3");
    let store = &mut fixture.store;
    let read = cut(store, &work);
    let declared = format!("r{}r", "\"".repeat(200));
    record(
        store,
        &judged_again(
            &work,
            &note,
            read,
            Some(revision(&declared, None)),
            "declared",
            25,
        ),
    )
    .expect("the declared evaluation records");
    // The widest host source text admission now takes: quotes, since a
    // control or bidirectional formatting character is refused in new host
    // source text. Stored control-character text keeps reading, and its
    // width is covered by the done-budget test's direct fixture below.
    host.basis.workspace_id = format!("w{}w", "\"".repeat(510));
    let long_revision = format!("v{}v", "\\".repeat(510));
    host.report(store, &[(false, Some(long_revision.as_str()))], 30);

    let (_, observation, message, value) = refused(record(
        store,
        &judged_again(
            &work,
            &note,
            read,
            Some(revision(&declared, None)),
            "declared-again",
            40,
        ),
    ));
    assert!(observation.is_some());
    assert!(!message.contains('\n'));
    assert!(
        message.chars().count() < 4_000,
        "{} characters",
        message.chars().count()
    );
    assert!(message.contains("bytes stored)"), "{message}");
    assert_eq!(
        value["error"]["details"]["deciding_observation"]["workspace"],
        host.basis.workspace_id
    );

    let viewer = AgentVerbs::new(
        database,
        work.project_id.clone(),
        "viewer".into(),
        crate::SessionId("viewer".into()),
        None,
    );
    for input in [
        ShowInput::default(),
        ShowInput {
            full: true,
            ..ShowInput::default()
        },
        ShowInput {
            evaluations: true,
            ..ShowInput::default()
        },
        ShowInput {
            observations: true,
            ..ShowInput::default()
        },
    ] {
        let receipt = viewer
            .show_records(&work.short_ref, &input, at(100))
            .expect("show fits");
        let emitted = format!("{}\n", receipt.text())
            .len()
            .max(serde_json::to_vec(&receipt.value).unwrap().len());
        assert!(
            emitted < crate::work_service::MAX_AGENT_WORK_RESPONSE_BYTES,
            "{emitted} bytes"
        );
    }
}

// A host treats a refusal whose text says "database is locked" as a locked
// store and retries it; stored text never makes the sentence say so.
#[test]
fn stored_text_never_makes_the_refusal_read_as_a_locked_store() {
    let observation = DecidingObservation {
        observation: ObjectId::from_canonical_bytes(b"observation"),
        position: 7,
        source_changed: false,
        admitted: true,
        workspace: Some("C:/work/The Database Is Locked/tree".into()),
        revision: Some("database is locked".into()),
        root_generation: None,
        reporting_session: crate::SessionId("database is locked".into()),
        observed_at: None,
        recorded_at: at(3),
        evaluated_revision: Some("DATABASE IS LOCKED".into()),
        evaluated_revision_declared: true,
    };
    let sentence = observation.sentence();
    assert!(
        !sentence.to_lowercase().contains("database is locked"),
        "{sentence}"
    );
    assert!(
        sentence.contains("The\\u{20}Database\\u{20}Is\\u{20}Locked"),
        "{sentence}"
    );
    // The CLI's text refusal normalizes whitespace; the phrase stays absent.
    assert!(
        !crate::work_service::terminal_error_line(&sentence)
            .to_lowercase()
            .contains("database is locked")
    );
}

// The CLI's JSON refusal on stderr never spells the phrase, and every field
// still decodes to what the host recorded.
#[test]
fn the_json_refusal_never_spells_the_locked_store_phrase() {
    let (mut fixture, work, note, mut host) = setup("project-deciding-locked-json");
    let store = &mut fixture.store;
    let read = cut(store, &work);
    let declared = || {
        judged_again(
            &work,
            &note,
            read,
            Some(revision("content-revision-2", None)),
            "declared",
            25,
        )
    };
    record(store, &declared()).expect("the declared evaluation records");
    host.basis.workspace_id = "C:/work/Database Is Locked".into();
    host.report(store, &[(false, Some("database is locked"))], 30);
    let error = record(
        store,
        &judged_again(
            &work,
            &note,
            read,
            Some(revision("content-revision-2", None)),
            "again",
            40,
        ),
    )
    .expect_err("refused");
    let value = crate::store_error_value(&error);
    let text = serde_json::to_string_pretty(&value).unwrap();
    assert!(text.to_lowercase().contains("database is locked"));
    let guarded = crate::storage::json_without_locked_store_phrase(&text);
    assert!(
        !guarded.to_lowercase().contains("database is locked"),
        "{guarded}"
    );
    let decoded: serde_json::Value = serde_json::from_str(&guarded).unwrap();
    assert_eq!(decoded, value, "every field decodes to what was recorded");
    assert_eq!(
        decoded["error"]["details"]["deciding_observation"]["revision"],
        "database is locked"
    );
    // A real lock error is not the concern of this refusal's guard.
    assert_eq!(
        crate::storage::json_without_locked_store_phrase("{\"a\":\"no phrase\"}"),
        "{\"a\":\"no phrase\"}"
    );
}

// A change to the declared revision in the declared workspace is exempt and
// never named; the same revision reported on another workspace is not.
#[test]
fn a_change_in_the_declared_workspace_is_exempt_and_another_workspace_is_named() {
    for other in [false, true] {
        let (mut fixture, work, note, mut host) = setup(if other {
            "project-deciding-declared-other-workspace"
        } else {
            "project-deciding-declared-workspace"
        });
        let store = &mut fixture.store;
        let read = cut(store, &work);
        let own = host.basis.workspace_id.clone();
        let recorded = record(
            store,
            &judged_again(
                &work,
                &note,
                read,
                Some(revision("content-revision-2", Some(own.as_str()))),
                "declared-in-workspace",
                25,
            ),
        )
        .expect("the declared evaluation records");
        if other {
            host.basis.workspace_id = "another-workspace".into();
        }
        host.report(store, &[(true, Some("content-revision-2"))], 30);
        if other {
            // The declared revision, but reported on another workspace than
            // the one declared: that change is not what was judged.
            let named = status_observation(store, &work).expect("the other workspace's change");
            assert_eq!(named.workspace.as_deref(), Some("another-workspace"));
            assert!(named.source_changed);
        } else {
            assert_eq!(freshness(store, &work), Some((recorded.evaluation, None)));
            assert_eq!(status_observation(store, &work), None);
        }
    }
}

// The widest admitted text: a title and attempt key of control characters,
// sixteen criteria, an undeclared evaluation whose cut revision and the quiet
// sighting after it are 512 escaped characters in a 512-byte workspace.
// Every show surface still answers within the agent budget.
#[test]
fn the_widest_admitted_text_fits_every_show_surface() {
    use crate::verbs::{AgentVerbs, ShowInput};
    let mut fixture = fixture("project-deciding-widest");
    let database = fixture.directory.path().join("engram.sqlite3");
    let claim = fixture.claim.clone();
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable",
        5,
    );
    disable_obligation_rules(store, 6);
    let work = revise(
        store,
        &fixture.work,
        &claim,
        WorkRevisionPatch {
            title: Some(format!("t{}t", "\u{1}".repeat(190))),
            acceptance: Some(
                (1..=16)
                    .map(|index| format!("criterion {index} holds"))
                    .collect(),
            ),
            ..empty_patch()
        },
        "widen",
        7,
    )
    .expect("revise the title and criteria");
    let claim =
        crate::storage::work::query::load_work_claim_optional(&store.connection, claim.run_id)
            .expect("claim read")
            .expect("the claim is held");
    let note = fixture.evidence.clone();
    let mut host = HostSession::bind(store, &work, &claim, 10);
    // Host source text is admitted only without control or bidirectional
    // formatting characters, so its widest admitted form is escaped quotes
    // and backslashes.
    host.basis.workspace_id = format!("w{}w", "\"".repeat(510));
    host.report(
        store,
        &[(false, Some(format!("a{}a", "\\".repeat(510)).as_str()))],
        20,
    );
    let read = cut(store, &work);
    let mut request = request(
        &work,
        read,
        "runner",
        Mode::SameSession,
        (1..=16)
            .map(|position| {
                verdict(
                    position,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Judgment,
                    std::slice::from_ref(&note),
                )
            })
            .collect(),
        25,
    );
    request.attempt_key = Some(format!("k{}k", "\u{1}".repeat(254)));
    record(store, &request).expect("the undeclared evaluation records");
    host.report(
        store,
        &[(false, Some(format!("b{}b", "\"".repeat(510)).as_str()))],
        30,
    );
    assert!(status_observation(store, &work).is_some());

    let viewer = AgentVerbs::new(
        database,
        work.project_id.clone(),
        "viewer".into(),
        crate::SessionId("viewer".into()),
        None,
    );
    for input in [
        ShowInput::default(),
        ShowInput {
            evaluations: true,
            ..ShowInput::default()
        },
        ShowInput {
            observations: true,
            ..ShowInput::default()
        },
    ] {
        let receipt = viewer
            .show_records(&work.short_ref, &input, at(100))
            .unwrap_or_else(|error| panic!("{input:?}: {error}"));
        let emitted = format!("{}\n", receipt.text())
            .len()
            .max(serde_json::to_vec(&receipt.value).unwrap().len());
        assert!(
            emitted < crate::work_service::MAX_AGENT_WORK_RESPONSE_BYTES,
            "{input:?}: {emitted} bytes"
        );
    }
    let window = viewer
        .show_records(
            &work.short_ref,
            &ShowInput {
                evaluations: true,
                ..ShowInput::default()
            },
            at(100),
        )
        .expect("evaluations window");
    assert_eq!(
        window.value["evaluations"].as_array().map(Vec::len),
        Some(1)
    );
    let shown = window.value["evaluations"][0]["stale_observation"]["revision"]
        .as_str()
        .expect("the shown revision");
    assert!(shown.ends_with("… (512 bytes stored)"), "{shown}");
}

// Doubled spaces and other whitespace, which the CLI's text refusal
// collapses into one space, never let the phrase form either.
#[test]
fn whitespace_variants_never_make_the_refusal_read_as_a_locked_store() {
    for spelled in [
        "database  is locked",
        "database\u{a0}is\u{a0}locked",
        "Database\u{3000}is locked",
        "database\tis\nlocked",
        "database \u{2009} is \u{a0} locked",
    ] {
        let observation = DecidingObservation {
            observation: ObjectId::from_canonical_bytes(b"observation"),
            position: 7,
            source_changed: false,
            admitted: true,
            workspace: Some(format!("C:/work/{spelled}/tree")),
            revision: Some(spelled.into()),
            root_generation: None,
            reporting_session: crate::SessionId(spelled.into()),
            observed_at: None,
            recorded_at: at(3),
            evaluated_revision: Some(spelled.into()),
            evaluated_revision_declared: true,
        };
        let shown = crate::work_service::terminal_error_line(&observation.sentence());
        assert!(
            !shown.to_lowercase().contains("database is locked"),
            "{spelled:?}: {shown}"
        );
    }
}

// The guard's escapes count toward each field's bound: the widest admitted
// fields that spell the phrase keep the whole refusal well within the 4,000
// characters a host relays.
#[test]
fn the_guard_never_lengthens_the_refusal_past_its_bound() {
    let phrase = "database is locked";
    // A quiet admitted sighting, and the longer words of an accounted change
    // the host observed without admission.
    for (source_changed, admitted, reason) in [
        (
            false,
            true,
            "the source changed after evidence basis 46 and this evaluation did not judge that revision",
        ),
        (
            true,
            false,
            "a source change the host observed without admission was recorded after evidence basis 46, and whatever revision it reports this evaluation's checks did not follow it",
        ),
    ] {
        let observation = DecidingObservation {
            observation: ObjectId::from_canonical_bytes(b"observation"),
            position: 7,
            source_changed,
            admitted,
            workspace: Some(format!("{phrase}{}w", " ".repeat(493))),
            revision: Some(format!("{phrase}{}b", " ".repeat(493))),
            root_generation: None,
            reporting_session: crate::SessionId(format!("{phrase}{}s", " ".repeat(45))),
            observed_at: Some(at(2)),
            recorded_at: at(3),
            evaluated_revision: Some(format!("{phrase}{}a", " ".repeat(493))),
            evaluated_revision_declared: false,
        };
        let refusal = StoreError::AcceptanceEvaluationBasisMoved {
            work: crate::domain::WorkId::new(),
            moved: EvaluationBasisMove::SourceChanged,
            reason: format!("{reason}; {}", EvaluationBasisMove::SourceChanged.remedy()),
            observation: Some(Box::new(observation)),
        }
        .to_string();
        assert!(!refusal.contains('\n'));
        assert!(
            refusal.chars().count() < 2_500,
            "{} characters: {refusal}",
            refusal.chars().count()
        );
        assert!(
            !crate::work_service::terminal_error_line(&refusal)
                .to_lowercase()
                .contains(phrase)
        );
    }
}

/// A done refused for a stale evaluation, from the shared fixture: a flagged
/// change to `revision` decided it, or, with `decided` false, the policy no
/// longer admits its mode.
fn stale_done(
    name: &str,
    revision: &str,
    decided: bool,
) -> (
    crate::test_support::TempHome,
    std::path::PathBuf,
    crate::storage::work::test_support::StaleDecidingFixture,
) {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let fixture = crate::storage::stale_deciding_refusal_fixture(
        &database,
        name,
        "runner",
        "revision-judged",
        "C:/work/other tree",
        revision,
        decided,
        10,
    );
    (directory, database, fixture)
}

// done's storage completion carries the deciding observation beside the
// cause on every path: the preflight, the recovery the protocol returns, and
// the raw refusal, whose message keeps its words and never prints the
// recorded text. A stale evaluation for another reason carries none.
#[test]
fn completion_carries_the_deciding_observation_beside_the_unchanged_cause() {
    for decided in [true, false] {
        let (_directory, database, fixture) =
            stale_done("project-done-deciding", "database is locked", decided);
        let mut store = SqliteStore::open(&database).expect("store");
        let (work, claim) = (fixture.work.clone(), fixture.claim.clone());
        let expected = WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
            reason: if decided {
                AcceptanceStaleReason::Mutation
            } else {
                AcceptanceStaleReason::Policy
            },
        };
        let named = |observation: Option<&DecidingObservation>| {
            observation.map(|observation| {
                (
                    observation.position,
                    observation.revision.clone(),
                    observation.workspace.clone(),
                    observation.evaluated_revision.clone(),
                )
            })
        };
        let want = decided.then(|| {
            (
                fixture.position,
                Some("database is locked".to_owned()),
                Some("C:/work/other tree".to_owned()),
                Some("revision-judged".to_owned()),
            )
        });

        let crate::storage::AcceptanceEvaluationReadiness::Blocked(cause, context) = store
            .acceptance_evaluation_readiness(work.work_id, claim.run_id, None)
            .expect("readiness")
        else {
            panic!("a stale evaluation blocks completion");
        };
        assert_eq!(cause, expected);
        assert_eq!(named(context.deciding_observation.as_deref()), want);

        let all = store.work_run_evidence(claim.run_id).expect("evidence");
        checkpoint(&mut store, &work, &claim, "runner", "final", 60, &all);
        let mut request = completion_request(&work, &claim, "runner", &fixture.generic, "done", 61);
        request.evidence = all;
        request.acceptance = Vec::new();
        match store
            .complete_work_for_protocol(&request, &DevelopmentNoopRedactor)
            .expect("the protocol path answers with a recovery")
        {
            crate::storage::work::CompleteWorkStorageResult::Recovery(snapshot) => {
                assert_eq!(snapshot.recovery.cause, expected);
                assert_eq!(
                    named(snapshot.recovery.deciding_observation.as_deref()),
                    want
                );
            }
            crate::storage::work::CompleteWorkStorageResult::Completed(_) => {
                panic!("a stale evaluation never seals")
            }
        }
        let error = store
            .complete_work(&request, &DevelopmentNoopRedactor)
            .expect_err("the raw path refuses");
        let StoreError::WorkCompletionRecoveryRequired { cause, context, .. } = &error else {
            panic!("expected a recovery refusal, got {error:?}");
        };
        assert_eq!(cause, &expected);
        assert_eq!(named(context.deciding_observation.as_deref()), want);
        // The message is the cause's words alone, as before.
        assert_eq!(
            error.to_string(),
            format!(
                "completion for work {:?} requires recovery: {expected:?}",
                work.work_id
            )
        );
        assert!(!error.to_string().contains("database is locked"));
        let details = &crate::store_error_value(&error)["error"]["details"];
        assert_eq!(details["cause"], serde_json::json!(expected));
        if decided {
            assert_eq!(
                details["deciding_observation"]["revision"],
                "database is locked"
            );
            assert_eq!(
                details["deciding_observation"]["position"],
                fixture.position
            );
        } else {
            assert!(details.get("deciding_observation").is_none(), "{details}");
        }
    }
}

// The word's refusal: the "not done" line and the stale reminder keep their
// words, and the reminder adds the escaped sentence naming the observation;
// the receipt's JSON names it beside the cause. Nothing is named for a stale
// evaluation no observation decided.
#[test]
fn the_done_word_names_the_deciding_observation_after_its_words() {
    use crate::verbs::{AgentVerbs, DoneInput};
    for decided in [true, false] {
        let (_directory, database, fixture) =
            stale_done("project-done-word", "database is locked", decided);
        let verbs = AgentVerbs::new(
            database,
            fixture.work.project_id.clone(),
            "runner".into(),
            crate::SessionId("runner".into()),
            None,
        );
        let receipt = verbs
            .done(
                DoneInput {
                    work_ref: Some(fixture.work.short_ref.clone()),
                    summary: Some("delivered".into()),
                    ..DoneInput::default()
                },
                at(100),
            )
            .expect("an owed receipt");
        assert!(receipt.owed);
        let text = receipt.text();
        assert!(
            text.contains(&format!(
                "not done {} \"{}\": something is still owed",
                fixture.work.short_ref, fixture.work.title
            )),
            "{text}"
        );
        let reason = if decided { "mutation" } else { "policy" };
        let words = format!(
            "{} acceptance evaluation is stale ({reason})",
            fixture.work.short_ref
        );
        let reminder = text
            .lines()
            .find(|line| line.contains(&words))
            .unwrap_or_else(|| panic!("the stale reminder: {text}"));
        let value = &receipt.value;
        assert_eq!(value["code"], "acceptance_evaluation_stale");
        if decided {
            let observation = &value["recovery"]["deciding_observation"];
            let recorded: chrono::DateTime<chrono::Utc> =
                serde_json::from_value(observation["recorded_at"].clone()).expect("recorded time");
            let observed: chrono::DateTime<chrono::Utc> =
                serde_json::from_value(observation["observed_at"].clone()).expect("observed time");
            let sentence = format!(
                "The deciding source observation is at run-feed position {}: workspace C:/work/other tree, revision database\\u{{20}}is\\u{{20}}locked, reported by session runner, observed {} (recorded {}); the evaluation judged revision revision-judged at its cut.",
                fixture.position,
                observed.to_rfc3339(),
                recorded.to_rfc3339(),
            );
            assert!(
                reminder.ends_with(&format!("{words}; evaluate again. {sentence}")),
                "{reminder}"
            );
            assert!(
                !text.to_lowercase().contains("database is locked"),
                "{text}"
            );
            assert_eq!(observation["revision"], "database is locked");
        } else {
            assert!(
                !reminder.contains("deciding source observation"),
                "{reminder}"
            );
            assert!(value["recovery"].get("deciding_observation").is_none());
        }
    }
}

// The widest admitted host text, 512 control characters in the judged
// revision, the workspace and the revision, still leaves done's refusal
// within the agent budget, in
// text and in JSON: the receipt names the observation as show does, each
// field bounded with its stored length, and the storage refusal's details
// keep it whole.
#[test]
fn the_widest_deciding_observation_fits_done_within_the_agent_budget() {
    use crate::verbs::{AgentVerbs, DoneInput};
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let workspace = format!("w{}w", "\u{1}".repeat(510));
    let revision = format!("v{}v", "\u{2}".repeat(510));
    let judged = format!("j{}j", "\u{3}".repeat(510));
    let fixture = crate::storage::stale_deciding_refusal_fixture(
        &database,
        "project-done-widest",
        "runner",
        &judged,
        &workspace,
        &revision,
        true,
        10,
    );
    let verbs = AgentVerbs::new(
        database,
        fixture.work.project_id.clone(),
        "runner".into(),
        crate::SessionId("runner".into()),
        None,
    );
    let receipt = verbs
        .done(
            DoneInput {
                work_ref: Some(fixture.work.short_ref.clone()),
                summary: Some("delivered".into()),
                ..DoneInput::default()
            },
            at(100),
        )
        .expect("an owed receipt");
    let observation = &receipt.value["recovery"]["deciding_observation"];
    assert_eq!(observation["position"], fixture.position);
    for field in ["workspace", "revision", "evaluated_revision"] {
        let shown = observation[field].as_str().expect("the shown field");
        assert!(
            shown.ends_with("… (512 bytes stored)"),
            "{field}: {shown:?}"
        );
    }
    let text = format!("{}\n", receipt.text());
    assert!(text.contains("The deciding source observation"), "{text}");
    for emitted in [
        text.len(),
        serde_json::to_vec(&receipt.value).unwrap().len(),
    ] {
        assert!(
            emitted < crate::work_service::MAX_AGENT_WORK_RESPONSE_BYTES,
            "{emitted} bytes"
        );
    }
}

// Every show surface names an unadmitted change as the barrier it is, with
// the revisions compared and the fresh-evaluation remedy on the line, and
// the JSON carries the observation's admission.
#[test]
fn every_show_surface_names_an_unadmitted_change_with_its_remedy() {
    use crate::verbs::{AgentVerbs, ShowInput};
    for (reported, compared) in [
        ("content-revision-2", "the same revision"),
        ("content-revision-9", "another revision"),
    ] {
        let (mut fixture, work, note, host) =
            setup(&format!("project-deciding-unadmitted-show-{reported}"));
        let database = fixture.directory.path().join("engram.sqlite3");
        let claim = fixture.claim.clone();
        let store = &mut fixture.store;
        let read = cut(store, &work);
        let recorded = record(
            store,
            &judged_again(
                &work,
                &note,
                read,
                Some(revision("content-revision-2", None)),
                "declared",
                25,
            ),
        )
        .expect("the declared evaluation records");
        let change = super::super::citation_sources::unadmitted_change(
            store,
            &host,
            &claim,
            reported,
            "late-report",
            30,
        );
        let named = status_observation(store, &work).expect("the deciding observation");
        assert_eq!(named.observation, change.observation, "{reported}");

        let viewer = AgentVerbs::new(
            database.clone(),
            work.project_id.clone(),
            "viewer".into(),
            crate::SessionId("viewer".into()),
            None,
        );
        let show = |input: ShowInput| {
            viewer
                .show_records(&work.short_ref, &input, at(100))
                .expect("show")
        };
        let line = format!(
            "unadmitted source change at run-feed position {}: workspace {}, revision {reported}, reported by",
            named.position, host.basis.workspace_id
        );
        let ending = format!(
            "; the evaluation declared revision content-revision-2, {compared}; a barrier whatever revision it reports, so request a fresh evaluation"
        );
        let plain = show(ShowInput::default());
        let observation = &plain.value["acceptance_evaluation"]["stale_observation"];
        assert_eq!(
            plain.value["acceptance_evaluation"]["stale"], "unadmitted_change",
            "{}",
            plain.value
        );
        assert_eq!(observation["admitted"], false, "{}", plain.value);
        assert_eq!(observation["source_changed"], true, "{}", plain.value);
        assert_eq!(
            observation["revisions_compared"], compared,
            "{}",
            plain.value
        );
        assert!(plain.text().contains(&line), "{}", plain.text());
        assert!(plain.text().contains(&ending), "{}", plain.text());
        assert!(
            !plain.text().contains("source moved at"),
            "{}",
            plain.text()
        );
        let full = show(ShowInput {
            full: true,
            ..ShowInput::default()
        });
        assert_eq!(
            full.value["work"]["evaluation"]["stale_observation"]["admitted"], false,
            "{}",
            full.value
        );
        assert!(full.text().contains(&ending), "{}", full.text());
        let window = show(ShowInput {
            evaluations: true,
            ..ShowInput::default()
        });
        assert_eq!(
            window.value["evaluations"][0]["stale"], "unadmitted_change",
            "{}",
            window.value
        );
        assert!(window.text().contains(&ending), "{}", window.text());
        let detail = show(ShowInput {
            evaluation: Some(recorded.evaluation.as_str().to_owned()),
            ..ShowInput::default()
        });
        assert_eq!(
            detail.value["evaluation"]["stale_observation"]["revisions_compared"], compared,
            "{}",
            detail.value
        );
        assert!(detail.text().contains(&ending), "{}", detail.text());
    }
}

// The revisions are compared before any display shortening, and a side the
// records do not name is never called the same.
#[test]
fn revisions_are_compared_whole_and_an_unnamed_side_is_not_called_the_same() {
    let observation = |revision: Option<&str>, evaluated: Option<&str>| DecidingObservation {
        observation: ObjectId::from_canonical_bytes(b"observation"),
        position: 7,
        source_changed: true,
        admitted: false,
        workspace: Some("workspace".into()),
        revision: revision.map(Into::into),
        root_generation: None,
        reporting_session: crate::SessionId("host-session".into()),
        observed_at: None,
        recorded_at: at(3),
        evaluated_revision: evaluated.map(Into::into),
        evaluated_revision_declared: true,
    };
    let long = format!("r{}", "x".repeat(600));
    let cases = [
        (Some("same"), Some("same"), "the same revision"),
        (
            Some(long.as_str()),
            Some(long.as_str()),
            "the same revision",
        ),
        (Some("one"), Some("two"), "another revision"),
        (
            None,
            Some("two"),
            "a revision one of the records does not name",
        ),
        (
            Some("one"),
            None,
            "a revision one of the records does not name",
        ),
        (None, None, "a revision one of the records does not name"),
    ];
    for (revision, evaluated, expected) in cases {
        let named = observation(revision, evaluated);
        assert_eq!(
            named.revisions_compared(),
            expected,
            "{revision:?} vs {evaluated:?}"
        );
        let sentence = named.sentence();
        assert!(
            sentence.contains(&format!(", {expected}; whatever revision")),
            "{sentence}"
        );
        assert!(!sentence.contains('\n'), "{sentence}");
        // The whole revision is compared; the sentence shows it bounded.
        if revision == Some(long.as_str()) {
            assert!(!sentence.contains(long.as_str()), "{sentence}");
        }
    }
}

// done's refusal for an evaluation voided by a change the host observed
// without admission names that barrier, the deciding observation with both
// revisions compared, and the fresh-evaluation remedy, never a content
// mutation.
#[test]
fn done_names_an_unadmitted_change_as_a_barrier_with_the_revisions_compared() {
    use crate::verbs::{AgentVerbs, DoneInput};
    let (fixture, work, note, host) = setup("project-deciding-unadmitted-done");
    let Fixture {
        mut store,
        directory,
        claim,
        ..
    } = fixture;
    let read = cut(&store, &work);
    let declared = judged_again(
        &work,
        &note,
        read,
        Some(revision("content-revision-2", None)),
        "declared",
        25,
    );
    record(&mut store, &declared).expect("the declared evaluation records");
    let change = super::super::citation_sources::unadmitted_change(
        &mut store,
        &host,
        &claim,
        "content-revision-2",
        "late-report",
        30,
    );
    drop(store);
    let verbs = AgentVerbs::new(
        directory.path().join("engram.sqlite3"),
        work.project_id.clone(),
        "runner".into(),
        crate::SessionId("runner".into()),
        None,
    );
    let receipt = verbs
        .done(
            DoneInput {
                work_ref: Some(work.short_ref.clone()),
                summary: Some("delivered".into()),
                ..DoneInput::default()
            },
            at(100),
        )
        .expect("an owed receipt");
    assert!(receipt.owed);
    let value = &receipt.value;
    assert_eq!(value["code"], "acceptance_evaluation_stale", "{value}");
    assert_eq!(
        value["recovery"]["cause"]["reason"], "unadmitted_change",
        "{value}"
    );
    let observation = &value["recovery"]["deciding_observation"];
    assert_eq!(observation["observation"], change.observation.as_str());
    assert_eq!(observation["admitted"], false);
    assert_eq!(observation["source_changed"], true);
    assert_eq!(observation["revision"], "content-revision-2");
    assert_eq!(observation["evaluated_revision"], "content-revision-2");
    assert_eq!(observation["evaluated_revision_declared"], true);
    assert_eq!(observation["revisions_compared"], "the same revision");
    let text = receipt.text();
    let words = format!(
        "{} acceptance evaluation is stale (unadmitted_change)",
        work.short_ref
    );
    let reminder = text
        .lines()
        .find(|line| line.contains(&words))
        .unwrap_or_else(|| panic!("the stale reminder: {text}"));
    assert!(
        reminder.contains(
            "request a fresh acceptance evaluation of the current source, then retry done. The deciding source record is a source change the host observed without admission, at run-feed position"
        ),
        "{reminder}"
    );
    assert!(
        reminder
            .contains("the evaluation declared revision content-revision-2, the same revision;"),
        "{reminder}"
    );
    assert!(!text.contains("(mutation)"), "{text}");
    let remedy = value["remedy"].as_str().unwrap_or_default();
    assert!(
        remedy.starts_with(
            "a source change the host observed without admission followed the evaluated cut and is a barrier whatever revision it reports"
        ),
        "{value}"
    );
}

// An accounted change the host observed without admission voids the
// evaluation whatever revision it reports, and is named as that barrier, not
// as a content mutation: the status reads stale with reason
// `unadmitted_change`, the observation carries its admission and both
// revisions, the sentence says whether they are the same, and a submission
// kept at the pre-change cut is refused in the same words.
#[test]
fn an_unadmitted_change_is_named_as_a_barrier_with_the_revisions_compared() {
    for (reported, compared) in [
        ("content-revision-2", "the same revision"),
        ("content-revision-9", "another revision"),
    ] {
        let (mut fixture, work, note, host) =
            setup(&format!("project-deciding-unadmitted-{reported}"));
        let claim = fixture.claim.clone();
        let store = &mut fixture.store;
        let read = cut(store, &work);
        let declared = judged_again(
            &work,
            &note,
            read,
            Some(revision("content-revision-2", None)),
            "declared",
            25,
        );
        let recorded = record(store, &declared).expect("the declared evaluation records");
        let change = super::super::citation_sources::unadmitted_change(
            store,
            &host,
            &claim,
            reported,
            "late-report",
            30,
        );
        assert!(
            matches!(
                change.accounting,
                crate::domain::ObservationAccounting::SourceChange { .. }
            ),
            "{reported}: {:?}",
            change.accounting
        );

        let status = store
            .acceptance_evaluation_status(work.work_id, None)
            .expect("status read")
            .expect("an evaluation");
        assert_eq!(status.evaluation, recorded.evaluation, "{reported}");
        assert_eq!(
            status.stale,
            Some(AcceptanceStaleReason::UnadmittedChange),
            "{reported}"
        );
        let named = status.stale_observation.expect("the deciding observation");
        assert_eq!(named.observation, change.observation, "{reported}");
        assert!(named.source_changed, "{reported}");
        assert!(!named.admitted, "{reported}");
        assert_eq!(named.revision.as_deref(), Some(reported));
        assert_eq!(
            named.evaluated_revision.as_deref(),
            Some("content-revision-2")
        );
        assert!(named.evaluated_revision_declared, "{reported}");
        assert_eq!(named.revisions_compared(), compared, "{reported}");
        let sentence = named.sentence();
        assert!(
            sentence.starts_with(
                "The deciding source record is a source change the host observed without admission, at run-feed position"
            ),
            "{sentence}"
        );
        assert!(
            sentence.contains(&format!(
                "revision {reported}, reported by session {}, observed",
                host.session_id.0
            )),
            "{sentence}"
        );
        assert!(
            sentence.contains(&format!(
                "; the evaluation declared revision content-revision-2, {compared}; whatever revision such a change reports"
            )),
            "{sentence}"
        );
        assert!(
            sentence.ends_with("so request a fresh evaluation."),
            "{sentence}"
        );

        // A submission that keeps the pre-change cut is refused as a source
        // move decided by that observation, worded as the barrier it is.
        let mut resubmitted = declared.clone();
        resubmitted.attempt_key = Some("resubmitted-at-the-old-cut".into());
        let (moved, observation, message, value) = refused(record(store, &resubmitted));
        assert_eq!(moved, EvaluationBasisMove::SourceChanged, "{reported}");
        let observation = observation.expect("the refusal names the observation");
        assert_eq!(observation.observation, change.observation, "{reported}");
        assert!(!observation.admitted, "{reported}");
        assert!(
            message.contains(
                "a source change the host observed without admission was recorded after evidence basis"
            ),
            "{message}"
        );
        assert!(
            !message.contains("did not judge that revision"),
            "{message}"
        );
        assert_eq!(
            value["error"]["details"]["deciding_observation"]["admitted"],
            serde_json::Value::Bool(false),
            "{value}"
        );
        assert_eq!(
            value["error"]["details"]["deciding_observation"]["revision"],
            serde_json::Value::String(reported.into()),
            "{value}"
        );
    }
}
