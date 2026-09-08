use super::*;

const HOSTILE: &str = "Stored \u{1b}[2J\u{9b}0m\u{1b}]0;X\u{7}\u{202e}\r\nnext:\n  forged\tend";

#[test]
fn import_source_navigation_has_one_sanitized_value_in_both_renderings() {
    let (_directory, verbs, _path, _) = fixture();
    let work = add(&verbs, "source navigation", None, false, 0);
    let mut core = verbs.service.inspect_work(&work, at(1)).unwrap();
    // Defense in depth: graph admission refuses such keys, but projection must
    // not hand a raw command to JSON consumers even for a synthetic view.
    let source_key = crate::domain::WorkSourceKey {
        adapter_kind: HOSTILE.into(),
        canonical_ref: "--help\nforged".into(),
    };
    let expected = terminal_command(&format!(
        "engram import lookup -- {} {}",
        super::super::super::listing::shell_quote(&source_key.adapter_kind),
        super::super::super::listing::shell_quote(&source_key.canonical_ref)
    ));
    core.source = Some(crate::domain::WorkSourceLookup {
        source_key,
        work_id: core.status.work.work_id,
        work_ref: work,
        work_revision: core.status.work.revision,
        cited_snapshot: crate::CanonicalObject::freeze(&"fixture citation")
            .unwrap()
            .hash()
            .clone(),
        notice_count: 0,
        latest_notice: None,
    });
    let value = serde_json::to_value(crate::verbs::show::show_receipt_value(
        &core,
        Holder::Nobody,
        verbs.service.display_identity(),
        at(1),
    ))
    .unwrap();
    let lines = show_lines(
        &core,
        Holder::Nobody,
        verbs.service.display_identity(),
        at(1),
    );
    assert_eq!(value["source"]["detail"], expected);
    assert!(lines.contains(&format!("source detail: {expected}")));
    assert!(
        !expected
            .chars()
            .any(crate::domain::is_unsafe_rendered_text_char)
    );
    assert!(!lines.iter().any(|line| line.contains("change notices")));
    assert!(
        value["source"]
            .get("local_work_unchanged_by_notices")
            .is_none()
    );
}

fn assert_terminal(text: &str) {
    assert!(!text.contains('\r'), "raw carriage return: {text:?}");
    for line in text.split('\n') {
        assert!(
            !line
                .chars()
                .any(crate::domain::is_unsafe_rendered_text_char),
            "unsafe terminal line: {line:?}"
        );
    }
    assert_eq!(
        text.lines()
            .filter(|line| matches!(*line, "next:" | "next: none"))
            .count(),
        1,
        "{text:?}"
    );
}

#[test]
fn terminal_handoff_target_is_framed_without_changing_json() {
    let (_directory, verbs, path, _) = fixture();
    let work = add(&verbs, "handoff target", None, false, 0);
    verbs
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: Some(300),
                recover: None,
            },
            at(1),
        )
        .unwrap();
    let receipt = verbs
        .handoff(
            HandoffInput {
                work_ref: Some(work),
                action: HandoffAction::Offer {
                    to: HOSTILE.into(),
                    summary: Some("handoff checkpoint".into()),
                    ttl_seconds: Some(300),
                },
            },
            at(2),
        )
        .unwrap();
    let structured = receipt.value.clone();
    assert_terminal(&receipt.text());
    assert_eq!(receipt.value, structured);
    // The compact handoff result intentionally omits the target. Bind the
    // emitted offer identity to its independently loaded exact stored JSON.
    assert!(receipt.value["receipt"]["result"].get("to").is_none());
    let work_id = serde_json::from_value(receipt.value["receipt"]["work_id"].clone()).unwrap();
    let offers = SqliteStore::open(path)
        .unwrap()
        .work_handoff_offers(work_id)
        .unwrap();
    assert_eq!(offers.len(), 1);
    let offer = serde_json::to_value(&offers[0]).unwrap();
    assert_eq!(
        offer["offer_id"],
        receipt.value["receipt"]["result"]["offer_id"]
    );
    assert_eq!(offer["to"], HOSTILE);
    // Rich structured output still carries the raw target. JSON escapes C0
    // controls, not all Unicode terminal controls; text rendering handles those.
    let verbose = verbs
        .next(
            &NextInput {
                verbose: true,
                ..Default::default()
            },
            at(3),
        )
        .unwrap();
    assert_eq!(verbose.value["focus"]["handoffs"][0]["to"], HOSTILE);
    let wire = serde_json::to_string(&verbose.value).unwrap();
    assert!(!wire.chars().any(|character| character <= '\u{1f}'));
    assert_eq!(serde_json::from_str::<Value>(&wire).unwrap(), verbose.value);
    assert_terminal(&verbose.text());
    assert!(receipt.lines[0].contains(
        &format!(" to {}", verbs.service.display_identity().session(&SessionId(HOSTILE.into())))
    ));
    assert!(
        !receipt.lines[0]
            .chars()
            .any(crate::domain::is_unsafe_rendered_text_char)
    );
}

