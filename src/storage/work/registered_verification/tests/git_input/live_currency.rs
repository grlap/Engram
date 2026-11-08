//! Real Git endpoint decisions, independent of source-report delivery.

use super::*;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

pub(super) fn git(root: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    for (name, _) in std::env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(name);
        }
    }
    // Checkouts and restores must use the same config scope as the reader.
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let output = command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null)
        .env("GIT_ATTR_NOSYSTEM", "1")
        .args(["-c", &format!("core.attributesFile={null}")])
        .args(["-c", &format!("core.excludesFile={null}")])
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

pub(super) fn commit(root: &Path) -> String {
    git(
        root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--no-gpg-sign",
            "-qm",
            "fixture",
            "--allow-empty",
        ],
    );
    git(root, &["rev-parse", "HEAD"])
}

pub(super) fn attach_root(f: &mut Fixture) {
    let home = crate::test_support::temp_home().unwrap();
    let root = home.path().join("source");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("tracked"), "checked\n").unwrap();
    git(&root, &["add", "tracked"]);
    let checked = commit(&root);
    let workspace = root.canonicalize().unwrap().to_string_lossy().into_owned();
    f.request.registration.root.workspace_id = workspace.clone();
    f.request.input = RegisteredVerificationInput::Commit { commit: checked };
    let binding = &f.request.registration.binding;
    f.store
        .bind_named_root(
            &f.project,
            &f.session,
            &f.connection,
            crate::storage::HostSessionAuthority::Work,
            binding.claim_id,
            binding.claim_fence,
            &workspace,
            1,
            at(4),
            NamedRootBindingKind::Bound,
            None,
            &mut actor("runner"),
            "name-live",
            at(4),
        )
        .unwrap();
    f.source = Some(home);
}

pub(super) fn live_fixture(bound: bool, evaluated: bool) -> (PathBuf, Fixture) {
    let f = if evaluated {
        prepared(bound, true)
    } else {
        unevaluated(bound)
    };
    let root = PathBuf::from(&f.request.registration.root.workspace_id);
    (root, f)
}

fn another_check(
    f: &mut Fixture,
    key: &str,
    kind: VerificationKind,
    input: RegisteredVerificationInput,
) -> ObjectId {
    f.request.operation_key = format!("termal-run:test-{key}");
    f.request.registration.host_run_id = format!("test-{key}");
    f.request.registration.request_identity = format!("request-{key}");
    f.request.input = input;
    f.request.checks[0].check_kind = kind;
    f.request.checks[0].check_fingerprint =
        ObjectId::from_canonical_bytes(format!("{kind:?} fixture check").as_bytes());
    f.store
        .record_registered_verification(
            &f.project,
            &f.session,
            &f.connection,
            crate::storage::HostSessionAuthority::Work,
            &actor("runner"),
            &f.request,
            at(13),
        )
        .unwrap()
        .verifications[0]
        .record_id
        .clone()
}

pub(super) fn unlocated_change(f: &mut Fixture) {
    unlocated_change_at(f, 11);
}

fn unlocated_change_at(f: &mut Fixture, second: i64) -> ObjectId {
    let work = f
        .store
        .get_work_item(f.request.registration.binding.work_id)
        .unwrap();
    let held = f.store.claim_fixture(work.work_id);
    crate::storage::work::test_support::source_mutation_from_basis(
        &mut f.store,
        &work,
        &held,
        "runner",
        "unlocated-change",
        second,
        None,
        None,
    )
}

