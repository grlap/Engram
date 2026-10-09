//! A refusal, remedy or reminder that tells the caller to pass an argument
//! names the CLI flag on the CLI and the MCP field over MCP; runnable
//! commands and the raw core envelope keep CLI syntax.

use super::*;
use crate::argument_names::ArgumentNames;
use crate::verbs::argument_wording::{self as wording, registered, respell};

/// One word call that a refusal answers.
type Call<'a> = Box<dyn Fn(&AgentVerbs) -> Result<Receipt, VerbError> + 'a>;

/// One store and two callers on it: the CLI words and the MCP words. Fields
/// drop in declaration order, so the words, which hold the store open, are
/// declared before the home they live in.
struct Surfaces {
    cli: AgentVerbs,
    mcp: AgentVerbs,
    setup: AgentVerbs,
    _directory: crate::test_support::TempHome,
}

fn surfaces() -> Surfaces {
    let directory = crate::test_support::temp_home().expect("temp");
    let path = directory.path().join("work.db");
    let project = ProjectId("argument-wording".into());
    let words = |session: &str| {
        AgentVerbs::new(
            path.clone(),
            project.clone(),
            "agent".into(),
            SessionId(session.into()),
            None,
        )
    };
    Surfaces {
        cli: words("cli-agent"),
        mcp: words("mcp-agent").with_mcp_argument_names(),
        setup: words("setup-agent"),
        _directory: directory,
    }
}

/// Every text an agent reads from a refusal: the message, the structured
/// details' reason and remedy, and the reminders.
fn refusal_texts(verbs: &AgentVerbs, error: &VerbError) -> Vec<String> {
    let mut projected = verbs.project_error(error, crate::store_error_value(&error.error));
    let guidance = verbs.error_guidance(error);
    if verbs.argument_names == ArgumentNames::Mcp {
        projected["error"]["reminders"] = json!(guidance.reminders);
        crate::mcp::prose_sweep::assert_prose_names_fields(&projected);
    }
    let message = verbs.error_message(error);
    assert_eq!(projected["error"]["message"], json!(message));
    let mut texts = vec![message];
    for key in ["reason", "remedy"] {
        if let Some(text) = projected["error"]["details"][key].as_str() {
            texts.push(text.to_owned());
        }
    }
    texts.extend(guidance.reminders);
    texts
}

/// Each registered sentence has a CLI spelling naming flags and an MCP
/// spelling naming fields; no CLI spelling ends another, so respelling a
/// text never picks the wrong one; and the CLI keeps every text unchanged.
#[test]
fn every_registered_sentence_names_flags_for_the_cli_and_fields_for_mcp() {
    let table = registered();
    assert!(table.len() >= 40, "{}", table.len());
    for (cli, mcp) in table {
        assert!(cli.contains("--"), "{cli}");
        assert!(!mcp.contains("--"), "{mcp}");
        assert_ne!(cli, mcp);
        assert_eq!(respell(ArgumentNames::Cli, cli), cli.as_str());
        assert_eq!(respell(ArgumentNames::Mcp, cli), mcp.as_str());
        let led = format!("criterion 2 cites --caller-text: {cli}");
        assert_eq!(
            respell(ArgumentNames::Mcp, &led),
            format!("criterion 2 cites --caller-text: {mcp}"),
            "only the sentence that ends the text changes"
        );
        // A registered sentence that does not end the text is left alone.
        let trailing = format!("{cli}; then more");
        assert_eq!(respell(ArgumentNames::Mcp, &trailing), trailing.as_str());
        for (other, _) in table {
            assert!(
                other == cli || !cli.ends_with(other.as_str()),
                "{cli} ends with {other}"
            );
        }
    }
}

