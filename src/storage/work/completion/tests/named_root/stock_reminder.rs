//! The stock source-change reminder an agent reads says what `done` then
//! does: each case below reads the reminder through the agent words, checks
//! the classification it came from, and then lets completion decide.

use super::*;
use crate::storage::WorkObligationCompletionAction as Action;

const DONE_WAIVES: &str = "tests have not run since your last source change — run them; the host records the result, and done records the change as untested without one";
const DONE_DISPLACES: &str = "a source change made in another workspace before the root was named is open — no action is needed; done records it as displaced";
const CHECK_OR_WAIVER: &str = "tests have not run since a source change done cannot record as untested — run the credited check or obtain an authorized waiver; done refuses until one of them resolves it";
const NAME_ROOT: &str = "tests have not run since a source change whose workspace is unknown — name a source root and run its credited check, or obtain an authorized waiver; done refuses until one of them resolves it";
const WAIVER_ONLY: &str = "a source change made outside the named root while it was bound is open — only an authorized human waiver resolves it, since no check in a named root can; done refuses until then";
const STOCK_WORDS: [&str; 5] = [
    DONE_WAIVES,
    DONE_DISPLACES,
    CHECK_OR_WAIVER,
    NAME_ROOT,
    WAIVER_ONLY,
];

/// A file store with one claimed item, and the agent words for its holder.
struct Case {
    store: SqliteStore,
    work: WorkItem,
    claim: WorkClaim,
    verbs: crate::verbs::AgentVerbs,
    // Dropped last, after every handle on its files.
    _directory: crate::test_support::TempHome,
}

fn case(name: &str, evaluated: bool) -> Case {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let database = directory.path().join(format!("{name}.sqlite3"));
    let mut store = SqliteStore::open(&database).expect("store");
    if evaluated {
        store
            .set_acceptance_evaluation_policy(
                &crate::domain::AcceptanceEvaluationPolicy {
                    allowed_modes: vec![crate::domain::AcceptanceEvaluationMode::SameSession],
                    mechanical_basis: crate::domain::MechanicalBasis::Asserted,
                    require_source_freshness: false,
                },
                &actor("policy-admin"),
                "enable-evaluation",
                None,
                at(1),
                &DevelopmentNoopRedactor,
            )
            .expect("evaluated policy");
    }
    let mut request = root_request("project-a", name, 1);
    request.acceptance = vec!["describe the change".into()];
    let work = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("work");
    let claim = claim(
        &mut store,
        &work,
        "runner",
        &format!("{name}-claim"),
        2,
        3_600,
    );
    let verbs = crate::verbs::AgentVerbs::new(
        database,
        work.project_id.clone(),
        "runner".into(),
        SessionId("runner".into()),
        None,
    );
    Case {
        store,
        work,
        claim,
        verbs,
        _directory: directory,
    }
}

impl Case {
    /// The stock reminders `show` gives the holder at `second`.
    fn stock_reminders(&self, second: i64) -> Vec<String> {
        self.verbs
            .show(&self.work.short_ref, at(second))
            .expect("show")
            .reminders
            .into_iter()
            .filter(|reminder| STOCK_WORDS.contains(&reminder.as_str()))
            .collect()
    }

    fn done(&self, second: i64) -> crate::verbs::Receipt {
        self.verbs
            .done(
                crate::verbs::DoneInput {
                    work_ref: Some(self.work.short_ref.clone()),
                    summary: Some("described the change".into()),
                    ..crate::verbs::DoneInput::default()
                },
                at(second),
            )
            .expect("done answers")
    }

    /// The reason `done` gives when it refuses on a source change it cannot
    /// resolve by itself; the agent reads it as the receipt's line.
    fn refusal(&self, second: i64) -> String {
        let refused = self
            .verbs
            .done(
                crate::verbs::DoneInput {
                    work_ref: Some(self.work.short_ref.clone()),
                    summary: Some("described the change".into()),
                    ..crate::verbs::DoneInput::default()
                },
                at(second),
            )
            .expect_err("done refuses");
        match refused.error {
            StoreError::WorkCompletionRefused { reason, .. } => reason,
            other => panic!("unexpected refusal: {other:?}"),
        }
    }

    fn lifecycle(&self) -> crate::WorkLifecycle {
        self.store
            .get_work_item(self.work.work_id)
            .expect("item")
            .lifecycle
    }
}

