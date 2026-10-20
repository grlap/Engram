use super::*;
use crate::storage::{SealedSourceTransportFixture, sealed_source_transport_fixture};

fn verbs(fixture: &SealedSourceTransportFixture) -> AgentVerbs {
    AgentVerbs::new(
        fixture.database.clone(),
        fixture.work.project_id.clone(),
        "runner".into(),
        SessionId("runner".into()),
        None,
    )
}

fn assert_terse_source(value: &Value, text: &str) {
    for field in [
        "seal",
        "evaluation",
        "run",
        "root_execution",
        "original_claim",
        "named_root_binding",
        "judged_source",
        "first_difference",
        "latest_endpoint",
        "evaluation_detail",
    ] {
        assert!(value.get(field).is_none(), "raw field {field}: {value}");
    }
    let emitted = format!("{value} {text}");
    for word in emitted.split(|ch: char| !ch.is_ascii_hexdigit() && ch != '-') {
        assert!(
            !matches!(word.len(), 32 | 64) || !word.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "raw identity or fingerprint: {word}"
        );
        if word.len() == 36 {
            let groups = word.split('-').map(str::len).collect::<Vec<_>>();
            assert_ne!(groups, [8, 4, 4, 4, 12], "raw UUID: {word}");
        }
    }
}

fn assert_terse_source_receipt(receipt: &Receipt, value: &Value) {
    // Criterion evidence has its own explicit record locators. Check the
    // actual diagnostic text block, alongside its structured projection.
    let start = receipt
        .lines
        .iter()
        .position(|line| {
            line.trim_start().starts_with("sealed source:")
                || line
                    .trim_start()
                    .starts_with("sealed source diagnostic unavailable")
        })
        .unwrap();
    let lines = receipt.lines[start..]
        .iter()
        .enumerate()
        .take_while(|(index, line)| *index == 0 || line.starts_with("  "))
        .map(|(_, line)| line.as_str())
        .collect::<Vec<_>>();
    assert_terse_source(value, &lines.join("\n"));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the same real sealed history is read through show, next and historical pages"
)]
fn sealed_source_show_and_next_share_bounded_wording_and_historical_navigation() {
    for (case, expected) in [
        (
            "difference",
            "later source sighting differs from sealed evaluated source",
        ),
        ("agreement", "reported endpoint agreement"),
        ("missing", "no comparable later closing sighting"),
    ] {
        let mut fixture = sealed_source_transport_fixture(case);
        let words = verbs(&fixture);
        let now = Utc::now();
        let shown = words.show(&fixture.work.short_ref, now).unwrap();
        assert_eq!(shown.value["sealed_source"]["status"], expected);
        assert_terse_source_receipt(&shown, &shown.value["sealed_source"]);
        assert!(shown.text().contains(expected));
        assert!(shown.text().contains("Completion remains final"));
        assert!(emitted_receipt_bytes(&shown) <= MAX_AGENT_WORK_RESPONSE_BYTES);
        words
            .service
            .work_focus(&fixture.work.short_ref, now)
            .unwrap();
        let next = words
            .next(
                &NextInput {
                    peek: true,
                    ..Default::default()
                },
                now,
            )
            .unwrap();
        assert_terse_source_receipt(&next, &next.value["focus"]["sealed_source"]);
        for field in ["status", "differences", "agreements", "observations_detail"] {
            assert_eq!(
                next.value["focus"]["sealed_source"][field],
                shown.value["sealed_source"][field]
            );
        }
        assert!(next.text().contains(expected));
        let command = shown.value["sealed_source"]["observations_detail"]
            .as_str()
            .unwrap();
        let token = command.split(" --after ").nth(1).unwrap();
        let page = words
            .show_observations(&fixture.work.short_ref, Some(token), now, 12_000)
            .unwrap();
        assert!(emitted_receipt_bytes(&page) <= 12_000);
        assert_eq!(page.value["sealed_source"]["seal"], fixture.seal.as_str());
        if case != "missing" {
            assert!(page.text().contains("root state named"));
            assert!(!page.text().contains("root state Some("));
        }
        if case == "difference" {
            assert_eq!(page.value["sealed_source"]["differences"], 1);
            assert_eq!(
                page.value["sealed_source"]["latest_endpoint"]["comparison"],
                "agreement"
            );
            let after = page.value["observations_window"]["after"]
                .as_str()
                .unwrap()
                .to_owned();
            let old_run = page.value["sealed_source"]["run"].clone();
            fixture.append_later_sighting();
            words
                .service
                .work_update(
                    WorkUpdateInput::Reopen {
                        reason: "a distinct new run".into(),
                        idempotency_key: "reopen".into(),
                    },
                    now + Duration::seconds(1),
                )
                .unwrap();
            let later = words
                .show_observations(
                    &fixture.work.short_ref,
                    Some(&after),
                    now + Duration::seconds(2),
                    12_000,
                )
                .unwrap();
            assert_eq!(later.value["sealed_source"]["run"], old_run);
            assert_eq!(later.value["sealed_source"]["seal"], fixture.seal.as_str());
            assert_eq!(
                later.value["sealed_source"]["total"], 40,
                "appended observation is outside the pinned horizon"
            );
            assert!(
                words
                    .show(&fixture.work.short_ref, now + Duration::seconds(2))
                    .unwrap()
                    .value
                    .get("sealed_source")
                    .is_none()
            );
        }
    }
}

