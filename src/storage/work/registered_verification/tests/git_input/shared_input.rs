//! Shared endpoint identity, with the ordinary run and named-root fences.

use super::*;
use crate::source::{SourceId, Submodule};
use std::path::Path;

fn observed(root: &Path) -> SourceId {
    let observation = crate::source::inspect(root, crate::source::MAX_BUDGET);
    observation.source_id.expect("fixture source identity")
}

fn shared_fixture(bound: bool, dirty: bool) -> (std::path::PathBuf, Fixture, SourceId) {
    let (root, mut f) = live_currency::live_fixture(bound, false);
    if dirty {
        std::fs::write(root.join("tracked"), "local change\n").unwrap();
    }
    let source_id = observed(&root);
    f.request.input = RegisteredVerificationInput::SourceId {
        source_id: source_id.clone(),
    };
    f.request.post_run = RegisteredVerificationPostRun::SourceId {
        source_id: source_id.clone(),
    };
    (root, f, source_id)
}

fn finish(f: &mut Fixture, check: &ObjectId) -> Result<CompletionSeal, StoreError> {
    let work = f
        .store
        .get_work_item(f.request.registration.binding.work_id)
        .unwrap();
    let held = f.store.claim_fixture(work.work_id);
    checkpoint(
        &mut f.store,
        &work,
        &held,
        "runner",
        "shared-checkpoint",
        13,
        std::slice::from_ref(check),
    );
    let mut request = completion_request(&work, &held, "runner", check, "shared-done", 14);
    request.source_fingerprint = None;
    f.store.complete_work(&request, &DevelopmentNoopRedactor)
}