/// An in-root change of the stock rule: the reminder promises an untested
/// record and never a refusal, and `done` records the change as untested.
#[test]
fn an_in_root_change_is_recorded_as_untested_as_the_reminder_says() {
    let mut case = case("in-root", false);
    name_root(&mut case.store, &case.work, &case.claim, 9, 3);
    let change = source_mutation_from_basis(
        &mut case.store,
        &case.work,
        &case.claim,
        "runner",
        "change-in-B",
        4,
        Some(basis("workspace-B", "B4", Some(9))),
        None,
    );
    assert_eq!(
        stock_completion_action(&case.store, &case.claim, &change),
        Action::DoneWaives
    );
    assert_eq!(case.stock_reminders(5), [DONE_WAIVES]);
    let receipt = case.done(6);
    assert!(!receipt.owed, "{:?}", receipt.reminders);
    assert_eq!(case.lifecycle(), crate::WorkLifecycle::Completed);
    assert_eq!(
        obligation(&case.store, &case.claim, &Pick::TriggeredBy(&change)).0,
        WorkObligationState::Waived
    );
}

/// The same change under an evaluated policy: the holder's disclosure and a
/// refused `done` receipt give the same disposition.
#[test]
fn an_evaluated_policy_gives_the_same_in_root_disposition() {
    let mut case = case("in-root-evaluated", true);
    name_root(&mut case.store, &case.work, &case.claim, 9, 3);
    let change = source_mutation_from_basis(
        &mut case.store,
        &case.work,
        &case.claim,
        "runner",
        "change-in-B",
        4,
        Some(basis("workspace-B", "B4", Some(9))),
        None,
    );
    assert_eq!(
        stock_completion_action(&case.store, &case.claim, &change),
        Action::DoneWaives
    );
    let shown = case
        .verbs
        .show(&case.work.short_ref, at(5))
        .expect("show")
        .value["evaluation_obligations"]
        .clone();
    assert_eq!(shown["action_required_total"], 0);
    assert!(
        shown["items"][0]["remedy"]
            .as_str()
            .expect("remedy")
            .contains("done records the source change as untested")
    );
    // No evaluation is recorded, so done refuses for that; its reminder still
    // says what it will do with the change.
    let refused = case.done(6);
    assert!(refused.owed);
    assert!(
        refused.reminders.contains(&DONE_WAIVES.to_owned()),
        "{:?}",
        refused.reminders
    );
    assert!(!refused.reminders.iter().any(|reminder| {
        STOCK_WORDS
            .iter()
            .any(|words| *words != DONE_WAIVES && reminder == words)
    }));
}

/// A change in another workspace while the root is bound: the reminder asks
/// for an authorized human waiver, and `done` refuses on that obligation.
#[test]
fn a_foreign_change_under_a_bound_root_needs_the_waiver_the_reminder_names() {
    let mut case = case("bound-foreign", false);
    name_root(&mut case.store, &case.work, &case.claim, 9, 3);
    let change = source_mutation_from_basis(
        &mut case.store,
        &case.work,
        &case.claim,
        "runner",
        "change-in-A",
        4,
        Some(basis("workspace-A", "A4", Some(9))),
        None,
    );
    assert_eq!(
        stock_completion_action(&case.store, &case.claim, &change),
        Action::WaiverOnly
    );
    assert_eq!(case.stock_reminders(5), [WAIVER_ONLY]);
    // A check in the root does not resolve it, as the reminder says.
    host_verification_from_basis(
        &mut case.store,
        &case.work,
        &case.claim,
        "runner",
        "test-in-B",
        VerificationKind::Test,
        VerificationResult::Passed,
        6,
        basis("workspace-B", "B6", Some(9)),
    );
    assert_eq!(case.stock_reminders(7), [WAIVER_ONLY]);
    let reason = case.refusal(8);
    assert!(reason.contains("human waiver"), "{reason}");
    assert!(!reason.contains("untested"), "{reason}");
    assert_eq!(case.lifecycle(), crate::WorkLifecycle::Open);

    // Once the root ends, no root is bound, and the change still needs the
    // waiver: the same words, and the same refusal.
    end_root(
        &mut case.store,
        &case.work,
        &case.claim,
        "workspace-B",
        9,
        3,
        9,
    );
    assert_eq!(
        stock_completion_action(&case.store, &case.claim, &change),
        Action::WaiverOnly
    );
    assert_eq!(case.stock_reminders(10), [WAIVER_ONLY]);
    let reason = case.refusal(11);
    assert!(reason.contains("human waiver"), "{reason}");
    assert!(!reason.contains("untested"), "{reason}");
    assert_eq!(case.lifecycle(), crate::WorkLifecycle::Open);
}

