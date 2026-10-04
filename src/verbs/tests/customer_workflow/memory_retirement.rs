use super::*;

fn remember(
    verbs: &AgentVerbs,
    key: &str,
    text: &str,
    target: Option<String>,
    revise: bool,
    at_ms: i64,
) {
    verbs
        .remember(
            RememberInput {
                text: text.into(),
                key: Some(key.into()),
                revise,
                expected_revision: None,
                retires_with: target,
                clear_retires_with: false,
                append: false,
                section: None,
            },
            at(at_ms),
        )
        .unwrap();
}

#[test]
fn completion_surfaces_live_memory_candidate_and_leaves_forget_explicit() {
    let (_directory, verbs, _, _) = fixture();
    let work = add(&verbs, "Retiring fix", None, false, 0);
    remember(
        &verbs,
        "linked-rule",
        "Initial workaround",
        Some(format!("local:{work}")),
        false,
        1,
    );
    remember(&verbs, "linked-rule", "Corrected workaround", None, true, 2);
    remember(
        &verbs,
        "unrelated-rule",
        "Unrelated guidance",
        None,
        false,
        3,
    );
    remember(
        &verbs,
        "external-rule",
        "External workaround",
        Some("external:other-project#issue-7".into()),
        false,
        4,
    );
    let before = verbs
        .memories(
            &MemoriesInput {
                query: Some("linked-rule".into()),
                full: true,
                ..MemoriesInput::default()
            },
            at(5),
        )
        .unwrap();
    assert_eq!(before.value["retiring_target"]["kind"], "local");
    assert_eq!(before.value["retiring_target"]["work_ref"], work);
    assert_eq!(before.value["retiring_state"]["lifecycle"], "open");
    assert_eq!(before.value["workaround"], true);
    let historical = verbs
        .memories(
            &MemoriesInput {
                query: Some("linked-rule".into()),
                full: true,
                revision: Some(1),
                ..MemoriesInput::default()
            },
            at(5),
        )
        .unwrap();
    assert_eq!(
        historical.value["retiring_target"],
        before.value["retiring_target"]
    );
    verbs
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(6),
        )
        .unwrap();
    verbs
        .note(
            &NoteInput {
                status: false,
                work_ref: Some(work.clone()),
                text: "fix recorded".into(),
                refs: Vec::new(),
            },
            at(7),
        )
        .unwrap();
    let input = DoneInput {
        work_ref: Some(work),
        summary: Some("fix delivered".into()),
        ..DoneInput::default()
    };
    let completed = verbs.done(input.clone(), at(8)).unwrap();
    assert!(!completed.owed, "{}", completed.text());
    assert_eq!(completed.value["memory_retirement"]["total"], 1);
    assert_eq!(completed.value["memory_retirement"]["omitted"], 0);
    assert_eq!(
        completed.value["memory_retirement"]["items"][0]["key"],
        "linked-rule"
    );
    assert_eq!(
        completed.value["memory_retirement"]["items"][0]["forget_command"],
        "engram work forget linked-rule"
    );
    let replay = verbs.done(input, at(8)).unwrap();
    assert_eq!(
        replay.value["memory_retirement"]["items"][0]["key"],
        "linked-rule"
    );
    let current = verbs
        .memories(
            &MemoriesInput {
                query: Some("linked-rule".into()),
                full: true,
                ..MemoriesInput::default()
            },
            at(9),
        )
        .unwrap();
    assert_eq!(current.value["retiring_state"]["lifecycle"], "completed");
    assert!(current.text().contains("forget candidate"));
    let listed = verbs.memories(&MemoriesInput::default(), at(9)).unwrap();
    let rows = listed.value["memories"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert!(listed.text().contains("workaround retires with local"));
    let external = rows
        .iter()
        .find(|row| row["key"] == "external-rule")
        .unwrap();
    assert_eq!(external["retiring_target"]["kind"], "external");
    assert!(external.get("retiring_state").is_none());
    verbs
        .forget(
            ForgetInput {
                key: "linked-rule".into(),
            },
            at(10),
        )
        .unwrap();
    let after = verbs.memories(&MemoriesInput::default(), at(11)).unwrap();
    assert_eq!(after.value["memories"].as_array().unwrap().len(), 2);
}

