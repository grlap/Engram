//! The criteria of a held, self-asserted item that carry no evidence link
//! yet are named while linking is still possible, on `show` and on the
//! holder's `gate` receipt, not only once the seal has frozen them.

use super::*;
use crate::work_service::WorkCriterionLinkInput;

fn add_item(verbs: &AgentVerbs, title: &str, acceptance: &[&str], second: i64) -> String {
    verbs
        .add(
            AddInput {
                title: title.into(),
                acceptance: acceptance.iter().map(|text| (*text).to_owned()).collect(),
                ..Default::default()
            },
            at(second),
        )
        .expect("add")
        .value["work"]["short_ref"]
        .as_str()
        .expect("work ref")
        .to_owned()
}

fn claim_item(verbs: &AgentVerbs, reference: &str, second: i64) {
    verbs
        .claim(
            ClaimInput {
                work_ref: reference.to_owned(),
                ttl_seconds: Some(3_600),
                recover: None,
            },
            at(second),
        )
        .expect("claim");
}

fn gate_on(verbs: &AgentVerbs, reference: &str, name: &str, second: i64) -> Receipt {
    verbs
        .gate(
            GateInput {
                work_ref: Some(reference.to_owned()),
                name: name.into(),
                failed: vec![],
                evidence_ref: None,
            },
            at(second),
        )
        .expect("gate")
}

/// The reminder lines naming criteria without a link.
fn unlinked_lines(receipt: &Receipt) -> Vec<String> {
    receipt
        .reminders
        .iter()
        .filter(|line| line.contains("no evidence link yet"))
        .cloned()
        .collect()
}

#[test]
fn a_held_self_asserted_item_names_its_unlinked_criteria_before_the_seal() {
    let (_home, verbs, path, project) = fixture();
    let reference = add_item(
        &verbs,
        "Linked before sealing",
        &["First outcome", "Second outcome", "Third outcome"],
        0,
    );
    claim_item(&verbs, &reference, 1);
    let three = "criteria 1, 2, 3 have no evidence link yet; link evidence in done, or pass the bound check first";

    // The holder's show names them after the holder's existing guidance, and
    // its JSON states the fact.
    let shown = verbs.show(&reference, at(2)).expect("show");
    assert_eq!(unlinked_lines(&shown), vec![three]);
    let progress = shown
        .reminders
        .iter()
        .position(|line| line == "you hold this item but have not noted progress yet")
        .expect("the progress reminder");
    let unlinked = shown
        .reminders
        .iter()
        .position(|line| line == three)
        .expect("the unlinked reminder");
    assert!(progress < unlinked, "{:?}", shown.reminders);
    assert!(shown.text().contains(three), "{}", shown.text());
    assert_eq!(
        shown.value["unlinked_criteria"],
        json!({"count": 3, "positions": [1, 2, 3]})
    );
    // MCP reads the same sentence.
    let mcp = AgentVerbs::new(
        path.clone(),
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    )
    .with_mcp_argument_names();
    assert_eq!(
        unlinked_lines(&mcp.show(&reference, at(2)).expect("mcp show")),
        vec![three]
    );
    // Another session neither sees the fact nor is told to link: only the
    // holder completes, and a peer's show stays as small as before.
    let other = AgentVerbs::new(
        path,
        project,
        "agent".into(),
        SessionId("other".into()),
        None,
    );
    let theirs = other.show(&reference, at(2)).expect("other show");
    assert!(unlinked_lines(&theirs).is_empty(), "{:?}", theirs.reminders);
    assert!(theirs.value.get("unlinked_criteria").is_none());

    // A gate is where a citable locator appears, so its receipt repeats it.
    let gated = gate_on(&verbs, &reference, "first-check", 3);
    assert_eq!(unlinked_lines(&gated), vec![three]);
    // MCP's gate receipt reads the same sentence.
    assert_eq!(
        unlinked_lines(&gate_on(&mcp, &reference, "mcp-check", 3)),
        vec![three]
    );

    // A revision is read as it now stands.
    verbs
        .update(
            UpdateInput {
                work_ref: Some(reference.clone()),
                action: UpdateAction::Revise {
                    external: None,
                    clear_external: false,
                    title: None,
                    outcome: None,
                    acceptance: Some(vec!["Second outcome".into(), "First outcome".into()]),
                    bindings: None,
                    assignee: None,
                    priority: None,
                    defer: None,
                    kind: None,
                    labels: Vec::new(),
                    unlabels: Vec::new(),
                },
            },
            at(4),
        )
        .expect("revise");
    let revised = verbs.show(&reference, at(5)).expect("show revised");
    assert_eq!(
        revised.value["unlinked_criteria"],
        json!({"count": 2, "positions": [1, 2]})
    );

    // Linking in done answers exactly the positions it links; the seal
    // leaves the rest unlinked, as the reminder said.
    let gate_locator = verbs
        .show_records(
            &reference,
            &ShowInput {
                notes: true,
                gates: true,
                ..Default::default()
            },
            at(5),
        )
        .expect("records")
        .value["notes"][0]["locator"]
        .as_str()
        .expect("gate locator")
        .to_owned();
    let basis = revised.value["acceptance_basis"].as_i64().expect("basis");
    let done = verbs
        .done(
            DoneInput {
                source_fingerprint: None,
                work_ref: Some(reference.clone()),
                summary: Some("Delivered".into()),
                links: vec![WorkCriterionLinkInput {
                    criterion: 2,
                    locator: gate_locator,
                }],
                link_basis: Some(basis),
                ..Default::default()
            },
            at(6),
        )
        .expect("done");
    assert!(!done.owed, "{}", done.text());
    assert_eq!(
        done.value["acceptance_evidence"]["unlinked_positions"],
        json!([1])
    );
    // Completed work asks nothing more.
    let completed = verbs.show(&reference, at(7)).expect("show completed");
    assert!(completed.value.get("unlinked_criteria").is_none());
    let after_seal = unlinked_lines(&completed);
    assert!(after_seal.is_empty(), "{after_seal:?}");
}

