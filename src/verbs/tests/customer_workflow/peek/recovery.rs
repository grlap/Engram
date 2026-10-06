use super::*;

fn hold(verbs: &AgentVerbs, reference: &str, now: i64) {
    verbs
        .claim(
            ClaimInput {
                work_ref: reference.into(),
                ttl_seconds: Some(3600),
                recover: None,
            },
            at(now),
        )
        .unwrap();
}

fn status(verbs: &AgentVerbs, reference: &str, body: &str, now: i64) {
    verbs
        .note(
            &NoteInput {
                status: true,
                work_ref: Some(reference.into()),
                text: body.into(),
                refs: vec![],
            },
            at(now),
        )
        .unwrap();
}

#[test]
fn compact_peek_keeps_lifecycle_commands_once_and_short_multiline_status_intact() {
    let (_home, verbs, _, _) = fixture();
    let current = add(&verbs, "Current work", None, false, 0);
    hold(&verbs, &current, 1);
    let body = "WAIT for review.\nSTOP:  do not land yet.";
    status(&verbs, &current, body, 2);
    let other = add(&verbs, "Ready candidate", None, false, 3);
    // Renew current focus without changing its status.
    hold(&verbs, &current, 5);
    let receipt = verbs.next(&peek_input(false), at(6)).unwrap();
    assert_eq!(receipt.value["focus"]["ref"], current);
    assert_eq!(receipt.value["ready_limit"], 1);
    assert_eq!(
        receipt.value["focus"]["current_status"]["body_or_first_line"],
        body
    );
    assert_eq!(receipt.value["focus"]["current_status"]["complete"], true);
    for command in [
        "engram work memories".to_owned(),
        format!("engram work note {current} \"…\""),
        format!("engram work done {current} \"…\""),
    ] {
        assert_eq!(
            receipt.next.iter().filter(|row| **row == command).count(),
            1,
            "{command}: {:?}",
            receipt.next
        );
    }
    assert!(receipt.next.contains(&format!("engram work show {other}")));
    assert_eq!(
        receipt.value["details"],
        "engram work next --peek --verbose"
    );
}

#[test]
fn compact_peek_keeps_current_peer_notes_and_recovers_unscanned_notes_without_delivery() {
    let (_home, verbs, path, project) = fixture();
    let current = add(&verbs, "Current duty", None, false, 0);
    hold(&verbs, &current, 1);
    let unrelated = add(&verbs, "Unrelated discussion", None, false, 2);
    hold(&verbs, &current, 3);
    let peer = AgentVerbs::new(path, project, "peer".into(), SessionId("peer".into()), None);
    note(&peer, &unrelated, "Old unrelated detail", 4);
    for index in 0..6 {
        note(
            &peer,
            &current,
            &format!(
                "STOP {index}: inspect before landing. {}",
                "retained context ".repeat(12)
            ),
            5 + index,
        );
    }
    let compact = verbs.next(&peek_input(false), at(12)).unwrap();
    let rows = compact.value["changes"].as_array().unwrap();
    // The raw feed preview includes checkpoints and earlier own entries.
    // It reaches these two peer notes; the remaining four are not scanned.
    assert_eq!(rows.len(), 2);
    for (index, row) in rows.iter().enumerate() {
        assert!(row.as_str().unwrap().contains(&format!("STOP {index}:")));
    }
    assert!(
        rows.iter()
            .all(|row| row.as_str().unwrap().contains(&current))
    );
    assert!(!compact.text().contains("Old unrelated detail"));
    assert_eq!(compact.value["peek"]["delivery_advanced"], false);
    assert_eq!(compact.value["peek"]["more_changes_available"], true);
    let first = rows[0].as_str().unwrap();
    let command = first.split("; ").last().unwrap();
    let words = command.split_whitespace().collect::<Vec<_>>();
    assert_eq!(&words[..5], ["engram", "work", "show", &current, "--note"]);
    let exact = verbs
        .show_records(
            &current,
            &ShowInput {
                note: Some(words[5].into()),
                ..Default::default()
            },
            at(12),
        )
        .unwrap();
    assert!(exact.text().contains("STOP 0: inspect before landing"));
    assert!(exact.text().contains("retained context retained context"));
    let evidence_command = compact.value["focus"]["recovery"]["evidence_detail"]
        .as_str()
        .unwrap();
    assert_eq!(
        evidence_command,
        format!("engram work show {current} --notes --gates")
    );
    let records = verbs
        .show_records(
            &current,
            &ShowInput {
                notes: true,
                gates: true,
                ..Default::default()
            },
            at(12),
        )
        .unwrap();
    for index in 0..6 {
        assert!(
            records
                .text()
                .contains(&format!("STOP {index}: inspect before landing"))
        );
    }
    assert_eq!(
        verbs.next(&peek_input(false), at(12)).unwrap().value["changes"],
        compact.value["changes"]
    );
}

