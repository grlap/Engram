//! Every reader that shows an unadmitted observation says so: a peer's
//! `next` delta, the item's source-observation window, and the holder's
//! own `show` and `done`, which the record never blocks or credits.

use super::*;
use crate::verbs::{AgentVerbs, DoneInput, NextInput, ShowInput};

fn recorded_store() -> (crate::test_support::TempHome, std::path::PathBuf, Fixture) {
    let directory = crate::test_support::temp_home().expect("directory");
    let path = directory.path().join("engram.db");
    let mut fixture = fixture_on(SqliteStore::open(&path).expect("store"));
    let change = inter_turn_change(&fixture, "reader-change");
    observe(&mut fixture, change, 7).expect("inter-turn change");
    let mut turn = inter_turn_change(&fixture, "reader-turn");
    turn.occurrence = ObservedOccurrence::UnadmittedTurn {
        host_turn_ref: "turn-9".into(),
        source_change: None,
        observed_checks: vec![passed_check("cargo-test")],
    };
    turn.causality = ObservationCausality::HostAssertion {
        claimed_actor: Box::new(actor("runner")),
        basis: "terminal ownership".into(),
    };
    observe(&mut fixture, turn, 8).expect("unadmitted turn");
    (directory, path, fixture)
}

fn verbs(path: &std::path::Path, session: &str) -> AgentVerbs {
    AgentVerbs::new(
        path.to_path_buf(),
        ProjectId("project-a".into()),
        session.into(),
        crate::SessionId(session.into()),
        None,
    )
}

#[test]
fn a_peers_next_names_each_record_unadmitted_with_its_cause_and_uncredited_checks() {
    let (_directory, path, _fixture) = recorded_store();
    let next = verbs(&path, "peer")
        .next(&NextInput::default(), at(20))
        .expect("next");
    let text = next.text();
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("unadmitted_observation"))
        .collect();
    assert_eq!(lines.len(), 2, "{text}");
    // The bounded line keeps the cause, the credit and the accounting.
    assert!(
        lines
            .iter()
            .any(|line| line.contains("cause unknown; audit only")),
        "{text}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("unverified cause; 1 check uncredited; audit only")),
        "{text}"
    );
    assert!(
        lines.iter().all(|line| line.contains("unadmitted")),
        "{text}"
    );
}

#[test]
fn the_observation_window_shows_admission_cause_and_every_check_uncredited() {
    let (_directory, path, fixture) = recorded_store();
    let shown = verbs(&path, "peer")
        .show_records(
            &fixture.work.short_ref,
            &ShowInput {
                observations: true,
                ..ShowInput::default()
            },
            at(20),
        )
        .expect("observations window");
    let text = shown.text();
    assert_eq!(text.matches("unadmitted observation").count(), 2, "{text}");
    assert!(text.contains("cause unknown"), "{text}");
    assert!(
        text.contains("host-asserted cause, not verified: runner"),
        "{text}"
    );
    assert!(
        text.contains("observed check, uncredited: cargo-test test reported passed"),
        "{text}"
    );
    assert!(text.contains("audit only"), "{text}");
    let rows = shown.value["observations"]
        .as_array()
        .or_else(|| shown.value["rows"].as_array())
        .expect("rows");
    assert!(
        rows.iter().all(|row| row["admission"] == "unadmitted"),
        "{rows:?}"
    );
    assert!(
        rows.iter()
            .flat_map(|row| row["unadmitted"]["checks"]
                .as_array()
                .cloned()
                .unwrap_or_default())
            .all(|check| check["credit"] == "uncredited")
    );
}

#[test]
fn the_holder_shows_and_completes_with_the_records_on_its_run() {
    let (_directory, path, fixture) = recorded_store();
    let holder = verbs(&path, "runner");
    holder.show(&fixture.work.short_ref, at(20)).expect("show");
    let done = holder
        .done(
            DoneInput {
                work_ref: Some(fixture.work.short_ref.clone()),
                summary: Some("delivered with host records on the run".into()),
                ..DoneInput::default()
            },
            at(21),
        )
        .expect("an audit-only record neither blocks nor credits completion");
    assert!(
        done.text().contains("completed") || done.value["work"]["lifecycle"] == "completed",
        "{}",
        done.text()
    );
    holder
        .show(&fixture.work.short_ref, at(22))
        .expect("show after completion");
    holder
        .show_records(
            &fixture.work.short_ref,
            &ShowInput {
                history: true,
                ..ShowInput::default()
            },
            at(22),
        )
        .expect("history after completion");
    assert!(fixture.store.verify_all().expect("doctor").is_healthy());
}

