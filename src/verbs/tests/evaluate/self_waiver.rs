//! Independent evaluation by default: an unmarked task takes the completing
//! session's own evaluation only where the project admits no other mode, and
//! a same-session mark waives independence only while the session that set
//! it neither evaluates, holds nor executes the run. The mark's author is the
//! session of the event that set it, rechecked when completion consumes it.

use super::*;

/// One project, its database and three sessions: `agent` executes, `peer`
/// plans or reviews, and `judge` never touches the run.
struct Project {
    database: std::path::PathBuf,
    id: ProjectId,
    agent: AgentVerbs,
    peer: AgentVerbs,
    judge: AgentVerbs,
    _directory: crate::test_support::TempHome,
}

fn project(name: &str, modes: &[AcceptanceEvaluationMode]) -> Project {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let id = ProjectId(format!("self-waiver-{name}"));
    let session = |who: &str| {
        AgentVerbs::new(
            database.clone(),
            id.clone(),
            who.into(),
            SessionId(who.into()),
            None,
        )
    };
    let (agent, peer, judge) = (session("agent"), session("peer"), session("judge"));
    enable(&database, modes, 0);
    Project {
        database,
        id,
        agent,
        peer,
        judge,
        _directory: directory,
    }
}

const BOTH: [AcceptanceEvaluationMode; 2] = [
    AcceptanceEvaluationMode::SameSession,
    AcceptanceEvaluationMode::IndependentSession,
];

#[allow(
    clippy::unused_self,
    reason = "each helper reads as one step of the same project"
)]
impl Project {
    /// `creator` adds an item, marked `mark`; returns its ref.
    fn add(&self, creator: &AgentVerbs, mark: Option<&str>, second: i64) -> String {
        creator
            .add(
                AddInput {
                    title: format!("Evaluated item {second}"),
                    acceptance: vec!["the change is verified".into()],
                    evaluation_mode: mark.map(str::to_owned),
                    ..AddInput::default()
                },
                at(second),
            )
            .expect("add")
            .value["work"]["short_ref"]
            .as_str()
            .expect("ref")
            .to_owned()
    }

    /// `holder` claims `work` and records a passing gate on it.
    fn take(&self, holder: &AgentVerbs, work: &str, second: i64) {
        holder
            .claim(
                ClaimInput {
                    work_ref: work.into(),
                    ttl_seconds: Some(3_600),
                    recover: None,
                },
                at(second),
            )
            .expect("claim");
        holder
            .gate(
                GateInput {
                    work_ref: Some(work.into()),
                    name: "cargo-test".into(),
                    failed: Vec::new(),
                    evidence_ref: None,
                },
                at(second + 1),
            )
            .expect("gate");
    }

    fn mark(&self, by: &AgentVerbs, work: &str, mode: Option<&str>, second: i64) {
        by.update(
            UpdateInput {
                work_ref: Some(work.into()),
                action: UpdateAction::EvaluationMode {
                    mode: mode.map(str::to_owned),
                },
            },
            at(second),
        )
        .expect("mark the evaluation mode");
    }