#[test]
fn recovery_counts_proposed_children_in_the_visible_prefix() {
    let (_home, verbs, _, _) = fixture();
    let current = add(&verbs, "Parent duty", None, false, 0);
    hold(&verbs, &current, 1);
    let child = add(&verbs, "Child duty", Some(&current), false, 2);
    let mut view = verbs.service.work_focus_for_agent(&current, at(3)).unwrap();
    let row = view
        .children
        .iter_mut()
        .find(|row| row.short_ref == child)
        .unwrap();
    row.lifecycle = WorkLifecycle::Proposed;
    let recovery =
        serde_json::to_value(crate::verbs::next_recovery::Recovery::from_focus(&view)).unwrap();
    assert_eq!(recovery["unfinished_children"], 1);
    assert!(
        recovery["dependency_preview"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row.as_str().unwrap().contains(&child))
    );
}

#[test]
fn compact_peek_recovers_objective_counts_and_late_constraints_by_exact_note() {
    let (_home, verbs, _, _) = fixture();
    let objective = format!(
        "Repair the recovery projection. {}",
        "Detailed scope. ".repeat(25).trim_end()
    );
    let created = verbs
        .add(
            AddInput {
                title: "Current task".into(),
                outcome: Some(objective.clone()),
                acceptance: vec!["Correct recovery".into(), "Bounded packet".into()],
                ..AddInput::default()
            },
            at(0),
        )
        .unwrap();
    let reference = created.value["work"]["short_ref"].as_str().unwrap();
    hold(&verbs, reference, 1);
    let body = format!(
        "READY to inspect. {} STOP: do not land before the remaining review.",
        "Context retained. ".repeat(30)
    );
    status(&verbs, reference, &body, 2);
    let receipt = verbs.next(&peek_input(false), at(3)).unwrap();
    let focus = &receipt.value["focus"];
    assert_eq!(focus["ref"], reference);
    assert_eq!(focus["recovery"]["objective_complete"], false);
    assert_eq!(focus["recovery"]["acceptance_count"], 2);
    assert_eq!(focus["recovery"]["acceptance_shown"], 0);
    assert_eq!(focus["recovery"]["blockers"], 0);
    assert_eq!(focus["current_status"]["complete"], false);
    assert!(
        !focus["current_status"]["body_or_first_line"]
            .as_str()
            .unwrap()
            .contains("STOP")
    );
    let locator = focus["current_status"]["locator"].as_str().unwrap();
    assert!(
        receipt
            .next
            .contains(&format!("engram work show {reference} --note {locator}"))
    );
    let full_note = verbs
        .show_records(
            reference,
            &ShowInput {
                note: Some(locator.into()),
                ..ShowInput::default()
            },
            at(3),
        )
        .unwrap();
    assert!(full_note.text().contains("STOP: do not land"));
    let full = verbs
        .show_records(
            reference,
            &ShowInput {
                full: true,
                ..ShowInput::default()
            },
            at(3),
        )
        .unwrap();
    assert_eq!(full.value["work"]["outcome"], objective);
    assert!(
        receipt.value["held"].as_array().unwrap().is_empty(),
        "focus already carries the same holder capture"
    );
    assert!(
        receipt
            .reminders
            .iter()
            .any(|line| line.contains("clipped prefix grants no permission"))
    );
}

