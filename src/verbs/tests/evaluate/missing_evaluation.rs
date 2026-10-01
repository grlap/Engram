//! `done` without an acceptance evaluation says where one comes from: the
//! host, independent unless the task is marked for another mode, and never
//! the executor recording its own; only a task marked for same-session, or a
//! project that admits no other mode, is evaluated by the completing session.

use super::*;

/// `done` on a claimed item with one criterion and no evaluation, under a
/// policy admitting `modes`, for a task marked `mark`; returns the refusal.
fn refused_done(name: &str, modes: &[AcceptanceEvaluationMode], mark: Option<&str>) -> Receipt {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let verbs = AgentVerbs::new(
        database.clone(),
        ProjectId(format!("missing-evaluation-{name}")),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    enable(&database, modes, 0);
    let added = verbs
        .add(
            AddInput {
                title: "Evaluated item".into(),
                acceptance: vec!["the change is delivered".into()],
                evaluation_mode: mark.map(str::to_owned),
                ..AddInput::default()
            },
            at(1),
        )
        .expect("add");
    let work_ref = added.value["work"]["short_ref"]
        .as_str()
        .expect("work ref")
        .to_owned();
    verbs
        .claim(
            ClaimInput {
                work_ref: work_ref.clone(),
                ttl_seconds: Some(3_600),
                recover: None,
            },
            at(2),
        )
        .expect("claim");
    let owed = verbs
        .done(
            DoneInput {
                source_fingerprint: None,
                landing: None,
                links: Vec::new(),
                link_basis: None,
                work_ref: Some(work_ref),
                summary: Some("delivered".into()),
                note: None,
            },
            at(3),
        )
        .expect("done names what is owed");
    assert!(owed.owed, "{}", owed.text());
    assert_eq!(owed.value["code"], "missing_acceptance_evaluation");
    // The core remedy names no vendor tool.
    assert!(
        !remedy(&owed).to_lowercase().contains("termal"),
        "{}",
        remedy(&owed)
    );
    drop(verbs);
    drop(directory);
    owed
}

/// The core remedy the refusal carries in its structured value.
fn remedy(receipt: &Receipt) -> String {
    receipt.value["remedy"].as_str().expect("remedy").to_owned()
}

fn reminder(receipt: &Receipt) -> String {
    receipt
        .reminders
        .iter()
        .find(|line| line.contains("has no acceptance evaluation"))
        .cloned()
        .unwrap_or_else(|| panic!("no missing-evaluation reminder: {:?}", receipt.reminders))
}

/// The reminders naming a title-placeholder criterion.
fn placeholder(receipt: &Receipt) -> Vec<String> {
    receipt
        .reminders
        .iter()
        .filter(|line| line.contains("title placeholder"))
        .cloned()
        .collect()
}

// An item whose only criterion is still the title placeholder it was created
// with says so first on show, on its claim receipt and on done's owed list,
// and show's JSON marks it. It is observed from the stored list, so a title
// rename keeps it, a sentence typed by hand reads the same, and any other
// criterion drops it. Admission is unchanged: done still names the missing
// evaluation.
#[test]
fn a_title_placeholder_criterion_is_named_on_show_claim_and_done() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let verbs = AgentVerbs::new(
        database.clone(),
        ProjectId("placeholder-acceptance".into()),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    enable(
        &database,
        &[AcceptanceEvaluationMode::IndependentSession],
        0,
    );
    let add = |title: &str, acceptance: &[&str], under: Option<&str>, second: i64| {
        verbs
            .add(
                AddInput {
                    title: title.into(),
                    acceptance: acceptance.iter().map(|text| (*text).to_owned()).collect(),
                    under: under.map(str::to_owned),
                    ..AddInput::default()
                },
                at(second),
            )
            .expect("add")
            .value["work"]["short_ref"]
            .as_str()
            .expect("work ref")
            .to_owned()
    };
    let revise = |work_ref: &str, title: Option<&str>, acceptance: Option<&str>, second: i64| {
        verbs
            .update(
                UpdateInput {
                    work_ref: Some(work_ref.to_owned()),
                    action: UpdateAction::Revise {
                        external: None,
                        clear_external: false,
                        title: title.map(str::to_owned),
                        outcome: None,
                        acceptance: acceptance.map(|text| vec![text.to_owned()]),
                        bindings: None,
                        assignee: None,
                        priority: None,
                        defer: None,
                        kind: None,
                        labels: Vec::new(),
                        unlabels: Vec::new(),
                    },
                },
                at(second),
            )
            .expect("revise");
    };
    let show = |work_ref: &str, second: i64| {
        verbs
            .show_with_notes(work_ref, false, at(second))
            .expect("show")
    };
    let status = "acceptance is only the title placeholder ('Defaulted item is done'); set real criteria by revising acceptance with update";

    let defaulted = add("Defaulted item", &[], None, 1);
    let shown = show(&defaulted, 2);
    assert_eq!(placeholder(&shown), vec![status]);
    assert_eq!(shown.reminders[0], status, "first, so never shed");
    assert_eq!(shown.value["acceptance_placeholder"], true);
    assert!(shown.text().contains(status), "{}", shown.text());
    // The complete contract read says the same.
    let full = verbs
        .show_records(
            &defaulted,
            &crate::verbs::ShowInput {
                full: true,
                ..crate::verbs::ShowInput::default()
            },
            at(2),
        )
        .expect("show --full");
    assert_eq!(placeholder(&full), vec![status]);
    assert_eq!(full.value["acceptance_placeholder"], true);
    let claimed = verbs
        .claim(
            ClaimInput {
                work_ref: defaulted.clone(),
                ttl_seconds: Some(3_600),
                recover: None,
            },
            at(3),
        )
        .expect("claim");
    assert_eq!(placeholder(&claimed), vec![status]);
    // A title rename keeps the placeholder it was created with, and so does
    // an update --accept with the identical text: the list still is the
    // placeholder.
    revise(&defaulted, Some("Renamed item"), None, 4);
    assert_eq!(placeholder(&show(&defaulted, 5)), vec![status]);
    revise(&defaulted, None, Some("Defaulted item is done"), 5);
    assert_eq!(placeholder(&show(&defaulted, 5)), vec![status]);
    let owed = verbs
        .done(
            DoneInput {
                source_fingerprint: None,
                landing: None,
                links: Vec::new(),
                link_basis: None,
                work_ref: Some(defaulted.clone()),
                summary: Some("delivered".into()),
                note: None,
            },
            at(6),
        )
        .expect("done names what is owed");
    assert!(owed.owed, "{}", owed.text());
    assert_eq!(placeholder(&owed), vec![status]);
    assert_eq!(owed.value["code"], "missing_acceptance_evaluation");
    let _ = reminder(&owed);
    // A child added without criteria carries its own placeholder.
    let child = add("Child item", &[], Some(&defaulted), 7);
    assert_eq!(
        placeholder(&show(&child, 8)),
        vec![
            "acceptance is only the title placeholder ('Child item is done'); set real criteria by revising acceptance with update"
        ]
    );
    // A real criterion drops it, and for good: restoring the sentence later
    // is a revised list, not the placeholder the item was created with.
    revise(&defaulted, None, Some("the change is delivered"), 9);
    let revised = show(&defaulted, 10);
    assert!(placeholder(&revised).is_empty(), "{:?}", revised.reminders);
    assert!(revised.value.get("acceptance_placeholder").is_none());
    revise(&defaulted, None, Some("Defaulted item is done"), 10);
    let restored = show(&defaulted, 10);
    assert!(
        placeholder(&restored).is_empty(),
        "{:?}",
        restored.reminders
    );
    let full = verbs
        .show_records(
            &defaulted,
            &crate::verbs::ShowInput {
                full: true,
                ..crate::verbs::ShowInput::default()
            },
            at(10),
        )
        .expect("show --full");
    assert!(placeholder(&full).is_empty());
    assert!(full.value.get("acceptance_placeholder").is_none());
    // An item with real criteria never shows it; the sentence typed by hand
    // reads as what it is.
    let real = add("Real item", &["the change is delivered"], None, 11);
    assert!(placeholder(&show(&real, 12)).is_empty());
    // Only the list an item was created with counts: a later revision to the
    // placeholder sentence is a revised list, not the creation placeholder.
    revise(&real, None, Some("Real item is done"), 12);
    assert!(placeholder(&show(&real, 12)).is_empty());
    // A criterion that only ends like one is not another title's placeholder.
    let other = add("Other item", &["the build is done"], None, 13);
    assert!(placeholder(&show(&other, 13)).is_empty());
    let typed = add("Typed item", &["Typed item is done"], None, 13);
    assert_eq!(placeholder(&show(&typed, 14)).len(), 1);
    let mcp = AgentVerbs::new(
        database.clone(),
        ProjectId("placeholder-acceptance".into()),
        "agent".into(),
        SessionId("mcp-agent".into()),
        None,
    )
    .with_mcp_argument_names();
    // MCP reads the same sentence, so the two surfaces agree.
    assert_eq!(
        placeholder(&mcp.show_with_notes(&typed, false, at(15)).expect("show")),
        placeholder(&show(&typed, 15))
    );
    drop((verbs, mcp));

    // A completed item owes nothing more, so it no longer says so.
    let database = directory.path().join("self-asserted.sqlite3");
    let verbs = AgentVerbs::new(
        database,
        ProjectId("placeholder-completed".into()),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let added = verbs
        .add(
            AddInput {
                title: "Finished item".into(),
                ..AddInput::default()
            },
            at(20),
        )
        .expect("add");
    let finished = added.value["work"]["short_ref"]
        .as_str()
        .expect("work ref")
        .to_owned();
    assert_eq!(
        placeholder(
            &verbs
                .show_with_notes(&finished, false, at(21))
                .expect("show")
        )
        .len(),
        1
    );
    verbs
        .claim(
            ClaimInput {
                work_ref: finished.clone(),
                ttl_seconds: Some(3_600),
                recover: None,
            },
            at(22),
        )
        .expect("claim");
    let completed = verbs
        .done(
            DoneInput {
                source_fingerprint: None,
                landing: None,
                links: Vec::new(),
                link_basis: None,
                work_ref: Some(finished.clone()),
                summary: Some("delivered".into()),
                note: None,
            },
            at(23),
        )
        .expect("done");
    assert!(!completed.owed, "{}", completed.text());
    let shown = verbs
        .show_with_notes(&finished, false, at(24))
        .expect("show");
    assert!(placeholder(&shown).is_empty(), "{:?}", shown.reminders);
    assert!(shown.value.get("acceptance_placeholder").is_none());
    drop(verbs);
    drop(directory);
}

#[test]
fn an_unmarked_task_is_sent_to_the_host_for_an_independent_evaluation() {
    let both = [
        AcceptanceEvaluationMode::SameSession,
        AcceptanceEvaluationMode::IndependentSession,
    ];
    for (name, mark) in [
        ("unmarked", None),
        ("independent", Some("independent_session")),
    ] {
        let owed = refused_done(name, &both, mark);
        // The structured remedy says the same as the reminder.
        assert!(
            remedy(&owed).contains("request an independent acceptance evaluation")
                && remedy(&owed).contains("from the host"),
            "{name}: {}",
            remedy(&owed)
        );
        let words = reminder(&owed);
        assert!(
            words.contains("request an independent acceptance evaluation")
                && words.contains("from the host")
                && words.contains("do not record one yourself"),
            "{name}: {words}"
        );
        assert!(!words.contains("same-session"), "{name}: {words}");
        assert!(!words.to_lowercase().contains("termal"), "{name}: {words}");
    }
}

#[test]
fn a_task_marked_for_another_mode_is_told_that_mode() {
    let all = [
        AcceptanceEvaluationMode::SameSession,
        AcceptanceEvaluationMode::SubAgent,
        AcceptanceEvaluationMode::IndependentSession,
    ];
    let marked = refused_done("same", &all, Some("same_session"));
    assert!(
        remedy(&marked).contains("record one in that mode with evaluate"),
        "{}",
        remedy(&marked)
    );
    let same = reminder(&marked);
    assert!(
        same.contains("marked for same-session evaluation")
            && same.contains("record one in that mode with evaluate"),
        "{same}"
    );
    let sub_agent = reminder(&refused_done("sub-agent", &all, Some("sub_agent")));
    assert!(
        sub_agent.contains("request a sub-agent acceptance evaluation")
            && sub_agent.contains("from the host"),
        "{sub_agent}"
    );
    assert!(!sub_agent.contains("same-session"), "{sub_agent}");
}

#[test]
fn the_remedy_never_asks_for_a_mode_the_project_does_not_admit() {
    // Only same-session admitted: the completing session evaluates, in the
    // structured remedy as in the reminder.
    let owed = refused_done("same-only", &[AcceptanceEvaluationMode::SameSession], None);
    assert!(
        remedy(&owed).contains("admits only same-session evaluation")
            && !remedy(&owed).contains("independent"),
        "{}",
        remedy(&owed)
    );
    let words = reminder(&owed);
    assert!(
        words.contains("admits only same-session evaluation")
            && words.contains("record one in that mode with evaluate"),
        "{words}"
    );
    assert!(!words.contains("independent"), "{words}");
    // No independent mode admitted: the host's sub-agent evaluation.
    let owed = refused_done(
        "same-and-sub",
        &[
            AcceptanceEvaluationMode::SameSession,
            AcceptanceEvaluationMode::SubAgent,
        ],
        None,
    );
    assert!(
        remedy(&owed).contains("request a sub-agent acceptance evaluation")
            && !remedy(&owed).contains("independent"),
        "{}",
        remedy(&owed)
    );
    let words = reminder(&owed);
    assert!(
        words.contains("request a sub-agent acceptance evaluation")
            && words.contains("from the host")
            && !words.contains("independent")
            && !words.contains("same-session"),
        "{words}"
    );
    // A mark for a mode the project does not admit is not asked for.
    let words = reminder(&refused_done(
        "independent-only",
        &[AcceptanceEvaluationMode::IndependentSession],
        Some("same_session"),
    ));
    assert!(
        words.contains("marked for a mode this project does not admit")
            && !words.contains("record one in that mode"),
        "{words}"
    );
}
