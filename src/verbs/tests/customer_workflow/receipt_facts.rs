use super::*;

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one native gate sequence verifies terse, detail, late and restored receipts"
)]
fn gates_stay_typed_in_terse_notes_windows_details_and_late_rows() {
    let (directory, verbs, path, project) = fixture();
    let work = add(&verbs, "Typed terse gates", None, false, 0);
    verbs
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(1),
        )
        .unwrap();
    for (time, failures) in [(2, vec!["failure".into()]), (3, Vec::new())] {
        verbs
            .gate(
                GateInput {
                    work_ref: Some(work.clone()),
                    name: "Checks".into(),
                    failed: failures,
                    evidence_ref: None,
                },
                at(time),
            )
            .unwrap();
    }
    note(
        &verbs,
        &work,
        "Checks: passed (this prose is not a gate)",
        4,
    );
    let show = verbs.show(&work, at(5)).unwrap();
    let rows = show.value["notes"].as_array().unwrap();
    let gates = rows
        .iter()
        .filter_map(|row| row.get("gate"))
        .collect::<Vec<_>>();
    assert_eq!(
        gates,
        vec![
            &json!({"name":"checks", "passed":false}),
            &json!({"name":"checks", "passed":true})
        ]
    );
    assert!(rows.iter().all(|row| row["kind"] == "generic"));
    assert!(show.text().contains("latest generic"));
    assert!(rows.last().unwrap().get("gate").is_none());
    verbs
        .done(
            DoneInput {
                work_ref: Some(work.clone()),
                summary: Some("Delivered".into()),
                ..Default::default()
            },
            at(6),
        )
        .unwrap();
    verbs
        .gate(
            GateInput {
                work_ref: Some(work.clone()),
                name: "Late check".into(),
                failed: vec!["late defect".into()],
                evidence_ref: None,
            },
            at(7),
        )
        .unwrap();
    let window = verbs
        .show_records(
            &work,
            &ShowInput {
                notes: true,
                gates: true,
                ..Default::default()
            },
            at(8),
        )
        .unwrap();
    assert_eq!(
        window.value["notes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row.get("gate").is_some())
            .count(),
        3
    );
    let late = window.value["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["gate"]["name"] == "late check")
        .unwrap();
    assert_eq!(late["gate"], json!({"name":"late check", "passed":false}));
    let detail = verbs
        .show_records(
            &work,
            &ShowInput {
                note: Some(late["locator"].as_str().unwrap().into()),
                ..Default::default()
            },
            at(8),
        )
        .unwrap();
    assert_eq!(detail.value["note"]["gate"], late["gate"]);
    let snapshot = super::review::snapshot(&path, &project, &work);
    let home = directory.path().join("restored");
    std::fs::create_dir_all(&home).unwrap();
    let (restored, _store, _path) = super::review::load(&home, &snapshot);
    let carried = restored
        .show_records(
            &work,
            &ShowInput {
                notes: true,
                gates: true,
                ..Default::default()
            },
            at(9),
        )
        .unwrap();
    assert_eq!(
        carried.value["notes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row.get("gate").is_some())
            .count(),
        3
    );
}

#[test]
fn restored_history_byte_pressure_retains_the_newest_generation_and_counts_older_rows() {
    let (_directory, verbs, path, project) = fixture();
    let work = add(&verbs, "Restored history pressure", None, false, 0);
    let service = LocalWorkService::new(
        path,
        project,
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let mut view = service.work_focus_for_agent(&work, at(1)).unwrap();
    let actor = crate::ActorContext {
        actor_id: "historical actor".into(),
        actor_kind: "agent".into(),
        assurance: crate::domain::AssuranceLevel::Asserted,
        run_id: None,
        session_id: None,
        source_tool: None,
        source_skill: None,
        provenance_chain: Vec::new(),
        reason: "fixture attribution".into(),
    };
    view.history.items.clear();
    view.history.omitted = view.history.total;
    view.restored_history = crate::work_service::RestoredHistoryView {
        total: 3,
        omitted: 0,
        items: [
            (0, "generic", at(100)),
            (0, "released", at(-100)),
            (1, "completed", at(-100)),
        ]
        .into_iter()
        .enumerate()
        .map(
            |(index, (generation, kind, created_at))| crate::work_service::RestoredHistoryEntry {
                generation_index: generation,
                kind: kind.into(),
                summary: format!("Member {index} {}", "x".repeat(170)),
                actor: actor.clone(),
                created_at,
            },
        )
        .collect(),
    };
    let original_bytes = serde_json::to_vec(&view.restored_history).unwrap();
    let mut expected = view.clone();
    expected.restored_history.items.drain(..2);
    expected.restored_history.omitted = 2;
    expected.omissions.push(WorkSectionOmission {
        section: WorkNextSection::Focus,
        reason: WorkSectionOmissionReason::ByteBudget,
        omitted_count: 2,
    });
    let core_budget = serde_json::to_vec(&crate::work_service::WorkInspectView::from_focus(
        expected.clone(),
    ))
    .unwrap()
    .len();
    let mut core = crate::work_service::WorkInspectView::from_focus(view.clone());
    core.fit_within(core_budget).unwrap();
    assert_eq!(core.view().restored_history.omitted, 2);
    assert_eq!(core.view().restored_history.items.len(), 1);
    assert_eq!(core.view().restored_history.items[0].generation_index, 1);
    assert!(
        core.view().restored_history.items[0]
            .summary
            .starts_with("Member 2")
    );
    let budget = emitted_receipt_bytes(&verbs.render_show(&expected, at(2)).unwrap()) + 1;
    let fitted = crate::verbs::show::fit_show_receipt(
        view.clone(),
        |view| verbs.render_show(view, at(2)),
        budget,
    )
    .unwrap();
    assert_eq!(fitted.value["restored_history"]["omitted"], 2);
    assert_eq!(
        fitted.value["restored_history"]["items"][0]["kind"],
        "completed"
    );
    assert_eq!(
        fitted.value["restored_history"]["items"][0]["generation"],
        1
    );
    assert!(
        fitted.value["restored_history"]["items"][0]["summary"]
            .as_str()
            .unwrap()
            .starts_with("Member 2")
    );
    assert_eq!(
        serde_json::to_vec(&view.restored_history).unwrap(),
        original_bytes
    );
}