#[test]
fn compact_peek_omits_broad_history_with_a_working_inspection_route() {
    let (_home, verbs, path, project) = fixture();
    let peer = AgentVerbs::new(path, project, "peer".into(), SessionId("peer".into()), None);
    for index in 0..7 {
        let reference = add(
            &peer,
            &format!("Other work {index}"),
            None,
            false,
            index * 3,
        );
        note(
            &verbs,
            &reference,
            &format!(
                "Historical discussion {index}. {}",
                "Old detail. ".repeat(10)
            ),
            index * 3 + 1,
        );
    }
    let current = add(&verbs, "Current objective", None, false, 30);
    hold(&verbs, &current, 31);
    status(
        &verbs,
        &current,
        "WAIT for review; inspect the evidence next.",
        32,
    );
    let compact = verbs.next(&peek_input(false), at(33)).unwrap();
    let detailed = verbs.next(&peek_input(true), at(33)).unwrap();
    assert_eq!(
        compact.value["details"],
        "engram work next --peek --verbose"
    );
    assert!(
        compact.value["participated"]
            .as_array()
            .is_none_or(Vec::is_empty)
    );
    assert!(compact.value["participated_omitted"].as_u64().unwrap() > 0);
    assert_eq!(
        compact.value["changes"].as_array().unwrap().as_slice(),
        &[] as &[serde_json::Value]
    );
    assert_eq!(compact.value["peek"]["more_changes_available"], true);
    assert_eq!(compact.value["ready"].as_array().unwrap().len(), 1);
    assert!(
        compact.value["ready_next"]
            .as_str()
            .unwrap()
            .contains(" --after ")
    );
    assert!(!compact.text().contains("Historical discussion"));
    // Verbose is still a bounded preview; its fitter may shed discovery rows.
    // Follow the catalog route instead of assuming a historical body fits it.
    assert_eq!(
        compact.value["catalog_detail"],
        "engram work ls --all --limit 20"
    );
    let mut words = compact.value["catalog_detail"]
        .as_str()
        .unwrap()
        .split_whitespace();
    assert_eq!(words.next(), Some("engram"));
    assert_eq!(words.next(), Some("work"));
    assert_eq!(words.next(), Some("ls"));
    assert_eq!(words.next(), Some("--all"));
    assert_eq!(words.next(), Some("--limit"));
    let mut input = LsInput {
        all: true,
        limit: Some(words.next().unwrap().parse().unwrap()),
        ..LsInput::default()
    };
    assert!(words.next().is_none());
    let mut recovered_history = 0;
    loop {
        let page = verbs.ls(&input, at(33)).unwrap();
        for row in page.value["items"].as_array().unwrap() {
            let reference = row["ref"].as_str().unwrap();
            let records = verbs
                .show_records(
                    reference,
                    &ShowInput {
                        notes: true,
                        ..ShowInput::default()
                    },
                    at(33),
                )
                .unwrap();
            if records.text().contains("Historical discussion") {
                recovered_history += 1;
            }
        }
        input.after = page.value["after"].as_str().map(str::to_owned);
        if input.after.is_none() {
            break;
        }
    }
    assert_eq!(
        recovered_history, 7,
        "every omitted discussion is still reachable"
    );
    assert!(detailed.value["focus"].get("recovery").is_none());
    assert!(detailed.value.get("details").is_none());
    assert!(detailed.value.get("catalog_detail").is_none());
    // Rich JSON and terse terminal output are different projections; verbose
    // fitting can shed the text's history even when its raw JSON is larger.
    assert!(
        serde_json::to_vec(&compact.value).unwrap().len()
            < serde_json::to_vec(&detailed.value).unwrap().len()
    );
    let advancing = verbs.next(&NextInput::default(), at(33)).unwrap();
    assert!(advancing.value["focus"].get("recovery").is_none());
    assert!(advancing.value.get("details").is_none());
    assert!(advancing.value.get("catalog_detail").is_none());
    assert_ne!(
        advancing.value["changes"].as_array().unwrap().as_slice(),
        &[] as &[serde_json::Value]
    );
    assert!(
        compact.text().len() < advancing.text().len(),
        "compact={} advancing={}",
        compact.text().len(),
        advancing.text().len()
    );
}

#[test]
fn recovery_keeps_other_held_work_and_expired_focus_never_restores_owner_status() {
    let (_home, verbs, _, _) = fixture();
    let first = add(&verbs, "First live duty", None, false, 0);
    hold(&verbs, &first, 1);
    status(&verbs, &first, "First live wait", 2);
    let second = add(&verbs, "Second live duty", None, false, 3);
    hold(&verbs, &second, 4);
    status(&verbs, &second, "Second live wait", 5);
    let active = verbs.next(&peek_input(false), at(6)).unwrap();
    assert_eq!(active.value["focus"]["ref"], second);
    assert_eq!(active.value["held"][0]["ref"], first);
    assert!(active.text().contains("First live wait"));
    let expired = verbs
        .next(
            &peek_input(false),
            at(6 + crate::DEFAULT_WORK_CLAIM_TTL_SECONDS),
        )
        .unwrap();
    assert_eq!(expired.value["focus"]["ref"], second);
    assert!(expired.value["focus"]["holder"].is_null());
    assert!(expired.value["focus"]["current_status"].is_null());
    assert_eq!(
        expired.value["held"].as_array().unwrap().as_slice(),
        &[] as &[serde_json::Value]
    );
    assert!(expired.reminders.iter().any(|line| line.contains("lapsed")));
}