// The largest record the operation accepts still fits the window: 16 checks
// with the longest ids, revisions and references, in the longest workspace.
#[test]
fn the_largest_accepted_record_fits_the_observation_window() {
    use crate::domain::{
        MAX_OBSERVATION_LABEL_BYTES, MAX_OBSERVED_CHECKS, MAX_OBSERVED_EVIDENCE_REF_BYTES,
        MAX_OBSERVED_SOURCE_BYTES,
    };
    let directory = crate::test_support::temp_home().expect("directory");
    let path = directory.path().join("engram.db");
    let mut fixture = fixture_on(SqliteStore::open(&path).expect("store"));
    let workspace = "w".repeat(MAX_OBSERVED_SOURCE_BYTES);
    let checks = (0..MAX_OBSERVED_CHECKS)
        .map(|index| {
            let mut check = passed_check(&format!(
                "{index:02}{}",
                "c".repeat(MAX_OBSERVATION_LABEL_BYTES - 2)
            ));
            check.source_basis = Some(ExecutionSourceBasis {
                workspace_id: workspace.clone(),
                source_revision: "r".repeat(MAX_OBSERVED_SOURCE_BYTES),
                source_root_generation: None,
                source_root_state: None,
            });
            check.host_evidence_ref = Some("e".repeat(MAX_OBSERVED_EVIDENCE_REF_BYTES));
            check
        })
        .collect();
    let mut input = inter_turn_change(&fixture, "largest");
    input.occurrence = ObservedOccurrence::UnadmittedTurn {
        host_turn_ref: "t".repeat(MAX_OBSERVATION_LABEL_BYTES),
        source_change: Some(ObservedSourceChange::AssumedMissingBaseline {
            workspace_id: workspace.clone(),
            sighting: MeasuredSighting {
                source_basis: ExecutionSourceBasis {
                    workspace_id: workspace,
                    source_revision: "s".repeat(MAX_OBSERVED_SOURCE_BYTES),
                    source_root_generation: None,
                    source_root_state: None,
                },
                observed_at: at(5),
            },
        }),
        observed_checks: checks,
    };
    observe(&mut fixture, input, 7).expect("the largest record is accepted");
    let shown = verbs(&path, "peer")
        .show_records(
            &fixture.work.short_ref,
            &ShowInput {
                observations: true,
                ..ShowInput::default()
            },
            at(20),
        )
        .expect("the window fits the largest record");
    assert_eq!(
        shown.text().matches("observed check, uncredited").count(),
        MAX_OBSERVED_CHECKS
    );
}

// An observer's own records are already in its receipts: its `next` does not
// list them as changes by others.
#[test]
fn the_observer_does_not_see_its_own_records_as_changes_by_others() {
    let (_directory, path, _fixture) = recorded_store();
    let own = verbs(&path, "observer")
        .next(&NextInput::default(), at(20))
        .expect("next");
    assert!(
        !own.text().contains("unadmitted_observation"),
        "{}",
        own.text()
    );
}

// Escaping cannot overflow the window either: every displayed field is
// bounded by its escaped size, so a record of quotes, backslashes and control
// characters at every maximum fits a window, and the window pages to the next
// such record instead of failing.
#[test]
fn escape_heavy_records_fit_the_observation_window() {
    use crate::domain::{
        MAX_CLAIMED_ACTOR_TEXT_BYTES, MAX_OBSERVATION_LABEL_BYTES, MAX_OBSERVED_CHECKS,
        MAX_OBSERVED_EVIDENCE_REF_BYTES, MAX_OBSERVED_SOURCE_BYTES,
    };
    let directory = crate::test_support::temp_home().expect("directory");
    let path = directory.path().join("engram.db");
    let mut fixture = fixture_on(SqliteStore::open(&path).expect("store"));
    let workspace = "\\".repeat(MAX_OBSERVED_SOURCE_BYTES);
    for key in ["escapes-1", "escapes-2"] {
        let checks = (0..MAX_OBSERVED_CHECKS)
            .map(|index| {
                let mut check = passed_check(&format!(
                    "{index:02}{}",
                    "\u{1}".repeat(MAX_OBSERVATION_LABEL_BYTES - 2)
                ));
                check.source_basis = Some(ExecutionSourceBasis {
                    workspace_id: workspace.clone(),
                    source_revision: "\"".repeat(MAX_OBSERVED_SOURCE_BYTES),
                    source_root_generation: None,
                    source_root_state: None,
                });
                check.host_evidence_ref = Some(format!(
                    "r{}",
                    "\u{1}".repeat(MAX_OBSERVED_EVIDENCE_REF_BYTES - 1)
                ));
                check
            })
            .collect();
        let mut claimed = actor("runner");
        claimed.actor_id = format!(
            "a{}",
            "\u{202e}".repeat(MAX_CLAIMED_ACTOR_TEXT_BYTES / 3 - 1)
        );
        let mut input = inter_turn_change(&fixture, key);
        input.occurrence = ObservedOccurrence::UnadmittedTurn {
            host_turn_ref: "turn".into(),
            source_change: Some(ObservedSourceChange::AssumedMissingBaseline {
                workspace_id: workspace.clone(),
                sighting: MeasuredSighting {
                    source_basis: ExecutionSourceBasis {
                        workspace_id: workspace.clone(),
                        // Host source text refuses control characters, so
                        // its widest admitted escape is a quote.
                        source_revision: "\"".repeat(MAX_OBSERVED_SOURCE_BYTES),
                        source_root_generation: None,
                        source_root_state: None,
                    },
                    observed_at: at(5),
                },
            }),
            observed_checks: checks,
        };
        input.causality = ObservationCausality::HostAssertion {
            claimed_actor: Box::new(claimed),
            basis: "terminal ownership".into(),
        };
        observe(&mut fixture, input, 7).expect("an escape-heavy record is accepted");
    }
    let shown = verbs(&path, "peer")
        .show_records(
            &fixture.work.short_ref,
            &ShowInput {
                observations: true,
                ..ShowInput::default()
            },
            at(20),
        )
        .expect("the window fits escape-heavy records");
    assert!(
        shown.text().matches("observed check, uncredited").count() == MAX_OBSERVED_CHECKS,
        "{}",
        shown.text()
    );
    // The second record is reached through the window's continuation and is
    // shown whole there too.
    let after = shown.value["observations_window"]["after"]
        .as_str()
        .expect("a continuation to the older record")
        .to_owned();
    let older = verbs(&path, "peer")
        .show_records(
            &fixture.work.short_ref,
            &ShowInput {
                observations: true,
                after: Some(after),
                ..ShowInput::default()
            },
            at(21),
        )
        .expect("the continuation fits the older record");
    assert_eq!(
        older.text().matches("observed check, uncredited").count(),
        MAX_OBSERVED_CHECKS
    );
}