#[test]
fn cancel_and_supersede_report_current_memory_heads_without_retargeting() {
    let (_directory, verbs, _, _) = fixture();
    let cancelled = add(&verbs, "Cancelled fix", None, false, 0);
    let superseded = add(&verbs, "Replaced fix", None, false, 1);
    let replacement = add(&verbs, "Replacement", None, false, 2);
    remember(
        &verbs,
        "cancel-note",
        "Wait for cancelled fix",
        Some(format!("local:{cancelled}")),
        false,
        3,
    );
    remember(
        &verbs,
        "replace-note",
        "Wait for replaced fix",
        Some(format!("local:{superseded}")),
        false,
        4,
    );
    let cancelled_receipt = verbs
        .update(
            UpdateInput {
                work_ref: Some(cancelled),
                action: UpdateAction::Cancel {
                    reason: "No longer planned".into(),
                },
            },
            at(5),
        )
        .unwrap();
    assert_eq!(cancelled_receipt.value["memory_retirement"]["total"], 1);
    assert_eq!(
        cancelled_receipt.value["memory_retirement"]["items"][0]["key"],
        "cancel-note"
    );
    assert!(cancelled_receipt.text().contains("memory stays in force"));
    let superseded_receipt = verbs
        .update(
            UpdateInput {
                work_ref: Some(superseded.clone()),
                action: UpdateAction::Supersede {
                    replacement: replacement.clone(),
                    reason: "Scope moved".into(),
                },
            },
            at(6),
        )
        .unwrap();
    assert_eq!(superseded_receipt.value["memory_retirement"]["total"], 1);
    assert_eq!(
        superseded_receipt.value["memory_retirement"]["items"][0]["key"],
        "replace-note"
    );
    assert!(superseded_receipt.text().contains(&replacement));
    let full = verbs
        .memories(
            &MemoriesInput {
                query: Some("replace-note".into()),
                full: true,
                ..MemoriesInput::default()
            },
            at(7),
        )
        .unwrap();
    assert_eq!(full.value["retiring_state"]["lifecycle"], "superseded");
    assert_eq!(full.value["retiring_target"]["work_ref"], superseded);
    assert_eq!(
        superseded_receipt.value["memory_retirement"]["replacement"],
        replacement
    );
    assert!(
        superseded_receipt
            .text()
            .contains(&format!("--retires-with local:{replacement}"))
    );
}

#[test]
fn a_rejected_child_and_a_detached_child_surface_the_memories_that_name_them() {
    let (_directory, verbs, _, _) = fixture();
    let parent = add(&verbs, "Open parent", None, false, 0);
    let finding = add(&verbs, "Refuted finding", Some(&parent), false, 1);
    remember(
        &verbs,
        "finding-note",
        "Holds until the finding is settled",
        Some(format!("local:{finding}")),
        false,
        2,
    );
    let rejected = verbs
        .update(
            UpdateInput {
                work_ref: Some(finding),
                action: UpdateAction::Reject {
                    reason: "Evidence disproves it".into(),
                },
            },
            at(3),
        )
        .expect("reject");
    assert_eq!(rejected.value["memory_retirement"]["action"], "cancelled");
    assert_eq!(
        rejected.value["memory_retirement"]["items"][0]["key"],
        "finding-note"
    );

    let (_, child) = super::detach::stranded_child(&verbs);
    remember(
        &verbs,
        "follow-up-note",
        "Holds until the follow-up lands",
        Some(format!("local:{child}")),
        false,
        5,
    );
    let detached = verbs
        .update(
            UpdateInput {
                work_ref: Some(child),
                action: UpdateAction::Detach {
                    reason: "Continue as independent work".into(),
                },
            },
            at(6),
        )
        .expect("detach");
    let root = detached.value["receipt"]["work_ref"]
        .as_str()
        .expect("new root")
        .to_owned();
    assert_eq!(detached.value["memory_retirement"]["action"], "superseded");
    assert_eq!(detached.value["memory_retirement"]["replacement"], root);
    assert_eq!(
        detached.value["memory_retirement"]["items"][0]["key"],
        "follow-up-note"
    );
}

#[test]
fn a_clear_needs_a_revise_on_every_route() {
    let (_directory, verbs, _, _) = fixture();
    let work = add(&verbs, "Retiring fix", None, false, 0);
    remember(
        &verbs,
        "clear-me",
        "Workaround",
        Some(format!("local:{work}")),
        false,
        1,
    );
    let refused = verbs
        .remember(
            RememberInput {
                text: "Workaround".into(),
                key: Some("clear-me".into()),
                revise: false,
                expected_revision: None,
                retires_with: None,
                clear_retires_with: true,
                append: false,
                section: None,
            },
            at(2),
        )
        .expect_err("a clear without --revise");
    assert!(
        refused.to_string().contains("requires --revise"),
        "{refused}"
    );
    let cleared = verbs
        .remember(
            RememberInput {
                text: "Workaround, no longer tied to the fix".into(),
                key: Some("clear-me".into()),
                revise: true,
                expected_revision: None,
                retires_with: None,
                clear_retires_with: true,
                append: false,
                section: None,
            },
            at(3),
        )
        .expect("clear");
    assert_eq!(cleared.value["revision"], 2);
    let full = verbs
        .memories(
            &MemoriesInput {
                query: Some("clear-me".into()),
                full: true,
                ..MemoriesInput::default()
            },
            at(4),
        )
        .unwrap();
    assert!(full.value.get("retiring_target").is_none());
    assert!(full.value.get("retiring_target_dropped").is_none());
}