#[test]
fn the_unlinked_reminder_names_one_criterion_and_caps_many_with_an_exact_count() {
    let (_home, verbs, _path, _project) = fixture();
    let one = add_item(&verbs, "One criterion", &["Only outcome"], 0);
    claim_item(&verbs, &one, 1);
    assert_eq!(
        unlinked_lines(&verbs.show(&one, at(2)).expect("show")),
        vec![
            "criterion 1 has no evidence link yet; link evidence in done, or pass the bound check first"
        ]
    );
    let criteria: Vec<String> = (1..=10).map(|n| format!("Outcome {n}")).collect();
    let criteria: Vec<&str> = criteria.iter().map(String::as_str).collect();
    let many = add_item(&verbs, "Many criteria", &criteria, 3);
    claim_item(&verbs, &many, 4);
    let shown = verbs.show(&many, at(5)).expect("show");
    assert_eq!(
        unlinked_lines(&shown),
        vec![
            "criteria 1, 2, 3, 4, 5, 6, 7, 8 and 2 more have no evidence link yet; link evidence in done, or pass the bound check first"
        ]
    );
    assert_eq!(
        shown.value["unlinked_criteria"],
        json!({"count": 10, "positions": [1, 2, 3, 4, 5, 6, 7, 8]})
    );
}

// Under an evaluated policy the seal cites the evaluation's citations, and a
// pass cannot cite nothing, so neither show nor a gate receipt asks.
#[test]
fn an_evaluated_policy_names_no_unlinked_criteria() {
    let (_home, verbs, path, _project) = fixture();
    super::super::evaluate::enable(
        &path,
        &[crate::domain::AcceptanceEvaluationMode::IndependentSession],
        0,
    );
    let reference = add_item(&verbs, "Evaluated item", &["First", "Second"], 1);
    claim_item(&verbs, &reference, 2);
    let shown = verbs.show(&reference, at(3)).expect("show");
    assert!(shown.value.get("unlinked_criteria").is_none());
    assert!(unlinked_lines(&shown).is_empty(), "{:?}", shown.reminders);
    let gated = gate_on(&verbs, &reference, "evaluated-check", 4);
    assert!(unlinked_lines(&gated).is_empty(), "{:?}", gated.reminders);
}

// Every criterion linked is a known zero, not an absent fact, and asks
// nothing of the holder.
#[test]
fn every_criterion_linked_reads_as_a_zero_count_without_a_reminder() {
    let none = crate::verbs::acceptance::UnlinkedCriteria::from_positions(&[]);
    assert_eq!(
        serde_json::to_value(&none).expect("serialize"),
        json!({"count": 0, "positions": []})
    );
    assert_eq!(none.reminder(), None);
}

// An open item restored from a snapshot has no run and no holder until it is
// claimed, so its show carries nothing; the claim gives it a run with no
// obligation that could link, and the holder then sees every criterion.
#[test]
fn a_restored_item_without_a_run_names_every_criterion() {
    let (directory, verbs, path, project) = fixture();
    let reference = add_item(&verbs, "Restored item", &["First", "Second"], 0);
    let mut source = SqliteStore::open(&path).expect("source store");
    let actor = source
        .resolve_work_ref(&project, &reference)
        .expect("item")
        .created_by;
    let document = source
        .save_work_graph_snapshot(
            &project,
            &actor,
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(1),
            &crate::DevelopmentNoopRedactor,
        )
        .expect("save snapshot")
        .document;
    let restored_path = directory.path().join("restored.db");
    let mut restored = SqliteStore::open(&restored_path).expect("restored store");
    restored
        .load_work_graph_snapshot(
            &project,
            &actor,
            &serde_json::to_vec(&document).expect("encode snapshot"),
            false,
            at(2),
            &crate::DevelopmentNoopRedactor,
        )
        .expect("restore snapshot");
    let reader = AgentVerbs::new(
        restored_path,
        project,
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let before = reader.show(&reference, at(3)).expect("show restored");
    assert!(before.value.get("unlinked_criteria").is_none());
    let not_held = unlinked_lines(&before);
    assert!(not_held.is_empty(), "{not_held:?}");
    claim_item(&reader, &reference, 4);
    let held = reader.show(&reference, at(5)).expect("show claimed");
    assert_eq!(
        held.value["unlinked_criteria"],
        json!({"count": 2, "positions": [1, 2]})
    );
    assert_eq!(
        unlinked_lines(&held),
        vec![
            "criteria 1, 2 have no evidence link yet; link evidence in done, or pass the bound check first"
        ]
    );
}
