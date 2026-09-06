use super::*;
use crate::storage::WorkRecordKind;
use crate::verbs::record_windows::{continuation_header, fit_window};

mod followups;

fn bytes(value: &Value) -> usize {
    serde_json::to_vec_pretty(value).unwrap().len()
}

fn assert_compact(receipt: &Receipt, title: &str) {
    assert_eq!(receipt.value["work"]["title"], title);
    assert_eq!(receipt.value.to_string().matches("\"title\":").count(), 1);
    assert_eq!(receipt.value.to_string().matches(title).count(), 1);
    for absent in [
        "status",
        "focus",
        "history",
        "parent",
        "receipt",
        "control_binding",
        "allowed_next",
    ] {
        assert!(
            receipt.value.get(absent).is_none(),
            "{absent}: {}",
            receipt.value
        );
    }
    assert_eq!(receipt.text().matches("full detail:").count(), 1);
    let detail = receipt.value["full_detail"].as_str().unwrap();
    assert!(detail.starts_with("engram work show '"));
    assert!(receipt.text().contains(detail));
    assert!(!receipt.next.iter().any(|command| command == detail));
    assert!(receipt.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
    assert!(bytes(&receipt.value) < MAX_AGENT_WORK_RESPONSE_BYTES);
}

#[test]
fn mutation_receipts_keep_one_item_and_measure_the_same_core_fixture() {
    let (_directory, verbs, _, _) = fixture();
    let title = "One unique mutation title";
    let outcome = "The complete durable outcome. "
        .repeat(35)
        .trim()
        .to_owned();
    let acceptance = vec![
        "The complete acceptance contract. "
            .repeat(12)
            .trim()
            .to_owned(),
    ];
    let input = AddInput {
        title: title.into(),
        outcome: Some(outcome.clone()),
        acceptance: acceptance.clone(),
        ..AddInput::default()
    };
    let added = verbs.add(input, at(0)).unwrap();
    let work_ref = added.value["work"]["short_ref"]
        .as_str()
        .unwrap()
        .to_owned();
    let root = verbs
        .service
        .work_propose(
            WorkProposeInput::Root {
                external_ref: None,
                notes: Vec::new(),
                title: title.into(),
                outcome,
                acceptance,
                work_kind: None,
                priority: None,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                idempotency_key: String::new(),
            },
            at(0),
        )
        .unwrap();
    let mut pairs = vec![(
        added,
        serde_json::to_value(root).unwrap(),
        verbs.service.inspect_work(&work_ref, at(0)).unwrap(),
    )];
    assert_eq!(pairs[0].1["work"]["short_ref"], work_ref);
    let claim = ClaimInput {
        work_ref: work_ref.clone(),
        ttl_seconds: Some(7200),
        recover: None,
    };
    let claimed = verbs.claim(claim, at(1)).unwrap();
    let core = verbs
        .service
        .work_update_on(
            Some(&work_ref),
            WorkUpdateInput::Claim {
                ttl_seconds: Some(7200),
                recovery_reason: None,
                idempotency_key: String::new(),
            },
            at(1),
        )
        .unwrap();
    assert_eq!(claimed.value["claim"]["holder"], "you");
    assert_eq!(
        claimed.value["claim"]["held_until"],
        core.receipt.result["expires_at"]
    );
    assert!(claimed.value.to_string().find("fence").is_none());
    pairs.push((
        claimed,
        serde_json::to_value(core).unwrap(),
        verbs.service.inspect_work(&work_ref, at(1)).unwrap(),
    ));
    let gated = verbs
        .gate(
            GateInput {
                work_ref: Some(work_ref.clone()),
                name: "fixed-test".into(),
                failed: Vec::new(),
                evidence_ref: Some("test:fixed".into()),
            },
            at(2),
        )
        .unwrap();
    let core = verbs
        .service
        .work_gate_on(
            Some(&work_ref),
            "fixed-test",
            &[],
            Some("test:fixed"),
            at(2),
        )
        .unwrap();
    pairs.push((
        gated,
        serde_json::to_value(core).unwrap(),
        verbs.service.inspect_work(&work_ref, at(2)).unwrap(),
    ));
    let body = "The entire durable evidence body. "
        .repeat(25)
        .trim()
        .to_owned();
    let noted = verbs
        .note(
            &NoteInput {
                status: false,
                work_ref: Some(work_ref.clone()),
                text: body.clone(),
                refs: vec!["test:fixed".into()],
            },
            at(3),
        )
        .unwrap();
    let core = verbs
        .service
        .work_note_on(Some(&work_ref), &body, &["test:fixed".into()], at(3))
        .unwrap();
    assert_eq!(noted.value["checkpoint"], core.receipt.result);
    assert_eq!(noted.value["evidence"], core.evidence.result);
    pairs.push((
        noted,
        serde_json::to_value(core).unwrap(),
        verbs.service.inspect_work(&work_ref, at(3)).unwrap(),
    ));
    let done = verbs
        .done(
            DoneInput {
                work_ref: Some(work_ref.clone()),
                summary: Some("Delivered".into()),
                note: None,
            },
            at(4),
        )
        .unwrap();
    let core = verbs
        .service
        .work_complete_on(
            Some(&work_ref),
            WorkCompleteInput {
                capture: Some(WorkCompletionCaptureInput {
                    summary: "Delivered".into(),
                    refs: Vec::new(),
                }),
                evidence: Vec::new(),
                acceptance: None,
                note: None,
                idempotency_key: String::new(),
            },
            at(4),
        )
        .unwrap();
    assert_eq!(
        done.value["seal"],
        serde_json::to_value(&core).unwrap()["seal"]
    );
    pairs.push((
        done,
        serde_json::to_value(core).unwrap(),
        verbs.service.inspect_work(&work_ref, at(4)).unwrap(),
    ));
    let mut full_total = 0;
    let mut compact_total = 0;
    for (compact, result, focus) in pairs {
        assert_compact(&compact, title);
        // Both are live contracts. The bare completion result was already
        // small; comparison includes the full item context the envelope replaces.
        let bare = bytes(&result);
        let full = bytes(&json!({"result": result, "focus": focus}));
        let compact_bytes = bytes(&compact.value);
        full_total += full;
        compact_total += compact_bytes;
        eprintln!(
            "{}: bare core={bare}, full result+focus={full}, compact JSON={compact_bytes}, text={}",
            compact.value["operation"],
            compact.text().len()
        );
        assert!(compact_bytes < full);
    }
    eprintln!(
        "five-operation JSON aggregate: full result+focus={full_total}, compact={compact_total}"
    );
    assert!(compact_total < full_total);
    let records = verbs.show_with_notes(&work_ref, true, at(5)).unwrap();
    assert!(
        records.value["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["summary"] == body)
    );
}

#[test]
fn compact_refusal_keeps_owed_and_omission_signals_without_repeating_its_item() {
    let (_directory, verbs, _, _) = fixture();
    let title = "Distinct compact refusal title";
    let work_ref = add(&verbs, title, None, false, 0);
    let mut view = verbs.service.inspect_work(&work_ref, at(1)).unwrap();
    view.obligation_page =
        crate::verbs::tests::page(VerificationKind::Test, WorkObligationState::Open);
    view.obligation_page.omitted_count = 3;
    view.omissions = vec![WorkSectionOmission {
        section: WorkNextSection::Focus,
        reason: WorkSectionOmissionReason::ByteBudget,
        omitted_count: 2,
    }];
    let refusal = crate::work_service::WorkCompleteRefusal {
        code: "missing_acceptance".into(),
        work_id: view.status.work.work_id,
        obligation_page: view.obligation_page.clone(),
        remedy: format!(
            "resolve missing acceptance for {work_ref} {title:?}, then retry completion"
        ),
        recovery: crate::WorkCompletionRecovery {
            cause: crate::WorkCompletionRecoveryCause::MissingAcceptance {
                criterion: "Required proof".into(),
            },
            item: crate::WorkReferenceCandidate {
                work_id: view.status.work.work_id,
                short_ref: work_ref.clone(),
                title: title.into(),
                lifecycle: WorkLifecycle::Open,
            },
            command: format!("engram work done {work_ref} \"…\""),
        },
        required_child_successor: None,
    };
    let mut reminders = crate::verbs::handlers::obligation_reminders(&view.obligation_page);
    reminders.push(crate::verbs::handlers::completion_recovery_reminder(
        &refusal.recovery,
        false,
    ));
    let compact = crate::verbs::mutation::receipt(
        &view,
        "done",
        crate::verbs::child_obligations::done_refusal_value(&refusal, None).unwrap(),
        vec![format!("not done {work_ref} \"{title}\"")],
        Guidance {
            reminders: reminders.clone(),
            next: vec![refusal.recovery.command.clone()],
        },
        Holder::You(at(7200)),
        true,
    )
    .unwrap();
    assert_compact(&compact, title);
    assert!(compact.owed);
    assert_eq!(compact.value["code"], refusal.code);
    assert_eq!(
        compact.value["remedy"],
        format!("resolve missing acceptance for {work_ref}, then retry completion")
    );
    assert!(refusal.remedy.contains(title));
    assert_eq!(
        compact.value["recovery"]["item"],
        json!({"ref": work_ref, "state": "open"})
    );
    assert_eq!(
        compact.value["obligations"],
        json!({"open": 1, "omitted": 3})
    );
    assert_eq!(compact.value["omissions"], json!(view.omissions));
    assert_eq!(compact.value["reminders"], json!(reminders));
    assert!(
        compact
            .text()
            .contains("more obligations are open than shown here")
    );
    assert_eq!(compact.next, vec![refusal.recovery.command]);
    assert!(
        compact
            .with_effective_session_id(&SessionId("local-process-v1-fixture".into()))
            .value
            .get("effective_session_id")
            .is_none()
    );
}

#[test]
fn continuation_headers_reduce_same_row_bytes_and_fixed_backlog_page_count() {
    let (_directory, verbs, _, _) = fixture();
    let added = verbs
        .add(
            AddInput {
                title: "Long header window".into(),
                outcome: Some("The complete outcome belongs to the item read. ".repeat(30)),
                acceptance: (0..22)
                    .map(|index| {
                        format!(
                            "Criterion {index:02}: {}",
                            "explicit acceptance contract ".repeat(5)
                        )
                    })
                    .collect(),
                ..AddInput::default()
            },
            at(0),
        )
        .unwrap();
    let work = added.value["work"]["short_ref"].as_str().unwrap();
    for index in 0..48 {
        note(
            &verbs,
            work,
            &format!("Record {index:02}: {}", "body ".repeat(110)),
            index + 1,
        );
        verbs
            .update(
                UpdateInput {
                    work_ref: Some(work.into()),
                    action: UpdateAction::Revise {
                        external: None,
                        title: None,
                        outcome: None,
                        acceptance: None,
                        assignee: None,
                        priority: Some(i32::try_from(index % 4).unwrap()),
                        defer: None,
                        kind: None,
                        labels: Vec::new(),
                        unlabels: Vec::new(),
                    },
                },
                at(index + 1),
            )
            .unwrap();
    }
    for kind in [WorkRecordKind::Notes, WorkRecordKind::History] {
        let (view, mut page) = verbs
            .service
            .work_record_window(work, kind, None, at(60))
            .unwrap();
        page.rows.truncate(1);
        let full = fit_window(
            view.clone(),
            &page,
            |view| verbs.render_show(view, at(60)),
            "agent",
            MAX_AGENT_WORK_RESPONSE_BYTES,
        )
        .unwrap();
        let compact = fit_window(
            view,
            &page,
            |view| Ok(continuation_header(view)),
            "agent",
            MAX_AGENT_WORK_RESPONSE_BYTES,
        )
        .unwrap();
        assert!(compact.text().len() < full.text().len());
        assert!(bytes(&compact.value) < bytes(&full.value));
        let metadata = if kind.is_notes() {
            &compact.value["notes_window"]
        } else {
            &compact.value["history"]["window"]
        };
        assert_eq!(metadata["read_cut"], json!(page.read_cut()));
        assert_eq!(metadata["byte_budget"], MAX_AGENT_WORK_RESPONSE_BYTES);
        assert!(
            compact
                .text()
                .contains(&format!("byte budget: {MAX_AGENT_WORK_RESPONSE_BYTES}"))
        );
        eprintln!(
            "{kind:?} same row: full text/JSON={}/{}, compact={}/{}",
            full.text().len(),
            bytes(&full.value),
            compact.text().len(),
            bytes(&compact.value)
        );
        let traverse = |compact: bool| {
            let mut after = None;
            let mut pages = 0;
            let mut locators = Vec::new();
            loop {
                let (view, page) = verbs
                    .service
                    .work_record_window(work, kind, after.as_deref(), at(60))
                    .unwrap();
                let receipt = fit_window(
                    view,
                    &page,
                    |view| {
                        if compact && after.is_some() {
                            Ok(continuation_header(view))
                        } else {
                            verbs.render_show(view, at(60))
                        }
                    },
                    "agent",
                    MAX_AGENT_WORK_RESPONSE_BYTES,
                )
                .unwrap();
                let (rows, meta) = if kind.is_notes() {
                    (&receipt.value["notes"], &receipt.value["notes_window"])
                } else {
                    (
                        &receipt.value["history"]["items"],
                        &receipt.value["history"]["window"],
                    )
                };
                locators.extend(
                    rows.as_array()
                        .unwrap()
                        .iter()
                        .rev()
                        .map(|row| row["locator"].clone()),
                );
                pages += 1;
                assert!(receipt.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
                assert!(bytes(&receipt.value) < MAX_AGENT_WORK_RESPONSE_BYTES);
                after = meta["after"].as_str().map(str::to_owned);
                if after.is_none() {
                    break;
                }
                assert!(pages <= page.total);
            }
            (pages, locators)
        };
        let (full_pages, full_rows) = traverse(false);
        let (compact_pages, compact_rows) = traverse(true);
        assert_eq!(compact_rows, full_rows);
        eprintln!(
            "{kind:?} fixed backlog: full-header pages={full_pages}, compact-continuation pages={compact_pages}"
        );
        assert!(compact_pages < full_pages);
    }
}