    /// `evaluator` records a verdict in `mode` on the bases `show` prints; a
    /// pass cites the run's evidence.
    fn evaluate(
        &self,
        evaluator: &AgentVerbs,
        work: &str,
        mode: &str,
        verdict_word: &str,
        second: i64,
    ) -> Result<Receipt, VerbError> {
        let shown = evaluator.show(work, at(second)).expect("show");
        let citations = if verdict_word == "pass" {
            let store = SqliteStore::open(&self.database).expect("store");
            let run = store
                .resolve_work_ref(&self.id, work)
                .ok()
                .and_then(|item| item.active_run_id);
            run.map(|run| {
                store
                    .work_run_evidence(run)
                    .expect("run evidence")
                    .into_iter()
                    .map(|hash| hash.as_str().to_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
        } else {
            Vec::new()
        };
        // A sub-agent names the holder, `agent`, as its parent.
        let sub_agent = mode == "sub_agent";
        evaluator.evaluate(
            EvaluateInput {
                mode: mode.into(),
                execution_identity: sub_agent.then(|| "sub-agent".into()),
                parent_session: sub_agent.then(|| "agent".into()),
                acceptance_basis: shown.value["acceptance_basis"]
                    .as_i64()
                    .expect("acceptance basis"),
                ..evaluate_input(
                    work,
                    shown.value["evidence_basis"]
                        .as_i64()
                        .expect("evidence basis"),
                    vec![verdict(1, verdict_word, "judgment", &citations)],
                )
            },
            at(second),
        )
    }

    fn done(&self, holder: &AgentVerbs, work: &str, second: i64) -> Receipt {
        holder
            .done(
                DoneInput {
                    work_ref: Some(work.into()),
                    summary: Some("finished".into()),
                    ..DoneInput::default()
                },
                at(second),
            )
            .expect("done answers")
    }
}

fn refusal(result: Result<Receipt, VerbError>) -> String {
    result.expect_err("the evaluation refuses").to_string()
}

#[test]
fn an_unmarked_task_refuses_its_holders_own_evaluation_while_another_mode_is_admitted() {
    let both = project("unmarked-both", &BOTH);
    let work = both.add(&both.agent, None, 1);
    both.take(&both.agent, &work, 2);
    let words = refusal(both.evaluate(&both.agent, &work, "same_session", "fail", 4));
    assert!(
        words.contains("not marked for same-session evaluation")
            && words.contains("request an independent evaluation from the host")
            && words.contains("marked for it by someone other than its executor"),
        "{words}"
    );
    // A session that never held the run evaluates instead.
    both.evaluate(&both.judge, &work, "independent_session", "fail", 5)
        .expect("an independent evaluation records");

    // A project that admits only same-session is unchanged.
    let same_only = project(
        "unmarked-same-only",
        &[AcceptanceEvaluationMode::SameSession],
    );
    let work = same_only.add(&same_only.agent, None, 1);
    same_only.take(&same_only.agent, &work, 2);
    same_only
        .evaluate(&same_only.agent, &work, "same_session", "fail", 4)
        .expect("the holder's own evaluation records where no other mode is admitted");
}

#[test]
fn a_mark_its_executor_set_never_waives_independence() {
    // Set at creation by the session that later executes.
    let created = project("self-created", &BOTH);
    let work = created.add(&created.agent, Some("same_session"), 1);
    created.take(&created.agent, &work, 2);
    let words = refusal(created.evaluate(&created.agent, &work, "same_session", "fail", 4));
    assert!(
        words.contains("same-session mark was set by a session that evaluates, holds or executes"),
        "{words}"
    );
    // The same holds in a project that admits only same-session.
    let same_only = project(
        "self-created-same-only",
        &[AcceptanceEvaluationMode::SameSession],
    );
    let work = same_only.add(&same_only.agent, Some("same_session"), 1);
    same_only.take(&same_only.agent, &work, 2);
    let words = refusal(same_only.evaluate(&same_only.agent, &work, "same_session", "fail", 4));
    assert!(
        words.contains("same-session mark was set by a session")
            && words.contains("clearing the mark alone lets its executor evaluate"),
        "{words}"
    );

    // Set by the holder to escape a failed independent evaluation.
    let escape = project("escape", &BOTH);
    let work = escape.add(&escape.agent, None, 1);
    escape.take(&escape.agent, &work, 2);
    escape
        .evaluate(&escape.judge, &work, "independent_session", "fail", 4)
        .expect("an independent evaluation fails");
    escape.mark(&escape.agent, &work, Some("same_session"), 5);
    let words = refusal(escape.evaluate(&escape.agent, &work, "same_session", "pass", 6));
    assert!(
        words.contains("same-session mark was set by a session that evaluates, holds or executes"),
        "{words}"
    );
    // Clearing the mark again leaves an unmarked task: still refused.
    escape.mark(&escape.agent, &work, None, 7);
    assert!(
        refusal(escape.evaluate(&escape.agent, &work, "same_session", "pass", 8))
            .contains("not marked for same-session evaluation"),
    );
}

#[test]
fn a_peers_mark_is_accepted_and_keeps_its_author_through_other_revisions() {
    let project = project("peer-mark", &BOTH);
    let work = project.add(&project.peer, Some("same_session"), 1);
    project.take(&project.agent, &work, 2);
    // A revision of another field by the holder keeps the peer as the
    // mark's author; so does reasserting the unchanged mark.
    project
        .agent
        .update(
            UpdateInput {
                work_ref: Some(work.clone()),
                action: UpdateAction::Revise {
                    external: None,
                    clear_external: false,
                    title: Some("Retitled by the holder".into()),
                    outcome: None,
                    acceptance: None,
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
        .expect("retitle");
    project.mark(&project.agent, &work, Some("same_session"), 5);
    project
        .evaluate(&project.agent, &work, "same_session", "pass", 6)
        .expect("the holder evaluates under the peer's mark");
    let done = project.done(&project.agent, &work, 7);
    assert!(!done.owed, "{}", done.text());
    assert_eq!(done.value["acceptance"]["mode"], "same_session");
}

#[test]
fn a_mark_whose_author_later_holds_the_run_no_longer_admits_same_session() {
    let project = project("author-holds", &BOTH);
    let work = project.add(&project.peer, Some("same_session"), 1);
    project.take(&project.agent, &work, 2);
    project
        .evaluate(&project.agent, &work, "same_session", "pass", 4)
        .expect("the holder evaluates under the peer's mark");
    // The holder hands the run back and the mark's author takes it.
    project
        .agent
        .update(
            UpdateInput {
                work_ref: Some(work.clone()),
                action: UpdateAction::Release {
                    reason: Some("handing the run to the planner".into()),
                },
            },
            at(5),
        )
        .expect("release");
    project
        .peer
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: Some(3_600),
                recover: None,
            },
            at(6),
        )
        .expect("the author claims");
    // Completion no longer consumes the evaluation made under that mark.
    let refused = project.done(&project.peer, &work, 7);
    assert!(refused.owed, "{}", refused.text());
    assert_eq!(refused.value["code"], "acceptance_evaluation_stale");
    // The task stays marked, so the remedy names its mode, not another.
    assert!(
        refused.text().contains("stale (policy)")
            && refused
                .text()
                .contains("marked for same-session evaluation")
            && !refused.text().contains("independent acceptance evaluation"),
        "{}",
        refused.text()
    );
    // And the author's own evaluation under its mark refuses.
    assert!(
        refusal(project.evaluate(&project.peer, &work, "same_session", "pass", 8))
            .contains("same-session mark was set by a session"),
    );
}

#[test]
fn a_sub_agent_evaluation_counts_only_from_its_own_child_session() {
    let project = project(
        "sub-agent",
        &[
            AcceptanceEvaluationMode::SameSession,
            AcceptanceEvaluationMode::SubAgent,
            AcceptanceEvaluationMode::IndependentSession,
        ],
    );
    let child = AgentVerbs::new(
        project.database.clone(),
        project.id.clone(),
        "agent-child".into(),
        SessionId("agent-child".into()),
        None,
    );
    let work = project.add(&project.peer, None, 1);
    project.take(&project.agent, &work, 2);
    // Recorded from the holder's own session: the executor's own evaluation.
    let words = refusal(project.evaluate(&project.agent, &work, "sub_agent", "pass", 4));
    assert!(
        words.contains("must be recorded from a distinct child session")
            && words.contains("request an independent evaluation from the host"),
        "{words}"
    );
    // From a distinct child session under the holder: admitted and consumed.
    project
        .evaluate(&child, &work, "sub_agent", "pass", 5)
        .expect("the holder's sub-agent evaluates from its own session");
    let done = project.done(&project.agent, &work, 6);
    assert!(!done.owed, "{}", done.text());
    assert_eq!(done.value["acceptance"]["mode"], "sub_agent");

    // Once the child session takes the run itself, completion no longer
    // consumes the evaluation it recorded.
    let second = project.add(&project.peer, None, 10);
    project.take(&project.agent, &second, 11);
    project
        .evaluate(&child, &second, "sub_agent", "pass", 13)
        .expect("the sub-agent evaluates");
    project
        .agent
        .update(
            UpdateInput {
                work_ref: Some(second.clone()),
                action: UpdateAction::Release {
                    reason: Some("handing the run to the sub-agent".into()),
                },
            },
            at(14),
        )
        .expect("release");
    child
        .claim(
            ClaimInput {
                work_ref: second.clone(),
                ttl_seconds: Some(3_600),
                recover: None,
            },
            at(15),
        )
        .expect("the sub-agent's session claims");
    let refused = project.done(&child, &second, 16);
    assert!(refused.owed, "{}", refused.text());
    assert_eq!(refused.value["code"], "acceptance_evaluation_stale");
    assert!(
        refused.text().contains("stale (policy)"),
        "{}",
        refused.text()
    );
}

// A stored record without its evaluator's session, which admission never
// writes, reads stale identity at done. The reminder gives the remedy a
// missing evaluation of the task gets: a task marked for same-session goes
// back to its holder, not to an independent evaluator the project refuses.
#[test]
fn a_record_without_its_evaluator_is_stale_identity_with_the_tasks_remedy() {
    let project = project("identity-shape", &[AcceptanceEvaluationMode::SameSession]);
    let work = project.add(&project.peer, Some("same_session"), 1);
    project.take(&project.agent, &work, 2);
    let recorded = project
        .evaluate(&project.agent, &work, "same_session", "pass", 4)
        .expect("the holder evaluates under the peer's mark");
    let id = crate::canonical::ObjectId::from_stored(
        recorded.value["evaluation"]["hash"]
            .as_str()
            .expect("record id")
            .to_owned(),
    )
    .expect("stored record id");
    // Rewrite the stored record under its id, as an import or edit could.
    let store = SqliteStore::open(&project.database).expect("store");
    let mut stored: crate::domain::AcceptanceEvaluation =
        store.get(&id).expect("read").expect("stored record");
    drop(store);
    stored.evaluator.session_id = None;
    let object =
        crate::canonical::CanonicalObject::identified(&id, &stored).expect("rewritten record");
    let changed = rusqlite::Connection::open(&project.database)
        .expect("open the database")
        .execute(
            "UPDATE objects SET canonical_json = ?2 WHERE object_id = ?1",
            rusqlite::params![id.as_str(), object.bytes()],
        )
        .expect("rewrite the stored record");
    assert_eq!(changed, 1);

    let refused = project.done(&project.agent, &work, 6);
    assert!(refused.owed, "{}", refused.text());
    assert_eq!(refused.value["code"], "acceptance_evaluation_stale");
    let text = refused.text();
    assert!(
        text.contains("stale (identity)")
            && text.contains("or the record lacks a session its mode requires")
            && text.contains("record one in that mode with evaluate")
            && !text.contains("request an independent"),
        "{text}"
    );
    // Following it: the holder evaluates again, and done completes.
    project
        .evaluate(&project.agent, &work, "same_session", "pass", 7)
        .expect("a fresh same-session evaluation");
    let done = project.done(&project.agent, &work, 8);
    assert!(!done.owed, "{}", done.text());
}

#[test]
fn a_mark_a_detach_carries_over_has_no_author_on_the_successor() {
    let project = project("detached", &BOTH);
    // The parent completes on an independent evaluation.
    let parent = project.add(&project.peer, None, 1);
    // Its executor-to-be adds an optional child and marks it for itself.
    let child = project
        .agent
        .add(
            AddInput {
                title: "Detached follow-up".into(),
                acceptance: vec!["the change is verified".into()],
                evaluation_mode: Some("same_session".into()),
                under: Some(parent.clone()),
                optional: true,
                ..AddInput::default()
            },
            at(2),
        )
        .expect("add the child")
        .value["work"]["short_ref"]
        .as_str()
        .expect("ref")
        .to_owned();
    project.take(&project.peer, &parent, 3);
    project
        .evaluate(&project.judge, &parent, "independent_session", "pass", 5)
        .expect("the parent is evaluated");
    let done = project.done(&project.peer, &parent, 6);
    assert!(!done.owed, "{}", done.text());
    // A peer detaches the stranded child; the successor keeps the mark.
    let successor = project
        .peer
        .update(
            UpdateInput {
                work_ref: Some(child),
                action: UpdateAction::Detach {
                    reason: "continue as independent work".into(),
                },
            },
            at(7),
        )
        .expect("detach")
        .value["receipt"]["work_ref"]
        .as_str()
        .expect("successor ref")
        .to_owned();
    project.take(&project.agent, &successor, 8);
    // The session that detached it carried the mark over; it did not set
    // it, so the executor's own mark still waives nothing.
    let words = refusal(project.evaluate(&project.agent, &successor, "same_session", "pass", 10));
    assert!(
        words.contains("has no author recorded on this item")
            && words.contains("clear the mark and set it again"),
        "{words}"
    );
    // Once a peer clears the mark and sets it again, the peer is its author.
    project
        .agent
        .update(
            UpdateInput {
                work_ref: Some(successor.clone()),
                action: UpdateAction::Release {
                    reason: Some("let the planner set the mark".into()),
                },
            },
            at(11),
        )
        .expect("release");
    project.mark(&project.peer, &successor, None, 12);
    project.mark(&project.peer, &successor, Some("same_session"), 13);
    project.take(&project.agent, &successor, 14);
    project
        .evaluate(&project.agent, &successor, "same_session", "pass", 16)
        .expect("the holder evaluates under the peer's mark");
}

#[test]
fn a_policy_stale_reminder_names_no_mode_the_project_does_not_admit() {
    let project = project("stale-same-only", &[AcceptanceEvaluationMode::SameSession]);
    let work = project.add(&project.peer, Some("same_session"), 1);
    project.take(&project.agent, &work, 2);
    project
        .evaluate(&project.agent, &work, "same_session", "pass", 4)
        .expect("the holder evaluates under the peer's mark");
    project
        .agent
        .update(
            UpdateInput {
                work_ref: Some(work.clone()),
                action: UpdateAction::Release {
                    reason: Some("handing the run to the planner".into()),
                },
            },
            at(5),
        )
        .expect("release");
    project
        .peer
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: Some(3_600),
                recover: None,
            },
            at(6),
        )
        .expect("the author claims");
    let refused = project.done(&project.peer, &work, 7);
    assert_eq!(refused.value["code"], "acceptance_evaluation_stale");
    let text = refused.text();
    assert!(
        text.contains("stale (policy)")
            && text.contains("marked for same-session evaluation")
            && !text.contains("independent acceptance evaluation")
            && !text.contains("sub-agent acceptance evaluation"),
        "{text}"
    );
}

// B67: an independent failure stands through a title edit, a mode edit and a
// claim renewal, none of which is new evidence; a correction note lets the
// next evaluation replace it, and completion then seals on that pass.
#[test]
fn a_failure_stands_through_edits_and_renewals_until_a_correction_note() {
    let project = project("reroll", &BOTH);
    let work = project.add(&project.peer, None, 1);
    project.take(&project.agent, &work, 2);
    project
        .evaluate(&project.judge, &work, "independent_session", "fail", 4)
        .expect("an independent evaluation fails");
    let rerolled = |second: i64, case: &str| {
        let words =
            refusal(project.evaluate(&project.judge, &work, "independent_session", "pass", second));
        assert!(
            words.contains("nothing that could change it was recorded"),
            "{case}: {words}"
        );
    };
    rerolled(5, "on the same evidence");
    project
        .agent
        .update(
            UpdateInput {
                work_ref: Some(work.clone()),
                action: UpdateAction::Revise {
                    external: None,
                    clear_external: false,
                    title: Some("Retitled after the failure".into()),
                    outcome: None,
                    acceptance: None,
                    bindings: None,
                    assignee: None,
                    priority: None,
                    defer: None,
                    kind: None,
                    labels: Vec::new(),
                    unlabels: Vec::new(),
                },
            },
            at(6),
        )
        .expect("retitle");
    rerolled(7, "after a title edit");
    project.mark(&project.agent, &work, Some("independent_session"), 8);
    rerolled(9, "after a mode edit");
    project
        .agent
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: Some(3_600),
                recover: None,
            },
            at(10),
        )
        .expect("renew the claim");
    rerolled(11, "after a claim renewal");
    project
        .agent
        .note(
            &NoteInput {
                status: false,
                work_ref: Some(work.clone()),
                text: "correction: the failing case is fixed".into(),
                refs: Vec::new(),
            },
            at(12),
        )
        .expect("correction note");
    project
        .evaluate(&project.judge, &work, "independent_session", "pass", 13)
        .expect("an evaluation after the correction replaces the failure");
    let done = project.done(&project.agent, &work, 14);
    assert!(!done.owed, "{}", done.text());
    assert_eq!(done.value["acceptance"]["mode"], "independent_session");
}

