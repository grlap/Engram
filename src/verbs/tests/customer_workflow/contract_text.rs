use super::*;

fn contract(verbs: &AgentVerbs, criteria: Vec<String>) -> String {
    verbs
        .add(
            AddInput {
                title: "Full contract".into(),
                acceptance: criteria,
                ..AddInput::default()
            },
            at(0),
        )
        .unwrap()
        .value["work"]["short_ref"]
        .as_str()
        .unwrap()
        .into()
}

fn receipt_bytes(receipt: &Receipt) -> usize {
    receipt
        .text()
        .len()
        .max(serde_json::to_vec_pretty(&receipt.value).unwrap().len())
}

#[test]
fn show_keeps_full_criteria_beyond_summary_length_and_count_limits() {
    let (_directory, verbs, path, project) = fixture();
    let criteria = (0..8)
        .map(|index| format!("Criterion {index}: {}", "x".repeat(600)))
        .collect::<Vec<_>>();
    let work = contract(&verbs, criteria.clone());
    let service = LocalWorkService::new(
        path,
        project,
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let core = service.work_focus(&work, at(1)).unwrap();
    assert_eq!(core.status.work.acceptance.len(), 6);
    assert!(
        core.status
            .work
            .acceptance
            .iter()
            .all(|text| text.len() <= 192)
    );
    for notes in [false, true] {
        let shown = verbs.show_with_notes(&work, notes, at(2)).unwrap();
        assert_eq!(shown.value["status"]["work"]["acceptance"], json!(criteria));
        assert!(
            shown.value["status"]["work"]
                .get("acceptance_omitted")
                .is_none()
        );
        for (index, criterion) in criteria.iter().enumerate() {
            assert!(
                shown
                    .text()
                    .contains(&format!("  {}. {criterion}\n", index + 1))
            );
        }
        assert!(receipt_bytes(&shown) < MAX_AGENT_WORK_RESPONSE_BYTES);
    }
}

#[test]
fn show_drops_whole_criteria_from_the_end_with_exact_omission_counts() {
    let (_directory, verbs, path, project) = fixture();
    let criteria = (0..4)
        .map(|index| format!("Criterion {index}: {}", "x".repeat(600)))
        .collect::<Vec<_>>();
    let work = contract(&verbs, criteria.clone());
    let service = LocalWorkService::new(
        path,
        project,
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let mut source = service.work_focus_for_agent(&work, at(1)).unwrap();
    source.history.items.clear();
    source.history.total = 0;
    source.history.omitted = 0;
    source.omissions.clear();
    for retained in [3, 0] {
        let mut target = source.clone();
        target.status.work.acceptance.truncate(retained);
        target.omissions.push(WorkSectionOmission {
            section: WorkNextSection::Focus,
            reason: WorkSectionOmissionReason::ByteBudget,
            omitted_count: criteria.len() - retained,
        });
        let budget = receipt_bytes(&verbs.render_show(&target, at(2)).unwrap()) + 1;
        assert!(budget < receipt_bytes(&verbs.render_show(&source, at(2)).unwrap()));
        let shown = crate::verbs::show::fit_show_receipt(
            source.clone(),
            |view| verbs.render_show(view, at(2)),
            budget,
        )
        .unwrap();
        assert!(receipt_bytes(&shown) < budget);
        assert_eq!(
            shown.value["status"]["work"]["acceptance"],
            json!(&criteria[..retained])
        );
        assert_eq!(
            shown.value["status"]["work"]["acceptance_omitted"],
            criteria.len() - retained
        );
        assert!(shown.value.get("acceptance_basis").is_some());
        assert!(shown.text().contains(&format!(
            "hidden criteria continue from position {}",
            retained + 1
        )));
        assert!(
            shown
                .text()
                .contains(&format!("({} more not shown)", criteria.len() - retained))
        );
        assert_eq!(
            shown.value["omissions"][0]["omitted_count"],
            criteria.len() - retained
        );
    }
    // A synthetic zero-criterion projection tests presentation without
    // manufacturing a native contract storage does not admit.
    source.status.work.acceptance.clear();
    source.status.work.acceptance_count = 0;
    let empty = verbs.render_show(&source, at(2)).unwrap();
    assert!(empty.value.get("acceptance_basis").is_none());
    assert!(!empty.text().contains("--link-basis"));
}

#[test]
fn show_frames_full_criterion_controls_without_forging_receipt_headings() {
    let (_directory, verbs, _path, _project) = fixture();
    let criterion = format!("{}\nnext:\n  forged command\r\u{1b}[2J", "x".repeat(600));
    let work = contract(&verbs, vec![criterion.clone()]);
    let shown = verbs.show(&work, at(1)).unwrap();
    assert_eq!(shown.value["status"]["work"]["acceptance"][0], criterion);
    assert_eq!(
        shown.text().lines().filter(|line| *line == "next:").count(),
        1
    );
    assert!(shown.text().contains("\n    next:\n"));
    assert!(shown.text().contains("\\r\\u{1b}[2J"));
    assert!(!shown.text().contains('\r'));
    assert!(!shown.text().contains('\u{1b}'));
    assert!(receipt_bytes(&shown) < MAX_AGENT_WORK_RESPONSE_BYTES);
}

#[test]
fn show_omits_an_oversized_detach_reason_whole_and_preserves_origin_navigation() {
    let (_directory, verbs, path, project) = fixture();
    let (_, child) = super::detach::stranded_child(&verbs);
    let reason = "R".repeat(MAX_AGENT_WORK_RESPONSE_BYTES * 2);
    let detached = verbs
        .update(
            UpdateInput {
                work_ref: Some(child.clone()),
                action: UpdateAction::Detach { reason },
            },
            at(5),
        )
        .unwrap();
    let successor = detached.value["receipt"]["work_ref"].as_str().unwrap();
    add(&verbs, "Successor child", Some(successor), true, 6);
    note(&verbs, successor, "Useful successor finding", 7);
    let service = LocalWorkService::new(
        path,
        project,
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let original = verbs
        .render_show(
            &service.work_focus_for_agent(successor, at(8)).unwrap(),
            at(8),
        )
        .unwrap();
    assert!(!original.value["children"].as_array().unwrap().is_empty());
    assert!(
        !original.value["history"]["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(!original.value["notes"].as_array().unwrap().is_empty());
    let shown = verbs.show(successor, at(8)).unwrap();
    assert_eq!(
        shown.value["detached_from"],
        json!({"ref":child, "reason_omitted":1})
    );
    assert!(shown.text().contains("1 detach reason not shown"));
    assert!(!shown.text().contains("detach reason shortened"));
    assert!(shown.next.contains(&format!("engram work show {child}")));
    assert_eq!(
        shown.value["status"]["work"]["acceptance"],
        original.value["status"]["work"]["acceptance"]
    );
    for section in [
        "children",
        "history",
        "notes",
        "blockers",
        "prerequisites",
        "next",
    ] {
        assert_eq!(shown.value[section], original.value[section], "{section}");
    }
    assert!(receipt_bytes(&shown) < MAX_AGENT_WORK_RESPONSE_BYTES);
}

fn unnoted_contract(
    text: &str,
    detached: bool,
) -> (
    crate::test_support::TempHome,
    AgentVerbs,
    LocalWorkService,
    String,
) {
    let (directory, verbs, path, project) = fixture();
    let service = LocalWorkService::new(
        path,
        project,
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let work = if detached {
        let (_, child) = super::detach::stranded_child(&verbs);
        verbs
            .update(
                UpdateInput {
                    work_ref: Some(child),
                    action: UpdateAction::Detach {
                        reason: text.into(),
                    },
                },
                at(5),
            )
            .unwrap()
            .value["receipt"]["work_ref"]
            .as_str()
            .unwrap()
            .to_owned()
    } else {
        contract(&verbs, vec![text.into()])
    };
    (directory, verbs, service, work)
}

#[test]
fn contract_fixture_bindings_close_both_services_before_directory() {
    let path;
    {
        let (directory, verbs, service, work) = unnoted_contract("Fixture lifetime", false);
        verbs.show(&work, at(10)).unwrap();
        service.work_focus_for_agent(&work, at(11)).unwrap();
        path = directory.path().to_owned();
    }
    assert!(!path.exists());
}

#[test]
fn show_notes_fits_the_final_envelope_at_the_contract_boundary() {
    for detached in [false, true] {
        // Equal-shape fresh fixtures differ only in source text and fixed-width
        // runtime references. Calibrate against the actual plain-show renderer.
        let (_probe_directory, probe, probe_service, probe_ref) = unnoted_contract("x", detached);
        let base = if detached {
            probe.show(&probe_ref, at(6)).unwrap()
        } else {
            // Criteria outrank history. Calibrate the exhausted-history shape
            // so the extra envelope cannot be paid for by one history row.
            let mut view = probe_service
                .work_focus_for_agent(&probe_ref, at(6))
                .unwrap();
            let removed = view.history.items.len();
            assert!(removed > 0);
            view.history.items.clear();
            view.history.omitted += removed;
            view.omissions.push(WorkSectionOmission {
                section: WorkNextSection::Focus,
                reason: WorkSectionOmissionReason::ByteBudget,
                omitted_count: removed,
            });
            probe.render_show(&view, at(6)).unwrap()
        };
        assert_eq!(base.value["notes"], json!([]));
        let text = "x".repeat(MAX_AGENT_WORK_RESPONSE_BYTES - receipt_bytes(&base));
        let (_directory, verbs, _service, work) = unnoted_contract(&text, detached);
        let plain = verbs.show(&work, at(6)).unwrap();
        assert_eq!(receipt_bytes(&plain), MAX_AGENT_WORK_RESPONSE_BYTES - 1);
        let full = verbs
            .show_with_notes(&work, true, at(6))
            .expect("full-note envelope must shed a whole contract entry instead of refusing");
        assert!(receipt_bytes(&full) < MAX_AGENT_WORK_RESPONSE_BYTES);
        assert_eq!(full.value["notes"], json!([]));
        assert_eq!(full.value["notes_omitted"], 0);
        if detached {
            assert_eq!(full.value["detached_from"]["reason_omitted"], 1);
            assert!(full.value["detached_from"].get("reason").is_none());
            assert_eq!(
                full.value["status"]["work"]["acceptance"],
                plain.value["status"]["work"]["acceptance"]
            );
        } else {
            assert_eq!(full.value["status"]["work"]["acceptance"], json!([]));
            assert_eq!(full.value["status"]["work"]["acceptance_omitted"], 1);
        }
    }
}

#[test]
fn show_large_acceptance_uses_bounded_prefix_probes() {
    let (_directory, verbs, path, project) = fixture();
    let work = contract(&verbs, vec!["Criterion".into()]);
    let service = LocalWorkService::new(
        path,
        project,
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let mut view = service.work_focus_for_agent(&work, at(1)).unwrap();
    view.history.items.clear();
    view.history.total = 0;
    view.history.omitted = 0;
    view.omissions.clear();
    for count in [256_usize, 1024] {
        view.status.work.acceptance = (0..count)
            .map(|index| format!("Criterion {index:04}: {}", "x".repeat(80)))
            .collect();
        view.status.work.acceptance_count = count;
        let calls = std::cell::Cell::new(0_u32);
        let shown = crate::verbs::show::fit_show_receipt(
            view.clone(),
            |view| {
                calls.set(calls.get() + 1);
                verbs.render_show(view, at(2))
            },
            MAX_AGENT_WORK_RESPONSE_BYTES,
        )
        .unwrap();
        assert!(
            calls.get() <= count.ilog2() + 4,
            "{} renders for {count} criteria",
            calls.get()
        );
        let retained = shown.value["status"]["work"]["acceptance"]
            .as_array()
            .unwrap()
            .len();
        assert!(retained > 0 && retained < count);
        assert_eq!(
            shown.value["status"]["work"]["acceptance"],
            json!(&view.status.work.acceptance[..retained])
        );
        assert_eq!(
            shown.value["status"]["work"]["acceptance_omitted"],
            count - retained
        );
        assert!(receipt_bytes(&shown) < MAX_AGENT_WORK_RESPONSE_BYTES);
    }
}

#[test]
fn show_notes_fits_contract_and_populated_note_envelopes_together() {
    let (_directory, verbs, path, project) = fixture();
    let criteria = (0..4)
        .map(|index| format!("Criterion {index}: {}", "x".repeat(1000)))
        .collect::<Vec<_>>();
    let work = contract(&verbs, criteria.clone());
    for index in 1..=2 {
        verbs
            .note(
                &NoteInput {
                    status: false,
                    work_ref: Some(work.clone()),
                    // A second row must cost more than the cursor removed by
                    // a complete page, or both notes could fit more cheaply.
                    text: format!("Full note {index}: {}", "n".repeat(2000)),
                    refs: vec![],
                },
                at(index),
            )
            .unwrap();
    }
    let service = LocalWorkService::new(
        path,
        project,
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let mut source = service.work_focus_for_agent(&work, at(3)).unwrap();
    source.history.items.clear();
    source.history.total = 0;
    source.history.omitted = 0;
    source.omissions.clear();
    let mut target = source.clone();
    target.status.work.acceptance.pop();
    target.omissions.push(WorkSectionOmission {
        section: WorkNextSection::Focus,
        reason: WorkSectionOmissionReason::ByteBudget,
        omitted_count: 1,
    });
    let (_, mut target_page) = service
        .work_record_window(&work, crate::storage::WorkRecordKind::Notes, None, at(3))
        .unwrap();
    assert_eq!(target_page.total, 2);
    target_page.rows.truncate(1);
    let target = crate::verbs::record_windows::fit_window(
        target,
        &target_page,
        |view| verbs.render_show(view, at(3)),
        "agent",
        MAX_AGENT_WORK_RESPONSE_BYTES,
    )
    .unwrap();
    let budget = receipt_bytes(&target) + 1;
    let (_, page) = service
        .work_record_window(&work, crate::storage::WorkRecordKind::Notes, None, at(3))
        .unwrap();
    let shown = crate::verbs::record_windows::fit_window(
        source,
        &page,
        |view| verbs.render_show(view, at(3)),
        "agent",
        budget,
    )
    .unwrap();
    assert!(receipt_bytes(&shown) < budget);
    assert_eq!(
        shown.value["status"]["work"]["acceptance"],
        json!(&criteria[..3])
    );
    assert_eq!(shown.value["status"]["work"]["acceptance_omitted"], 1);
    assert_eq!(shown.value["notes"].as_array().unwrap().len(), 1);
    assert_eq!(shown.value["notes_omitted"], 1);
    assert_eq!(shown.value["notes"], target.value["notes"]);
    assert_eq!(
        shown
            .text()
            .lines()
            .filter(|line| line.starts_with("notes:"))
            .count(),
        1
    );
    assert!(shown.text().contains("1 omitted (1 older, 0 newer)"));
    assert_eq!(shown.value["omissions"][0]["omitted_count"], 1);
}