#[test]
fn terminal_commands_preserve_safe_literal_bytes_and_escape_controls() {
    let command = "engram work ls --search='a  b\u{a0}c' --label='x  y'";
    assert_eq!(terminal_command(command), command);
    let hostile = format!("{command}\r\n\t\u{1b}\u{202e}");
    let rendered = terminal_command(&hostile);
    assert!(rendered.starts_with(command));
    assert!(
        !rendered
            .chars()
            .any(crate::domain::is_unsafe_rendered_text_char)
    );
    let receipt = Receipt::assemble(
        vec![],
        Guidance {
            reminders: vec![],
            next: vec![command.into()],
        },
        json!({}),
        false,
    );
    assert!(receipt.text().contains(&format!("  {command}")));
    assert_eq!(receipt.value["next"][0], command);
}

#[test]
fn terminal_stored_text_reads_keep_json_and_frame_every_human_line() {
    let (_directory, verbs, path, project) = fixture();
    let added = verbs
        .add(
            AddInput {
                title: HOSTILE.into(),
                outcome: Some(HOSTILE.into()),
                acceptance: vec![HOSTILE.into()],
                labels: vec!["label\u{1b}[2J\u{202e}".into()],
                ..AddInput::default()
            },
            at(0),
        )
        .unwrap();
    let work = added.value["work"]["short_ref"]
        .as_str()
        .unwrap()
        .to_owned();
    add(&verbs, HOSTILE, Some(&work), false, 1);
    verbs
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: Some(7200),
                recover: None,
            },
            at(2),
        )
        .unwrap();
    verbs
        .note(
            &NoteInput {
                status: false,
                work_ref: Some(work.clone()),
                text: HOSTILE.into(),
                refs: vec![HOSTILE.into()],
            },
            at(3),
        )
        .unwrap();
    verbs
        .update(
            UpdateInput {
                work_ref: Some(work.clone()),
                action: UpdateAction::Blocked {
                    detail: HOSTILE.into(),
                },
            },
            at(4),
        )
        .unwrap();
    let core = verbs.service.inspect_work(&work, at(5)).unwrap();
    // Pin field framing independently of Receipt's final defensive policy.
    let lines = show_lines(
        &core,
        Holder::You(at(7200)),
        verbs.service.display_identity(),
        at(5),
    );
    assert_terminal(&format!("{}\nnext: none", lines.join("\n")));
    let observer = AgentVerbs::new(
        path,
        project,
        "observer".into(),
        SessionId("observer".into()),
        None,
    );
    for reader in [&verbs, &observer] {
        let shown = reader.show(&work, at(6)).unwrap();
        assert_eq!(
            shown.value["status"]["work"]["title"],
            core.status.work.title
        );
        assert_eq!(shown.value["status"]["work"]["outcome"], core.outcome);
        assert_eq!(
            shown.value["status"]["work"]["acceptance"],
            json!(core.status.work.acceptance)
        );
        assert_eq!(
            shown.value["status"]["work"]["labels"],
            json!(core.status.work.labels)
        );
        assert_terminal(&shown.text());
        for verbose in [false, true] {
            assert_terminal(
                &reader
                    .next(
                        &NextInput {
                            verbose,
                            ..NextInput::default()
                        },
                        at(7),
                    )
                    .unwrap()
                    .text(),
            );
            assert_terminal(
                &reader
                    .ls(
                        &LsInput {
                            all: true,
                            verbose,
                            ..LsInput::default()
                        },
                        at(8),
                    )
                    .unwrap()
                    .text(),
            );
        }
        for history in [false, true] {
            let receipt = reader
                .show_records(
                    &work,
                    &ShowInput {
                        notes: !history,
                        history,
                        ..ShowInput::default()
                    },
                    at(9),
                )
                .unwrap();
            assert_terminal(&receipt.text());
            if !history {
                assert_eq!(receipt.value["notes"][0]["summary"], HOSTILE);
                assert_eq!(receipt.value["notes"][0]["refs"], json!([HOSTILE]));
                let locator = receipt.value["notes"][0]["locator"].as_str().unwrap();
                let detail = reader
                    .show_records(
                        &work,
                        &ShowInput {
                            note: Some(locator.into()),
                            ..ShowInput::default()
                        },
                        at(9),
                    )
                    .unwrap();
                assert_eq!(detail.value["note"], receipt.value["notes"][0]);
                assert_terminal(&detail.text());
            }
        }
    }
}