// B72: a completion sealed before these rules applied stays as sealed. An
// unmarked task's own same_session evaluation, sealed in a same-session-only
// project, still validates and still reads as that evaluation after the
// project widens its policy, although a new one like it is now refused.
#[test]
fn a_completion_sealed_before_the_rules_tightened_is_unchanged() {
    let project = project("sealed-before", &[AcceptanceEvaluationMode::SameSession]);
    let work = project.add(&project.agent, None, 1);
    project.take(&project.agent, &work, 2);
    project
        .evaluate(&project.agent, &work, "same_session", "pass", 4)
        .expect("the holder evaluates where no other mode is admitted");
    let done = project.done(&project.agent, &work, 5);
    assert!(!done.owed, "{}", done.text());
    let sealed = done.value["acceptance"].clone();
    assert_eq!(sealed["mode"], "same_session");

    enable_as(&project.database, &BOTH, "widen-the-policy", 10);
    // The rules now refuse a new evaluation like the sealed one.
    let later = project.add(&project.agent, None, 11);
    project.take(&project.agent, &later, 12);
    assert!(
        refusal(project.evaluate(&project.agent, &later, "same_session", "pass", 14))
            .contains("not marked for same-session evaluation")
    );
    // The seal is unchanged: it validates and reads as it was sealed.
    let store = SqliteStore::open(&project.database).expect("store");
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
    let shown = project
        .agent
        .show(&work, at(15))
        .expect("show the sealed item");
    assert_eq!(shown.value["acceptance"], sealed, "{}", shown.text());
    assert!(
        shown
            .text()
            .contains("acceptance: evaluated (same_session, asserted) by "),
        "{}",
        shown.text()
    );
}