/// The item state of a local target is read when the memory is, so a body
/// admitted at the edge of the full-read limit must still read in full once
/// the target completes and the read adds the forget-candidate lines. Two
/// reserves cover this end to end: the history-navigation envelope and the
/// completed read form; the storage tests pin the second on its own.
#[test]
fn a_full_read_admitted_at_its_limit_still_reads_once_the_target_completes() {
    let (_directory, verbs, _, _) = fixture();
    let work = add(&verbs, "Retiring fix", None, false, 0);
    let unit = '\u{e000}'.len_utf8();
    let admits = |count: usize, key: &str| {
        verbs
            .remember(
                RememberInput {
                    text: "\u{e000}".repeat(count),
                    key: Some(key.into()),
                    revise: false,
                    expected_revision: None,
                    retires_with: Some(format!("local:{work}")),
                    clear_retires_with: false,
                    append: false,
                    section: None,
                },
                at(1),
            )
            .is_ok()
    };
    let (mut low, mut high) = (1, crate::domain::MAX_PROJECT_MEMORY_BODY_BYTES / unit);
    assert!(admits(low, "edge-probe-low"));
    assert!(
        !admits(high, "edge-probe-high"),
        "the escaped body must reach the full-read limit"
    );
    while high - low > 1 {
        let middle = low.midpoint(high);
        if admits(middle, &format!("edge-probe-{middle}")) {
            low = middle;
        } else {
            high = middle;
        }
    }
    let edge = format!("edge-probe-{low}");
    verbs
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(2),
        )
        .unwrap();
    note(&verbs, &work, "fix recorded", 3);
    let completed = verbs
        .done(
            DoneInput {
                work_ref: Some(work),
                summary: Some("fix delivered".into()),
                ..DoneInput::default()
            },
            at(4),
        )
        .unwrap();
    assert!(!completed.owed, "{}", completed.text());
    let full = verbs
        .memories(
            &MemoriesInput {
                query: Some(if low == 1 {
                    "edge-probe-low".into()
                } else {
                    edge
                }),
                full: true,
                ..MemoriesInput::default()
            },
            at(5),
        )
        .expect("the edge body reads in full after its target completed");
    assert_eq!(full.value["retiring_state"]["lifecycle"], "completed");
    assert!(full.text().contains("forget candidate"));
}

/// Under a tight budget the advisory sheds key rows but keeps the exact total
/// and counts every row it could not show as omitted; a failed lookup is
/// disclosed by class and leaves the lifecycle receipt intact.
#[test]
fn the_advisory_keeps_its_count_under_a_tight_budget_and_discloses_a_failed_lookup() {
    use crate::verbs::memory_retirement::{RetirementAction, append, reserve};
    let base = Receipt::assemble(
        vec!["completed the item".into()],
        crate::verbs::receipts::Guidance::default(),
        serde_json::json!({"operation": "done"}),
        false,
    );
    let candidates = Ok(crate::domain::ProjectMemoryRetirementCandidates {
        total: 20,
        omitted: 4,
        keys: (0..16)
            .map(|index| format!("candidate-key-{index:02}"))
            .collect(),
    });
    let roomy = append(
        &base,
        &candidates,
        &RetirementAction::Completed,
        crate::work_service::MAX_AGENT_WORK_RESPONSE_BYTES,
        crate::argument_names::ArgumentNames::Cli,
    )
    .unwrap();
    assert_eq!(roomy.value["memory_retirement"]["total"], 20);
    assert_eq!(roomy.value["memory_retirement"]["omitted"], 4);
    assert_eq!(
        roomy.value["memory_retirement"]["items"]
            .as_array()
            .unwrap()
            .len(),
        16
    );

    let base_bytes = base
        .text()
        .len()
        .max(crate::verbs::receipts::compact_receipt_json_bytes(&base.value).unwrap());
    let smallest = reserve(
        &base,
        &candidates,
        &RetirementAction::Completed,
        crate::argument_names::ArgumentNames::Cli,
    )
    .unwrap();
    let tight = append(
        &base,
        &candidates,
        &RetirementAction::Completed,
        base_bytes + smallest + 200,
        crate::argument_names::ArgumentNames::Cli,
    )
    .unwrap();
    let shown = tight.value["memory_retirement"]["items"]
        .as_array()
        .unwrap()
        .len();
    assert!(shown < 16, "the tight budget sheds rows");
    assert_eq!(tight.value["memory_retirement"]["total"], 20);
    assert_eq!(tight.value["memory_retirement"]["omitted"], 20 - shown);

    let over = append(
        &base,
        &candidates,
        &RetirementAction::Completed,
        base_bytes / 2,
        crate::argument_names::ArgumentNames::Cli,
    )
    .unwrap();
    assert_eq!(
        over.value["memory_retirement"]["total"], 20,
        "the count survives even when nothing fits"
    );
    assert_eq!(over.value["memory_retirement"]["omitted"], 20);

    let failed = Err(crate::StoreError::InvalidMemoryProjection(
        "candidate key is invalid".into(),
    ));
    let disclosed = append(
        &base,
        &failed,
        &RetirementAction::Completed,
        crate::work_service::MAX_AGENT_WORK_RESPONSE_BYTES,
        crate::argument_names::ArgumentNames::Cli,
    )
    .unwrap();
    assert!(disclosed.value["memory_retirement"]["error_class"].is_string());
    assert!(
        disclosed
            .text()
            .contains("retirement candidates unavailable")
    );
    assert!(disclosed.text().contains("completed the item"));
}