/// Every sentence a successful receipt can carry is respelled after the
/// receipt was fitted to its byte budget, so none of them grows; each is a
/// registered sentence. A refusal-only sentence is not respelled in a
/// receipt, so one that reaches a receipt later fails the prose sweep until
/// it joins the receipt sentences and passes this check.
#[test]
fn sentences_respelled_after_fitting_never_grow() {
    for twin in wording::RECEIPT_SENTENCES {
        assert!(twin.mcp.len() <= twin.cli.len(), "{}", twin.mcp);
        assert!(
            registered()
                .iter()
                .any(|(cli, mcp)| cli == twin.cli && mcp == twin.mcp),
            "{}",
            twin.cli
        );
    }
    let surfaces = surfaces();
    let refusal_only = crate::work_service::READ_RUN_EVIDENCE_REMEDY;
    assert!(refusal_only.mcp.len() > refusal_only.cli.len());
    let receipt = Receipt::assemble(
        Vec::new(),
        Guidance {
            reminders: vec![refusal_only.cli.into()],
            next: Vec::new(),
        },
        json!({}),
        false,
    );
    assert_eq!(
        surfaces.mcp.spell_receipt(receipt.clone()).value,
        receipt.value
    );
}

/// Every refusal a word raises with an argument-naming sentence names the
/// flag on the CLI and the field over MCP, in the message, the details and
/// the reminders, while the code and the offered commands stay as they are.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one table of every reachable refusal"
)]
fn word_refusals_name_flags_on_the_cli_and_fields_over_mcp() {
    let surfaces = surfaces();
    let item = surfaces
        .setup
        .add(
            AddInput {
                title: "Shared item".into(),
                ..AddInput::default()
            },
            at(0),
        )
        .expect("add")
        .value["work"]["short_ref"]
        .as_str()
        .expect("ref")
        .to_owned();
    surfaces
        .setup
        .remember(
            RememberInput {
                text: "a remembered note".into(),
                key: Some("kept".into()),
                revise: false,
                expected_revision: None,
                retires_with: None,
                clear_retires_with: false,
                append: false,
                section: None,
            },
            at(1),
        )
        .expect("remember");
    let remember = |text: &str| RememberInput {
        text: text.into(),
        key: None,
        revise: false,
        expected_revision: None,
        retires_with: None,
        clear_retires_with: false,
        append: false,
        section: None,
    };
    let show = |verbs: &AgentVerbs, input: ShowInput| verbs.show_records(&item, &input, at(5));
    let gate = |name: String, evidence_ref: Option<String>| GateInput {
        work_ref: None,
        name,
        failed: Vec::new(),
        evidence_ref,
    };
    let calls: Vec<(String, String, Call)> = vec![
        (
            wording::GATE_NEEDS_TARGET.cli.into(),
            wording::GATE_NEEDS_TARGET.mcp.into(),
            Box::new(|verbs| verbs.gate(gate("unit".into(), None), at(2))),
        ),
        (
            wording::EVALUATE_NEEDS_TARGET.cli.into(),
            wording::EVALUATE_NEEDS_TARGET.mcp.into(),
            Box::new(|verbs| {
                verbs.evaluate(
                    EvaluateInput {
                        work_ref: None,
                        mode: "independent_session".into(),
                        acceptance_basis: 1,
                        evidence_basis: 1,
                        verdicts: Vec::new(),
                        attempt: None,
                        source_fingerprint: None,
                        model: None,
                        execution_identity: None,
                        parent_session: None,
                        supersedes: None,
                    },
                    at(2),
                )
            }),
        ),
        (
            crate::domain::GATE_INPUT_TOO_LARGE_REMEDY.cli.into(),
            crate::domain::GATE_INPUT_TOO_LARGE_REMEDY.mcp.into(),
            Box::new(|verbs| {
                verbs.gate(
                    gate("x".repeat(crate::domain::MAX_GATE_NAME_BYTES + 1), None),
                    at(2),
                )
            }),
        ),
        (
            crate::domain::gate_ref_refusal_twin().0,
            crate::domain::gate_ref_refusal_twin().1,
            Box::new(|verbs| verbs.gate(gate("unit".into(), Some("bad\u{7}ref".into())), at(2))),
        ),
        (
            wording::REVISION_NEEDS_FULL_REFUSAL.cli.into(),
            wording::REVISION_NEEDS_FULL_REFUSAL.mcp.into(),
            Box::new(|verbs| {
                verbs.memories(
                    &MemoriesInput {
                        query: Some("kept".into()),
                        revision: Some(1),
                        ..MemoriesInput::default()
                    },
                    at(2),
                )
            }),
        ),
        (
            wording::FULL_WITH_AFTER_REFUSAL.cli.into(),
            wording::FULL_WITH_AFTER_REFUSAL.mcp.into(),
            Box::new(|verbs| {
                verbs.memories(
                    &MemoriesInput {
                        query: Some("kept".into()),
                        after: Some("kept".into()),
                        full: true,
                        ..MemoriesInput::default()
                    },
                    at(2),
                )
            }),
        ),
        (
            wording::FULL_NEEDS_KEY_REFUSAL.cli.into(),
            wording::FULL_NEEDS_KEY_REFUSAL.mcp.into(),
            Box::new(|verbs| {
                verbs.memories(
                    &MemoriesInput {
                        full: true,
                        ..MemoriesInput::default()
                    },
                    at(2),
                )
            }),
        ),
        (
            crate::storage::FILTERED_SEARCH_AFTER_REFUSAL.cli.into(),
            crate::storage::FILTERED_SEARCH_AFTER_REFUSAL.mcp.into(),
            Box::new(|verbs| {
                verbs.memories(
                    &MemoriesInput {
                        query: Some("remembered".into()),
                        after: Some("kept".into()),
                        ..MemoriesInput::default()
                    },
                    at(2),
                )
            }),
        ),
        (
            crate::storage::REVISE_NEEDS_KEY_REFUSAL.cli.into(),
            crate::storage::REVISE_NEEDS_KEY_REFUSAL.mcp.into(),
            Box::new(|verbs| {
                verbs.remember(
                    RememberInput {
                        revise: true,
                        ..remember("a revised note")
                    },
                    at(2),
                )
            }),
        ),
        (
            crate::storage::PARTIAL_EDIT_NEEDS_REVISE_REFUSAL.cli.into(),
            crate::storage::PARTIAL_EDIT_NEEDS_REVISE_REFUSAL.mcp.into(),
            Box::new(|verbs| {
                verbs.remember(
                    RememberInput {
                        key: Some("kept".into()),
                        append: true,
                        ..remember("an appended paragraph")
                    },
                    at(2),
                )
            }),
        ),
        (
            wording::APPEND_WITH_SECTION_REFUSAL.cli.into(),
            wording::APPEND_WITH_SECTION_REFUSAL.mcp.into(),
            Box::new(|verbs| {
                verbs.remember(
                    RememberInput {
                        key: Some("kept".into()),
                        append: true,
                        section: Some("part".into()),
                        ..remember("an edit")
                    },
                    at(2),
                )
            }),
        ),
        (
            wording::RETIRES_WITH_COMBINED_REFUSAL.cli.into(),
            wording::RETIRES_WITH_COMBINED_REFUSAL.mcp.into(),
            Box::new(|verbs| {
                verbs.remember(
                    RememberInput {
                        key: Some("kept".into()),
                        retires_with: Some(format!("local:{item}")),
                        clear_retires_with: true,
                        ..remember("a retargeted note")
                    },
                    at(2),
                )
            }),
        ),
        (
            crate::storage::CLEAR_TARGET_NEEDS_REVISE_REFUSAL.cli.into(),
            crate::storage::CLEAR_TARGET_NEEDS_REVISE_REFUSAL.mcp.into(),
            Box::new(|verbs| {
                verbs.remember(
                    RememberInput {
                        key: Some("fresh".into()),
                        clear_retires_with: true,
                        ..remember("a note without a target")
                    },
                    at(2),
                )
            }),
        ),
        (
            crate::storage::UNSAFE_KEY_REFUSAL.cli.into(),
            crate::storage::UNSAFE_KEY_REFUSAL.mcp.into(),
            Box::new(|verbs| verbs.remember(remember("!!!"), at(2))),
        ),
        (
            wording::READY_WITH_BLOCKED_REFUSAL.cli.into(),
            wording::READY_WITH_BLOCKED_REFUSAL.mcp.into(),
            Box::new(|verbs| {
                verbs.ls(
                    &LsInput {
                        ready: true,
                        blocked: true,
                        ..LsInput::default()
                    },
                    at(2),
                )
            }),
        ),
        (
            wording::CHILD_FILTER_REFUSAL.cli.into(),
            wording::CHILD_FILTER_REFUSAL.mcp.into(),
            Box::new(|verbs| {
                verbs.ls(
                    &LsInput {
                        optional: true,
                        ..LsInput::default()
                    },
                    at(2),
                )
            }),
        ),
        (
            wording::OPTIONAL_NEEDS_PARENT_REFUSAL.cli.into(),
            wording::OPTIONAL_NEEDS_PARENT_REFUSAL.mcp.into(),
            Box::new(|verbs| {
                verbs.add(
                    AddInput {
                        title: "Orphan option".into(),
                        optional: true,
                        ..AddInput::default()
                    },
                    at(2),
                )
            }),
        ),
        (
            wording::BLANK_EVALUATION_MODE_REFUSAL.cli.into(),
            wording::BLANK_EVALUATION_MODE_REFUSAL.mcp.into(),
            Box::new(|verbs| {
                verbs.add(
                    AddInput {
                        title: "Blank mode".into(),
                        evaluation_mode: Some("  ".into()),
                        ..AddInput::default()
                    },
                    at(2),
                )
            }),
        ),
        (
            wording::OBSERVATIONS_ALONE_REFUSAL.cli.into(),
            wording::OBSERVATIONS_ALONE_REFUSAL.mcp.into(),
            Box::new(|verbs| {
                show(
                    verbs,
                    ShowInput {
                        observations: true,
                        notes: true,
                        ..ShowInput::default()
                    },
                )
            }),
        ),
        (
            wording::EVALUATIONS_ALONE_REFUSAL.cli.into(),
            wording::EVALUATIONS_ALONE_REFUSAL.mcp.into(),
            Box::new(|verbs| {
                show(
                    verbs,
                    ShowInput {
                        evaluations: true,
                        notes: true,
                        ..ShowInput::default()
                    },
                )
            }),
        ),
        (
            wording::SHOW_WINDOWS_REFUSAL.cli.into(),
            wording::SHOW_WINDOWS_REFUSAL.mcp.into(),
            Box::new(|verbs| {
                show(
                    verbs,
                    ShowInput {
                        gates: true,
                        ..ShowInput::default()
                    },
                )
            }),
        ),
        (
            crate::work_service::UNKNOWN_EVALUATION_REFUSAL.cli.into(),
            crate::work_service::UNKNOWN_EVALUATION_REFUSAL.mcp.into(),
            Box::new(|verbs| {
                show(
                    verbs,
                    ShowInput {
                        evaluation: Some("0".repeat(64)),
                        ..ShowInput::default()
                    },
                )
            }),
        ),
        (
            wording::UPDATE_NEEDS_ACTION_REFUSAL.cli.into(),
            wording::UPDATE_NEEDS_ACTION_REFUSAL.mcp.into(),
            Box::new(|verbs| {
                verbs.update(
                    UpdateInput {
                        work_ref: Some(item.clone()),
                        action: UpdateAction::Revise {
                            external: None,
                            clear_external: false,
                            title: None,
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
                    at(2),
                )
            }),
        ),
        (
            wording::HANDOFF_LABEL_TARGET.cli.into(),
            wording::HANDOFF_LABEL_TARGET.mcp.into(),
            Box::new(|verbs| {
                verbs.handoff(
                    HandoffInput {
                        work_ref: Some(item.clone()),
                        action: HandoffAction::Offer {
                            to: format!("peer-{}", "a".repeat(24)),
                            summary: None,
                            ttl_seconds: None,
                        },
                    },
                    at(2),
                )
            }),
        ),
    ];
    for (cli, mcp, call) in &calls {
        let on_cli = call(&surfaces.cli).expect_err(cli);
        let on_mcp = call(&surfaces.mcp).expect_err(cli);
        assert_eq!(
            crate::store_error_value(&on_cli.error)["error"]["code"],
            crate::store_error_value(&on_mcp.error)["error"]["code"],
            "{cli}"
        );
        let cli_texts = refusal_texts(&surfaces.cli, &on_cli);
        assert!(cli_texts[0].ends_with(cli.as_str()), "{cli}: {cli_texts:?}");
        let mcp_texts = refusal_texts(&surfaces.mcp, &on_mcp);
        assert!(mcp_texts[0].ends_with(mcp.as_str()), "{mcp}: {mcp_texts:?}");
        for text in &mcp_texts {
            assert!(!text.contains(cli.as_str()), "{cli}: {text}");
        }
        // The raw core envelope and the offered commands keep CLI syntax.
        assert!(
            crate::store_error_value(&on_mcp.error)["error"]["message"]
                .as_str()
                .unwrap()
                .ends_with(cli.as_str()),
            "{cli}"
        );
        assert_eq!(
            surfaces.mcp.error_guidance(&on_mcp).next,
            surfaces.cli.error_guidance(&on_cli).next,
            "{cli}"
        );
    }
}

/// The message, reason and remedy of the raw core envelope.
fn core_texts(core: &Value) -> String {
    ["message", "details"]
        .iter()
        .flat_map(|key| match key {
            &"message" => vec![core["error"]["message"].as_str().unwrap_or_default()],
            _ => ["reason", "remedy"]
                .iter()
                .map(|field| core["error"]["details"][field].as_str().unwrap_or_default())
                .collect(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn blocker_ids_stay_in_core_and_selectors_are_projected_on_every_agent_text() {
    let surfaces = surfaces();
    for (reason, twin) in [
        (
            crate::work_service::MULTIPLE_BLOCKERS_REFUSAL,
            wording::MULTIPLE_BLOCKERS_REFUSAL,
        ),
        (
            crate::storage::UNKNOWN_BLOCKER_REFUSAL,
            wording::UNKNOWN_BLOCKER_REFUSAL,
        ),
    ] {
        let error = VerbError::at(StoreError::InvalidWork(reason.into()), "w-test-blockers");
        let core = core_texts(&crate::store_error_value(&error.error));
        assert!(core.contains("blocker_id"), "{core}");
        assert!(
            !core.contains("selector") && !core.contains("--blocker"),
            "{core}"
        );
        for (verbs, expected) in [(&surfaces.cli, twin.cli), (&surfaces.mcp, twin.mcp)] {
            let texts = refusal_texts(verbs, &error);
            assert!(
                texts.iter().filter(|text| text.contains(expected)).count() >= 3,
                "message, reason and reminder: {texts:?}"
            );
            assert!(
                texts.iter().all(|text| !text.contains("blocker_id")),
                "{texts:?}"
            );
            assert_eq!(
                verbs.error_guidance(&error).next,
                ["engram work show w-test-blockers"]
            );
        }
    }
}

/// Core refusals and remedies whose sentence names an argument, as the core
/// raises them: the agent projection names the field over MCP, the CLI
/// keeps the flag, and the core envelope keeps the CLI spelling.
#[test]
fn core_refusals_and_remedies_name_flags_on_the_cli_and_fields_over_mcp() {
    let surfaces = surfaces();
    let work = crate::WorkId::new();
    let admission: crate::domain::AcceptanceEvaluationAdmissionCause =
        serde_json::from_value(json!({
            "kind": "eligibility",
            "mismatch": "mode_disallowed",
            "requested_mode": "same_session",
            "task_mark": null,
            "admitted_modes": ["independent_session"],
            "remedy": "read_run_evidence",
        }))
        .unwrap();
    let carried = |refusal: crate::storage::CarriedFailureRefusal| {
        StoreError::AcceptanceEvaluationCarriedFailure {
            work,
            refusal,
            failed: None,
            reason: format!("the carried failure is not named; {}", refusal.remedy()),
        }
    };
    let link = |reason: &'static str| StoreError::WorkCriterionLinkInvalid {
        criterion: Some(1),
        reason,
    };
    let cases = [
        (
            link(crate::storage::AMBIGUOUS_LOCATOR_REFUSAL.cli),
            vec![
                crate::storage::AMBIGUOUS_LOCATOR_REFUSAL,
                crate::verbs::error_rendering::remedies::CRITERION_LINK_REMEDY,
            ],
        ),
        (
            link(crate::storage::CHECKPOINT_LOCATOR_REFUSAL.cli),
            vec![crate::storage::CHECKPOINT_LOCATOR_REFUSAL],
        ),
        (
            link(crate::storage::HISTORY_LOCATOR_REFUSAL.cli),
            vec![crate::storage::HISTORY_LOCATOR_REFUSAL],
        ),
        (
            link(crate::storage::FOREIGN_LOCATOR_REFUSAL.cli),
            vec![crate::storage::FOREIGN_LOCATOR_REFUSAL],
        ),
        (
            StoreError::AcceptanceEvaluationRefused {
                work,
                reason: format!(
                    "criterion 1 cites x--abc: {}",
                    crate::storage::FOREIGN_LOCATOR_REFUSAL.cli
                ),
            },
            vec![crate::storage::FOREIGN_LOCATOR_REFUSAL],
        ),
        (
            carried(crate::storage::CarriedFailureRefusal::Unacknowledged),
            vec![crate::storage::CarriedFailureRefusal::Unacknowledged.remedy_twin()],
        ),
        (
            carried(crate::storage::CarriedFailureRefusal::NothingToSupersede),
            vec![crate::storage::CarriedFailureRefusal::NothingToSupersede.remedy_twin()],
        ),
        (
            StoreError::AcceptanceEvaluationAdmissionRefused {
                work,
                reason: "a citation is not on this run".into(),
                cause: Box::new(admission),
            },
            vec![crate::work_service::READ_RUN_EVIDENCE_REMEDY],
        ),
        (
            StoreError::WorkCatalogCursorInvalid {
                reason: "the listing moved".into(),
            },
            vec![crate::verbs::error_rendering::remedies::CATALOG_CURSOR_REMEDY],
        ),
        (
            StoreError::WorkShowCursorInvalid {
                reason: crate::work_service::ASSESSMENT_CONTINUATION_REFUSAL
                    .cli
                    .into(),
            },
            vec![
                crate::work_service::ASSESSMENT_CONTINUATION_REFUSAL,
                crate::verbs::error_rendering::remedies::SHOW_CURSOR_REMEDY,
            ],
        ),
        (
            StoreError::WorkPeerDecompositionRefused { parent: work },
            vec![crate::verbs::error_rendering::remedies::PEER_DECOMPOSITION_REMEDY],
        ),
        (
            StoreError::InvalidWork(
                crate::storage::CHILD_REQUIREMENT_NEEDS_PARENT_REFUSAL
                    .cli
                    .into(),
            ),
            vec![crate::storage::CHILD_REQUIREMENT_NEEDS_PARENT_REFUSAL],
        ),
    ];
    for (error, twins) in cases {
        let label = error.to_string();
        let core = core_texts(&crate::store_error_value(&error));
        let error = VerbError::from(error);
        let cli = refusal_texts(&surfaces.cli, &error).join("\n");
        let mcp = refusal_texts(&surfaces.mcp, &error).join("\n");
        for twin in twins {
            assert!(cli.contains(twin.cli), "{label}: {cli}");
            assert!(core.contains(twin.cli), "{label}: {core}");
            assert!(mcp.contains(twin.mcp), "{label}: {mcp}");
            assert!(!mcp.contains(twin.cli), "{label}: {mcp}");
        }
        // The caller's citation before the sentence is kept as written.
        if label.contains("x--abc") {
            assert!(mcp.contains("criterion 1 cites x--abc: "), "{mcp}");
        }
    }
}

/// The project-memory refusals that name the key build their remedy and
/// reminders with the arguments spelled for the caller.
#[test]
fn project_memory_refusals_name_their_arguments_for_the_caller() {
    let surfaces = surfaces();
    let missing = crate::storage::MissingMemorySection {
        key: "kept".into(),
        revision: 2,
        section: "part".into(),
        sections: Vec::new(),
    };
    for (error, cli, mcp) in [
        (
            StoreError::ProjectMemoryExists("kept".into()),
            vec![
                "read memories kept --full; use remember with --key kept --revise to retain history",
                "use remember --key kept --revise",
            ],
            vec![
                "read memories with query kept and full; use remember with key kept and revise to retain history",
                "use remember with key kept and revise",
            ],
        ),
        (
            StoreError::ProjectMemoryRevisionConflict {
                key: "kept".into(),
                expected: 1,
                current: 2,
            },
            vec!["read memories kept --full and reconcile before revising"],
            vec!["read memories with query kept and full and reconcile before revising"],
        ),
        (
            StoreError::ProjectMemoryRevisionNotFound {
                key: "kept".into(),
                revision: 3,
                current: 2,
            },
            vec!["read memories kept --full for history navigation"],
            vec!["read memories with query kept and full for history navigation"],
        ),
        (
            StoreError::ProjectMemoryRetired("kept".into()),
            vec!["retry remember with an explicit --key"],
            vec!["retry remember with an explicit key"],
        ),
        (
            StoreError::ProjectMemorySectionNotFound(Box::new(missing)),
            vec!["add `part` with --append and its markers"],
            vec!["add `part` with append and its markers"],
        ),
    ] {
        let error = VerbError::from(error);
        let on_cli = refusal_texts(&surfaces.cli, &error).join("\n");
        let on_mcp = refusal_texts(&surfaces.mcp, &error).join("\n");
        for text in cli {
            assert!(on_cli.contains(text), "{on_cli}");
        }
        for text in mcp {
            assert!(on_mcp.contains(text), "{on_mcp}");
        }
        assert!(!on_mcp.contains(" --"), "{on_mcp}");
        // Offered commands stay CLI syntax on both surfaces.
        assert_eq!(
            surfaces.mcp.error_guidance(&error).next,
            surfaces.cli.error_guidance(&error).next
        );
    }
}

/// A receipt's reminders, listing hint and completion remedy name fields over
/// MCP; its runnable commands and the lines only the CLI prints keep flags.
#[test]
fn receipts_name_fields_over_mcp_and_keep_runnable_commands() {
    let surfaces = surfaces();
    let source = crate::work_service::MEASURE_SOURCE_REMEDY;
    let stale =
        |remedy: &str| format!("w-000000000001 acceptance evaluation is stale (source): {remedy}");
    let receipt = Receipt::assemble(
        vec![format!("text line: {}", wording::PAGE_LIMIT_HINT.cli)],
        Guidance {
            reminders: vec![
                wording::CLIPPED_STATUS.cli.into(),
                stale(source.cli),
                "a reminder that names no argument".into(),
            ],
            next: vec!["engram work memories kept --full".into()],
        },
        json!({"hint": wording::PAGE_LIMIT_HINT.cli, "remedy": source.cli}),
        true,
    );
    assert_eq!(
        surfaces.cli.spell_receipt(receipt.clone()).value,
        receipt.value
    );
    let spelled = surfaces.mcp.spell_receipt(receipt.clone());
    assert_eq!(spelled.value["hint"], json!(wording::PAGE_LIMIT_HINT.mcp));
    assert_eq!(spelled.value["remedy"], json!(source.mcp));
    let reminders = vec![
        wording::CLIPPED_STATUS.mcp.to_owned(),
        stale(source.mcp),
        "a reminder that names no argument".to_owned(),
    ];
    assert_eq!(spelled.value["reminders"], json!(reminders));
    assert_eq!(spelled.reminders, reminders);
    assert_eq!(spelled.value["next"], receipt.value["next"]);
    // Lines only the CLI prints keep the flag.
    assert_eq!(spelled.lines, receipt.lines);
}

/// A real listing page that stops at its limit gives the hint with the limit
/// spelled for the caller.
#[test]
fn a_listing_stopped_at_its_limit_names_the_limit_for_the_caller() {
    let surfaces = surfaces();
    for (second, title) in [(0, "First"), (1, "Second")] {
        surfaces
            .setup
            .add(
                AddInput {
                    title: title.into(),
                    ..AddInput::default()
                },
                at(second),
            )
            .expect("add");
    }
    let page = |verbs: &AgentVerbs| {
        verbs.spell_receipt(
            verbs
                .ls(
                    &LsInput {
                        limit: Some(1),
                        ..LsInput::default()
                    },
                    at(3),
                )
                .expect("ls"),
        )
    };
    let cli = page(&surfaces.cli);
    let mcp = page(&surfaces.mcp);
    assert_eq!(cli.value["hint"], json!(wording::PAGE_LIMIT_HINT.cli));
    assert_eq!(mcp.value["hint"], json!(wording::PAGE_LIMIT_HINT.mcp));
    assert_eq!(cli.value["next"], mcp.value["next"]);
    assert!(mcp.value["next"][0].as_str().unwrap().contains("--after"));
}

/// The reminders that carry their own data name the arguments that keep or
/// clear a dropped retirement target, or retarget a memory, as the caller
/// passes them.
#[test]
fn retirement_reminders_name_their_arguments_for_the_caller() {
    let dropped = crate::domain::ProjectMemoryRetiringTargetDropped {
        revision: 3,
        target: crate::domain::ProjectMemoryRetiringTarget::External {
            project: "other".into(),
            reference: "ref-1".into(),
        },
    };
    assert_eq!(
        crate::work_service::retiring_target_dropped_reminder(&dropped, ArgumentNames::Cli),
        "revision 3 dropped the retirement target without a clear; to keep it, revise with --retires-with external:other#ref-1, or to let it go, revise with --clear-retires-with"
    );
    assert_eq!(
        crate::work_service::retiring_target_dropped_reminder(&dropped, ArgumentNames::Mcp),
        "revision 3 dropped the retirement target without a clear; to keep it, revise with retires_with external:other#ref-1, or to let it go, revise with clear_retires_with"
    );
    let base = Receipt::assemble(Vec::new(), Guidance::default(), json!({}), false);
    let candidates = Ok(crate::domain::ProjectMemoryRetirementCandidates {
        total: 1,
        omitted: 0,
        keys: vec!["kept".into()],
    });
    let action = crate::verbs::memory_retirement::RetirementAction::Superseded {
        replacement: "w-000000000002".into(),
    };
    for (names, argument) in [
        (ArgumentNames::Cli, "--retires-with local:w-000000000002"),
        (ArgumentNames::Mcp, "retires_with local:w-000000000002"),
    ] {
        let receipt = crate::verbs::memory_retirement::append(
            &base,
            &candidates,
            &action,
            crate::work_service::MAX_AGENT_WORK_RESPONSE_BYTES,
            names,
        )
        .unwrap();
        let instruction = receipt.value["memory_retirement"]["instruction"]
            .as_str()
            .unwrap();
        assert!(
            instruction.contains(&format!("revise it with {argument},")),
            "{instruction}"
        );
    }
}