fn target_at_input(f: &mut Fixture) -> VerificationBindTarget {
    let target = f
        .store
        .create_work(
            &root_request("project-a", "bind-target", 5),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let held = claim(
        &mut f.store,
        &target,
        "runner",
        "bind-target-claim",
        6,
        3600,
    );
    f.store
        .bind_named_root(
            &f.project,
            &f.session,
            &f.connection,
            crate::storage::HostSessionAuthority::Work,
            held.claim_id,
            held.fence,
            &f.request.registration.root.workspace_id,
            1,
            at(7),
            NamedRootBindingKind::Bound,
            None,
            &mut actor("runner"),
            "bind-target-root",
            at(7),
        )
        .unwrap();
    let sighting = crate::storage::work::test_support::source_mutation_from_basis(
        &mut f.store,
        &target,
        &held,
        "runner",
        "bind-target-sighting",
        8,
        Some(ExecutionSourceBasis {
            workspace_id: f.request.registration.root.workspace_id.clone(),
            source_revision: f.request.input.revision(),
            source_root_generation: Some(1),
            source_root_state: Some(SourceRootState::Named),
            git_tree: None,
        }),
        None,
    );
    let run = f.store.get_work_run(held.run_id).unwrap();
    VerificationBindTarget {
        binding: ControlWorkBinding {
            root_execution_id: run.root_execution_id,
            work_id: target.work_id,
            run_id: held.run_id,
            work_revision: held.accepted_work_revision,
            claim_id: held.claim_id,
            claim_fence: held.fence,
        },
        sighting: VerificationBindSighting {
            observation: sighting,
            source_revision: f.request.input.revision(),
        },
        criteria: vec![1],
    }
}

#[test]
fn live_git_currency_unlocated_report_uses_git_at_evaluate_bind_and_done() {
    for dirty in [false, true] {
        for before_cut in [false, true] {
            let (root, mut f) = live_fixture(true, true);
            let check = f.record().unwrap().verifications[0].record_id.clone();
            let revision = f.request.input.revision();
            let mut input = evaluation(&f, &check, &revision);
            unlocated_change(&mut f);
            if before_cut {
                input.evaluated_through = f
                    .store
                    .work_feed_head(&FeedId::RunExecution(f.request.registration.binding.run_id))
                    .unwrap();
            }
            if dirty {
                // Establish an evaluation while Git still confirms the input,
                // then exercise both decisions after a real edit. Without it,
                // done stops earlier at MissingAcceptanceEvaluation.
                record_evaluation(&mut f, &input).unwrap();
                input.attempt_key = Some("dirty-unlocated-evaluation".into());
                std::fs::write(root.join("tracked"), "edited\n").unwrap();
            }
            let target = target_at_input(&mut f);
            let binding = VerificationBindInput {
                idempotency_key: "unlocated-bind".into(),
                original: check.clone(),
                measurement: BindMeasurement {
                    workspace_id: f.request.registration.root.workspace_id.clone(),
                    source_revision: revision.clone(),
                    measured_at: at(12),
                },
                targets: vec![target],
            };
            let bind = f.store.bind_verification(
                &f.project,
                &f.session,
                &f.connection,
                crate::storage::HostSessionAuthority::Work,
                &actor("runner"),
                &binding,
                at(12),
            );
            if dirty {
                let error = bind.unwrap_err();
                assert!(
                    matches!(error, StoreError::VerificationBindRefused(ref refused)
                    if refused.original == Some(VerificationBindOriginalRefusal::RootNotCurrent)),
                    "{error}"
                );
                let error = record_evaluation(&mut f, &input).unwrap_err();
                assert!(
                    matches!(error, StoreError::WorkCompletionRefused { .. }),
                    "{error}"
                );
                assert!(error.to_string().contains("dirty"), "{error}");
                let error =
                    complete(&mut f, &check, &revision, "unlocated-dirty-done").unwrap_err();
                assert!(
                    matches!(error, StoreError::WorkCompletionRefused { .. }),
                    "{error}"
                );
                assert!(error.to_string().contains("dirty"), "{error}");
                continue;
            }
            assert_eq!(bind.unwrap().bound.len(), 1);
            let recorded = record_evaluation(&mut f, &input).unwrap();
            let seal = complete(&mut f, &check, &revision, "unlocated-done").unwrap();
            assert_eq!(
                f.store.completion_seal_evaluation(&seal).unwrap(),
                Some(recorded.record)
            );
            assert!(f.store.verify_all().unwrap().is_healthy());
        }
    }
    let (_root, mut legacy) = live_fixture(true, false);
    legacy.request.input = RegisteredVerificationInput::StagedTree {
        tree: GitTreeId::parse(object_name("legacy staged input")).unwrap(),
        base_commit: object_name("legacy staged base"),
    };
    let check = legacy.record().unwrap().verifications[0].record_id.clone();
    unlocated_change(&mut legacy);
    let revision = legacy.request.input.revision();
    let error = complete(&mut legacy, &check, &revision, "legacy-unlocated").unwrap_err();
    assert!(
        matches!(error, StoreError::WorkCompletionRefused { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("fresh check"), "{error}");
}

#[test]
fn live_git_currency_nonpass_verdicts_remain_recordable_without_a_current_pass() {
    for verdict in [
        AcceptanceVerdict::Fail,
        AcceptanceVerdict::InsufficientEvidence,
        AcceptanceVerdict::NeedsHuman,
    ] {
        for state in ["dirty", "new-commit", "failed", "not-run"] {
            let (root, mut f) = live_fixture(false, true);
            if matches!(state, "failed" | "not-run") {
                f.request.run.outcome = RegisteredVerificationOutcome::Failed;
                f.request.checks[0].outcome = if state == "failed" {
                    RegisteredVerificationOutcome::Failed
                } else {
                    f.request.checks[0].started_at = None;
                    f.request.checks[0].completed_at = None;
                    RegisteredVerificationOutcome::NotRun
                };
            }
            f.record().unwrap();
            let note = quiet_evaluation::note(&mut f);
            let mut input = quiet_evaluation::judgment(&f, &note);
            input.verdicts[0].verdict = verdict;
            match state {
                "dirty" => std::fs::write(root.join("tracked"), "edited\n").unwrap(),
                "new-commit" => {
                    commit(&root);
                }
                _ => {}
            }
            let recorded = record_evaluation(&mut f, &input)
                .unwrap_or_else(|error| panic!("{verdict:?}/{state}: {error}"));
            assert!(!recorded.record.all_pass());
            assert_eq!(recorded.record.registered_input_decision, None);
            let revision = f.request.input.revision();
            let error = complete(&mut f, &note, &revision, "nonpass-done").unwrap_err();
            assert!(
                matches!(
                    error,
                    StoreError::WorkCompletionRecoveryRequired { .. }
                        | StoreError::WorkCompletionRefused { .. }
                ),
                "{error}"
            );
            assert!(
                f.store
                    .get_work_run(f.request.registration.binding.run_id)
                    .unwrap()
                    .completion_seal
                    .is_none()
            );
        }
    }
}

#[test]
fn live_git_currency_unlocated_stock_report_is_waived_after_a_current_lint_decision() {
    for evaluated in [false, true] {
        for state in ["lint-only", "older-test", "failed-test"] {
            let (root, mut f) = live_fixture(false, evaluated);
            if state != "lint-only" {
                if state == "failed-test" {
                    f.request.run.outcome = RegisteredVerificationOutcome::Failed;
                    f.request.checks[0].outcome = RegisteredVerificationOutcome::Failed;
                }
                f.record().unwrap();
            }
            let note = quiet_evaluation::note(&mut f);
            if state == "older-test" {
                std::fs::write(root.join("tracked"), "current lint commit\n").unwrap();
                git(&root, &["add", "tracked"]);
                f.request.input = RegisteredVerificationInput::Commit {
                    commit: commit(&root),
                };
            }
            f.request.run.outcome = RegisteredVerificationOutcome::Passed;
            f.request.checks[0].outcome = RegisteredVerificationOutcome::Passed;
            f.request.registration.registered_at = at(12);
            f.request.run.started_at = at(12);
            f.request.run.finished_at = at(12);
            f.request.checks[0].started_at = Some(at(12));
            f.request.checks[0].completed_at = Some(at(12));
            let checked_input = f.request.input.clone();
            let lint = another_check(
                &mut f,
                "lint-before-unlocated",
                VerificationKind::Lint,
                checked_input,
            );
            let mut input = quiet_evaluation::judgment(&f, &note);
            let change = unlocated_change_at(&mut f, 14);
            input.recorded_at = at(15);
            if evaluated {
                let receipt = record_evaluation(&mut f, &input).unwrap();
                assert_eq!(
                    receipt
                        .record
                        .registered_input_decision
                        .unwrap()
                        .verification,
                    lint
                );
            }
            let work = f
                .store
                .get_work_item(f.request.registration.binding.work_id)
                .unwrap();
            let held = f.store.claim_fixture(work.work_id);
            let evidence = [lint.clone(), note];
            checkpoint(
                &mut f.store,
                &work,
                &held,
                "runner",
                "lint-waiver-checkpoint",
                16,
                &evidence,
            );
            let mut request =
                completion_request(&work, &held, "runner", &lint, "lint-waiver-done", 17);
            request.evidence = evidence.to_vec();
            request.source_fingerprint = Some(f.request.input.revision());
            if evaluated {
                request.acceptance.clear();
            }
            let seal = f
                .store
                .complete_work(&request, &DevelopmentNoopRedactor)
                .unwrap_or_else(|error| panic!("{evaluated}/{state}: {error}"));
            assert!(seal.acceptance[0].satisfied);
            let records = f.store.work_run_obligations(held.run_id).unwrap();
            let record = records
                .iter()
                .find(|record| record.obligation.triggering_observation == change)
                .unwrap();
            assert_eq!(record.state, WorkObligationState::Waived);
            let event = record.resolution.as_ref().unwrap();
            assert!(matches!(
                event.resolution,
                WorkObligationResolution::Waived { .. }
            ));
            assert_eq!(
                event.registered_input_decision, None,
                "an untested waiver is not Test credit"
            );
            assert!(
                seal.obligations
                    .iter()
                    .any(|bound| bound.definition == record.definition_id)
            );
            f.store.completion_seal_evaluation(&seal).unwrap();
            std::fs::rename(&root, root.with_file_name("gone-after-lint-seal")).unwrap();
            let report = f.store.verify_all().unwrap();
            assert!(report.is_healthy(), "{evaluated}/{state}: {report:?}");
        }
    }
}

#[test]
fn live_git_currency_bound_lint_consumes_input_without_a_direct_citation() {
    for dirty in [false, true] {
        let mut create = root_request("project-a", "bound-lint-input", 1);
        create.acceptance_bindings = vec![AcceptanceBinding {
            criterion: 1,
            requirement: VerificationRequirement {
                check_kind: VerificationKind::Lint,
                check_fingerprint: None,
            },
        }];
        let mut f =
            fixture_with_create_and_root(SqliteStore::open_in_memory().unwrap(), 0, &create, false);
        attach_root(&mut f);
        let root = PathBuf::from(&f.request.registration.root.workspace_id);
        f.request.checks[0].check_kind = VerificationKind::Lint;
        let lint = f.record().unwrap().verifications[0].record_id.clone();
        let note = quiet_evaluation::note(&mut f);
        let change = unlocated_change_at(&mut f, 14);
        let work = f
            .store
            .get_work_item(f.request.registration.binding.work_id)
            .unwrap();
        let held = f.store.claim_fixture(work.work_id);
        let evidence = [note.clone(), lint.clone()];
        checkpoint(
            &mut f.store,
            &work,
            &held,
            "runner",
            "bound-lint-checkpoint",
            16,
            &evidence,
        );
        let mut request = completion_request(&work, &held, "runner", &note, "bound-lint-done", 17);
        request.evidence = evidence.to_vec();
        request.source_fingerprint = None;
        assert_eq!(
            request.acceptance[0].evidence.as_slice(),
            std::slice::from_ref(&note)
        );
        assert!(
            !f.store
                .acceptance_evaluation_policy()
                .unwrap()
                .is_evaluated()
        );
        if dirty {
            std::fs::write(root.join("tracked"), "edited after lint\n").unwrap();
            let error = f
                .store
                .complete_work(&request, &DevelopmentNoopRedactor)
                .unwrap_err();
            assert!(
                matches!(error, StoreError::WorkCompletionRefused { .. }),
                "{error}"
            );
            assert!(error.to_string().contains("dirty"), "{error}");
            let records = f.store.work_run_obligations(held.run_id).unwrap();
            assert_eq!(
                records
                    .iter()
                    .find(|record| record.obligation.triggering_observation == change)
                    .unwrap()
                    .state,
                WorkObligationState::Open,
                "a refused completion must not persist a stock waiver"
            );
            continue;
        }
        let seal = f
            .store
            .complete_work(&request, &DevelopmentNoopRedactor)
            .unwrap();
        assert!(seal.acceptance[0].evidence.contains(&note));
        assert!(seal.acceptance[0].evidence.contains(&lint));
        let records = f.store.work_run_obligations(held.run_id).unwrap();
        let stock = records
            .iter()
            .find(|record| record.obligation.triggering_observation == change)
            .unwrap();
        assert_eq!(stock.state, WorkObligationState::Waived);
        assert_eq!(
            stock.resolution.as_ref().unwrap().registered_input_decision,
            None
        );
        assert!(
            seal.obligations
                .iter()
                .any(|bound| bound.definition == stock.definition_id)
        );
        std::fs::rename(&root, root.with_file_name("gone-after-bound-lint-seal")).unwrap();
        let report = f.store.verify_all().unwrap();
        assert!(report.is_healthy(), "{report:?}");
    }
}

#[test]
fn live_git_currency_replacements_follow_the_current_binding_definition() {
    for mode in ["introduced", "rewritten", "reintroduced", "pinned"] {
        for fresh in [false, true] {
            let (root, mut f) = live_fixture(mode == "rewritten" || mode == "reintroduced", false);
            let fingerprint = ObjectId::from_canonical_bytes(b"Test fixture check");
            f.request.checks[0].check_fingerprint = fingerprint.clone();
            let old = f.record().unwrap().verifications[0].record_id.clone();
            let first_input = f.request.input.clone();
            let revise = |f: &mut Fixture, bindings, acceptance, key: &str, second| {
                let work = f
                    .store
                    .get_work_item(f.request.registration.binding.work_id)
                    .unwrap();
                let claim = f.store.claim_fixture(work.work_id);
                f.store
                    .revise_work(
                        &crate::domain::ReviseWorkRequest {
                            work_id: work.work_id,
                            expected_revision: work.revision,
                            patch: crate::domain::WorkRevisionPatch {
                                acceptance,
                                acceptance_bindings: Some(bindings),
                                ..crate::domain::WorkRevisionPatch::default()
                            },
                            authority: crate::domain::WorkPlanningAuthority::Claim {
                                run_id: claim.run_id,
                                holder: claim.holder.clone(),
                                claim_id: claim.claim_id,
                                claim_fence: claim.fence,
                            },
                            actor: actor("runner"),
                            idempotency_key: key.into(),
                            updated_at: at(second),
                        },
                        &DevelopmentNoopRedactor,
                    )
                    .unwrap();
            };
            if mode == "reintroduced" {
                revise(&mut f, vec![], None, "remove-binding", 11);
            }
            revise(
                &mut f,
                vec![AcceptanceBinding {
                    criterion: 1,
                    requirement: VerificationRequirement {
                        check_kind: VerificationKind::Test,
                        check_fingerprint: (mode == "pinned").then_some(fingerprint),
                    },
                }],
                (mode == "rewritten").then(|| vec!["the revised behavior is checked".into()]),
                "define-binding",
                12,
            );
            f.request.registration.binding.work_revision = f
                .store
                .get_work_item(f.request.registration.binding.work_id)
                .unwrap()
                .revision;
            std::fs::write(root.join("tracked"), "another checked commit\n").unwrap();
            git(&root, &["add", "tracked"]);
            let second_input = RegisteredVerificationInput::Commit {
                commit: commit(&root),
            };
            f.request.registration.registered_at = at(12);
            f.request.run.started_at = at(12);
            f.request.run.finished_at = at(12);
            f.request.checks[0].started_at = Some(at(12));
            f.request.checks[0].completed_at = Some(at(12));
            another_check(
                &mut f,
                "after-definition-at-b",
                VerificationKind::Test,
                second_input,
            );
            let records = f
                .store
                .work_run_obligations(f.request.registration.binding.run_id)
                .unwrap();
            assert!(
                records
                    .iter()
                    .all(|record| record.state == WorkObligationState::Satisfied)
            );
            let RegisteredVerificationInput::Commit {
                commit: first_commit,
            } = &first_input
            else {
                unreachable!()
            };
            git(&root, &["checkout", "--detach", "-q", first_commit]);
            let checked = if fresh {
                another_check(
                    &mut f,
                    "after-definition-at-a",
                    VerificationKind::Test,
                    first_input.clone(),
                )
            } else {
                old
            };
            let result = complete(
                &mut f,
                &checked,
                &first_input.revision(),
                "binding-definition-done",
            );
            if fresh {
                let seal = result.unwrap();
                assert!(seal.acceptance[0].satisfied);
                assert!(seal.acceptance[0].evidence.contains(&checked));
            } else {
                let error = result.unwrap_err();
                assert!(
                    matches!(error, StoreError::WorkCompletionRefused { .. }),
                    "{mode}: {error}"
                );
                assert!(error.to_string().contains("binding"), "{mode}: {error}");
                assert!(
                    f.store
                        .get_work_run(f.request.registration.binding.run_id)
                        .unwrap()
                        .completion_seal
                        .is_none()
                );
            }
            assert!(f.store.verify_all().unwrap().is_healthy(), "{mode}/{fresh}");
        }
    }
}

#[test]
fn live_git_currency_preserves_operator_post_change_requirements() {
    for evaluated in [false, true] {
        for rule_kind in ["custom", "stock-version", "stock-pinned"] {
            for fresh in [false, true] {
                let (_root, mut f) = live_fixture(false, evaluated);
                let fingerprint = ObjectId::from_canonical_bytes(b"Test fixture check");
                f.request.checks[0].check_fingerprint = fingerprint.clone();
                let rule = BuiltinObligationRuleRef {
                    rule_id: if rule_kind == "custom" {
                        "operator-post-change-test".into()
                    } else {
                        crate::control::SOURCE_CHANGE_RULE_ID.into()
                    },
                    rule_version: if rule_kind == "stock-version" { 2 } else { 1 },
                };
                f.store
                    .set_obligation_rule_set(
                        &ObligationRuleSet {
                            schema_version: OBLIGATION_RULE_SET_SCHEMA_VERSION,
                            rules: vec![ObligationRuleDefinition {
                                rule: rule.clone(),
                                trigger: BuiltinObligationTrigger::SourceChanged,
                                requirement: VerificationRequirement {
                                    check_kind: VerificationKind::Test,
                                    check_fingerprint: (rule_kind == "stock-pinned")
                                        .then_some(fingerprint),
                                },
                            }],
                        },
                        &actor("operator"),
                        "operator-post-change-rule",
                        None,
                        at(5),
                        &DevelopmentNoopRedactor,
                    )
                    .unwrap();
                let earlier = f.record().unwrap().verifications[0].record_id.clone();
                actual_change(&mut f, true, 11);
                let check = if fresh {
                    f.request.registration.registered_at = at(12);
                    f.request.run.started_at = at(12);
                    f.request.run.finished_at = at(12);
                    f.request.checks[0].started_at = Some(at(12));
                    f.request.checks[0].completed_at = Some(at(12));
                    let input = f.request.input.clone();
                    another_check(&mut f, "post-change", VerificationKind::Test, input)
                } else {
                    earlier
                };
                let revision = f.request.input.revision();
                if evaluated {
                    let mut input = evaluation(&f, &check, &revision);
                    input.recorded_at = at(14);
                    record_evaluation(&mut f, &input).unwrap();
                }
                let result = complete(&mut f, &check, &revision, "operator-rule-done");
                if fresh {
                    assert!(result.unwrap().acceptance[0].satisfied);
                } else {
                    let error = result.unwrap_err();
                    assert!(
                        matches!(error, StoreError::OpenWorkObligations { .. }),
                        "{evaluated}/{rule_kind}: {error}"
                    );
                    assert!(
                        f.store
                            .get_work_run(f.request.registration.binding.run_id)
                            .unwrap()
                            .completion_seal
                            .is_none()
                    );
                }
                let records = f
                    .store
                    .work_run_obligations(f.request.registration.binding.run_id)
                    .unwrap();
                let selected = records
                    .iter()
                    .find(|record| record.obligation.rule == rule)
                    .unwrap();
                if fresh {
                    assert_eq!(selected.state, WorkObligationState::Satisfied);
                    assert_eq!(
                        selected
                            .resolution
                            .as_ref()
                            .unwrap()
                            .registered_input_decision,
                        None
                    );
                } else {
                    assert_eq!(selected.state, WorkObligationState::Open);
                    assert!(selected.resolution.is_none());
                }
                let report = f.store.verify_all().unwrap();
                assert!(
                    report.is_healthy(),
                    "{evaluated}/{rule_kind}/{fresh}: {report:?}"
                );
            }
        }
    }
}

#[test]
fn live_git_currency_stock_reports_do_not_gate_current_lint_or_staged_evidence() {
    for state in [
        "older-commit",
        "failed-test",
        "staged-lint",
        "older-generation",
    ] {
        let (root, mut f) = live_fixture(false, true);
        if state == "failed-test" {
            f.request.run.outcome = RegisteredVerificationOutcome::Failed;
            f.request.checks[0].outcome = RegisteredVerificationOutcome::Failed;
        }
        f.record().unwrap();
        let note = quiet_evaluation::note(&mut f);
        actual_change(&mut f, true, 11);
        let input = match state {
            "older-commit" => {
                std::fs::write(root.join("tracked"), "committed correction\n").unwrap();
                git(&root, &["add", "tracked"]);
                RegisteredVerificationInput::Commit {
                    commit: commit(&root),
                }
            }
            "staged-lint" => {
                std::fs::write(root.join("tracked"), "staged correction\n").unwrap();
                git(&root, &["add", "tracked"]);
                RegisteredVerificationInput::StagedTree {
                    tree: GitTreeId::parse(git(&root, &["write-tree"])).unwrap(),
                    base_commit: git(&root, &["rev-parse", "HEAD"]),
                }
            }
            "older-generation" => {
                f.store
                    .bind_named_root(
                        &f.project,
                        &f.session,
                        &f.connection,
                        crate::storage::HostSessionAuthority::Work,
                        f.request.registration.binding.claim_id,
                        f.request.registration.binding.claim_fence,
                        &f.request.registration.root.workspace_id,
                        2,
                        at(11),
                        NamedRootBindingKind::Bound,
                        None,
                        &mut actor("runner"),
                        "new-generation-for-lint",
                        at(11),
                    )
                    .unwrap();
                f.request.registration.root.generation = 2;
                f.request.input.clone()
            }
            _ => f.request.input.clone(),
        };
        f.request.run.outcome = RegisteredVerificationOutcome::Passed;
        f.request.checks[0].outcome = RegisteredVerificationOutcome::Passed;
        f.request.registration.registered_at = at(12);
        f.request.run.started_at = at(12);
        f.request.run.finished_at = at(12);
        f.request.checks[0].started_at = Some(at(12));
        f.request.checks[0].completed_at = Some(at(12));
        let lint = another_check(&mut f, "current-lint", VerificationKind::Lint, input);
        let revision = f.request.input.revision();
        let mut input = quiet_evaluation::judgment(&f, &note);
        input.recorded_at = at(14);
        let evaluated =
            record_evaluation(&mut f, &input).unwrap_or_else(|error| panic!("{state}: {error}"));
        let seal =
            complete_with_evidence(&mut f, &[lint, note], &revision, "lint-with-report-done")
                .unwrap_or_else(|error| panic!("{state}: {error}"));
        assert!(seal.acceptance[0].satisfied);
        assert_eq!(
            f.store.completion_seal_evaluation(&seal).unwrap(),
            Some(evaluated.record)
        );
        let report = f.store.verify_all().unwrap();
        assert!(report.is_healthy(), "{state}: {report:?}");
    }
}

#[test]
fn live_git_currency_mixed_verdicts_select_a_passing_source_witness() {
    let mut create = root_request("project-a", "mixed-verdicts", 1);
    create.acceptance = vec!["tests checked".into(), "lint checked".into()];
    let mut f =
        fixture_with_create_and_root(SqliteStore::open_in_memory().unwrap(), 0, &create, false);
    attach_root(&mut f);
    f.store
        .set_acceptance_evaluation_policy(
            &AcceptanceEvaluationPolicy {
                allowed_modes: vec![AcceptanceEvaluationMode::IndependentSession],
                mechanical_basis: MechanicalBasis::Observed,
                require_source_freshness: true,
            },
            &actor("policy-admin"),
            "mixed-verdict-policy",
            None,
            at(5),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let passed = f.record().unwrap().verifications[0].record_id.clone();
    let checked_input = f.request.input.clone();
    another_check(
        &mut f,
        "passed-lint",
        VerificationKind::Lint,
        checked_input.clone(),
    );
    f.request.run.outcome = RegisteredVerificationOutcome::Failed;
    f.request.checks[0].outcome = RegisteredVerificationOutcome::Failed;
    let failed = another_check(&mut f, "failed-lint", VerificationKind::Lint, checked_input);
    let revision = f.request.input.revision();
    let mut input = evaluation(&f, &passed, &revision);
    input.recorded_at = at(14);
    input.verdicts.push(CriterionVerdictInput {
        criterion: 2,
        verdict: AcceptanceVerdict::Fail,
        basis: AcceptanceBasis::Observed,
        rationale: "lint failed independently of the passing test".into(),
        evidence: vec![failed],
    });
    let recorded = record_evaluation(&mut f, &input).unwrap();
    assert_eq!(
        recorded
            .record
            .registered_input_decision
            .as_ref()
            .unwrap()
            .verification,
        passed
    );
    assert!(!recorded.record.all_pass());
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn live_git_currency_nonpass_keeps_registered_witness_and_legacy_admission_rules() {
    for staged in [false, true] {
        let (root, mut f) = live_fixture(false, true);
        if staged {
            f.request.input = RegisteredVerificationInput::StagedTree {
                tree: GitTreeId::parse(git(&root, &["write-tree"])).unwrap(),
                base_commit: git(&root, &["rev-parse", "HEAD"]),
            };
        }
        let check = f.record().unwrap().verifications[0].record_id.clone();
        let note = quiet_evaluation::note(&mut f);
        let mut input = quiet_evaluation::judgment(&f, &note);
        input.verdicts[0].verdict = AcceptanceVerdict::Fail;
        quiet_evaluation::quiet_sighting(&mut f, "nonpass-after-cut", 11);
        let recorded = record_evaluation(&mut f, &input).unwrap();
        assert_eq!(recorded.record.registered_input_witness, Some(check));
        assert_eq!(recorded.record.registered_input_decision, None);
        assert!(f.store.verify_all().unwrap().is_healthy());
    }
    let (_root, mut legacy) = live_fixture(false, true);
    let note = quiet_evaluation::note(&mut legacy);
    let mut input = quiet_evaluation::judgment(&legacy, &note);
    input.verdicts[0].verdict = AcceptanceVerdict::Fail;
    let before = legacy.snapshot();
    let error = record_evaluation(&mut legacy, &input).unwrap_err();
    assert!(
        matches!(
            error,
            StoreError::AcceptanceEvaluationAdmissionRefused { .. }
        ),
        "{error}"
    );
    assert!(error.to_string().contains("no sighting yet"), "{error}");
    assert_eq!(legacy.snapshot(), before);
}

#[test]
fn live_git_currency_newer_same_kind_staged_check_keeps_dirty_staged_lane() {
    let (root, mut f) = live_fixture(true, true);
    f.record().unwrap();
    actual_change(&mut f, true, 11);
    std::fs::write(root.join("tracked"), "staged correction\n").unwrap();
    git(&root, &["add", "tracked"]);
    let staged = RegisteredVerificationInput::StagedTree {
        tree: GitTreeId::parse(git(&root, &["write-tree"])).unwrap(),
        base_commit: git(&root, &["rev-parse", "HEAD"]),
    };
    f.request.registration.registered_at = at(12);
    f.request.run.started_at = at(12);
    f.request.run.finished_at = at(12);
    f.request.checks[0].started_at = Some(at(12));
    f.request.checks[0].completed_at = Some(at(12));
    let checked = another_check(&mut f, "new-staged-test", VerificationKind::Test, staged);
    let currency = crate::storage::work::registered_commit_currency::LiveCurrency::measure(
        &f.store,
        f.request.registration.binding.work_id,
        f.request.registration.binding.run_id,
    )
    .unwrap();
    let cut = f
        .store
        .work_feed_head(&FeedId::RunExecution(f.request.registration.binding.run_id))
        .unwrap();
    assert!(
        currency
            .select_on(
                &f.store.connection,
                &VerificationRequirement {
                    check_kind: VerificationKind::Test,
                    check_fingerprint: None,
                },
                cut
            )
            .unwrap()
            .is_none(),
        "a newer staged check must avoid the older commit's dirty refusal"
    );
    let revision = f.request.input.revision();
    let mut input = evaluation(&f, &checked, &revision);
    input.recorded_at = at(14);
    record_evaluation(&mut f, &input).unwrap();
    assert!(
        complete(&mut f, &checked, &revision, "same-kind-staged-done")
            .unwrap()
            .acceptance[0]
            .satisfied
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn live_git_currency_late_same_commit_pass_does_not_depend_on_content_sightings() {
    for after_recording in [false, true] {
        let (_root, mut f) = live_fixture(true, true);
        let first = f.record().unwrap().verifications[0].record_id.clone();
        quiet_evaluation::quiet_sighting(&mut f, "content-before-late-check", 11);
        let revision = f.request.input.revision();
        let input = evaluation(&f, &first, &revision);
        if after_recording {
            record_evaluation(&mut f, &input).unwrap();
        }
        let checked_input = f.request.input.clone();
        let late = another_check(
            &mut f,
            "late-same-input",
            VerificationKind::Test,
            checked_input,
        );
        if !after_recording {
            record_evaluation(&mut f, &input).unwrap();
        }
        let shown = f
            .store
            .acceptance_evaluation_status(f.request.registration.binding.work_id, None)
            .unwrap()
            .unwrap();
        assert_eq!(shown.stale, None);
        let seal =
            complete_with_evidence(&mut f, &[first, late], &revision, "late-pass-done").unwrap();
        assert!(seal.acceptance[0].satisfied);
    }
}

#[test]
fn live_git_currency_binding_resolution_is_stale_not_corrupt_and_staged_checks_recover() {
    let mut create = root_request("project-a", "mixed-registered-inputs", 1);
    create.acceptance = vec!["behavior checked".into(), "tests checked".into()];
    create.acceptance_bindings = vec![AcceptanceBinding {
        criterion: 2,
        requirement: VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: None,
        },
    }];
    let mut f =
        fixture_with_create_and_root(SqliteStore::open_in_memory().unwrap(), 0, &create, false);
    attach_root(&mut f);
    f.store
        .set_acceptance_evaluation_policy(
            &AcceptanceEvaluationPolicy {
                allowed_modes: vec![AcceptanceEvaluationMode::IndependentSession],
                mechanical_basis: MechanicalBasis::Observed,
                require_source_freshness: true,
            },
            &actor("policy-admin"),
            "mixed-policy",
            None,
            at(5),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    f.request.checks[0].check_kind = VerificationKind::Lint;
    let first = f.record().unwrap().verifications[0].record_id.clone();
    let note = quiet_evaluation::note(&mut f);
    let revision = f.request.input.revision();
    let mut input = evaluation(&f, &first, &revision);
    input.verdicts.push(CriterionVerdictInput {
        criterion: 2,
        verdict: AcceptanceVerdict::InsufficientEvidence,
        basis: AcceptanceBasis::Judgment,
        rationale: "test is still missing".into(),
        evidence: vec![note.clone()],
    });
    let first_evaluation = record_evaluation(&mut f, &input).unwrap();
    assert!(first_evaluation.record.registered_input_decision.is_some());
    let root = PathBuf::from(&f.request.registration.root.workspace_id);
    let stage = RegisteredVerificationInput::StagedTree {
        tree: GitTreeId::parse(git(&root, &["write-tree"])).unwrap(),
        base_commit: git(&root, &["rev-parse", "HEAD"]),
    };
    let tested = another_check(&mut f, "staged-test", VerificationKind::Test, stage);
    let work_id = f.request.registration.binding.work_id;
    let shown = f
        .store
        .acceptance_evaluation_status(work_id, None)
        .unwrap()
        .unwrap();
    assert_eq!(shown.stale, Some(AcceptanceStaleReason::Mutation));
    let history = f
        .store
        .acceptance_evaluation_entries(work_id)
        .unwrap()
        .unwrap();
    let assessed = f
        .store
        .assess_acceptance_evaluations(
            work_id,
            history.run_id,
            &history.entries,
            &first_evaluation.evaluation,
        )
        .unwrap();
    assert_eq!(assessed.len(), 1);
    let refused = complete(&mut f, &tested, &revision, "needs-new-evaluation").unwrap_err();
    assert!(
        !matches!(refused, StoreError::InvalidWorkProjection(_)),
        "{refused}"
    );
    let mut current = evaluation(&f, &tested, &f.request.input.revision());
    current.verdicts.push(CriterionVerdictInput {
        criterion: 2,
        ..current.verdicts[0].clone()
    });
    current.attempt_key = Some("staged-recovery".into());
    let admitted = record_evaluation(&mut f, &current).unwrap();
    assert_eq!(admitted.record.registered_input_decision, None);
    let stage_revision = f.request.input.revision();
    assert!(
        complete(&mut f, &tested, &stage_revision, "mixed-input-done")
            .unwrap()
            .acceptance
            .iter()
            .all(|value| value.satisfied)
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn live_git_currency_refuses_direct_unbound_citations_with_evaluations_off() {
    for change in ["dirty", "new-commit", "missing"] {
        let (root, mut f) = live_fixture(false, false);
        let check = f.record().unwrap().verifications[0].record_id.clone();
        let revision = f.request.input.revision();
        match change {
            "dirty" => std::fs::write(root.join("tracked"), "edited\n").unwrap(),
            "new-commit" => {
                commit(&root);
            }
            "missing" => std::fs::rename(&root, root.with_file_name("unavailable-source")).unwrap(),
            _ => unreachable!(),
        }
        let error = complete(&mut f, &check, &revision, "direct-off-refusal").unwrap_err();
        assert!(
            matches!(error, StoreError::WorkCompletionRefused { .. }),
            "{error}"
        );
        assert!(
            error.to_string().contains(match change {
                "dirty" => "dirty",
                "missing" => "unavailable",
                _ => "current commit",
            }),
            "{error}"
        );
    }
}

#[test]
fn live_git_currency_bind_faults_keep_the_typed_host_refusal_and_write_nothing() {
    for change in ["dirty", "new-commit", "missing"] {
        let (root, mut f) = live_fixture(false, false);
        let original = f.record().unwrap().verifications[0].record_id.clone();
        let input = VerificationBindInput {
            idempotency_key: "live-bind-fault".into(),
            original,
            measurement: BindMeasurement {
                workspace_id: f.request.registration.root.workspace_id.clone(),
                source_revision: f.request.input.revision(),
                measured_at: at(11),
            },
            targets: Vec::new(),
        };
        match change {
            "dirty" => std::fs::write(root.join("tracked"), "edited\n").unwrap(),
            "new-commit" => {
                commit(&root);
            }
            "missing" => std::fs::rename(&root, root.with_file_name("unavailable-source")).unwrap(),
            _ => unreachable!(),
        }
        let before = f.snapshot();
        let error = f
            .store
            .bind_verification(
                &f.project,
                &f.session,
                &f.connection,
                crate::storage::HostSessionAuthority::Work,
                &actor("runner"),
                &input,
                at(12),
            )
            .unwrap_err();
        let StoreError::VerificationBindRefused(ref refusal) = error else {
            panic!("{error}");
        };
        assert_eq!(
            refusal.original,
            Some(VerificationBindOriginalRefusal::RootNotCurrent)
        );
        assert!(
            refusal.request.is_some(),
            "other request faults remain collected"
        );
        assert_eq!(
            crate::host::store_error_code(&error),
            "verification_bind_refused"
        );
        assert_eq!(f.snapshot(), before);
    }
}

#[test]
fn live_git_currency_service_prose_completion_does_not_read_git() {
    use crate::work_service::{
        LocalWorkService, WorkCompleteInput, WorkCompleteResult, WorkCompletionCaptureInput,
    };
    let database_home = crate::test_support::temp_home().unwrap();
    let database = database_home.path().join("service.sqlite3");
    let create = root_request("project-a", "prose-service", 1);
    let mut f =
        fixture_with_create_and_root(SqliteStore::open(&database).unwrap(), 0, &create, false);
    attach_root(&mut f);
    f.record().unwrap();
    let root = PathBuf::from(&f.request.registration.root.workspace_id);
    assert!(crate::committed_input::read(&root).is_ok());
    let work = f
        .store
        .get_work_item(f.request.registration.binding.work_id)
        .unwrap();
    let service = LocalWorkService::new(
        database,
        f.project.clone(),
        "runner".into(),
        f.session.clone(),
        Some("fixture".into()),
    );
    std::fs::rename(&root, root.with_file_name("unavailable-source")).unwrap();
    let before = crate::committed_input::read_count();
    let result = service
        .work_complete_on(
            Some(&work.short_ref),
            WorkCompleteInput {
                source_fingerprint: None,

                source_id: None,
                landing: None,
                links: Vec::new(),
                link_basis: None,
                capture: Some(WorkCompletionCaptureInput {
                    summary: "prose delivered".into(),
                    refs: Vec::new(),
                }),
                evidence: Vec::new(),
                acceptance: None,
                note: None,
                idempotency_key: "prose-service-done".into(),
            },
            at(14),
        )
        .unwrap();
    assert!(matches!(result, WorkCompleteResult::Completed(_)));
    assert_eq!(crate::committed_input::read_count(), before);
}

#[test]
fn live_git_currency_handoff_and_return_keep_the_original_check_and_generation() {
    let (_root, mut f) = live_fixture(true, true);
    let original_binding = f.request.registration.binding.clone();
    let check = f.record().unwrap().verifications[0].record_id.clone();
    let original = f
        .store
        .get::<VerificationEvidence>(&check)
        .unwrap()
        .unwrap();
    for (from, to, key, second) in [
        ("runner", "second", "away", 11),
        ("second", "runner", "back", 13),
    ] {
        let work = f.store.get_work_item(original_binding.work_id).unwrap();
        let held = f.store.claim_fixture(work.work_id);
        let offer = f
            .store
            .offer_work_handoff(
                &OfferWorkHandoffRequest {
                    work_id: work.work_id,
                    run_id: held.run_id,
                    expected_work_revision: work.revision,
                    from: SessionId(from.into()),
                    to: SessionId(to.into()),
                    claim_id: held.claim_id,
                    claim_fence: held.fence,
                    ttl_seconds: 300,
                    checkpoint_summary: "handoff without releasing the named root".into(),
                    actor: actor(from),
                    idempotency_key: format!("offer-{key}"),
                    offered_at: at(second),
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        f.store
            .accept_work_handoff(
                &AcceptWorkHandoffRequest {
                    work_id: work.work_id,
                    offer_id: offer.offer_id,
                    to: SessionId(to.into()),
                    actor: actor(to),
                    idempotency_key: format!("accept-{key}"),
                    accepted_at: at(second + 1),
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
    }
    let work = f.store.get_work_item(original_binding.work_id).unwrap();
    let held = f.store.claim_fixture(work.work_id);
    assert_eq!(held.claim_id, original_binding.claim_id);
    assert!(held.fence > original_binding.claim_fence);
    let root = f
        .store
        .read_named_root(
            &f.project,
            &f.session,
            &f.connection,
            crate::storage::HostSessionAuthority::Work,
            held.run_id,
            held.claim_id,
        )
        .unwrap();
    assert!(matches!(
        root.named_root,
        NamedRootState::Bound { generation: 1, .. }
    ));
    let revision = f.request.input.revision();
    let mut input = evaluation(&f, &check, &revision);
    input.recorded_at = at(15);
    record_evaluation(&mut f, &input).unwrap();
    checkpoint(
        &mut f.store,
        &work,
        &held,
        "runner",
        "handoff-checkpoint",
        16,
        std::slice::from_ref(&check),
    );
    let mut request = completion_request(&work, &held, "runner", &check, "handoff-done", 17);
    request.acceptance.clear();
    request.source_fingerprint = Some(revision);
    f.store
        .complete_work(&request, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!(
        f.store
            .get::<VerificationEvidence>(&check)
            .unwrap()
            .unwrap(),
        original
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn live_git_currency_accepts_accounted_reports_and_preserves_replay_and_doctor() {
    for bound in [false, true] {
        for before_cut in [false, true] {
            let (root, mut f) = live_fixture(bound, true);
            let receipt = f.record().unwrap();
            let check = receipt.verifications[0].record_id.clone();
            let note = quiet_evaluation::note(&mut f);
            let mut input = if bound {
                evaluation(&f, &check, &f.request.input.revision())
            } else {
                quiet_evaluation::judgment(&f, &note)
            };
            actual_change(&mut f, true, 11);
            quiet_evaluation::quiet_sighting(&mut f, "after-report", 12);
            if before_cut {
                input.evaluated_through = f
                    .store
                    .work_feed_head(&FeedId::RunExecution(f.request.registration.binding.run_id))
                    .unwrap();
            }
            input.recorded_at = at(13);
            let admitted = record_evaluation(&mut f, &input).unwrap();
            assert!(admitted.record.registered_input_decision.is_some());
            assert!(admitted.record.registered_input_witness.is_none());
            let revision = f.request.input.revision();
            let seal =
                complete_with_evidence(&mut f, &[check, note], &revision, "complete-live").unwrap();
            assert!(seal.acceptance[0].satisfied);
            std::fs::rename(&root, root.with_file_name("removed-source")).unwrap();
            let replay = record_evaluation(&mut f, &input).unwrap();
            assert!(replay.replayed);
            assert_eq!(replay.record, admitted.record);
            assert_eq!(
                f.store.completion_seal_evaluation(&seal).unwrap(),
                Some(admitted.record)
            );
            let health = f.store.verify_all().unwrap();
            assert!(health.is_healthy(), "{health:?}");
        }
    }
}

#[test]
fn live_git_currency_refuses_dirty_new_commit_and_missing_root_at_evaluate_and_done() {
    for evaluated in [false, true] {
        for change in ["dirty", "new-commit", "missing"] {
            let (root, mut f) = live_fixture(true, evaluated);
            let receipt = f.record().unwrap();
            let check = receipt.verifications[0].record_id.clone();
            let revision = f.request.input.revision();
            let input = evaluation(&f, &check, &revision);
            if evaluated {
                record_evaluation(&mut f, &input).unwrap();
            }
            match change {
                "dirty" => std::fs::write(root.join("tracked"), "edited\n").unwrap(),
                "new-commit" => {
                    commit(&root);
                }
                "missing" => std::fs::rename(&root, root.with_file_name("removed-source")).unwrap(),
                _ => unreachable!(),
            }
            if evaluated {
                let mut retry = input.clone();
                retry.attempt_key = Some("new-decision".into());
                let error = record_evaluation(&mut f, &retry).unwrap_err().to_string();
                assert!(
                    error.contains(if change == "dirty" {
                        "dirty"
                    } else if change == "missing" {
                        "unavailable"
                    } else {
                        "current commit"
                    }),
                    "evaluated={evaluated}, change={change}: {error}"
                );
            }
            let error = complete(&mut f, &check, &revision, "refuse-live")
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(if change == "dirty" {
                    "dirty"
                } else if change == "missing" {
                    "unavailable"
                } else {
                    "current commit"
                }),
                "evaluated={evaluated}, change={change}: {error}"
            );
            let seals: i64 = f
                .store
                .connection
                .query_row(
                    "SELECT count(*) FROM objects WHERE object_kind='completion_seal'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(seals, 0);
        }
    }
}

#[test]
fn live_git_currency_accepts_an_edit_reverted_to_the_checked_commit() {
    let (root, mut f) = live_fixture(true, true);
    let receipt = f.record().unwrap();
    let check = receipt.verifications[0].record_id.clone();
    std::fs::write(root.join("tracked"), "edited\n").unwrap();
    actual_change(&mut f, true, 11);
    git(&root, &["restore", "tracked"]);
    let revision = f.request.input.revision();
    let input = evaluation(&f, &check, &revision);
    record_evaluation(&mut f, &input).unwrap();
    assert!(
        complete(&mut f, &check, &revision, "reverted-live")
            .unwrap()
            .acceptance[0]
            .satisfied
    );
    let health = f.store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
}

#[test]
fn live_git_currency_requires_a_commit_declaration_for_a_prose_evaluation() {
    for declared in [None, Some("content-v1:earlier-capture")] {
        let (_root, mut f) = live_fixture(false, true);
        f.record().unwrap();
        let note = quiet_evaluation::note(&mut f);
        let mut input = quiet_evaluation::judgment(&f, &note);
        input.source_basis = declared.map(|fingerprint| {
            AcceptanceSourceBasis::legacy(
                Some(f.request.registration.root.workspace_id.clone()),
                fingerprint.into(),
            )
        });
        let before = f.snapshot();
        let error = record_evaluation(&mut f, &input).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("declare the intended git-commit"),
            "{error}"
        );
        assert_eq!(before, f.snapshot());
    }
}

#[test]
fn live_git_currency_does_not_fall_back_after_a_failed_run_without_an_executed_check() {
    let (_root, mut f) = live_fixture(false, true);
    f.request.run.outcome = RegisteredVerificationOutcome::Failed;
    f.request.checks[0].outcome = RegisteredVerificationOutcome::NotRun;
    f.request.checks[0].started_at = None;
    f.request.checks[0].completed_at = None;
    assert_eq!(
        f.record().unwrap().verifications,
        [] as [crate::domain::RegisteredVerificationRecord; 0]
    );
    let note = quiet_evaluation::note(&mut f);
    let input = quiet_evaluation::judgment(&f, &note);
    let error = record_evaluation(&mut f, &input).unwrap_err();
    assert!(error.to_string().contains("no executed check"), "{error}");
}

#[test]
fn live_git_currency_does_not_gate_unevaluated_prose_without_a_check_citation() {
    let (root, mut f) = live_fixture(false, false);
    f.record().unwrap();
    let note = quiet_evaluation::note(&mut f);
    std::fs::rename(&root, root.with_file_name("unavailable-source")).unwrap();
    let revision = f.request.input.revision();
    let seal = complete(&mut f, &note, &revision, "prose-without-check").unwrap();
    assert!(seal.acceptance[0].satisfied);
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn live_git_currency_reuses_original_evidence_after_same_generation_recovery() {
    let (_root, mut f) = live_fixture(true, true);
    let original = f.request.registration.binding.clone();
    let receipt = f.record().unwrap();
    let check = receipt.verifications[0].record_id.clone();
    let work = f.store.get_work_item(original.work_id).unwrap();
    let recovered = claim(
        &mut f.store,
        &work,
        "runner",
        "recover-original-check",
        4000,
        3600,
    );
    assert_eq!(recovered.claim_id, original.claim_id);
    assert!(recovered.fence > original.claim_fence);
    let revision = f.request.input.revision();
    let mut input = evaluation(&f, &check, &revision);
    input.recorded_at = at(4002);
    let record = record_evaluation(&mut f, &input).unwrap();
    assert!(record.record.registered_input_decision.is_some());
    let work = f.store.get_work_item(original.work_id).unwrap();
    checkpoint(
        &mut f.store,
        &work,
        &recovered,
        "runner",
        "recovered-checkpoint",
        4003,
        std::slice::from_ref(&check),
    );
    let mut request = completion_request(
        &work,
        &recovered,
        "runner",
        &check,
        "recovered-completion",
        4004,
    );
    request.acceptance.clear();
    request.source_fingerprint = Some(revision);
    f.store
        .complete_work(&request, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!(
        f.store.load_verification_evidence(&check).unwrap().binding,
        original
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn live_git_currency_new_generation_requires_a_new_registered_check() {
    let (_root, mut f) = live_fixture(true, true);
    let check = f.record().unwrap().verifications[0].record_id.clone();
    let binding = f.request.registration.binding.clone();
    f.store
        .bind_named_root(
            &f.project,
            &f.session,
            &f.connection,
            crate::storage::HostSessionAuthority::Work,
            binding.claim_id,
            binding.claim_fence,
            &f.request.registration.root.workspace_id,
            2,
            at(11),
            NamedRootBindingKind::Bound,
            None,
            &mut actor("runner"),
            "rename-generation",
            at(11),
        )
        .unwrap();
    let input = evaluation(&f, &check, &f.request.input.revision());
    let error = record_evaluation(&mut f, &input).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("root is unavailable or has been rebound"),
        "{error}"
    );
    f.request.operation_key = "termal-run:test-renamed".into();
    f.request.registration.host_run_id = "test-renamed".into();
    f.request.registration.root.generation = 2;
    f.request.registration.registered_at = at(12);
    f.request.run.started_at = at(12);
    f.request.run.finished_at = at(13);
    f.request.checks[0].started_at = Some(at(12));
    f.request.checks[0].completed_at = Some(at(13));
    let receipt = f
        .store
        .record_registered_verification(
            &f.project,
            &f.session,
            &f.connection,
            crate::storage::HostSessionAuthority::Work,
            &actor("runner"),
            &f.request,
            at(14),
        )
        .unwrap();
    let check = receipt.verifications[0].record_id.clone();
    let mut input = evaluation(&f, &check, &f.request.input.revision());
    input.recorded_at = at(15);
    record_evaluation(&mut f, &input).unwrap();
    let work = f.store.get_work_item(binding.work_id).unwrap();
    let held = f.store.claim_fixture(work.work_id);
    checkpoint(
        &mut f.store,
        &work,
        &held,
        "runner",
        "renamed-checkpoint",
        16,
        std::slice::from_ref(&check),
    );
    let mut request = completion_request(&work, &held, "runner", &check, "renamed-completion", 17);
    request.acceptance.clear();
    request.source_fingerprint = Some(f.request.input.revision());
    f.store
        .complete_work(&request, &DevelopmentNoopRedactor)
        .unwrap();
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn live_git_currency_explicit_release_then_recover_does_not_restore_the_root() {
    let (_root, mut f) = live_fixture(true, true);
    let original = f.request.registration.binding.clone();
    let check = f.record().unwrap().verifications[0].record_id.clone();
    let revision = f.request.input.revision();
    let mut input = evaluation(&f, &check, &revision);
    input.recorded_at = at(11);
    let historical = record_evaluation(&mut f, &input).unwrap();
    let work = f.store.get_work_item(original.work_id).unwrap();
    let held = f.store.claim_fixture(work.work_id);
    f.store
        .release_work(
            &ReleaseWorkRequest {
                work_id: work.work_id,
                run_id: held.run_id,
                expected_work_revision: work.revision,
                holder: held.holder.clone(),
                claim_id: held.claim_id,
                claim_fence: held.fence,
                reason: "release before recovering".into(),
                waiver_reason: Some("fixture has no contribution".into()),
                actor: actor("runner"),
                idempotency_key: "explicit-release".into(),
                released_at: at(12),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let work = f.store.get_work_item(original.work_id).unwrap();
    let recovered = claim(
        &mut f.store,
        &work,
        "runner",
        "claim-after-release",
        13,
        3600,
    );
    assert_eq!(recovered.claim_id, original.claim_id);
    assert!(recovered.fence > original.claim_fence);
    let root = f
        .store
        .read_named_root(
            &f.project,
            &f.session,
            &f.connection,
            crate::storage::HostSessionAuthority::Work,
            recovered.run_id,
            recovered.claim_id,
        )
        .unwrap();
    assert!(
        matches!(
            root.named_root,
            NamedRootState::UnboundByRelease {
                last_generation: 1,
                ..
            }
        ),
        "{root:?}"
    );
    let mut retry = evaluation(&f, &check, &revision);
    retry.attempt_key = Some("after-explicit-release".into());
    retry.recorded_at = at(14);
    let error = record_evaluation(&mut f, &retry).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("root is unavailable or has been rebound"),
        "{error}"
    );
    checkpoint(
        &mut f.store,
        &work,
        &recovered,
        "runner",
        "released-checkpoint",
        15,
        std::slice::from_ref(&check),
    );
    let mut request = completion_request(
        &work,
        &recovered,
        "runner",
        &check,
        "released-completion",
        16,
    );
    request.acceptance.clear();
    request.source_fingerprint = Some(revision);
    let error = f
        .store
        .complete_work(&request, &DevelopmentNoopRedactor)
        .unwrap_err();
    assert!(
        matches!(
            error,
            StoreError::WorkCompletionRefused { .. }
                | StoreError::WorkCompletionRecoveryRequired { .. }
        ),
        "{error}"
    );
    assert!(
        f.store
            .get_work_run(recovered.run_id)
            .unwrap()
            .completion_seal
            .is_none()
    );
    assert_eq!(
        record_evaluation(&mut f, &input).unwrap().record,
        historical.record
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn live_git_currency_validates_saved_decisions_without_rediscovery() {
    let (root, mut f) = live_fixture(false, true);
    let check = f.record().unwrap().verifications[0].record_id.clone();
    let note = quiet_evaluation::note(&mut f);
    let input = quiet_evaluation::judgment(&f, &note);
    let recorded = record_evaluation(&mut f, &input).unwrap();
    for damage in ["clean", "check", "root", "input"] {
        let mut bad = recorded.record.clone();
        let proof = bad.registered_input_decision.as_mut().unwrap();
        match damage {
            "clean" => {
                let RegisteredDecisionSource::Legacy { clean, .. } = &mut proof.source else {
                    panic!("legacy decision")
                };
                *clean = false;
            }
            "check" => proof.verification = note.clone(),
            "root" => proof.named_root_binding = note.clone(),
            "input" => {
                proof.source = RegisteredDecisionSource::Legacy {
                    source_revision: format!("git-commit:{}", object_name("another input")),
                    clean: true,
                };
            }
            _ => unreachable!(),
        }
        assert!(
            crate::storage::work::acceptance_evaluation::validate_evaluation_input_witness_on(
                &f.store.connection,
                &bad
            )
            .is_err(),
            "{damage}"
        );
    }
    let revision = f.request.input.revision();
    let seal =
        complete_with_evidence(&mut f, &[check, note], &revision, "proof-completion").unwrap();
    std::fs::rename(&root, root.with_file_name("gone-after-seal")).unwrap();
    assert_eq!(
        f.store.completion_seal_evaluation(&seal).unwrap(),
        Some(recorded.record)
    );
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let export = directory.path().join("export.jsonl");
    let target = directory.path().join("imported.db");
    let objects = |store: &SqliteStore| -> Vec<(String, Vec<u8>)> {
        store
            .connection
            .prepare("SELECT object_id, canonical_json FROM objects ORDER BY object_id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let before = objects(&f.store);
    let schema = crate::storage::running_schema_reference().unwrap();
    f.store
        .connection
        .execute("VACUUM INTO ?1", [source.to_str().unwrap()])
        .unwrap();
    SqliteStore::readiness(&source, None).unwrap();
    crate::storage::migration::export_json(&source, &export).unwrap();
    crate::storage::migration::import_json(&export, &target).unwrap();
    let imported = SqliteStore::open(&target).unwrap();
    assert_eq!(objects(&imported), before);
    assert_eq!(
        crate::storage::store_schema_reference(&target).unwrap(),
        schema
    );
    assert!(imported.verify_all().unwrap().is_healthy());
    SqliteStore::readiness(&target, None).unwrap();
}
