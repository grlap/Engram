use super::*;
use crate::work_service::{
    WorkAcceptanceInput, WorkCompleteInput, WorkCompleteResult, WorkUpdateInput,
};

fn claimed(verbs: &AgentVerbs, criteria: Vec<String>) -> String {
    let created = verbs
        .add(
            AddInput {
                title: "Frozen criterion evidence".into(),
                acceptance: criteria,
                ..AddInput::default()
            },
            at(0),
        )
        .unwrap();
    let reference = created.value["work"]["short_ref"]
        .as_str()
        .unwrap()
        .to_owned();
    verbs
        .claim(
            ClaimInput {
                work_ref: reference.clone(),
                ttl_seconds: Some(3600),
                recover: None,
            },
            at(1),
        )
        .unwrap();
    reference
}

fn assert_disclosure(receipt: &Receipt, count: usize, positions: &[usize]) {
    let value = &receipt.value["acceptance_evidence"];
    assert_eq!(value["criteria_count"], count);
    assert_eq!(value["unlinked_count"], positions.len());
    assert_eq!(value["unlinked_positions"], json!(positions));
    assert_eq!(value["omitted_count"], 0);
    assert_eq!(
        value["unlinked_label"],
        "no evidence linked to this criterion"
    );
    for position in 1..=count {
        assert_eq!(
            receipt.text().contains(&format!(
                "criterion {position}: no evidence linked to this criterion"
            )),
            positions.contains(&position)
        );
    }
    assert!(!receipt.text().contains("no evidence exists"));
}