#[test]
fn a_historical_observation_page_never_offers_its_continuation_as_the_detail_locator() {
    // The page is the observation detail; only show and next point into it.
    // Paging goes through the window's own continuation and next commands.
    let fixture = sealed_source_transport_fixture("difference");
    let words = verbs(&fixture);
    let now = Utc::now();
    let shown = words.show(&fixture.work.short_ref, now).unwrap();
    let entry = shown.value["sealed_source"]["observations_detail"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut token = entry.split(" --after ").nth(1).unwrap().to_owned();
    let mut pages = 0;
    loop {
        let page = words
            .show_observations(&fixture.work.short_ref, Some(&token), now, 12_000)
            .unwrap();
        pages += 1;
        assert!(pages <= 40, "each continuation makes progress");
        let diagnostic = &page.value["sealed_source"];
        assert_eq!(diagnostic["seal"], fixture.seal.as_str());
        assert!(
            diagnostic.get("observations_detail").is_none(),
            "page {pages}: {diagnostic}"
        );
        let Some(after) = page.value["observations_window"]["after"].as_str() else {
            break;
        };
        assert!(
            page.value["next"]
                .as_array()
                .unwrap()
                .iter()
                .any(|command| command
                    .as_str()
                    .is_some_and(|command| command.ends_with(after))),
            "page {pages} pages through its next command"
        );
        token = after.to_owned();
    }
    assert!(pages > 1, "the window has a last page after a first one");
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one replay scenario checks persisted bytes, transient hydration and own-action suppression"
)]
fn sealed_source_next_rehydrates_staged_bytes_and_preserves_an_own_late_observation() {
    let fixture = sealed_source_transport_fixture("difference");
    let words = verbs(&fixture);
    let now = Utc::now();
    words
        .service
        .work_focus(&fixture.work.short_ref, now)
        .unwrap();
    let store = SqliteStore::open(&fixture.database).unwrap();
    let query = || WorkNextQuery {
        sections: vec![WorkNextSection::Changes],
        ..Default::default()
    };
    let mut found = false;
    for _ in 0..20 {
        let first = words.service.work_next(50, query(), now).unwrap();
        let changes = first.changes.as_ref().unwrap();
        if changes.is_empty() {
            break;
        }
        let before = store
            .staged_work_session_delivery_payload(
                &fixture.work.project_id,
                &SessionId("runner".into()),
            )
            .unwrap()
            .unwrap();
        assert!(!String::from_utf8_lossy(&before).contains("sealed_source"));
        let confirmed = store
            .work_session_state(&fixture.work.project_id, &SessionId("runner".into()), now)
            .unwrap()
            .project_cursor;
        let replay = words
            .service
            .work_next_with_delivery_token(50, Some(confirmed), None, query(), now)
            .unwrap();
        assert_eq!(
            before,
            store
                .staged_work_session_delivery_payload(
                    &fixture.work.project_id,
                    &SessionId("runner".into())
                )
                .unwrap()
                .unwrap()
        );
        let fresh = changes
            .iter()
            .filter_map(|change| change.sealed_source.as_ref())
            .count();
        let hydrated = replay
            .changes
            .as_ref()
            .unwrap()
            .iter()
            .filter_map(|change| change.sealed_source.as_ref())
            .count();
        assert_eq!(fresh, hydrated);
        if fresh == 0 {
            continue;
        }
        let mut changes = replay.changes.clone().unwrap();
        let duplicate = changes
            .iter()
            .find(|change| change.sealed_source.is_some())
            .unwrap()
            .clone();
        changes.push(duplicate);
        for change in &mut changes {
            if change.sealed_source.is_some() {
                change.from_current_session = true;
                let producer = change.display_producer.as_mut().unwrap();
                producer.actor_id = "runner".into();
                producer.actor_kind = crate::work_service::WORD_ACTOR_KIND.into();
                producer.session_id = Some(SessionId("runner".into()));
            }
        }
        let lines = collapsed_changes(&changes, words.service.display_identity());
        let important = lines
            .iter()
            .filter(|line| {
                line.line
                    .contains("later source sighting differs from sealed evaluated source")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            important.len(),
            1,
            "one atomic diagnostic per seal, even for own observations"
        );
        let command = important[0].source_detail.as_ref().unwrap().clone();
        assert!(command.contains(" --observations --after sd1-"));
        // Exercise both final compact emitters, with the completed item out
        // of focus. The old test stopped before short() clipped the command.
        let mut outside_focus = replay.clone();
        outside_focus.focus = None;
        let compact = crate::verbs::receipts::compact_next_receipt(
            &outside_focus,
            &[],
            &[],
            &lines,
            &std::collections::HashMap::new(),
            &Guidance::default(),
            None,
        )
        .unwrap();
        let emitted = Receipt::assemble(
            crate::verbs::receipts::compact_next_lines(&compact),
            compact.guidance.clone(),
            crate::verbs::receipts::compact_next_value(&compact),
            false,
        );
        assert!(emitted.text().contains(&command));
        let emitted_line = emitted.value["changes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .find(|line| line.contains("sealed source:"))
            .unwrap();
        assert!(
            emitted_line.contains("later source sighting differs from sealed evaluated source")
        );
        assert!(emitted_line.ends_with(&command), "{emitted_line}");
        assert!(emitted_receipt_bytes(&emitted) <= MAX_AGENT_WORK_RESPONSE_BYTES);
        let token = command.split(" --after ").nth(1).unwrap();
        let detail = words
            .show_observations(&fixture.work.short_ref, Some(token), now, 12_000)
            .unwrap();
        assert_eq!(detail.value["sealed_source"]["seal"], fixture.seal.as_str());

        for change in &mut changes {
            if let Some(source) = &mut change.sealed_source {
                source.diagnostic = None;
                source.unavailable = Some("unavailable");
                source.continuation = None;
            }
        }
        let unavailable = collapsed_changes(&changes, words.service.display_identity());
        assert_eq!(
            unavailable
                .iter()
                .filter(|row| row.line.contains("sealed source diagnostic unavailable"))
                .count(),
            1,
            "unavailable diagnostics are deduplicated by seal too"
        );
        found = true;
    }
    assert!(found, "the late observation was delivered and hydrated");
}

#[test]
fn sealed_source_unreadable_bound_evaluation_is_unavailable_not_agreement() {
    let fixture = sealed_source_transport_fixture("agreement");
    let words = verbs(&fixture);
    let view = words
        .service
        .work_focus_for_agent(&fixture.work.short_ref, Utc::now())
        .unwrap();
    let evaluation = view
        .sealed_source
        .as_ref()
        .unwrap()
        .diagnostic
        .as_ref()
        .unwrap()
        .evaluation
        .as_ref()
        .unwrap();
    let connection = rusqlite::Connection::open(&fixture.database).unwrap();
    connection
        .execute(
            "UPDATE objects SET canonical_json = CAST('not json' AS BLOB) WHERE object_id = ?1",
            [evaluation.as_str()],
        )
        .unwrap();
    let unavailable = words.show(&fixture.work.short_ref, Utc::now()).unwrap();
    assert_eq!(unavailable.value["sealed_source"]["status"], "unavailable");
    assert_terse_source_receipt(&unavailable, &unavailable.value["sealed_source"]);
    assert!(!unavailable.text().contains("reported endpoint agreement"));
}

#[test]
fn sealed_source_show_sheds_detail_before_the_outcome_and_acceptance() {
    let fixture = sealed_source_transport_fixture("agreement");
    let words = verbs(&fixture);
    let now = Utc::now();
    let full = words.show(&fixture.work.short_ref, now).unwrap();
    let budget = emitted_receipt_bytes(&full) - 1;
    let bounded = words
        .show_within(&fixture.work.short_ref, now, budget)
        .unwrap();
    assert!(emitted_receipt_bytes(&bounded) <= budget);
    assert_eq!(bounded.value["sealed_source"]["details_omitted"], true);
    assert_terse_source_receipt(&bounded, &bounded.value["sealed_source"]);
    assert_eq!(
        bounded.value["sealed_source"]["status"],
        full.value["sealed_source"]["status"]
    );
    assert_eq!(
        bounded.value["status"]["work"],
        full.value["status"]["work"]
    );
    assert_eq!(
        bounded.value["acceptance_evidence"],
        full.value["acceptance_evidence"]
    );
    assert_eq!(
        bounded.value["sealed_source"]["observations_detail"],
        full.value["sealed_source"]["observations_detail"]
    );
    assert!(
        bounded
            .text()
            .contains("details omitted; completion remains final")
    );
}

#[test]
fn sealed_source_without_an_evaluation_is_one_short_status_line() {
    let fixture = sealed_source_transport_fixture("missing");
    let words = verbs(&fixture);
    let mut view = words
        .service
        .work_focus_for_agent(&fixture.work.short_ref, Utc::now())
        .unwrap();
    let source = view.sealed_source.as_mut().unwrap();
    let diagnostic = source.diagnostic.as_mut().unwrap();
    diagnostic.evaluation = None;
    diagnostic.judged = None;
    let rendered =
        crate::verbs::sealed_source::SourceDiagnostic::terse(source, &fixture.work.short_ref);
    assert_eq!(rendered.lines, ["sealed source: no bound evaluation"]);
    assert_eq!(rendered.value["status"], "no bound evaluation");
    assert!(serde_json::to_vec(&rendered.value).unwrap().len() < 200);
    assert!(rendered.detail.is_none());
    assert_terse_source(&rendered.value, &rendered.lines.join("\n"));
}

const UNAVAILABLE_JUDGED_BASIS: &str =
    "judged source or retained matching root basis is unavailable";

fn unavailable_judged_view(
    words: &AgentVerbs,
    fixture: &SealedSourceTransportFixture,
    now: DateTime<Utc>,
) -> crate::work_service::WorkFocusView {
    let mut view = words
        .service
        .work_focus_for_agent(&fixture.work.short_ref, now)
        .unwrap();
    // Exercise a legitimate unavailable read-model state without altering
    // canonical evaluation records or the fixture's sealed history.
    let facts = view
        .sealed_source
        .as_mut()
        .unwrap()
        .diagnostic
        .as_mut()
        .unwrap();
    assert!(facts.evaluation.is_some());
    facts.judged = None;
    facts.unavailable = Some(UNAVAILABLE_JUDGED_BASIS.into());
    facts.incomparable = facts.total;
    facts.differences = 0;
    facts.agreements = 0;
    facts.first_difference = None;
    facts.latest_endpoint = None;
    view
}

fn render_source_next(
    view: &crate::work_service::WorkNextView,
    changes: &[crate::verbs::next_context::CompactChange],
) -> Receipt {
    let compact = crate::verbs::receipts::compact_next_receipt(
        view,
        &[],
        &[],
        changes,
        &std::collections::HashMap::new(),
        &Guidance::default(),
        None,
    )
    .unwrap();
    Receipt::assemble(
        crate::verbs::receipts::compact_next_lines(&compact),
        compact.guidance.clone(),
        crate::verbs::receipts::compact_next_value(&compact),
        false,
    )
}

#[test]
fn sealed_source_text_unavailable_basis_survives_show_shedding() {
    let fixture = sealed_source_transport_fixture("agreement");
    let words = verbs(&fixture);
    let now = Utc::now();
    let mut view = unavailable_judged_view(&words, &fixture, now);
    for shed in [false, true] {
        view.sealed_source.as_mut().unwrap().details_omitted = shed;
        let shown = words.render_show(&view, now).unwrap();
        assert_eq!(
            shown.value["sealed_source"]["status"],
            UNAVAILABLE_JUDGED_BASIS
        );
        assert!(
            shown
                .text()
                .contains(&format!("sealed source: {UNAVAILABLE_JUDGED_BASIS}"))
        );
        assert!(
            !shown
                .text()
                .contains("no comparable later closing sighting")
        );
        assert_terse_source_receipt(&shown, &shown.value["sealed_source"]);
        assert!(
            crate::verbs::receipts::agent_receipt_fits(&shown, MAX_AGENT_WORK_RESPONSE_BYTES)
                .unwrap()
        );
    }
}

#[test]
fn sealed_source_text_unavailable_basis_survives_compact_focus() {
    let fixture = sealed_source_transport_fixture("agreement");
    let words = verbs(&fixture);
    let now = Utc::now();
    let mut view = words
        .service
        .work_next(50, WorkNextQuery::default(), now)
        .unwrap();
    view.focus = Some(unavailable_judged_view(&words, &fixture, now));
    for shed in [false, true] {
        view.focus
            .as_mut()
            .unwrap()
            .sealed_source
            .as_mut()
            .unwrap()
            .details_omitted = shed;
        let next = render_source_next(&view, &[]);
        assert_eq!(
            next.value["focus"]["sealed_source"]["status"],
            UNAVAILABLE_JUDGED_BASIS
        );
        assert!(next.text().contains(UNAVAILABLE_JUDGED_BASIS));
        assert_terse_source_receipt(&next, &next.value["focus"]["sealed_source"]);
        assert!(
            crate::verbs::receipts::agent_receipt_fits(&next, MAX_AGENT_WORK_RESPONSE_BYTES)
                .unwrap()
        );
    }
}

#[test]
fn sealed_source_text_unavailable_basis_survives_compact_changes() {
    let fixture = sealed_source_transport_fixture("agreement");
    let words = verbs(&fixture);
    let now = Utc::now();
    let source = unavailable_judged_view(&words, &fixture, now)
        .sealed_source
        .unwrap();
    let mut found = false;
    for _ in 0..20 {
        let mut view = words
            .service
            .work_next(
                50,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Changes],
                    ..Default::default()
                },
                now,
            )
            .unwrap();
        let Some(mut change) = view
            .changes
            .as_ref()
            .and_then(|changes| changes.iter().find(|change| change.sealed_source.is_some()))
            .cloned()
        else {
            continue;
        };
        change.sealed_source = Some(source.clone());
        view.focus = None;
        let changes = collapsed_changes(&[change], words.service.display_identity());
        let next = render_source_next(&view, &changes);
        let line = next.value["changes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .find(|line| line.contains("sealed source:"))
            .unwrap();
        assert!(line.contains(UNAVAILABLE_JUDGED_BASIS), "{line}");
        assert!(next.text().contains(UNAVAILABLE_JUDGED_BASIS));
        let command =
            crate::verbs::sealed_source::SourceDiagnostic::terse(&source, &fixture.work.short_ref)
                .detail
                .unwrap();
        assert!(line.ends_with(&command));
        assert_terse_source(&json!({"line": line}), line);
        assert!(
            crate::verbs::receipts::agent_receipt_fits(&next, MAX_AGENT_WORK_RESPONSE_BYTES)
                .unwrap()
        );
        found = true;
        break;
    }
    assert!(
        found,
        "a completed source observation reaches compact changes"
    );
}

#[test]
fn sealed_source_text_unavailable_basis_does_not_repeat_the_status_reason() {
    let fixture = sealed_source_transport_fixture("agreement");
    let words = verbs(&fixture);
    let view = unavailable_judged_view(&words, &fixture, Utc::now());
    let mut source = view.sealed_source.unwrap();
    let distinct_reason = "the evaluation's source revision was not retained";
    for reason in [None, Some(UNAVAILABLE_JUDGED_BASIS), Some(distinct_reason)] {
        source.diagnostic.as_mut().unwrap().unavailable = reason.map(str::to_owned);
        let rendered =
            crate::verbs::sealed_source::SourceDiagnostic::new(&source, &fixture.work.short_ref);
        let text = rendered.lines.join("\n");
        assert_eq!(rendered.value["status"], UNAVAILABLE_JUDGED_BASIS);
        assert_eq!(text.matches(UNAVAILABLE_JUDGED_BASIS).count(), 1);
        if reason == Some(distinct_reason) {
            assert!(text.contains(&format!("judged basis unavailable: {distinct_reason}")));
        } else {
            assert!(!text.contains("judged basis unavailable:"));
        }
    }
}

#[test]
fn sealed_source_text_generation_without_a_value_is_not_recorded() {
    let fixture = sealed_source_transport_fixture("agreement");
    let words = verbs(&fixture);
    let mut view = words
        .service
        .work_focus_for_agent(&fixture.work.short_ref, Utc::now())
        .unwrap();
    let source = view.sealed_source.as_mut().unwrap();
    source
        .diagnostic
        .as_mut()
        .unwrap()
        .judged
        .as_mut()
        .unwrap()
        .source_root_generation = None;
    let rendered =
        crate::verbs::sealed_source::SourceDiagnostic::new(source, &fixture.work.short_ref);
    assert!(rendered.value["judged_source"]["root_generation"].is_null());
    let text = rendered.lines.join("\n");
    assert!(text.contains("; generation not recorded; revision "));
    assert!(!text.contains("generation None"));
    assert!(!text.contains("generation Some("));
}

fn sealed_source_token(words: &AgentVerbs, work_ref: &str, now: DateTime<Utc>) -> String {
    let shown = words.show(work_ref, now).unwrap();
    shown.value["sealed_source"]["observations_detail"]
        .as_str()
        .unwrap()
        .split(" --after ")
        .nth(1)
        .unwrap()
        .into()
}

fn assert_explicit_source_text(page: &Receipt, budget: usize) {
    let source = &page.value["sealed_source"];
    let judged = &source["judged_source"];
    let text = page.text();
    assert!(source.get("observations_detail").is_none());
    assert!(!text.contains("retained observation details:"));
    assert!(text.contains(&format!(
        "judged source: workspace {}; generation {}; revision {}",
        judged["workspace"].as_str().unwrap(),
        judged["root_generation"].as_i64().map_or_else(
            || "not recorded".into(),
            |generation| generation.to_string(),
        ),
        judged["revision"].as_str().unwrap()
    )));
    let first = &source["first_difference"];
    assert!(text.contains(&format!(
        "earlier differing observation {} at run position {} (recorded after seal cut)",
        first["observation"].as_str().unwrap(),
        first["run_position"].as_i64().unwrap()
    )));
    let latest = &source["latest_endpoint"];
    assert!(text.contains(&format!(
        "latest comparable endpoint {} at run position {}: {}",
        latest["observation"].as_str().unwrap(),
        latest["run_position"].as_i64().unwrap(),
        latest["comparison"].as_str().unwrap()
    )));
    assert!(text.contains(&format!(
        "sealed evaluation detail: {}",
        source["evaluation_detail"].as_str().unwrap()
    )));
    assert!(text.contains("Completion remains final"));
    assert!(crate::verbs::receipts::agent_receipt_terminal_bytes(&text) < budget);
    assert!(crate::verbs::receipts::compact_receipt_json_bytes(&page.value).unwrap() < budget);
}

#[test]
fn sealed_source_text_window_retains_historical_facts_outside_visible_rows() {
    let fixture = sealed_source_transport_fixture("difference");
    let words = verbs(&fixture);
    let now = Utc::now();
    let token = sealed_source_token(&words, &fixture.work.short_ref, now);
    let mut next = Some(token);
    let mut seen = std::collections::HashSet::new();
    let mut pages = 0;
    while let Some(token) = next {
        let page = words
            .show_observations(
                &fixture.work.short_ref,
                Some(&token),
                now,
                MAX_AGENT_WORK_RESPONSE_BYTES,
            )
            .unwrap();
        assert_explicit_source_text(&page, MAX_AGENT_WORK_RESPONSE_BYTES);
        let rows = page.value["observations"].as_array().unwrap();
        if pages == 0 {
            assert!(rows.iter().all(|row| {
                row["observation"] != page.value["sealed_source"]["first_difference"]["observation"]
            }));
            assert_eq!(
                page.value["sealed_source"]["latest_endpoint"]["comparison"],
                "agreement"
            );
        }
        for row in rows {
            assert!(seen.insert(row["observation"].as_str().unwrap().to_owned()));
        }
        pages += 1;
        assert!(pages <= 40, "each continuation must make progress");
        next = page.value["observations_window"]["after"]
            .as_str()
            .map(str::to_owned);
    }
    assert_eq!(seen.len(), 40);
    assert!(pages > 1);
}

#[test]
fn sealed_source_text_window_charges_disclosures_to_its_byte_budget() {
    let fixture = sealed_source_transport_fixture("difference");
    let words = verbs(&fixture);
    let now = Utc::now();
    let token = sealed_source_token(&words, &fixture.work.short_ref, now);
    let full = words
        .show_observations(
            &fixture.work.short_ref,
            Some(&token),
            now,
            MAX_AGENT_WORK_RESPONSE_BYTES,
        )
        .unwrap();
    let budget = emitted_receipt_bytes(&full) - 1;
    let bounded = words
        .show_observations(&fixture.work.short_ref, Some(&token), now, budget)
        .unwrap();
    assert_explicit_source_text(&bounded, budget);
    assert!(
        bounded.value["observations_window"]["shown"]
            .as_u64()
            .unwrap()
            < full.value["observations_window"]["shown"].as_u64().unwrap()
    );
    // Navigation advances by the selected row count; the historical facts
    // and evaluation command remain fixed when fitting selects fewer rows.
    for field in [
        "seal",
        "evaluation",
        "run",
        "completion_cut",
        "horizon",
        "judged_source",
        "first_difference",
        "latest_endpoint",
        "total",
        "evaluation_detail",
        "limits",
    ] {
        assert_eq!(
            bounded.value["sealed_source"][field],
            full.value["sealed_source"][field]
        );
    }
    assert!(
        bounded.value["observations_window"]["after"]
            .as_str()
            .is_some()
    );
}

#[test]
fn sealed_source_text_basis_preview_bounds_utf8_and_escapes_controls() {
    let fixture = sealed_source_transport_fixture("difference");
    let words = verbs(&fixture);
    let mut view = words
        .service
        .work_focus_for_agent(&fixture.work.short_ref, Utc::now())
        .unwrap();
    let source = view.sealed_source.as_mut().unwrap();
    let facts = source.diagnostic.as_mut().unwrap();
    // A multibyte character crosses the prefix boundary. Quotes and
    // backslashes exercise JSON escaping; retained controls exercise text
    // framing without adding a physical line to the disclosed basis.
    let workspace = format!("{}é-tail", "w".repeat(255));
    let revision = format!("\"\\\u{1}\n{}é-tail", "r".repeat(250));
    let judged = facts.judged.as_mut().unwrap();
    judged.workspace_id.clone_from(&workspace);
    judged.source_revision.clone_from(&revision);
    facts.first_difference.as_mut().unwrap().source_basis = Some(judged.clone());
    let rendered =
        crate::verbs::sealed_source::SourceDiagnostic::new(source, &fixture.work.short_ref);
    let basis = &rendered.value["judged_source"];
    assert_eq!(basis["workspace"], format!("{}…", "w".repeat(255)));
    assert_eq!(basis["workspace_bytes"], workspace.len());
    assert_eq!(basis["revision_bytes"], revision.len());
    assert_eq!(basis["truncated"], true);
    let preview = basis["revision"].as_str().unwrap();
    assert!(preview.ends_with('…'));
    assert!(preview.len() <= 256 + '…'.len_utf8());
    let judged_line = rendered
        .lines
        .iter()
        .find(|line| line.contains("judged source:"))
        .unwrap();
    assert!(judged_line.contains(&terminal_safe_line(preview)));
    assert!(judged_line.contains(&format!(
        " (previews truncated; workspace {} bytes, revision {} bytes)",
        basis["workspace_bytes"], basis["revision_bytes"],
    )));
    assert!(!judged_line.contains("see sealed evaluation detail below"));
    assert!(!judged_line.contains("observations detail retains"));
    let evaluation = rendered.value["evaluation_detail"].as_str().unwrap();
    assert!(
        rendered
            .lines
            .contains(&format!("  sealed evaluation detail: {evaluation}"))
    );
    assert!(!judged_line.contains(['\u{1}', '\n', '\r']));
    let closing_line = rendered
        .lines
        .iter()
        .find(|line| line.contains("reported closing revision"))
        .unwrap();
    assert!(closing_line.contains(&terminal_safe_line(preview)));
    assert!(closing_line.contains(" (truncated; see that observation's row)"));
    assert!(!closing_line.contains("see observation details"));
    assert_eq!(
        rendered.value["first_difference"]["source_basis"]["revision"],
        preview
    );
}