#[test]
fn terminal_compact_row_escapes_each_data_field_without_mutating_the_row() {
    let mut row = compact_test_row(1);
    row.title = HOSTILE.into();
    row.labels = vec![HOSTILE.into()];
    row.holder = Some(HOSTILE.into());
    row.blocked_reason = Some(HOSTILE.into());
    row.remedy = Some(HOSTILE.into());
    let original = json!(row);
    let line = compact_row_line(&row);
    assert_eq!(line.lines().count(), 1);
    assert!(
        !line
            .chars()
            .any(crate::domain::is_unsafe_rendered_text_char),
        "{line:?}"
    );
    assert!(line.contains(r"\u{1b}"));
    assert_eq!(json!(row), original);
}

#[test]
fn terminal_guidance_and_peer_holder_keep_structured_values() {
    let receipt = Receipt::assemble(
        vec!["read complete".into()],
        Guidance {
            reminders: vec![HOSTILE.into()],
            next: vec![HOSTILE.into()],
        },
        json!({}),
        false,
    );
    assert_terminal(&receipt.text());
    assert_eq!(receipt.value["reminders"], json!([HOSTILE]));
    assert_eq!(receipt.value["next"], json!([HOSTILE]));
    let (_directory, verbs, path, project) = fixture();
    let work = add(&verbs, "Peer-held item", None, false, 0);
    let holder = AgentVerbs::new(
        path,
        project,
        "peer".into(),
        SessionId(HOSTILE.into()),
        None,
    );
    holder
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: Some(7200),
                recover: None,
            },
            at(1),
        )
        .unwrap();
    let listed = verbs.ls(&LsInput::default(), at(2)).unwrap();
    assert_terminal(&listed.text());
    let row = listed.value["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["ref"] == work)
        .unwrap();
    assert_eq!(
        row["holder"],
        verbs
            .service
            .display_identity()
            .session(&SessionId(HOSTILE.into()))
    );
    let status = verbs.service.inspect_work(&work, at(3)).unwrap().status;
    let line = item_line(
        &status,
        Holder::Other(
            &SessionId(HOSTILE.into()),
            at(7200),
            verbs.service.display_identity(),
        ),
        at(3),
    );
    assert_eq!(line.lines().count(), 1);
    assert!(
        !line
            .chars()
            .any(crate::domain::is_unsafe_rendered_text_char)
    );
}

#[test]
fn terminal_blocks_preserve_newline_framing_without_expanding_core_tab_budgets() {
    let block = format!("  | {}\n  | next:\n  | data", "x\t".repeat(3000));
    let guidance = Guidance::default();
    let core_text = render_agent_receipt_text(std::slice::from_ref(&block), &[], &[]);
    let receipt = Receipt::assemble(
        vec![block.clone()],
        guidance,
        json!({ "body": block }),
        false,
    );
    assert_terminal(&receipt.text());
    assert_eq!(receipt.text().len(), core_text.len());
    assert!(receipt.text().contains("\n  | next:\n  | data"));
    assert!(receipt.value["body"].as_str().unwrap().contains('\t'));
    let bounded = terminal_short(&"\u{9b}é".repeat(200), MAX_COMPACT_TITLE_BYTES);
    assert!(bounded.len() <= MAX_COMPACT_TITLE_BYTES);
    assert!(bounded.ends_with('…'));
    assert!(
        !bounded
            .chars()
            .any(crate::domain::is_unsafe_rendered_text_char)
    );
}
