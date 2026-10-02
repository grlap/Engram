//! `next` and `next --peek` carry one backup line when a configured target
//! needs attention, after the direction to list memories, and never shed;
//! with no target configured they say nothing about backups at all.

use super::*;
use crate::backup::{
    record::AttemptOutcome,
    reminder::tests::{recent_attempt, record_target},
};

const DIRECTION: &str = "the host reports a new context for this session: before acting, list project memories through the continuation and read the relevant current entries in full";

const NEVER_CONFIRMED: &str =
    "backup: mode local (store: backup_never_confirmed); see engram backup status";

/// A store in the Engram home layout, so `next` finds the home's backup
/// records, with a focused item whose title says nothing about backups.
fn home_fixture() -> (crate::test_support::TempHome, AgentVerbs, ProjectId) {
    let home = crate::test_support::temp_home().expect("temp");
    let project = ProjectId("release-notes".into());
    let database = crate::project_database_path(home.path(), &project);
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    let verbs = AgentVerbs::new(
        database,
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let item = add(&verbs, "Coordinate the release notes", None, false, 0);
    verbs.service.select_work(&item, at(1)).unwrap();
    (home, verbs, project)
}

/// Every renderer: compact and verbose, ordinary and peek, under the
/// protocol budget, a small one and an impossible one.
fn every_next(verbs: &AgentVerbs, peek_generation: Option<&str>) -> Vec<(String, Receipt)> {
    let mut receipts = Vec::new();
    for verbose in [false, true] {
        for peek in [false, true] {
            for budget in [MAX_AGENT_WORK_RESPONSE_BYTES, 4_096, 1] {
                let input = NextInput {
                    verbose,
                    peek,
                    context_generation: peek.then(|| peek_generation.map(str::to_owned)).flatten(),
                    ..NextInput::default()
                };
                let receipt = verbs
                    .next_with_verbose_budget(&input, at(10), budget)
                    .expect("next");
                receipts.push((
                    format!("verbose {verbose}, peek {peek}, budget {budget}"),
                    receipt,
                ));
            }
        }
    }
    receipts
}

#[test]
fn a_project_with_no_target_configured_gets_no_backup_line_at_all() {
    let (_home, verbs, _) = home_fixture();
    for (case, receipt) in every_next(&verbs, None) {
        let text = receipt.text().to_lowercase();
        assert!(!text.contains("backup"), "{case}: {text}");
        let value = receipt.value.to_string().to_lowercase();
        assert!(!value.contains("backup"), "{case}: {value}");
    }
}

#[test]
fn a_configured_target_in_local_mode_is_reminded_whole_in_every_renderer() {
    let (home, verbs, project) = home_fixture();
    record_target(home.path(), &project, false, None);
    for (case, receipt) in every_next(&verbs, None) {
        assert_eq!(
            receipt.reminders.first().map(String::as_str),
            Some(NEVER_CONFIRMED),
            "{case}"
        );
        assert_eq!(receipt.value["reminders"][0], NEVER_CONFIRMED, "{case}");
        assert_eq!(
            receipt
                .text()
                .lines()
                .filter(|line| line.contains("backup"))
                .count(),
            1,
            "{case}: {}",
            receipt.text()
        );
    }
}

#[test]
fn the_backup_line_follows_the_direction_to_list_memories() {
    let (home, verbs, project) = home_fixture();
    record_target(home.path(), &project, false, None);
    for (case, receipt) in every_next(&verbs, Some("termal-11")) {
        let peek = receipt
            .value
            .get("peek")
            .is_some_and(|peek| !peek.is_null());
        let expected: &[&str] = if peek {
            &[DIRECTION, NEVER_CONFIRMED]
        } else {
            &[NEVER_CONFIRMED]
        };
        assert_eq!(
            receipt.reminders[..expected.len()]
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            expected,
            "{case}"
        );
    }
}

#[test]
fn a_qualifying_copy_gives_no_line_and_a_failed_last_push_shows_beside_it() {
    let (home, verbs, project) = home_fixture();
    record_target(
        home.path(),
        &project,
        true,
        Some(recent_attempt(AttemptOutcome::Uploaded, None)),
    );
    for (case, receipt) in every_next(&verbs, None) {
        assert!(!receipt.text().contains("backup"), "{case}");
    }

    let (home, verbs, project) = home_fixture();
    record_target(
        home.path(),
        &project,
        true,
        Some(recent_attempt(
            AttemptOutcome::Failed,
            Some("backup_target_unreachable"),
        )),
    );
    let failed = "backup: the last store push failed: backup_target_unreachable; an earlier copy still qualifies (store copy: off-host asserted; not verified); see engram backup status";
    for (case, receipt) in every_next(&verbs, None) {
        assert_eq!(
            receipt.reminders.first().map(String::as_str),
            Some(failed),
            "{case}"
        );
    }
}
