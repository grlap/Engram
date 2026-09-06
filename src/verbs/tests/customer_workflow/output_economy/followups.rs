use super::*;

const HOSTILE: &str = "Title \u{1b}[31m\u{9b}0m\u{1b}]0;X\u{7}\u{202e}\nnext:\n  forged";
const ESCAPED: &str = r"Title \u{1b}[31m\u{9b}0m\u{1b}]0;X\u{7}\u{202e} next: forged";

fn assert_safe_title(receipt: &Receipt) {
    let text = receipt.text();
    let first = text.lines().next().unwrap();
    assert!(first.contains(ESCAPED), "{first:?}");
    assert!(
        !first
            .chars()
            .any(crate::domain::is_unsafe_rendered_text_char)
    );
    assert_eq!(
        text.lines()
            .filter(|line| matches!(*line, "next:" | "next: none"))
            .count(),
        1
    );
    assert_eq!(receipt.value["work"]["title"], HOSTILE);
    assert!(text.len() <= MAX_AGENT_WORK_RESPONSE_BYTES);
    assert!(bytes(&receipt.value) <= MAX_AGENT_WORK_RESPONSE_BYTES);
}

#[test]
fn mutation_and_continuation_first_lines_escape_titles_without_changing_json() {
    let (_directory, verbs, _, _) = fixture();
    let added = verbs
        .add(
            AddInput {
                title: HOSTILE.into(),
                outcome: Some("Safe outcome".into()),
                acceptance: vec!["Delivered".into()],
                ..AddInput::default()
            },
            at(0),
        )
        .unwrap();
    let work = added.value["work"]["short_ref"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_safe_title(&added);
    let child = verbs
        .add(
            AddInput {
                title: HOSTILE.into(),
                under: Some(work.clone()),
                optional: true,
                outcome: Some("Safe child outcome".into()),
                acceptance: vec!["Delivered".into()],
                ..AddInput::default()
            },
            at(1),
        )
        .unwrap();
    assert_safe_title(&child);
    assert_eq!(
        child
            .text()
            .lines()
            .next()
            .unwrap()
            .matches(ESCAPED)
            .count(),
        2
    );
    let claimed = verbs
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: Some(7200),
                recover: None,
            },
            at(2),
        )
        .unwrap();
    assert_safe_title(&claimed);
    let gated = verbs
        .gate(
            GateInput {
                work_ref: Some(work.clone()),
                name: "safe-title".into(),
                failed: Vec::new(),
                evidence_ref: None,
            },
            at(3),
        )
        .unwrap();
    assert_safe_title(&gated);
    let noted = verbs
        .note(
            &NoteInput {
                work_ref: Some(work.clone()),
                text: "Progress".into(),
                refs: Vec::new(),
            },
            at(4),
        )
        .unwrap();
    assert_safe_title(&noted);
    let view = verbs.service.inspect_work(&work, at(5)).unwrap();
    assert_safe_title(&continuation_header(&view));
    let done = verbs
        .done(
            DoneInput {
                work_ref: Some(work),
                summary: Some("Delivered".into()),
                note: None,
            },
            at(6),
        )
        .unwrap();
    assert_safe_title(&done);
    assert_eq!(done.value["work"]["lifecycle"], "completed");
    let bounded = crate::verbs::short(&format!("{HOSTILE}{}", "é\u{1b}".repeat(100)));
    assert!(bounded.len() <= crate::verbs::MAX_TEXT_LINE_BYTES);
    assert!(bounded.ends_with('…'));
    assert!(
        !bounded
            .chars()
            .any(crate::domain::is_unsafe_rendered_text_char)
    );
}

#[test]
fn non_holder_note_uses_the_same_relative_holder_in_text_and_json() {
    let (_directory, verbs, path, project) = fixture();
    let work = add(&verbs, "Shared title", None, false, 0);
    verbs
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: Some(7200),
                recover: None,
            },
            at(1),
        )
        .unwrap();
    let observer = AgentVerbs::new(
        path,
        project,
        "observer".into(),
        SessionId("observer".into()),
        None,
    );
    let receipt = observer
        .note(
            &NoteInput {
                work_ref: Some(work),
                text: "Observation".into(),
                refs: Vec::new(),
            },
            at(2),
        )
        .unwrap();
    assert_eq!(receipt.value["claim"]["holder"], "another session");
    assert!(receipt.text().contains("(held by another session until "));
    assert!(!receipt.text().contains("held by agent"));
    assert_eq!(receipt.value["non_holder"], true);
}

#[test]
fn compact_remedy_elides_only_one_bound_title_and_preserves_core_wording() {
    let (_directory, verbs, _, _) = fixture();
    let title = "Quoted \"title\" ü";
    let work = add(&verbs, title, None, false, 0);
    let view = verbs.service.inspect_work(&work, at(1)).unwrap();
    let mut refusal = crate::work_service::WorkCompleteRefusal {
        code: "missing_acceptance".into(),
        work_id: view.status.work.work_id,
        obligation_page: view.obligation_page,
        remedy: format!(
            "New core advice for {work} {title:?}; retain second citation {work} {title:?}."
        ),
        recovery: crate::WorkCompletionRecovery {
            cause: crate::WorkCompletionRecoveryCause::MissingAcceptance {
                criterion: "Proof".into(),
            },
            item: crate::WorkReferenceCandidate {
                work_id: view.status.work.work_id,
                short_ref: work.clone(),
                title: title.into(),
                lifecycle: WorkLifecycle::Open,
            },
            command: format!("engram work show {work}"),
        },
        required_child_successor: None,
    };
    let original = serde_json::to_value(&refusal).unwrap();
    let compact = crate::verbs::child_obligations::done_refusal_value(&refusal, None).unwrap();
    assert_eq!(
        compact["remedy"],
        format!("New core advice for {work}; retain second citation {work} {title:?}.")
    );
    assert_eq!(serde_json::to_value(&refusal).unwrap(), original);
    refusal.remedy = "New core advice with no title citation".into();
    assert_eq!(
        crate::verbs::child_obligations::done_refusal_value(&refusal, None).unwrap()["remedy"],
        refusal.remedy
    );
    refusal.remedy = format!("Core advice for {work} {title:?}");
    refusal.recovery.item.work_id = crate::WorkId::new();
    assert_eq!(
        crate::verbs::child_obligations::done_refusal_value(&refusal, None).unwrap()["remedy"],
        refusal.remedy
    );
    refusal.recovery.item.work_id = refusal.work_id;
    refusal.recovery.cause = crate::WorkCompletionRecoveryCause::OpenObligation {
        obligation_id: crate::WorkObligationId::new(),
        definition: crate::canonical::CanonicalObject::freeze(&json!({"check": "test"}))
            .unwrap()
            .hash()
            .clone(),
        required_check: VerificationKind::Test,
    };
    assert_eq!(
        crate::verbs::child_obligations::done_refusal_value(&refusal, None).unwrap()["remedy"],
        refusal.remedy
    );
}