/// A change with no workspace while a root is bound needs the credited
/// check; once the root ends, it first needs a named root. A later credited
/// check in a newly named root clears the reminder, and `done` completes.
#[test]
fn an_unlocated_change_asks_for_the_credited_check_until_one_clears_it() {
    let mut case = case("unlocated", false);
    name_root(&mut case.store, &case.work, &case.claim, 9, 3);
    let change = source_mutation_from_basis(
        &mut case.store,
        &case.work,
        &case.claim,
        "runner",
        "change-without-workspace",
        4,
        None,
        None,
    );
    assert_eq!(
        stock_completion_action(&case.store, &case.claim, &change),
        Action::CheckOrWaiver
    );
    assert_eq!(case.stock_reminders(5), [CHECK_OR_WAIVER]);
    let reason = case.refusal(6);
    assert!(
        reason.contains("check") && reason.contains("waiver"),
        "{reason}"
    );
    assert!(!reason.contains("untested"), "{reason}");

    end_root(
        &mut case.store,
        &case.work,
        &case.claim,
        "workspace-B",
        9,
        3,
        7,
    );
    assert_eq!(
        stock_completion_action(&case.store, &case.claim, &change),
        Action::NameRootCheckOrWaiver
    );
    assert_eq!(case.stock_reminders(8), [NAME_ROOT]);
    let reason = case.refusal(9);
    assert!(reason.contains("waiver"), "{reason}");
    assert!(!reason.contains("untested"), "{reason}");
    assert_eq!(case.lifecycle(), crate::WorkLifecycle::Open);

    name_root(&mut case.store, &case.work, &case.claim, 10, 10);
    host_verification_from_basis(
        &mut case.store,
        &case.work,
        &case.claim,
        "runner",
        "test-in-B-10",
        VerificationKind::Test,
        VerificationResult::Passed,
        11,
        basis("workspace-B", "B11", Some(10)),
    );
    assert_eq!(
        obligation(&case.store, &case.claim, &Pick::TriggeredBy(&change)).0,
        WorkObligationState::Satisfied
    );
    let observed = case.stock_reminders(12);
    assert!(observed.is_empty(), "{observed:?}");
    let receipt = case.done(13);
    assert!(!receipt.owed, "{:?}", receipt.reminders);
    assert_eq!(case.lifecycle(), crate::WorkLifecycle::Completed);
}

/// A change in another workspace before the root was named: the reminder
/// says no action is needed, and `done` records the change as displaced.
#[test]
fn a_change_before_the_root_was_named_is_displaced_as_the_reminder_says() {
    let mut case = case("displaced", false);
    let change = source_mutation_from_basis(
        &mut case.store,
        &case.work,
        &case.claim,
        "runner",
        "change-in-A",
        3,
        Some(basis("workspace-A", "A3", None)),
        None,
    );
    name_root(&mut case.store, &case.work, &case.claim, 9, 4);
    assert_eq!(
        stock_completion_action(&case.store, &case.claim, &change),
        Action::DoneDisplaces
    );
    assert_eq!(case.stock_reminders(5), [DONE_DISPLACES]);
    let receipt = case.done(6);
    assert!(!receipt.owed, "{:?}", receipt.reminders);
    assert_eq!(case.lifecycle(), crate::WorkLifecycle::Completed);
    assert_eq!(
        obligation(&case.store, &case.claim, &Pick::TriggeredBy(&change)).0,
        WorkObligationState::Displaced
    );
}

/// The evaluated-policy disclosure of a foreign change under a bound root
/// names the waiver it needs, and keeps naming it, in words that hold after
/// the root ends as well: no check in any named root satisfies it.
#[test]
fn an_evaluated_policy_discloses_the_waiver_a_foreign_change_needs_before_and_after_its_root_ends()
{
    let mut case = case("bound-foreign-evaluated", true);
    name_root(&mut case.store, &case.work, &case.claim, 9, 3);
    let change = source_mutation_from_basis(
        &mut case.store,
        &case.work,
        &case.claim,
        "runner",
        "change-in-A",
        4,
        Some(basis("workspace-A", "A4", Some(9))),
        None,
    );
    assert_eq!(
        stock_completion_action(&case.store, &case.claim, &change),
        Action::WaiverOnly
    );
    let disclosure = |second| {
        let shown = case
            .verbs
            .show(&case.work.short_ref, at(second))
            .expect("show")
            .value["evaluation_obligations"]
            .clone();
        assert_eq!(shown["action_required_total"], 1, "{shown}");
        shown["items"][0]["remedy"]
            .as_str()
            .expect("remedy")
            .to_owned()
    };
    let expected = "obtain an authorized human waiver before evaluation; no check in a named root can satisfy this foreign change";
    assert_eq!(disclosure(5), expected);
    end_root(
        &mut case.store,
        &case.work,
        &case.claim,
        "workspace-B",
        9,
        3,
        6,
    );
    assert_eq!(
        stock_completion_action(&case.store, &case.claim, &change),
        Action::WaiverOnly
    );
    let after = disclosure(7);
    assert_eq!(after, expected);
    assert!(!after.contains("this root"), "{after}");
    // Completion still refuses on it, as before the root ended.
    let refused = case.done(8);
    assert!(refused.owed);
    assert_eq!(case.lifecycle(), crate::WorkLifecycle::Open);
}
