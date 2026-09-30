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