#[test]
fn recovery_dependency_counts_and_previews_share_the_focus_cut() {
    let (_home, verbs, _, _) = fixture();
    let prerequisite = add(&verbs, "Unresolved prerequisite", None, false, 0);
    let current = add(&verbs, "Blocked current task", None, false, 1);
    hold(&verbs, &current, 2);
    let child = add(&verbs, "Required child", Some(&current), false, 3);
    verbs
        .update(
            UpdateInput {
                work_ref: Some(current.clone()),
                action: UpdateAction::After {
                    prerequisite: prerequisite.clone(),
                },
            },
            at(4),
        )
        .unwrap();
    verbs
        .update(
            UpdateInput {
                work_ref: Some(current.clone()),
                action: UpdateAction::Blocked {
                    detail: "Await external decision".into(),
                },
            },
            at(5),
        )
        .unwrap();
    let receipt = verbs.next(&peek_input(false), at(6)).unwrap();
    let recovery = &receipt.value["focus"]["recovery"];
    assert_eq!(receipt.value["focus"]["ref"], current);
    assert_eq!(recovery["blockers"], 1);
    assert_eq!(recovery["unresolved_prerequisites"], 1);
    assert_eq!(recovery["unfinished_children"], 1);
    assert!(receipt.text().contains(&prerequisite));
    assert!(receipt.text().contains(&child));
    assert!(receipt.text().contains("Await external decision"));
}

#[test]
fn recovery_projection_is_skipped_by_core_serialization_and_retains_final_budgets() {
    let (_home, verbs, _, _) = fixture();
    let current = add(&verbs, "Escaped \u{1b}[31m title", None, false, 0);
    hold(&verbs, &current, 1);
    status(&verbs, &current, &"\u{1b}\n\"\\<context>".repeat(80), 2);
    let view = verbs
        .service
        .work_next_peek_for_agent(20, 5, false, WorkNextQuery::default(), at(3), |_| true)
        .unwrap();
    assert!(
        view.focus
            .as_ref()
            .unwrap()
            .status
            .work
            .current_status
            .is_some()
    );
    let core = serde_json::to_value(&view).unwrap();
    assert!(
        core["focus"]["status"]["work"]
            .get("current_status")
            .is_none()
    );
    assert!(core["focus"].get("recovery").is_none());
    for verbose in [false, true] {
        let receipt = verbs.next(&peek_input(verbose), at(3)).unwrap();
        assert!(agent_receipt_terminal_bytes(&receipt.text()) < MAX_AGENT_WORK_RESPONSE_BYTES);
        assert!(serde_json::to_vec(&receipt.value).unwrap().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
        assert!(!receipt.text().contains('\u{1b}'));
    }
}

#[test]
fn no_focus_and_completed_focus_keep_unresolved_assignments_visible() {
    let (_home, verbs, path, project) = fixture();
    let peer = AgentVerbs::new(path, project, "peer".into(), SessionId("peer".into()), None);
    let created = peer
        .add(
            AddInput {
                title: "Unresolved assigned duty".into(),
                assignee: Some("agent".into()),
                ..AddInput::default()
            },
            at(0),
        )
        .unwrap();
    let assigned = created.value["work"]["short_ref"].as_str().unwrap();
    let unfocused = verbs.next(&peek_input(false), at(1)).unwrap();
    assert!(unfocused.value["focus"].is_null());
    assert!(
        unfocused.value["assigned"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["ref"] == assigned)
    );
    assert!(unfocused.value["assigned"][0]["current_status"].is_null());
    let current = add(&verbs, "Completed previous task", None, false, 2);
    hold(&verbs, &current, 3);
    verbs
        .done(
            DoneInput {
                summary: Some("Delivered the previous task".into()),
                ..DoneInput::default()
            },
            at(4),
        )
        .unwrap();
    let completed = verbs.next(&peek_input(false), at(5)).unwrap();
    assert_eq!(completed.value["focus"]["state"], "completed");
    assert!(completed.value["focus"]["holder"].is_null());
    assert_eq!(
        completed.value["held"].as_array().unwrap().as_slice(),
        &[] as &[serde_json::Value]
    );
    assert!(
        completed.value["assigned"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["ref"] == assigned)
    );
}