fn assert_service_citation_refusal(malformed: bool) {
    let (_directory, verbs, path, project) = fixture();
    let reference = claimed(&verbs, vec!["delivered".into()]);
    note(&verbs, &reference, "real run evidence", 2);
    let outside = crate::CanonicalObject::freeze(&json!({"unrelated": "artifact"})).unwrap();
    let citation = if malformed {
        "not-a-hash".to_owned()
    } else {
        outside.hash().to_string()
    };
    let store = SqliteStore::open(&path).unwrap();
    let before = store.resolve_work_ref(&project, &reference).unwrap();
    let run = store.latest_work_run(before.work_id).unwrap().unwrap();
    let error = verbs
        .service
        .work_complete_on(
            Some(&reference),
            WorkCompleteInput {
                capture: None,
                evidence: Vec::new(),
                acceptance: Some(vec![WorkAcceptanceInput {
                    criterion: Some("delivered".into()),
                    satisfied: true,
                    evidence: vec![citation.clone()],
                    note: "explicit citation".into(),
                }]),
                note: None,
                idempotency_key: "refused-criterion-citation".into(),
            },
            at(3),
        )
        .unwrap_err();
    if malformed {
        assert!(matches!(error, StoreError::InvalidWork(reason)
            if reason == "expected a lowercase 64-character SHA-256 hash"));
    } else {
        let expected = format!(
            "acceptance criterion {:?} cites evidence {citation} outside the requested completion basis",
            "delivered"
        );
        assert!(
            matches!(error, StoreError::WorkCompletionRefused { work, reason }
            if work == before.work_id && reason == expected)
        );
    }
    // Admission may record a pending protocol attempt, but never seals or
    // mutates the work/run as a consequence of either refused citation.
    assert_eq!(
        store.resolve_work_ref(&project, &reference).unwrap(),
        before
    );
    assert_eq!(store.latest_work_run(before.work_id).unwrap().unwrap(), run);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn criterion_disclosure_service_refuses_citation_outside_completion_basis() {
    assert_service_citation_refusal(false);
}

#[test]
fn criterion_disclosure_service_refuses_syntactically_invalid_citation() {
    assert_service_citation_refusal(true);
}

#[test]
fn criterion_disclosure_fitters_exclude_the_exact_final_twin_ceiling() {
    let (_directory, verbs, _, _) = fixture();
    let reference = claimed(&verbs, vec!["delivered".into()]);
    add(&verbs, "Optional child", Some(&reference), true, 2);
    let view = verbs
        .service
        .work_focus_for_agent(&reference, at(3))
        .unwrap();
    let children = verbs
        .service
        .remaining_optional_children(view.status.work.work_id, 5, at(3));
    let facts = crate::work_service::WorkAcceptanceEvidence {
        // Enough removable JSON rows to pay for the omission manifest itself;
        // this probes fitting, not the fixed-metadata success fallback.
        criteria_count: 32,
        unlinked_count: 32,
        unlinked_positions: (1..=32).collect(),
    };
    let size = |receipt: &Receipt| {
        receipt
            .text()
            .len()
            .max(serde_json::to_vec_pretty(&receipt.value).unwrap().len())
    };
    for text_dominates in [false, true] {
        let base = Receipt::assemble(
            vec![if text_dominates {
                "x".repeat(4096)
            } else {
                "done".into()
            }],
            Guidance::default(),
            json!({"padding": if text_dominates { String::new() } else { "x".repeat(4096) }}),
            false,
        );
        let boundary = size(&base);
        assert_eq!(
            base.text().len() > serde_json::to_vec_pretty(&base.value).unwrap().len(),
            text_dominates
        );
        assert!(
            crate::verbs::show::fit_show_receipt(view.clone(), |_| Ok(base.clone()), boundary)
                .is_err()
        );
        assert!(
            crate::verbs::show::fit_show_receipt(view.clone(), |_| Ok(base.clone()), boundary + 1)
                .is_ok()
        );

        let render = |page: &crate::verbs::acceptance::AcceptanceEvidence| page.append(&base);
        let full = render(&crate::verbs::acceptance::AcceptanceEvidence::new(&facts)).unwrap();
        assert_eq!(
            full.text().len() > serde_json::to_vec_pretty(&full.value).unwrap().len(),
            text_dominates
        );
        let boundary = size(&full);
        let exact = crate::verbs::acceptance::fit_done(&facts, render, boundary).unwrap();
        let roomy = crate::verbs::acceptance::fit_done(&facts, render, boundary + 1).unwrap();
        assert!(
            exact.value["acceptance_evidence"]["omitted_count"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert!(size(&exact) < boundary);
        assert_eq!(roomy.value, full.value);
        assert_eq!(roomy.text(), full.text());

        let render_children = |budget| {
            crate::verbs::child_obligations::done_with_child_obligations(
                base.lines.clone(),
                Guidance::default(),
                base.value.clone(),
                &children,
                &reference,
                budget,
            )
            .unwrap()
        };
        let full = render_children(usize::MAX);
        assert_eq!(
            full.text().len() > serde_json::to_vec_pretty(&full.value).unwrap().len(),
            text_dominates
        );
        assert_eq!(
            full.value["child_obligations"]["open_optional"]["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let boundary = size(&full);
        let exact = render_children(boundary);
        let roomy = render_children(boundary + 1);
        assert_eq!(
            exact.value["child_obligations"]["open_optional"]["omitted"],
            1
        );
        assert!(size(&exact) < boundary);
        assert_eq!(roomy.value, full.value);
        assert_eq!(roomy.text(), full.text());
    }
}

#[test]
fn criterion_disclosure_words_keep_work_evidence_separate_and_frozen() {
    let (_directory, verbs, path, _) = fixture();
    let reference = claimed(&verbs, vec!["first".into(), "second".into()]);
    note(
        &verbs,
        &reference,
        "first and second are covered by this note",
        2,
    );
    verbs
        .gate(
            GateInput {
                work_ref: Some(reference.clone()),
                name: "both".into(),
                failed: Vec::new(),
                evidence_ref: None,
            },
            at(3),
        )
        .unwrap();
    let input = DoneInput {
        work_ref: Some(reference.clone()),
        summary: Some("Both delivered".into()),
        note: Some("This shared note is not a criterion citation".into()),
    };
    let done = verbs.done(input.clone(), at(4)).unwrap();
    assert!(!done.owed);
    assert_disclosure(&done, 2, &[1, 2]);
    let hash: ObjectHash = serde_json::from_value(done.value["seal"].clone()).unwrap();
    let store = SqliteStore::open(&path).unwrap();
    let seal: crate::CompletionSeal = store.get(&hash).unwrap().unwrap();
    assert!(!seal.evidence.is_empty());
    assert!(
        seal.acceptance
            .iter()
            .all(|row| row.satisfied && row.evidence.is_empty())
    );
    let canonical = crate::CanonicalObject::freeze(&seal).unwrap();
    note(
        &verbs,
        &reference,
        "late evidence must not clear either frozen label",
        5,
    );
    let replay = verbs.done(input, at(6)).unwrap();
    let show = verbs.show(&reference, at(7)).unwrap();
    for receipt in [&replay, &show] {
        assert_disclosure(receipt, 2, &[1, 2]);
        assert_eq!(
            receipt.value["acceptance_evidence"],
            done.value["acceptance_evidence"]
        );
    }
    let retained: crate::CompletionSeal = store.get(&hash).unwrap().unwrap();
    assert_eq!(
        crate::CanonicalObject::freeze(&retained).unwrap().bytes(),
        canonical.bytes()
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn criterion_disclosure_explicit_and_mixed_core_inputs_use_seal_positions() {
    for linked in [vec![true, true, true], vec![false, true, false]] {
        let (_directory, verbs, path, _) = fixture();
        // Shared prefixes do not identify criteria; the seal's position does.
        let criteria: Vec<_> = (1..=3)
            .map(|index| format!("{} tail {index}", "same ".repeat(30)))
            .collect();
        let reference = claimed(&verbs, criteria.clone());
        let evidence: ObjectHash = serde_json::from_value(
            verbs
                .service
                .work_update_on(
                    Some(&reference),
                    WorkUpdateInput::Evidence {
                        summary: "one artifact can genuinely support several criteria".into(),
                        refs: Vec::new(),
                        attach: None,
                        idempotency_key: "criterion-artifact".into(),
                    },
                    at(2),
                )
                .unwrap()
                .receipt
                .result,
        )
        .unwrap();
        verbs
            .service
            .work_update_on(
                Some(&reference),
                WorkUpdateInput::Checkpoint {
                    summary: "exact evidence cut".into(),
                    evidence: None,
                    idempotency_key: "criterion-checkpoint".into(),
                },
                at(2),
            )
            .unwrap();
        let input = WorkCompleteInput {
            capture: None,
            evidence: Vec::new(),
            note: None,
            idempotency_key: "criterion-complete".into(),
            acceptance: Some(
                criteria
                    .iter()
                    .zip(&linked)
                    .map(|(criterion, linked)| WorkAcceptanceInput {
                        criterion: Some(criterion.clone()),
                        satisfied: true,
                        evidence: if *linked {
                            vec![evidence.to_string()]
                        } else {
                            Vec::new()
                        },
                        note: "explicit assertion".into(),
                    })
                    .collect(),
            ),
        };
        let first = verbs
            .service
            .work_complete_on(Some(&reference), input.clone(), at(3))
            .unwrap();
        let replay = verbs
            .service
            .work_complete_on(Some(&reference), input, at(4))
            .unwrap();
        let positions: Vec<_> = linked
            .iter()
            .enumerate()
            .filter_map(|(i, linked)| (!linked).then_some(i + 1))
            .collect();
        for result in [first, replay] {
            let WorkCompleteResult::Completed(receipt) = result else {
                panic!("expected completion")
            };
            assert_eq!(
                receipt.acceptance_evidence.unwrap().unlinked_positions,
                positions
            );
        }
        // A word retry of core-completed work must disclose that same frozen seal.
        let done = verbs
            .done(
                DoneInput {
                    work_ref: Some(reference.clone()),
                    ..DoneInput::default()
                },
                at(5),
            )
            .unwrap();
        let show = verbs.show(&reference, at(6)).unwrap();
        assert_disclosure(&done, 3, &positions);
        assert_disclosure(&show, 3, &positions);
        let store = SqliteStore::open(&path).unwrap();
        let seal: crate::CompletionSeal = store
            .get(&serde_json::from_value(done.value["seal"].clone()).unwrap())
            .unwrap()
            .unwrap();
        for (row, linked) in seal.acceptance.iter().zip(linked) {
            assert!(row.satisfied);
            assert_eq!(
                row.evidence,
                if linked {
                    vec![evidence.clone()]
                } else {
                    Vec::new()
                }
            );
            if linked {
                // Equality with the entire work evidence set is not a reason
                // to reinterpret or discard an explicit criterion binding.
                assert_eq!(row.evidence, seal.evidence);
            }
        }
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn criterion_disclosure_restored_completion_states_unavailability_without_counts() {
    let (directory, verbs, path, project) = fixture();
    let reference = claimed(&verbs, vec!["original criterion".into()]);
    verbs
        .done(
            DoneInput {
                work_ref: Some(reference.clone()),
                summary: Some("delivered".into()),
                ..DoneInput::default()
            },
            at(2),
        )
        .unwrap();
    let document = super::review::snapshot(&path, &project, &reference);
    let (restored, store, _) = super::review::load(directory.path(), &document);
    let show = restored.show(&reference, at(102)).unwrap();
    assert_eq!(show.value["status"]["work"]["lifecycle"], "completed");
    assert!(show.value.get("acceptance_evidence").is_none());
    let explanation =
        "this store holds no per-criterion evidence record for this restored completion";
    assert_eq!(show.value["acceptance_evidence_unavailable"], explanation);
    assert!(show.text().contains(explanation));
    assert!(!show.text().contains("criteria unlinked"));
    assert!(!show.text().contains("criterion 1:"));
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn criterion_disclosure_large_seals_fit_with_exact_positional_omissions() {
    let (_directory, verbs, _, _) = fixture();
    let count = 400;
    let reference = claimed(
        &verbs,
        (1..=count).map(|i| format!("Criterion {i}")).collect(),
    );
    let done = verbs
        .done(
            DoneInput {
                work_ref: Some(reference.clone()),
                summary: Some("All asserted".into()),
                ..DoneInput::default()
            },
            at(2),
        )
        .unwrap();
    let show = verbs.show(&reference, at(3)).unwrap();
    for receipt in [done, show] {
        assert!(!receipt.owed);
        assert!(receipt.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
        assert!(
            serde_json::to_vec_pretty(&receipt.value).unwrap().len()
                < MAX_AGENT_WORK_RESPONSE_BYTES
        );
        let page = &receipt.value["acceptance_evidence"];
        let positions = page["unlinked_positions"].as_array().unwrap();
        assert!(!positions.is_empty() && positions.len() < count);
        assert_eq!(page["unlinked_count"], count);
        assert_eq!(page["omitted_count"], count - positions.len());
        assert_eq!(
            *positions,
            (1..=positions.len()).map(|i| json!(i)).collect::<Vec<_>>()
        );
        assert!(receipt.text().contains(&format!(
            "{} more unlinked criteria not shown",
            count - positions.len()
        )));
        assert!(
            receipt.value["omissions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["reason"] == "byte_budget")
        );
    }
}

fn assert_bounded_disclosure(receipt: &Receipt, count: usize) {
    assert!(receipt.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
    assert!(
        serde_json::to_vec_pretty(&receipt.value).unwrap().len() < MAX_AGENT_WORK_RESPONSE_BYTES
    );
    let page = &receipt.value["acceptance_evidence"];
    let positions = page["unlinked_positions"].as_array().unwrap();
    assert!(!positions.is_empty() && positions.len() < count);
    assert_eq!(page["criteria_count"], count);
    assert_eq!(page["unlinked_count"], count);
    assert_eq!(page["omitted_count"], count - positions.len());
    assert!(receipt.text().contains(&format!(
        "criterion evidence: {count} of {count} criteria unlinked ({} shown)",
        positions.len()
    )));
    for position in 1..=count {
        assert_eq!(
            receipt.text().contains(&format!(
                "criterion {position}: no evidence linked to this criterion"
            )),
            positions.contains(&json!(position))
        );
    }
    assert!(receipt.text().contains(&format!(
        "{} more unlinked criteria not shown",
        count - positions.len()
    )));
    assert!(!receipt.text().contains("no evidence exists"));
}

#[test]
fn criterion_disclosure_window_refitting_keeps_both_twins_and_one_omission_total() {
    let (_directory, verbs, _, _) = fixture();
    let count = 400;
    let reference = claimed(
        &verbs,
        (1..=count).map(|i| format!("Criterion {i}")).collect(),
    );
    for time in 2..=6 {
        note(&verbs, &reference, &format!("record {time}"), time);
    }
    verbs
        .done(
            DoneInput {
                work_ref: Some(reference.clone()),
                summary: Some("All asserted".into()),
                ..DoneInput::default()
            },
            at(7),
        )
        .unwrap();
    for history in [false, true] {
        let receipt = verbs
            .show_records(
                &reference,
                &crate::verbs::ShowInput {
                    notes: !history,
                    history,
                    ..crate::verbs::ShowInput::default()
                },
                at(8),
            )
            .unwrap();
        assert_bounded_disclosure(&receipt, count);
        let (window, marker) = if history {
            (&receipt.value["history"]["window"], "history: window ")
        } else {
            (&receipt.value["notes_window"], "notes: window ")
        };
        assert!(window["total"].as_u64().unwrap() > 1);
        assert!(window["shown"].as_u64().unwrap() >= 1);
        let text = receipt.text();
        assert!(text.find("criterion evidence:").unwrap() < text.find(marker).unwrap());
        let omissions: Vec<_> = receipt.value["omissions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["section"] == "focus" && row["reason"] == "byte_budget")
            .collect();
        assert_eq!(omissions.len(), 1);
        let acceptance_text_omitted = receipt.value["status"]["work"]["acceptance_omitted"]
            .as_u64()
            .unwrap();
        let positions_omitted = receipt.value["acceptance_evidence"]["omitted_count"]
            .as_u64()
            .unwrap();
        // Other context can be shed, but neither acceptance component can be lost.
        assert!(
            omissions[0]["omitted_count"].as_u64().unwrap()
                >= acceptance_text_omitted + positions_omitted
        );
    }
}

#[test]
fn criterion_disclosure_composes_many_positions_with_optional_child_guidance() {
    let (_directory, verbs, path, project) = fixture();
    let count = 400;
    let reference = claimed(
        &verbs,
        (1..=count).map(|i| format!("Criterion {i}")).collect(),
    );
    let children: Vec<_> = (2..=9)
        .map(|time| {
            add(
                &verbs,
                &format!("Optional {time}"),
                Some(&reference),
                true,
                time,
            )
        })
        .collect();
    let store = SqliteStore::open(&path).unwrap();
    let before: Vec<_> = children
        .iter()
        .map(|child| store.resolve_work_ref(&project, child).unwrap())
        .collect();
    let input = DoneInput {
        work_ref: Some(reference.clone()),
        summary: Some("Parent delivered".into()),
        ..DoneInput::default()
    };
    let done = verbs.done(input.clone(), at(10)).unwrap();
    let replay = verbs.done(input, at(11)).unwrap();
    for receipt in [&done, &replay] {
        assert!(!receipt.owed);
        assert_bounded_disclosure(receipt, count);
        let group = &receipt.value["child_obligations"]["open_optional"];
        let shown = group["items"].as_array().unwrap().len();
        assert!(shown < children.len());
        assert_eq!(group["count"], children.len());
        assert_eq!(group["omitted"], children.len() - shown);
        assert!(receipt.text().contains(&format!(
            "open optional children ({shown} of {} shown)",
            children.len()
        )));
        assert!(receipt.text().contains(&format!(
            "{} more open optional children not shown",
            children.len() - shown
        )));
        assert_eq!(group["navigation"], format!("engram work show {reference}"));
        // Done keeps child follow-ups before criterion disclosure; the header
        // placement rule belongs to show's record windows, not this receipt.
        let text = receipt.text();
        let child_end = text
            .find("optional children do not block this completion")
            .unwrap();
        let disclosure = text.find("criterion evidence:").unwrap();
        assert!(child_end < disclosure);
        assert!(disclosure < text.find("\nnext:").unwrap());
        for row in group["items"].as_array().unwrap() {
            assert!(receipt.text().contains(row["remedy"].as_str().unwrap()));
        }
    }
    assert_eq!(
        done.value["acceptance_evidence"],
        replay.value["acceptance_evidence"]
    );
    assert_eq!(
        done.value["child_obligations"],
        replay.value["child_obligations"]
    );
    for item in before {
        assert_eq!(
            store.resolve_work_ref(&project, &item.short_ref).unwrap(),
            item
        );
    }
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn criterion_disclosure_absent_transient_facts_still_render_optional_children() {
    let (_directory, verbs, _, _) = fixture();
    let reference = claimed(&verbs, vec!["delivered".into()]);
    let child = add(&verbs, "Optional follow-up", Some(&reference), true, 2);
    let result = verbs
        .service
        .work_complete_on(
            Some(&reference),
            WorkCompleteInput {
                capture: Some(crate::work_service::WorkCompletionCaptureInput {
                    summary: "delivered".into(),
                    refs: Vec::new(),
                }),
                evidence: Vec::new(),
                acceptance: None,
                note: None,
                idempotency_key: "missing-transient-facts".into(),
            },
            at(3),
        )
        .unwrap();
    let restored: WorkCompleteResult =
        serde_json::from_value(serde_json::to_value(result).unwrap()).unwrap();
    let WorkCompleteResult::Completed(completed) = restored else {
        panic!("expected completion")
    };
    assert!(completed.acceptance_evidence.is_none());
    let children = verbs
        .service
        .remaining_optional_children(completed.work_id, 5, at(4));
    let base = Receipt::assemble(
        vec!["done parent".into()],
        Guidance::default(),
        json!({"completed":true}),
        false,
    );
    let receipt = crate::verbs::child_obligations::done_with_acceptance(
        &base,
        completed.acceptance_evidence.as_ref(),
        completed.acceptance_evidence_error_class,
        &children,
        &reference,
        MAX_AGENT_WORK_RESPONSE_BYTES,
    )
    .unwrap();
    assert!(!receipt.owed);
    assert!(receipt.value.get("acceptance_evidence").is_none());
    assert_eq!(
        receipt.value["acceptance_evidence_unavailable"],
        crate::verbs::acceptance::REPLAY_UNAVAILABLE
    );
    assert!(
        receipt
            .text()
            .contains(crate::verbs::acceptance::REPLAY_UNAVAILABLE)
    );
    let minimal = crate::verbs::child_obligations::done_with_acceptance(
        &base,
        None,
        Some("canonical_object_invalid"),
        &children,
        &reference,
        1,
    )
    .unwrap();
    assert!(!minimal.owed);
    assert_eq!(minimal.value["completed"], true);
    assert_eq!(
        minimal.value["acceptance_evidence_error_class"],
        "canonical_object_invalid"
    );
    assert_eq!(
        minimal.value["child_obligations"]["open_optional"]["items"],
        json!([])
    );
    assert_eq!(
        minimal.value["child_obligations"]["open_optional"]["count"],
        1
    );
    assert_eq!(
        minimal.value["child_obligations"]["open_optional"]["omitted"],
        1
    );
    // Impossible budgets do not erase diagnostics from empty/error child pages
    // either. These exits have no advisory rows left to shed in the first place.
    for empty_or_error in [
        Ok(crate::work_service::WorkChildFollowupPage {
            items: Vec::new(),
            total: 0,
        }),
        Err(StoreError::InvalidWorkProjection(
            "diagnostic failed".into(),
        )),
    ] {
        let minimal = crate::verbs::child_obligations::done_with_acceptance(
            &base,
            None,
            Some("canonical_object_invalid"),
            &empty_or_error,
            &reference,
            1,
        )
        .unwrap();
        assert!(!minimal.owed);
        assert_eq!(minimal.value["completed"], true);
        assert_eq!(
            minimal.value["acceptance_evidence_error_class"],
            "canonical_object_invalid"
        );
        assert!(minimal.value.get("child_obligations").is_none());
        if empty_or_error.is_err() {
            assert_eq!(minimal.value["child_obligations_unavailable"], true);
            assert_eq!(
                minimal.value["child_obligations_error_class"],
                "work_projection_invalid"
            );
        } else {
            assert!(minimal.value.get("child_obligations_unavailable").is_none());
        }
    }
    let group = &receipt.value["child_obligations"]["open_optional"];
    assert_eq!(group["count"], 1);
    assert_eq!(group["omitted"], 0);
    assert_eq!(group["items"][0]["ref"], child);
    assert!(
        receipt
            .text()
            .contains(group["items"][0]["remedy"].as_str().unwrap())
    );
}

#[test]
fn criterion_disclosure_zero_criteria_emit_nothing_and_omissions_merge_exactly() {
    use crate::verbs::acceptance::AcceptanceEvidence;
    use crate::work_service::WorkAcceptanceEvidence;
    let base = Receipt::assemble(
        vec!["done parent".into()],
        Guidance::default(),
        json!({
            "completed": true,
            "omissions": [{"section":"focus", "reason":"byte_budget", "omitted_count":7}]
        }),
        false,
    );
    let zero = WorkAcceptanceEvidence {
        criteria_count: 0,
        unlinked_count: 0,
        unlinked_positions: Vec::new(),
    };
    let page = AcceptanceEvidence::new(&zero);
    assert!(page.lines().is_empty());
    let unchanged = page.append(&base).unwrap();
    assert_eq!(unchanged.text(), base.text());
    assert_eq!(unchanged.value, base.value);
    let partial = AcceptanceEvidence::new(&WorkAcceptanceEvidence {
        criteria_count: 5,
        unlinked_count: 5,
        unlinked_positions: vec![1, 2],
    })
    .append(&base)
    .unwrap();
    assert_eq!(
        partial.value["omissions"],
        json!([{"section":"focus", "reason":"byte_budget", "omitted_count":10}])
    );

    let (_directory, verbs, _, _) = fixture();
    let reference = claimed(&verbs, vec!["decorative text is separate".into()]);
    let mut view = verbs
        .service
        .work_focus_for_agent(&reference, at(2))
        .unwrap();
    view.status.work.acceptance.clear();
    view.status.work.acceptance_count = 0;
    view.acceptance_evidence = Some(zero);
    let show = verbs.render_show(&view, at(2)).unwrap();
    assert!(show.value.get("acceptance_evidence").is_none());
    assert!(!show.text().contains("criterion evidence:"));
}

#[test]
fn criterion_disclosure_damaged_native_run_refuses_without_writes() {
    let (_directory, verbs, path, project) = fixture();
    let reference = claimed(&verbs, vec!["delivered".into()]);
    verbs
        .done(
            DoneInput {
                work_ref: Some(reference.clone()),
                summary: Some("delivered".into()),
                ..DoneInput::default()
            },
            at(2),
        )
        .unwrap();
    let store = SqliteStore::open(&path).unwrap();
    let work = store.resolve_work_ref(&project, &reference).unwrap();
    let run = store.latest_work_run(work.work_id).unwrap().unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute(
        "UPDATE work_runs SET completion_seal_hash = NULL, run_json = CAST(json_set(run_json, '$.completion_seal', NULL) AS BLOB) WHERE work_id = ?1",
        [work.work_id.0.to_string()],
    ).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    for input in [
        crate::verbs::ShowInput::default(),
        crate::verbs::ShowInput {
            notes: true,
            ..crate::verbs::ShowInput::default()
        },
    ] {
        let error = verbs.show_records(&reference, &input, at(3)).unwrap_err();
        let expected = format!(
            "work run {:?} differs from its scalar or canonical event binding",
            run.run_id
        );
        assert!(
            matches!(error.error, StoreError::InvalidWorkProjection(ref reason) if reason == &expected),
            "unexpected refusal: {error:?}"
        );
    }
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        before
    );
}

#[test]
fn criterion_disclosure_seal_failure_preserves_replay_and_readable_audit_context() {
    for missing in [true, false] {
        let (_directory, verbs, path, _) = fixture();
        let reference = claimed(&verbs, vec!["delivered".into()]);
        let core_input = WorkCompleteInput {
            capture: Some(crate::work_service::WorkCompletionCaptureInput {
                summary: "delivered".into(),
                refs: Vec::new(),
            }),
            evidence: Vec::new(),
            acceptance: None,
            note: None,
            idempotency_key: "diagnostic-replay".into(),
        };
        let first = verbs
            .service
            .work_complete_on(Some(&reference), core_input.clone(), at(2))
            .unwrap();
        let original = serde_json::to_value(&first).unwrap();
        let WorkCompleteResult::Completed(completed) = first else {
            panic!("expected completion")
        };
        let word = DoneInput {
            work_ref: Some(reference.clone()),
            ..DoneInput::default()
        };
        // Seed the keyless post-completion attempt while the seal is readable.
        // The tested retries then recover existing canonical result bytes.
        let warm = verbs.done(word.clone(), at(3)).unwrap();
        assert_disclosure(&warm, 1, &[1]);
        note(&verbs, &reference, "audit trail remains readable", 3);
        let read_inputs = [
            crate::verbs::ShowInput::default(),
            crate::verbs::ShowInput {
                notes: true,
                ..crate::verbs::ShowInput::default()
            },
            crate::verbs::ShowInput {
                notes: true,
                gates: true,
                ..crate::verbs::ShowInput::default()
            },
            crate::verbs::ShowInput {
                history: true,
                ..crate::verbs::ShowInput::default()
            },
        ];
        let healthy_reads: Vec<_> = read_inputs
            .iter()
            .map(|input| {
                let receipt = verbs.show_records(&reference, input, at(4)).unwrap();
                assert!(receipt.text().contains("audit trail remains readable"));
                receipt
            })
            .collect();
        let connection = rusqlite::Connection::open(&path).unwrap();
        if missing {
            // Inject offline object loss only in this disposable fixture;
            // ordinary writes correctly prevent deleting a referenced seal.
            connection
                .pragma_update(None, "foreign_keys", false)
                .unwrap();
            connection
                .execute(
                    "DELETE FROM objects WHERE object_hash = ?1",
                    [completed.seal.as_str()],
                )
                .unwrap();
            connection
                .pragma_update(None, "foreign_keys", true)
                .unwrap();
        } else {
            connection
                .execute(
                    "UPDATE objects SET canonical_json = CAST('{}' AS BLOB) WHERE object_hash = ?1",
                    [completed.seal.as_str()],
                )
                .unwrap();
        }
        // Explicit targeting refreshes focus time; normalize it before taking
        // the no-additional-writes oracle, using the same timestamp as retries.
        verbs.service.select_work(&reference, at(4)).unwrap();
        let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
        let replay = verbs
            .service
            .work_complete_on(Some(&reference), core_input, at(4))
            .unwrap();
        assert_eq!(serde_json::to_value(&replay).unwrap(), original);
        let WorkCompleteResult::Completed(replayed) = replay else {
            panic!("replay must retain success")
        };
        assert!(replayed.acceptance_evidence.is_none());
        let error_class = if missing {
            "work_projection_invalid"
        } else {
            "canonical_object_invalid"
        };
        assert_eq!(replayed.acceptance_evidence_error_class, Some(error_class));
        let done = verbs.done(word, at(4)).unwrap();
        assert_eq!(done.value["acceptance_evidence_error_class"], error_class);
        assert!(
            done.text()
                .contains(&format!("diagnostic class: {error_class}"))
        );
        assert!(!done.owed);
        assert_eq!(done.value["seal"], warm.value["seal"]);
        assert_eq!(done.value["completed_at"], warm.value["completed_at"]);
        assert!(done.value.get("acceptance_evidence").is_none());
        assert_eq!(
            done.value["acceptance_evidence_unavailable"],
            crate::verbs::acceptance::REPLAY_UNAVAILABLE
        );
        assert!(
            done.text()
                .contains(crate::verbs::acceptance::REPLAY_UNAVAILABLE)
        );
        assert!(!done.text().contains("criteria unlinked"));
        assert!(!done.text().contains("criterion 1:"));
        assert!(done.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
        assert!(
            serde_json::to_vec_pretty(&done.value).unwrap().len() < MAX_AGENT_WORK_RESPONSE_BYTES
        );
        for (input, healthy) in read_inputs.iter().zip(healthy_reads) {
            let readable = verbs.show_records(&reference, input, at(4)).unwrap();
            assert!(!readable.owed);
            assert!(readable.value.get("acceptance_evidence").is_none());
            assert_eq!(
                readable.value["acceptance_evidence_unavailable"],
                crate::verbs::acceptance::REPLAY_UNAVAILABLE
            );
            assert_eq!(
                readable.value["acceptance_evidence_error_class"],
                error_class
            );
            let text = readable.text();
            assert!(text.contains(crate::verbs::acceptance::REPLAY_UNAVAILABLE));
            assert!(text.contains(&format!("diagnostic class: {error_class}")));
            assert!(text.contains("audit trail remains readable"));
            assert!(!text.contains("criteria unlinked"));
            assert!(text.len() < MAX_AGENT_WORK_RESPONSE_BYTES);
            assert!(
                serde_json::to_vec_pretty(&readable.value).unwrap().len()
                    < MAX_AGENT_WORK_RESPONSE_BYTES
            );
            // All item, note/history rows, navigation and read-cut data survive;
            // only the failed advisory disclosure changes on either read path.
            let without_disclosure = |mut value: serde_json::Value| {
                let fields = value.as_object_mut().unwrap();
                fields.remove("acceptance_evidence");
                fields.remove("acceptance_evidence_unavailable");
                fields.remove("acceptance_evidence_error_class");
                value
            };
            assert_eq!(
                without_disclosure(readable.value),
                without_disclosure(healthy.value)
            );
        }
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&connection).unwrap(),
            before
        );
    }
}

#[test]
fn criterion_disclosure_restored_predicate_is_shared_even_for_conflicting_projection_facts() {
    let (_directory, verbs, _, _) = fixture();
    let reference = claimed(&verbs, vec!["criterion".into()]);
    let mut view = verbs
        .service
        .work_focus_for_agent(&reference, at(2))
        .unwrap();
    for restored in [false, true] {
        for count in [None, Some(0), Some(1)] {
            view.completed_by_record = restored;
            view.acceptance_evidence =
                count.map(|count| crate::work_service::WorkAcceptanceEvidence {
                    criteria_count: count,
                    unlinked_count: count,
                    unlinked_positions: (1..=count).collect(),
                });
            let receipt = verbs.render_show(&view, at(2)).unwrap();
            let unavailable = restored && count.is_none();
            assert_eq!(
                receipt
                    .value
                    .get("acceptance_evidence_unavailable")
                    .is_some(),
                unavailable
            );
            assert_eq!(
                receipt
                    .text()
                    .contains(crate::verbs::acceptance::RESTORED_UNAVAILABLE),
                unavailable
            );
            assert_eq!(
                receipt.value.get("acceptance_evidence").is_some(),
                count == Some(1)
            );
            assert_eq!(
                receipt.text().contains("criteria unlinked"),
                count == Some(1)
            );
        }
    }
}

#[test]
fn criterion_disclosure_invalid_hand_constructed_counts_do_not_underflow() {
    let facts = crate::work_service::WorkAcceptanceEvidence {
        criteria_count: 1,
        unlinked_count: 1,
        unlinked_positions: vec![1, 2],
    };
    // Canonical producers obey the documented subset invariant. A malformed
    // transient projection still must not panic while computing its remainder.
    let page = crate::verbs::acceptance::AcceptanceEvidence::new(&facts);
    assert_eq!(page.omitted_count(), 0);
}
