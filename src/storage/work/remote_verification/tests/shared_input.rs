//! Full shared endpoints, through the same remote authority and evidence path.

use super::*;
use crate::source::{Change, Mode, SourceId, Submodule};

fn source(dirty: bool) -> SourceId {
    SourceId {
        commit: tested_commit(),
        tree: tested_tree().as_str().to_owned(),
        submodules: vec![],
        changes: if dirty { vec![change()] } else { vec![] },
    }
}

fn change() -> Change {
    Change {
        path: "tracked".into(),
        status: " M".into(),
        origin: None,
        blob: Some(object_name("local blob")),
        mode: Some(Mode::Regular),
    }
}

fn endpoints(before: Option<SourceId>, after: Option<SourceId>) -> RemoteRunInput {
    RemoteRunInput::SourceId(Box::new(RemoteSourceIdInput::new(before, after)))
}

fn equal(f: &mut Fixture, source: &SourceId) {
    f.request.completion.input = endpoints(Some(source.clone()), Some(source.clone()));
}

#[test]
fn remote_shared_equal_clean_and_dirty_qualify_for_both_purposes() {
    for dirty in [false, true] {
        for intended in [
            committed(RemoteVerificationPurpose::Verification),
            committed(RemoteVerificationPurpose::LandingGate),
            test_ref(),
        ] {
            let mut f = fixture();
            let source = source(dirty);
            f.request.association.intended_use = intended;
            equal(&mut f, &source);
            let receipt = f.record().unwrap();
            assert!(receipt.qualification.credited, "{receipt:?}");
            let evidence = f
                .store
                .load_verification_evidence(&receipt.verifications[0].record_id)
                .unwrap();
            assert_eq!(evidence.source_basis.source_id(), Some(&source));
            assert_eq!(evidence.source_basis.legacy_revision(), None);
            let mapped: Option<String> = f
                .store
                .connection
                .query_row(
                    "SELECT source_revision FROM work_run_evidence WHERE evidence_id=?1",
                    [receipt.verifications[0].record_id.as_str()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(mapped, None);
            assert_eq!(f.record().unwrap(), receipt, "the exact retry replays");
            f.assert_healthy();
        }
    }
}

#[test]
fn remote_shared_pass_credits_focused_runs_optional_exits_and_remote_clock_skew() {
    for dirty in [false, true] {
        for intended in [
            committed(RemoteVerificationPurpose::Verification),
            committed(RemoteVerificationPurpose::LandingGate),
            test_ref(),
        ] {
            for exit in [None, Some(0), Some(1)] {
                let mut f = fixture();
                f.request.association.intended_use = intended.clone();
                f.request.completion.mode = RemoteRunMode::Focused;
                f.request.completion.result.exit = exit;
                f.request.completion.finished_at = Some(at(11));
                let source = source(dirty);
                equal(&mut f, &source);
                let credited = f.record_at(10).unwrap();
                assert!(credited.qualification.credited, "{credited:?}");
                assert!(credited.origin.is_some());
                assert_eq!(credited.verifications.len(), 1);
                let evidence = f
                    .store
                    .load_verification_evidence(&credited.verifications[0].record_id)
                    .unwrap();
                assert_eq!(evidence.source_basis.source_id(), Some(&source));
                assert_eq!(evidence.completed_at, at(11));
                assert_eq!(evidence.recorded_at, at(10));
                assert_eq!(f.record_at(12).unwrap(), credited);
                f.assert_healthy();

                let mut failed = fixture();
                failed.request.association.intended_use = intended.clone();
                failed.request.completion.mode = RemoteRunMode::Focused;
                failed.request.completion.result.exit = exit;
                failed.request.completion.result.verdict = RemoteRunVerdict::Fail;
                failed.request.completion.finished_at = Some(at(11));
                equal(&mut failed, &source);
                let receipt = failed.record_at(10).unwrap();
                assert!(!receipt.qualification.credited);
                assert_eq!(
                    receipt.qualification.reason,
                    Some(RemoteVerificationReason::ResultNotPass)
                );
                assert_eq!(receipt.verifications, []);
                failed.assert_healthy();
            }
        }
    }
}

#[test]
fn remote_shared_compares_every_endpoint_component() {
    let mut before = source(true);
    before.changes[0].status = "R ".into();
    before.changes[0].origin = Some("original".into());
    before.submodules.push(Submodule {
        path: "child".into(),
        commit: object_name("child commit"),
        changes: vec![change()],
    });
    let mut variants = Vec::new();
    let mut add_case = |edit: fn(&mut SourceId)| {
        let mut value = before.clone();
        edit(&mut value);
        variants.push(value);
    };
    add_case(|s| s.commit = object_name("another commit"));
    add_case(|s| s.tree = object_name("another tree"));
    add_case(|s| s.changes[0].path = "other".into());
    add_case(|s| s.changes[0].status = " R".into());
    add_case(|s| s.changes[0].origin = Some("another original".into()));
    add_case(|s| s.changes[0].blob = Some(object_name("another blob")));
    add_case(|s| s.changes[0].mode = Some(Mode::Executable));
    add_case(|s| s.submodules[0].path = "another child".into());
    add_case(|s| s.submodules[0].commit = object_name("another child commit"));
    add_case(|s| s.submodules[0].changes[0].blob = Some(object_name("another child blob")));
    add_case(|s| s.submodules[0].changes[0].mode = Some(Mode::Symlink));
    add_case(|s| s.submodules[0].changes.clear());
    add_case(|s| s.changes.clear());
    for after in variants {
        before.validate().unwrap();
        after.validate().unwrap();
        for purpose in [
            RemoteVerificationPurpose::Verification,
            RemoteVerificationPurpose::LandingGate,
        ] {
            let mut f = fixture();
            f.request.association.intended_use = committed(purpose);
            f.request.completion.input = endpoints(Some(before.clone()), Some(after.clone()));
            f.assert_uncredited(RemoteVerificationReason::SourceIdMissingOrChanged);
        }
    }
}

#[test]
fn remote_shared_null_endpoints_are_reports_without_credit() {
    for (before, after) in [
        (None, None),
        (Some(source(false)), None),
        (None, Some(source(false))),
    ] {
        let mut f = fixture();
        f.request.completion.input = endpoints(before, after);
        let wire = serde_json::to_value(&f.request).unwrap();
        assert!(
            wire["completion"]["input"]
                .get("source_id_before")
                .is_some()
        );
        assert!(wire["completion"]["input"].get("source_id_after").is_some());
        let parsed: RemoteVerificationRequest = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, f.request);
        f.assert_uncredited(RemoteVerificationReason::SourceIdMissingOrChanged);
    }
}

#[test]
fn remote_shared_wire_refuses_missing_mixed_and_malformed_endpoints() {
    let mut f = fixture();
    equal(&mut f, &source(true));
    let wire = serde_json::to_value(&f.request).unwrap();
    for key in ["kind", "source_id_before", "source_id_after"] {
        let mut bad = wire.clone();
        bad["completion"]["input"]
            .as_object_mut()
            .unwrap()
            .remove(key);
        assert!(
            serde_json::from_value::<RemoteVerificationRequest>(bad).is_err(),
            "{key}"
        );
    }
    for extra in [
        "commit",
        "tree",
        "before_fingerprint",
        "after_fingerprint",
        "source_id",
        "unknown",
    ] {
        let mut bad = wire.clone();
        bad["completion"]["input"][extra] = serde_json::json!("unexpected");
        assert!(
            serde_json::from_value::<RemoteVerificationRequest>(bad).is_err(),
            "{extra}"
        );
    }
    for value in [serde_json::json!("unknown"), serde_json::Value::Null] {
        let mut bad = wire.clone();
        bad["completion"]["input"]["kind"] = value;
        assert!(serde_json::from_value::<RemoteVerificationRequest>(bad).is_err());
    }
    let mut bad = wire.clone();
    bad["completion"]["input"]["source_id_after"]["changes"][0]["extra"] = serde_json::json!(true);
    assert!(serde_json::from_value::<RemoteVerificationRequest>(bad).is_err());
    let mut bad = wire;
    bad["completion"]["input"]["source_id_before"]["commit"] = serde_json::json!("invalid");
    f.request = serde_json::from_value(bad).unwrap();
    // Stored shape reading is separate from the policy for new objects.
    assert!(f.request.validate().is_ok());
    assert_eq!(
        f.request.validate_new().unwrap_err().0,
        "input.source_id_before"
    );
    let original = f.snapshot();
    assert!(f.record().is_err());
    assert_eq!(f.snapshot(), original);
}

#[test]
fn remote_shared_keeps_result_association_and_root_guards() {
    type Edit = fn(&mut RemoteVerificationRequest);
    let cases: [(Edit, RemoteVerificationReason); 5] = [
        (
            |r| r.completion.result.verdict = RemoteRunVerdict::Fail,
            RemoteVerificationReason::ResultNotPass,
        ),
        (
            |r| r.completion.result.terminal = false,
            RemoteVerificationReason::ResultNotPass,
        ),
        (
            |r| r.completion.finished_at = None,
            RemoteVerificationReason::ResultNotPass,
        ),
        (
            |r| r.association.root.generation += 1,
            RemoteVerificationReason::AssociationStale,
        ),
        (
            |r| r.association.binding.claim_fence += 1,
            RemoteVerificationReason::AssociationStale,
        ),
    ];
    for (edit, reason) in cases {
        let mut f = fixture();
        equal(&mut f, &source(true));
        edit(&mut f.request);
        f.assert_uncredited(reason);
    }
    let mut f = fixture();
    equal(&mut f, &source(true));
    f.request.association.intended_use = RemoteVerificationUse::CommittedInput {
        expected_commit: object_name("wrong commit"),
        purpose: RemoteVerificationPurpose::Verification,
    };
    f.assert_uncredited(RemoteVerificationReason::CommitMismatch);
    let mut f = fixture();
    equal(&mut f, &source(true));
    f.request.association.intended_use = test_ref();
    let RemoteVerificationUse::AuthorizedTestRef { expected_tree, .. } =
        &mut f.request.association.intended_use
    else {
        unreachable!()
    };
    *expected_tree = GitTreeId::parse(object_name("wrong tree")).unwrap();
    f.assert_uncredited(RemoteVerificationReason::TreeMismatch);
}

fn finish(f: &mut Fixture, evidence: &ObjectId) -> Result<CompletionSeal, StoreError> {
    let work = f
        .store
        .get_work_item(f.request.association.binding.work_id)
        .unwrap();
    let held = f.store.claim_fixture(work.work_id);
    checkpoint(
        &mut f.store,
        &work,
        &held,
        "runner",
        "shared-remote-checkpoint",
        13,
        std::slice::from_ref(evidence),
    );
    let mut request =
        completion_request(&work, &held, "runner", evidence, "shared-remote-done", 14);
    request.source_fingerprint = None;
    f.store.complete_work(&request, &DevelopmentNoopRedactor)
}

#[test]
fn remote_shared_live_currency_allows_equal_dirty_and_refuses_changed_source() {
    for bound in [false, true] {
        for dirty in [false, true] {
            for changed in [false, true] {
                let mut f = downstream::unevaluated(bound);
                let _home = downstream::real_git_root(
                    &mut f,
                    committed(RemoteVerificationPurpose::Verification),
                );
                let root = std::path::PathBuf::from(&f.request.association.root.workspace_id);
                if dirty {
                    std::fs::write(root.join("tracked"), "before local delta\n").unwrap();
                }
                let source = crate::source::inspect(&root, crate::source::MAX_BUDGET)
                    .source_id
                    .unwrap();
                equal(&mut f, &source);
                let receipt = f.record().unwrap();
                assert!(receipt.qualification.credited);
                let check = &receipt.verifications[0].record_id;
                if changed {
                    std::fs::write(root.join("tracked"), "after different delta\n").unwrap();
                }
                let result = finish(&mut f, check);
                if changed {
                    let error = result.unwrap_err();
                    assert!(error.to_string().contains("source_id changed"), "{error}");
                } else {
                    assert!(result.unwrap().acceptance[0].satisfied);
                }
                f.assert_healthy();
            }
        }
    }
}

#[test]
fn remote_shared_and_legacy_import_keep_canonical_bytes_ids_and_decisions() {
    let directory = crate::test_support::temp_home().unwrap();
    let source_path = directory.path().join("source.db");
    let target = directory.path().join("target.db");
    let export = directory.path().join("store.jsonl");
    let store = SqliteStore::open_with_host_path_policy(
        &source_path,
        crate::HostPathPolicy::host_default(),
    )
    .unwrap();
    let mut f = fixture_in(store, 0, None);
    let legacy_input = serde_json::to_value(&f.request.completion.input).unwrap();
    assert_eq!(
        legacy_input,
        serde_json::json!({"commit":tested_commit(),"tree":tested_tree(),"before_fingerprint":"f".repeat(64),"after_fingerprint":"f".repeat(64)})
    );
    f.record().unwrap();
    f.request.operation_key = "remote:shared".into();
    f.request.completion.run_id = "test-shared".into();
    equal(&mut f, &source(true));
    let receipt = f.record().unwrap();
    let bytes = |store: &SqliteStore| -> Vec<(String, Vec<u8>)> {
        let mut statement = store
            .connection
            .prepare("SELECT object_id,canonical_json FROM objects ORDER BY object_id")
            .unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let original = bytes(&f.store);
    drop(f.store);
    crate::storage::migration::export_json(&source_path, &export).unwrap();
    crate::storage::migration::import_json(&export, &target).unwrap();
    f.store =
        SqliteStore::open_with_host_path_policy(&target, crate::HostPathPolicy::host_default())
            .unwrap();
    assert_eq!(bytes(&f.store), original);
    f.connection = f.store.resume_work_connection(&f.session, at(11)).unwrap();
    assert_eq!(f.record_at(12).unwrap(), receipt);
    f.assert_healthy();
}
