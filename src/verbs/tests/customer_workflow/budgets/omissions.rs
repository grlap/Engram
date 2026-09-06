use super::*;

fn receipt_size(receipt: &Receipt) -> usize {
    receipt
        .text()
        .len()
        .max(serde_json::to_vec_pretty(&receipt.value).unwrap().len())
}

fn focus_byte_omitted(receipt: &Receipt) -> usize {
    receipt.value["omissions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["section"] == "focus" && entry["reason"] == "byte_budget")
        .map(|entry| usize::try_from(entry["omitted_count"].as_u64().unwrap()).unwrap())
        .sum()
}

fn without_history(view: &mut WorkFocusView) {
    view.history.items.clear();
    view.history.omitted = view.history.total;
    view.restored_history.items.clear();
    view.restored_history.omitted = view.restored_history.total;
}

#[test]
fn show_pressure_preserves_blockers_and_counts_only_removed_rows() {
    let (_directory, verbs, service, root) = rich_focus(0);
    let mut view = service.work_focus_for_agent(&root, at(100)).unwrap();
    without_history(&mut view);
    view.blockers.push(crate::work_service::WorkBlockerSummary {
        blocker_id: "presentation-fixture".into(),
        kind: crate::WorkBlockerKind::HumanDecision,
        detail: "Wait for the coordinator to confirm the installation".into(),
    });
    view.status.availability = WorkAvailability::Blocked;
    let original = verbs.render_show(&view, at(100)).unwrap();
    let budget = receipt_size(&original);
    let fitted =
        crate::verbs::show::fit_show_receipt(view, |view| verbs.render_show(view, at(100)), budget)
            .unwrap();
    assert!(receipt_size(&fitted) < budget);
    assert_eq!(fitted.value["blockers"], original.value["blockers"]);
    assert!(
        fitted
            .text()
            .contains("Wait for the coordinator to confirm the installation")
    );
    assert_eq!(original.value["status"]["availability"], "blocked");
    assert_eq!(fitted.value["status"]["availability"], "blocked");
    let removed = original.value["children"].as_array().unwrap().len()
        - fitted.value["children"].as_array().unwrap().len();
    assert!(removed > 0);
    assert_eq!(focus_byte_omitted(&fitted), removed);
}

#[test]
fn show_pressure_preserves_prerequisites() {
    let (_directory, verbs, service, root) = rich_focus(0);
    note(&verbs, &root, &coordination_note(0), 100);
    let mut view = service.work_focus_for_agent(&root, at(101)).unwrap();
    without_history(&mut view);
    // The projection fitter receives bounded relation summaries; keep their
    // complete blocking context while a visible note supplies expendable bytes.
    view.prerequisites = std::mem::take(&mut view.children);
    for prerequisite in &mut view.prerequisites {
        prerequisite.prerequisite_state = Some(crate::WorkPrerequisiteState::Pending);
    }
    view.child_count = 0;
    view.child_obligations = None;
    view.status.availability = WorkAvailability::Blocked;
    let original = verbs.render_show(&view, at(101)).unwrap();
    let budget = receipt_size(&original);
    let fitted =
        crate::verbs::show::fit_show_receipt(view, |view| verbs.render_show(view, at(101)), budget)
            .unwrap();
    assert!(receipt_size(&fitted) < budget);
    assert_eq!(
        fitted.value["prerequisites"],
        original.value["prerequisites"]
    );
    for prerequisite in original.value["prerequisites"].as_array().unwrap() {
        assert!(
            fitted
                .text()
                .contains(prerequisite["short_ref"].as_str().unwrap())
        );
    }
    assert_eq!(focus_byte_omitted(&fitted), 1);
    assert_eq!(fitted.value["notes_omitted"], 1);
    assert_eq!(fitted.value["status"]["availability"], "blocked");
}

#[test]
fn compact_omission_reasons_share_one_terminal_total() {
    let (_directory, _verbs, service, root) = rich_focus(0);
    service.select_work(&root, at(100)).unwrap();
    let view = service
        .work_next_for_agent(20, 20, false, WorkNextQuery::default(), at(101))
        .unwrap();
    let guidance = Guidance {
        reminders: (0..MAX_COMPACT_REMINDER_ITEMS + 2)
            .map(|index| format!("Reminder {index}: {}", "r".repeat(100)))
            .collect(),
        next: vec!["engram work next".into()],
    };
    let mut compact = crate::verbs::receipts::compact_next_receipt(
        &view,
        &[],
        &[],
        &[],
        &std::collections::HashMap::new(),
        &guidance,
    )
    .unwrap();
    compact.memories = None;
    compact.focus = None;
    compact.discovery = crate::work_service::WorkDiscoveryView::default();
    let original = Receipt::assemble(
        compact_next_lines(&compact),
        compact.guidance.clone(),
        compact_next_value(&compact),
        false,
    )
    .with_build_identity(&compact.read_cut, compact.context_generation.as_deref());
    let fitted = fit_compact_next_to(compact, receipt_size(&original)).unwrap();
    let count =
        compact_omitted_for_reason(&fitted, "reminders", WorkSectionOmissionReason::CountLimit);
    let bytes =
        compact_omitted_for_reason(&fitted, "reminders", WorkSectionOmissionReason::ByteBudget);
    assert_eq!(count, 2);
    assert!(bytes > 0);
    let lines = compact_next_lines(&fitted);
    let totals = lines
        .iter()
        .filter(|line| line.contains("more reminders not shown"))
        .collect::<Vec<_>>();
    assert_eq!(totals.len(), 1);
    assert_eq!(
        totals[0],
        &format!("  ({} more reminders not shown)", count + bytes)
    );
    assert_eq!(
        fitted.guidance.reminders.len() + count + bytes,
        guidance.reminders.len()
    );
    let complete = Receipt::assemble(
        lines,
        fitted.guidance.clone(),
        compact_next_value(&fitted),
        false,
    )
    .with_build_identity(&fitted.read_cut, fitted.context_generation.as_deref());
    assert!(receipt_size(&complete) < receipt_size(&original));
}

#[test]
fn full_notes_replace_only_the_compact_note_budget_contribution() {
    let (_directory, verbs, path, project) = fixture();
    let service = LocalWorkService::new(
        path,
        project,
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let root = add(&verbs, "Full note replacement", None, false, 0);
    for index in 1..=8 {
        note(
            &verbs,
            &root,
            &format!("Note {index}: {}", "n".repeat(180)),
            index,
        );
    }
    add(&verbs, "First optional child", Some(&root), true, 9);
    add(&verbs, "Second optional child", Some(&root), true, 10);
    for keep_history in [false, true] {
        let (mut source, mut page) = service
            .work_record_window(&root, crate::storage::WorkRecordKind::Notes, None, at(11))
            .unwrap();
        if !keep_history {
            without_history(&mut source);
            source.children.clear();
            source.child_count = 0;
            source.child_obligations = None;
        }
        let original = verbs.render_show(&source, at(12)).unwrap();
        let history_rows = original.value["history"]["items"].as_array().unwrap().len();
        let child_rows = original.value["children"].as_array().unwrap().len();
        let summary_rows = child_summary_rows(&original);
        let note_rows = original.value["notes"].as_array().unwrap().len();
        assert_eq!(note_rows, 8);
        let mut target = source.clone();
        without_history(&mut target);
        target.children.clear();
        if let Some(groups) = &mut target.child_obligations {
            groups.required_owed.items.clear();
            groups.open_optional.items.clear();
        }
        let genuine_omissions = history_rows + child_rows + summary_rows;
        if genuine_omissions > 0 {
            target.omissions.push(WorkSectionOmission {
                section: WorkNextSection::Focus,
                reason: WorkSectionOmissionReason::ByteBudget,
                omitted_count: genuine_omissions,
            });
        }
        let remaining = page.rows.split_off(1);
        let expected = crate::verbs::record_windows::fit_window(
            target,
            &page,
            |view| verbs.render_show(view, at(12)),
            "agent",
            MAX_AGENT_WORK_RESPONSE_BYTES,
        )
        .unwrap();
        let budget = receipt_size(&expected) + 1;
        page.rows.extend(remaining);
        let fitted = crate::verbs::record_windows::fit_window(
            source,
            &page,
            |view| verbs.render_show(view, at(12)),
            "agent",
            budget,
        )
        .unwrap();
        assert!(receipt_size(&fitted) < budget);
        assert_eq!(fitted.value["notes"], expected.value["notes"]);
        assert_eq!(fitted.value["notes"].as_array().unwrap().len(), 1);
        assert_eq!(fitted.value["notes_omitted"], 7);
        assert_eq!(fitted.value["notes_window"]["total"], 8);
        // Removed compact rows are replaced, not attributed to byte shedding.
        assert_eq!(focus_byte_omitted(&fitted), genuine_omissions);
        assert_eq!(fitted.value["history"], expected.value["history"]);
        assert_eq!(
            fitted.value["children_omitted"],
            expected.value["children_omitted"]
        );
        assert_eq!(
            fitted.value["child_obligations"],
            expected.value["child_obligations"]
        );
        if !keep_history {
            assert!(fitted.value.get("omissions").is_none());
        }
    }
}

fn child_summary_rows(receipt: &Receipt) -> usize {
    ["required_owed", "open_optional"]
        .iter()
        .map(|key| {
            receipt.value["child_obligations"][key]["items"]
                .as_array()
                .map_or(0, Vec::len)
        })
        .sum()
}

#[test]
fn show_budget_exhaustion_refuses_without_dropping_essential_metadata() {
    let (_directory, verbs, service, root) = rich_focus(0);
    let view = service.work_focus_for_agent(&root, at(100)).unwrap();
    let error =
        crate::verbs::show::fit_show_receipt(view, |view| verbs.render_show(view, at(100)), 1)
            .unwrap_err();
    assert!(
        matches!(error.error, StoreError::InvalidWorkProjection(ref reason)
        if reason == "show metadata exceeds the agent response byte budget")
    );
}