#[test]
fn shared_input_equal_clean_and_dirty_complete_without_observations() {
    for bound in [false, true] {
        for dirty in [false, true] {
            let (root, mut f, source_id) = shared_fixture(bound, dirty);
            assert_eq!(source_id.changes.is_empty(), !dirty);
            let receipt = f.record().unwrap();
            let check = receipt.verifications[0].record_id.clone();
            let evidence = f.store.load_verification_evidence(&check).unwrap();
            assert_eq!(evidence.source_basis.source_id(), Some(&source_id));
            assert_eq!(evidence.source_basis.legacy_revision(), None);
            let mapped: Option<String> = f
                .store
                .connection
                .query_row(
                    "SELECT source_revision FROM work_run_evidence WHERE evidence_id=?1",
                    [check.as_str()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(mapped, None);
            let seal = finish(&mut f, &check).unwrap();
            assert!(seal.acceptance[0].satisfied);
            // Historical validation must not rediscover the current filesystem.
            std::fs::write(root.join("tracked"), "later source\n").unwrap();
            assert!(f.store.verify_all().unwrap().is_healthy());
            let observations: i64 = f.store.connection.query_row(
                "SELECT count(*) FROM objects WHERE object_kind IN ('execution_observation','source_observation','turn_grant')", [], |row| row.get(0),
            ).unwrap();
            assert_eq!(observations, 0);
        }
    }
}

#[test]
fn shared_input_live_change_refuses_even_when_commit_is_unchanged() {
    for dirty in [false, true] {
        let (root, mut f, source_id) = shared_fixture(true, dirty);
        let check = f.record().unwrap().verifications[0].record_id.clone();
        std::fs::write(root.join("tracked"), "a different local change\n").unwrap();
        let changed = observed(&root);
        assert_eq!(source_id.commit, changed.commit);
        assert_ne!(source_id, changed);
        let error = finish(&mut f, &check).unwrap_err();
        assert!(error.to_string().contains("source_id changed"), "{error}");
    }
}

#[test]
fn shared_input_wire_is_closed_and_requires_matching_representation() {
    let (_, mut f, _) = shared_fixture(false, true);
    let wire = serde_json::to_value(&f.request).unwrap();
    let parsed: RegisteredVerificationRequest = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(parsed, f.request);
    for pointer in ["/input/source_id", "/post_run/source_id"] {
        let mut bad = wire.clone();
        *bad.pointer_mut(pointer).unwrap() = serde_json::Value::Null;
        assert!(serde_json::from_value::<RegisteredVerificationRequest>(bad).is_err());
    }
    for extra in ["clean_before", "outcome", "unknown"] {
        let mut bad = wire.clone();
        bad["post_run"][extra] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<RegisteredVerificationRequest>(bad).is_err(),
            "{extra}"
        );
    }
    let snapshot = f.snapshot();
    f.request.post_run = LegacyRegisteredVerificationPostRun {
        outcome: RegisteredVerificationInputOutcome::Clean,
        clean_before: true,
        clean_after: true,
        dirty_paths: vec![],
        disposition: None,
    }
    .into();
    assert!(
        f.record()
            .unwrap_err()
            .to_string()
            .contains("representation")
    );
    assert_eq!(f.snapshot(), snapshot);
}

#[test]
fn shared_input_admission_compares_every_identity_component() {
    let (_, mut f, before) = shared_fixture(false, true);
    for field in ["commit", "tree", "blob", "path", "mode", "submodule"] {
        let mut after = before.clone();
        match field {
            "commit" => after.commit = before.tree.clone(),
            "tree" => after.tree = before.commit.clone(),
            "blob" => after.changes[0].blob = Some(before.tree.clone()),
            "path" => after.changes[0].path.push_str("-other"),
            "mode" => after.changes[0].mode = Some(crate::source::Mode::Executable),
            "submodule" => after.submodules.push(Submodule {
                path: "child".into(),
                commit: before.commit.clone(),
                changes: vec![],
            }),
            _ => unreachable!(),
        }
        assert_ne!(before, after, "{field}");
        f.request.post_run = RegisteredVerificationPostRun::SourceId { source_id: after };
        let snapshot = f.snapshot();
        assert!(
            f.record()
                .unwrap_err()
                .to_string()
                .contains("changed during"),
            "{field}"
        );
        assert_eq!(f.snapshot(), snapshot);
    }
    let mut invalid = before.clone();
    invalid.changes[0].path = "../outside".into();
    f.request.input = RegisteredVerificationInput::SourceId {
        source_id: invalid.clone(),
    };
    f.request.post_run = RegisteredVerificationPostRun::SourceId { source_id: invalid };
    let snapshot = f.snapshot();
    assert!(
        f.record()
            .unwrap_err()
            .to_string()
            .contains("invalid shared source identity")
    );
    assert_eq!(f.snapshot(), snapshot);
}

#[test]
fn shared_input_failed_aggregate_never_gains_credit() {
    let (_, mut f, _) = shared_fixture(true, true);
    f.request.run.outcome = RegisteredVerificationOutcome::Failed;
    let check = f.record().unwrap().verifications[0].record_id.clone();
    assert_eq!(
        f.store.load_verification_evidence(&check).unwrap().result,
        VerificationResult::Passed
    );
    assert!(finish(&mut f, &check).is_err());
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn shared_input_dirty_check_stays_current_after_a_report_without_source_change() {
    let (root, mut f, source_id) = shared_fixture(true, true);
    let check = f.record().unwrap().verifications[0].record_id.clone();
    live_currency::unlocated_change(&mut f);
    assert_eq!(observed(&root), source_id);
    assert!(finish(&mut f, &check).unwrap().acceptance[0].satisfied);
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn shared_input_import_preserves_canonical_record_bytes_and_links() {
    let (_, mut f, _) = shared_fixture(true, true);
    let receipt = f.record().unwrap();
    finish(&mut f, &receipt.verifications[0].record_id).unwrap();
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
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let export = directory.path().join("export.jsonl");
    let target = directory.path().join("imported.db");
    f.store
        .connection
        .execute("VACUUM INTO ?1", [source.to_str().unwrap()])
        .unwrap();
    crate::storage::migration::export_json(&source, &export).unwrap();
    crate::storage::migration::import_json(&export, &target).unwrap();
    let imported = SqliteStore::open(&target).unwrap();
    assert_eq!(objects(&imported), before);
    assert!(imported.verify_all().unwrap().is_healthy());
    assert_eq!(
        crate::storage::store_schema_reference(&target).unwrap(),
        crate::storage::running_schema_reference().unwrap()
    );
}

#[test]
fn shared_input_completion_refuses_string_declarations_for_cited_and_selected_checks() {
    for implicit in [false, true] {
        let (_, mut f, source_id) = shared_fixture(implicit, true);
        let check = f.record().unwrap().verifications[0].record_id.clone();
        let citation = if implicit {
            quiet_evaluation::note(&mut f)
        } else {
            check.clone()
        };
        let work = f
            .store
            .get_work_item(f.request.registration.binding.work_id)
            .unwrap();
        let held = f.store.claim_fixture(work.work_id);
        let evidence = if implicit {
            vec![check.clone(), citation.clone()]
        } else {
            vec![check.clone()]
        };
        checkpoint(
            &mut f.store,
            &work,
            &held,
            "runner",
            "declaration-checkpoint",
            13,
            &evidence,
        );
        let mut request = completion_request(&work, &held, "runner", &citation, "declare", 14);
        request.evidence = evidence;
        for declaration in [
            "content-v1:unrelated".to_owned(),
            format!("git-commit:{}", source_id.commit),
        ] {
            request.source_fingerprint = Some(declaration);
            let before = f.snapshot();
            let error = f
                .store
                .complete_work(&request, &DevelopmentNoopRedactor)
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("source_fingerprint cannot declare a source_id input"),
                "{error}"
            );
            assert_eq!(f.snapshot(), before);
        }
        request.source_fingerprint = None;
        assert!(
            f.store
                .complete_work(&request, &DevelopmentNoopRedactor)
                .unwrap()
                .acceptance[0]
                .satisfied
        );
    }
}

#[test]
fn shared_input_unadmitted_reports_do_not_gate_equal_source() {
    for operator in [false, true] {
        for unadmitted in [false, true] {
            let (root, mut f, _) = shared_fixture(false, true);
            if operator {
                f.store
                    .set_obligation_rule_set(
                        &ObligationRuleSet {
                            schema_version: OBLIGATION_RULE_SET_SCHEMA_VERSION,
                            rules: vec![ObligationRuleDefinition {
                                rule: BuiltinObligationRuleRef {
                                    rule_id: "operator-post-change".into(),
                                    rule_version: 1,
                                },
                                trigger: BuiltinObligationTrigger::SourceChanged,
                                requirement: VerificationRequirement {
                                    check_kind: VerificationKind::Test,
                                    check_fingerprint: None,
                                },
                            }],
                        },
                        &actor("operator"),
                        "operator-rule",
                        None,
                        at(5),
                        &DevelopmentNoopRedactor,
                    )
                    .unwrap();
            }
            // The report arrives after the check finished. Shared equality
            // decides currency; an unadmitted report adds no ordering gate.
            if !unadmitted {
                std::fs::write(root.join("tracked"), "later untested content\n").unwrap();
            }
            actual_change(&mut f, unadmitted, 10);
            let receipt = f.record().unwrap();
            let obligations = f
                .store
                .work_run_obligations(f.request.registration.binding.run_id)
                .unwrap();
            assert_eq!(obligations.len(), 1);
            assert!(obligations.iter().all(|record| record.state
                == if unadmitted {
                    WorkObligationState::Satisfied
                } else {
                    WorkObligationState::Open
                }));
            if unadmitted {
                finish(&mut f, &receipt.verifications[0].record_id).unwrap();
            }
            assert!(f.store.verify_all().unwrap().is_healthy());
        }
    }
}

#[test]
fn shared_input_large_identity_keeps_agent_note_windows_and_details_bounded() {
    let (_, mut f, mut source_id) = shared_fixture(false, true);
    // Display coverage needs a large valid wire object, not hundreds of Git
    // subprocesses. All blob references derive from the actual fixture.
    let change = source_id.changes[0].clone();
    source_id.changes = (0..180)
        .map(|index| {
            let mut entry = change.clone();
            entry.path = format!("file-{index:03}-{}", "path".repeat(12));
            entry
        })
        .collect();
    source_id.validate().unwrap();
    assert!(serde_json::to_vec(&source_id).unwrap().len() > 12 * 1024);
    f.request.input = RegisteredVerificationInput::SourceId {
        source_id: source_id.clone(),
    };
    f.request.post_run = RegisteredVerificationPostRun::SourceId {
        source_id: source_id.clone(),
    };
    f.request.checks[0].summary = Some("large check summary ".repeat(200).trim_end().to_owned());
    let record = f.record().unwrap().verifications[0].record_id.clone();
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("notes.db");
    f.store
        .connection
        .execute("VACUUM INTO ?1", [database.to_str().unwrap()])
        .unwrap();
    let work = f
        .store
        .get_work_item(f.request.registration.binding.work_id)
        .unwrap();
    let verbs = crate::verbs::AgentVerbs::new(
        database,
        f.project.clone(),
        "runner".into(),
        f.session.clone(),
        None,
    );
    for detail in [false, true] {
        let receipt = verbs
            .show_records(
                &work.short_ref,
                &crate::verbs::ShowInput {
                    notes: !detail,
                    note: detail.then(|| record.as_str().to_owned()),
                    ..crate::verbs::ShowInput::default()
                },
                at(12),
            )
            .unwrap();
        let row = if detail {
            &receipt.value["note"]
        } else {
            receipt.value["notes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["locator"].as_str() == Some(record.as_str()))
                .unwrap()
        };
        let facts = &row["verification"];
        assert!(facts.get("source_id").is_none());
        assert_eq!(facts["source_id_summary"]["commit"], source_id.commit);
        assert_eq!(
            facts["source_id_summary"]["changes"],
            source_id.changes.len()
        );
        assert!(receipt.value.to_string().len() < 12 * 1024);
        assert!(receipt.text().len() < 12 * 1024);
    }
    assert_eq!(
        f.store
            .load_verification_evidence(&record)
            .unwrap()
            .source_basis
            .source_id(),
        Some(&source_id)
    );
}

#[test]
fn shared_input_binding_refusal_names_the_unsupported_representation() {
    let (_, mut f, _) = shared_fixture(false, true);
    let check = f.record().unwrap().verifications[0].record_id.clone();
    let request = VerificationBindInput {
        idempotency_key: "unsupported-shared-bind".into(),
        original: check,
        measurement: BindMeasurement {
            workspace_id: f.request.registration.root.workspace_id.clone(),
            source_revision: "content-v1:cannot-declare-shared".into(),
            measured_at: at(11),
        },
        targets: vec![],
    };
    let before = f.snapshot();
    let error = f
        .store
        .bind_verification(
            &f.project,
            &f.session,
            &f.connection,
            crate::storage::HostSessionAuthority::Work,
            &actor("runner"),
            &request,
            at(11),
        )
        .unwrap_err();
    assert!(
        matches!(error, StoreError::VerificationBindRefused(ref refused)
        if refused.original == Some(VerificationBindOriginalRefusal::SourceIdUnsupported)),
        "{error}"
    );
    assert_eq!(f.snapshot(), before);
}

#[test]
fn shared_and_legacy_bases_roundtrip_without_surrogates_or_rewrites() {
    let (_, mut f, source_id) = shared_fixture(false, true);
    let receipt = f.record().unwrap();
    let evidence = f
        .store
        .load_verification_evidence(&receipt.verifications[0].record_id)
        .unwrap();
    let shared = serde_json::to_value(&evidence.source_basis).unwrap();
    assert_eq!(
        shared["source_id"],
        serde_json::to_value(source_id).unwrap()
    );
    for absent in ["source_revision", "git_tree", "clean"] {
        assert!(shared.get(absent).is_none());
    }
    let old = ExecutionSourceBasis {
        workspace_id: "fixture-workspace".into(),
        source_revision: "content-v1:historical".into(),
        git_tree: None,
        source_root_generation: Some(1),
        source_root_state: Some(SourceRootState::Named),
    };
    let original = crate::CanonicalObject::freeze(&old).unwrap();
    let decoded: VerificationSourceBasis =
        serde_json::from_value(serde_json::to_value(&old).unwrap()).unwrap();
    assert_eq!(
        crate::CanonicalObject::freeze(&decoded).unwrap().bytes(),
        original.bytes()
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
}