/// Only a read of the current version calls a completed target a forget
/// candidate: a historical version whose target completed after the memory
/// was retargeted shows the item's state without that label.
#[test]
fn a_historical_version_never_calls_its_completed_target_a_forget_candidate() {
    let (_directory, verbs, _, _) = fixture();
    let first = add(&verbs, "First fix", None, false, 0);
    let second = add(&verbs, "Second fix", None, false, 1);
    remember(
        &verbs,
        "moved-note",
        "Waits for the first fix",
        Some(format!("local:{first}")),
        false,
        2,
    );
    remember(
        &verbs,
        "moved-note",
        "Waits for the second fix now",
        Some(format!("local:{second}")),
        true,
        3,
    );
    verbs
        .claim(
            ClaimInput {
                work_ref: first.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(4),
        )
        .unwrap();
    note(&verbs, &first, "first fix recorded", 5);
    let completed = verbs
        .done(
            DoneInput {
                work_ref: Some(first),
                summary: Some("first fix delivered".into()),
                ..DoneInput::default()
            },
            at(6),
        )
        .unwrap();
    assert!(
        completed.value.get("memory_retirement").is_none(),
        "the current version names the second fix, so the first is no candidate"
    );
    let historical = verbs
        .memories(
            &MemoriesInput {
                query: Some("moved-note".into()),
                full: true,
                revision: Some(1),
                ..MemoriesInput::default()
            },
            at(7),
        )
        .unwrap();
    assert_eq!(historical.value["retiring_state"]["lifecycle"], "completed");
    assert!(
        !historical.text().contains("forget candidate"),
        "{}",
        historical.text()
    );
    let current = verbs
        .memories(
            &MemoriesInput {
                query: Some("moved-note".into()),
                full: true,
                ..MemoriesInput::default()
            },
            at(7),
        )
        .unwrap();
    assert_eq!(current.value["retiring_state"]["lifecycle"], "open");
    assert!(!current.text().contains("forget candidate"));
}

// A partial revise through the verbs: the receipt says what changed, with
// bounded excerpts, and offers full reads of both revisions; --append and
// --section together are refused before anything is written.
#[test]
fn a_partial_revise_receipt_shows_the_change_and_both_reads() {
    let (_directory, verbs, _, _) = fixture();
    remember(&verbs, "running-notes", "First finding.", None, false, 1);
    let partial = |append: bool, section: Option<&str>, at_ms: i64| {
        verbs.remember(
            RememberInput {
                text: "Second finding.".into(),
                key: Some("running-notes".into()),
                revise: true,
                expected_revision: Some(1),
                retires_with: None,
                clear_retires_with: false,
                append,
                section: section.map(str::to_owned),
            },
            at(at_ms),
        )
    };
    let both = partial(true, Some("notes"), 2).expect_err("append and section together");
    assert!(both.to_string().contains("alternatives"), "{both}");

    let appended = partial(true, None, 3).expect("append");
    let text = appended.text();
    assert!(
        text.contains("revised project memory running-notes: revision 1 → 2"),
        "{text}"
    );
    assert!(
        text.contains("changed (append): 14 → 31 bytes; 0 removed and 17 added at byte 14"),
        "{text}"
    );
    assert!(text.contains("  + Second finding."), "{text}");
    for revision in [1, 2] {
        let read = format!("engram work memories running-notes --full --revision {revision}");
        assert!(appended.next.contains(&read), "{:?}", appended.next);
    }
    assert_eq!(appended.value["change"]["edit"], "append");
    assert_eq!(appended.value["change"]["added_bytes"], 17);
}
