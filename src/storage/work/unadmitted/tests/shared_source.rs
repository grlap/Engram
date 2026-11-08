use super::*;
use crate::domain::{VerificationSourceBasis, VerificationSourceIdentity};
use crate::source::{Change, Mode, SourceId};

// Shared-input behavior exercises current ingress, not the historical seeder.
fn observe(
    f: &mut Fixture,
    input: ExecutionObserveInput,
    second: i64,
) -> Result<ExecutionObservationReceipt, StoreError> {
    observe_current(f, input, second)
}

pub(super) fn source(blob: char) -> SourceId {
    SourceId {
        commit: "a".repeat(40),
        tree: "b".repeat(40),
        submodules: Vec::new(),
        changes: vec![Change {
            path: "src/lib.rs".into(),
            status: " M".into(),
            origin: None,
            blob: Some(blob.to_string().repeat(40)),
            mode: Some(Mode::Regular),
        }],
    }
}

fn basis(source_id: SourceId) -> VerificationSourceBasis {
    VerificationSourceBasis {
        workspace_id: "workspace-A".into(),
        identity: VerificationSourceIdentity::SourceId { source_id },
        source_root_generation: None,
        source_root_state: None,
    }
}

fn check_scope() -> crate::domain::ObservedCheckSourceBasis {
    crate::domain::ObservedCheckSourceBasis {
        workspace_id: "workspace-A".into(),
        source_revision: None,
        git_tree: None,
        source_root_generation: None,
        source_root_state: None,
    }
}

pub(super) fn change(
    f: &Fixture,
    key: &str,
    before: SourceId,
    after: SourceId,
) -> ExecutionObserveInput {
    let mut input = accounting::accounted(f, key, "unused-before", "unused-after");
    input.occurrence = ObservedOccurrence::InterTurnChange {
        source_change: ObservedSourceChange::ContentComparison {
            workspace_id: "workspace-A".into(),
            baseline: MeasuredBaseline {
                workspace_id: "workspace-A".into(),
                identity: VerificationSourceIdentity::SourceId { source_id: before },
                observed_at: at(4),
            },
            sighting: MeasuredSighting {
                source_basis: basis(after),
                observed_at: at(5),
            },
        },
    };
    input
}

#[test]
fn new_legacy_sources_are_refused_but_retained_records_replay_and_import_unchanged() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("source.db");
    let exported = directory.path().join("export.json");
    let imported = directory.path().join("imported.db");
    let mut f = fixture_on(SqliteStore::open(&database).unwrap());
    let legacy_basis = VerificationSourceBasis::from(ExecutionSourceBasis {
        workspace_id: "workspace-A".into(),
        source_revision: "content-v1:retained".into(),
        git_tree: None,
        source_root_generation: None,
        source_root_state: None,
    });
    let mut cases = Vec::new();
    for turn in [false, true] {
        for (label, source_change) in [
            (
                "comparison",
                content_change("content-v1:before", "content-v1:after"),
            ),
            (
                "assumed",
                ObservedSourceChange::AssumedMissingBaseline {
                    workspace_id: "workspace-A".into(),
                    sighting: MeasuredSighting {
                        source_basis: legacy_basis.clone(),
                        observed_at: at(5),
                    },
                },
            ),
            (
                "watcher",
                ObservedSourceChange::WatcherOnly {
                    workspace_id: "workspace-A".into(),
                    observed_at: at(5),
                },
            ),
        ] {
            let mut input = inter_turn_change(&f, &format!("{label}-{turn}"));
            input.occurrence = if turn {
                ObservedOccurrence::UnadmittedTurn {
                    host_turn_ref: "turn".into(),
                    outcome: None,
                    source_change: Some(source_change),
                    observed_checks: vec![],
                }
            } else {
                ObservedOccurrence::InterTurnChange { source_change }
            };
            cases.push((
                input,
                if label == "watcher" {
                    "source_change.detection"
                } else if label == "comparison" {
                    "source_change.baseline.source_revision"
                } else {
                    "source_change.sighting.source_basis.source_revision"
                },
            ));
        }
    }
    let mut quiet = inter_turn_change(&f, "legacy-quiet");
    quiet.occurrence = ObservedOccurrence::SourceSighting {
        source_basis: legacy_basis,
        observed_at: at(5),
    };
    cases.push((quiet, "source_sighting.source_basis.source_revision"));
    for turn in [false, true] {
        let mut check = passed_check("private-caller-id");
        check.source_basis = Some(
            ExecutionSourceBasis {
                workspace_id: "workspace-A".into(),
                source_revision: "content-v1:retained".into(),
                git_tree: None,
                source_root_generation: None,
                source_root_state: None,
            }
            .into(),
        );
        let mut input = inter_turn_change(&f, &format!("legacy-check-{turn}"));
        input.occurrence = if turn {
            ObservedOccurrence::UnadmittedTurn {
                host_turn_ref: "turn".into(),
                outcome: None,
                source_change: None,
                observed_checks: vec![check],
            }
        } else {
            ObservedOccurrence::ObservedCheck {
                host_turn_ref: "turn".into(),
                check,
            }
        };
        cases.push((input, "observed_checks[0].source_basis"));
    }
    let mut retained = Vec::new();
    for (input, field) in cases {
        let before = footprint(&f.store);
        let error = observe(&mut f, input.clone(), 7).unwrap_err();
        assert!(
            matches!(error, StoreError::ExecutionObservationInvalid(ref reason)
            if reason.contains(field) && reason.contains("retired")
                && !reason.contains("private-caller-id") && !reason.contains("content-v1:retained")),
            "{error:?}"
        );
        assert_eq!(footprint(&f.store), before);
        // Seed the exact old record using its historical validator, never current ingress.
        let receipt = super::observe(&mut f, input.clone(), 7).unwrap();
        let record = stored(&f.store, &receipt.observation);
        record.validate_recorded_shape().unwrap();
        let bytes = crate::canonical::canonical_bytes(&record).unwrap();
        let before = footprint(&f.store);
        assert_eq!(observe(&mut f, input.clone(), 8).unwrap(), receipt);
        assert_eq!(footprint(&f.store), before);
        retained.push((input, receipt, bytes));
    }
    let report = f.store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
    // Shared measurements, scope-only checks and genuine absence still enter normally.
    let input = change(&f, "shared-after-cutoff", source('c'), source('d'));
    observe(&mut f, input, 8).unwrap();
    drop(f.store);
    crate::storage::migration::export_json(&database, &exported).unwrap();
    crate::storage::migration::import_json(&exported, &imported).unwrap();
    f.store = SqliteStore::open(&imported).unwrap();
    f.host.connection_token = f
        .store
        .resume_work_connection(&f.host.session_id, at(9))
        .unwrap();
    for (input, receipt, bytes) in retained {
        assert_eq!(
            crate::canonical::canonical_bytes(&stored(&f.store, &receipt.observation)).unwrap(),
            bytes
        );
        let before = footprint(&f.store);
        assert_eq!(observe(&mut f, input.clone(), 10).unwrap(), receipt);
        assert_eq!(footprint(&f.store), before);
        let mut fresh = input;
        fresh.idempotency_key.push_str("-new");
        assert!(matches!(
            observe(&mut f, fresh, 10),
            Err(StoreError::ExecutionObservationInvalid(_))
        ));
        assert_eq!(footprint(&f.store), before);
    }
    let report = f.store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
}

#[test]
fn shared_observations_compare_the_complete_dirty_source_and_keep_history() {
    let mut f = fixture();
    let legacy = accounting::accounted(&f, "legacy", "rev-a", "rev-b");
    let old = super::observe(&mut f, legacy.clone(), 7).unwrap();
    let old_bytes: Vec<u8> = f
        .store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id=?1",
            [old.observation.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    let before = footprint(&f.store);
    let equal = change(&f, "equal", source('c'), source('c'));
    assert!(observe(&mut f, equal, 8).is_err());
    assert_eq!(footprint(&f.store), before);

    let input = change(&f, "shared", source('c'), source('d'));
    let first = observe(&mut f, input.clone(), 8).unwrap();
    assert!(accounting::self_anchored(&first));
    assert_eq!(
        stored(&f.store, &first.observation).occurrence.reported(),
        input.occurrence
    );
    let repeated = change(&f, "repeat", source('c'), source('d'));
    let repeated = observe(&mut f, repeated, 9).unwrap();
    assert!(matches!(
        repeated.accounting,
        ObservationAccounting::Repeat { .. }
    ));

    // A quiet intervening measurement prevents a later report from hiding a move back.
    let mut quiet = change(&f, "quiet", source('c'), source('e'));
    quiet.occurrence = ObservedOccurrence::SourceSighting {
        source_basis: basis(source('e')),
        observed_at: at(5),
    };
    observe(&mut f, quiet, 10).unwrap();
    let returned = change(&f, "returned", source('c'), source('d'));
    assert!(accounting::self_anchored(
        &observe(&mut f, returned, 11).unwrap()
    ));
    assert_eq!(observe(&mut f, legacy, 12).unwrap(), old);
    let retained: Vec<u8> = f
        .store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id=?1",
            [old.observation.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, old_bytes);
    assert_eq!(
        count(
            &f.store,
            "SELECT COUNT(*) FROM objects WHERE object_kind='verification_evidence'"
        ),
        0
    );
    let report = f.store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
}

#[test]
fn shared_observation_ingress_is_closed_and_rolls_back_malformed_sources() {
    let mut f = fixture();
    let input = change(&f, "closed", source('c'), source('d'));
    let value = serde_json::to_value(&input).unwrap();
    for (pointer, replacement) in [
        (
            "/occurrence/source_change/baseline/source_id",
            serde_json::Value::Null,
        ),
        (
            "/occurrence/source_change/sighting/source_basis/source_id",
            serde_json::Value::Null,
        ),
    ] {
        let mut altered = value.clone();
        *altered.pointer_mut(pointer).unwrap() = replacement;
        assert!(serde_json::from_value::<ExecutionObserveInput>(altered).is_err());
    }
    let mut mixed = value;
    mixed["occurrence"]["source_change"]["baseline"]["source_revision"] = "content-v1:old".into();
    assert!(serde_json::from_value::<ExecutionObserveInput>(mixed).is_err());
    let before = footprint(&f.store);
    let mut malformed = source('d');
    malformed.changes[0].path = "../outside".into();
    let input = change(&f, "malformed", source('c'), malformed);
    assert!(observe(&mut f, input, 8).is_err());
    assert_eq!(footprint(&f.store), before);

    // Both individually valid representations must agree before any write.
    for shared_baseline in [false, true] {
        let mut input = change(&f, "mixed-pair", source('c'), source('d'));
        let ObservedOccurrence::InterTurnChange {
            source_change:
                ObservedSourceChange::ContentComparison {
                    baseline, sighting, ..
                },
        } = &mut input.occurrence
        else {
            unreachable!()
        };
        let legacy = ExecutionSourceBasis {
            workspace_id: "workspace-A".into(),
            source_revision: "content-v1:old".into(),
            git_tree: None,
            source_root_generation: None,
            source_root_state: None,
        };
        if shared_baseline {
            sighting.source_basis = legacy.into();
        } else {
            baseline.identity = VerificationSourceBasis::from(legacy).identity;
        }
        assert!(observe(&mut f, input, 8).is_err());
        assert_eq!(footprint(&f.store), before);
    }
    let mut input = change(&f, "tree-without-revision", source('c'), source('d'));
    let mut check = passed_check("invalid-scope");
    check.source_basis = Some(crate::domain::ObservedCheckSourceBasis {
        git_tree: Some(crate::domain::GitTreeId::parse("e".repeat(40)).unwrap()),
        ..check_scope()
    });
    input.occurrence = ObservedOccurrence::ObservedCheck {
        host_turn_ref: "turn".into(),
        check,
    };
    assert!(observe(&mut f, input, 8).is_err());
    assert_eq!(footprint(&f.store), before);
}

#[test]
fn a_shared_quiet_sighting_does_not_let_legacy_verification_complete_the_work() {
    let mut request = root_request("project-a", "legacy-after-shared", 1);
    request.acceptance_bindings = vec![crate::domain::AcceptanceBinding {
        criterion: 1,
        requirement: crate::domain::VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: None,
        },
    }];
    let mut f = fixture_of(SqliteStore::open_in_memory().unwrap(), &request);
    let legacy = accounting::accounted(&f, "legacy-change", "rev-before", "rev-checked");
    super::observe(&mut f, legacy, 7).unwrap();
    host_verification_from_basis(
        &mut f.store,
        &f.work,
        &f.claim,
        "runner",
        "legacy-check",
        VerificationKind::Test,
        VerificationResult::Passed,
        8,
        ExecutionSourceBasis {
            workspace_id: "workspace-A".into(),
            source_revision: "rev-checked".into(),
            git_tree: None,
            source_root_generation: None,
            source_root_state: None,
        },
    );
    assert!(
        accounting::obligations(&f)
            .iter()
            .all(|row| row.state == crate::domain::WorkObligationState::Satisfied)
    );
    let mut quiet = change(&f, "shared-quiet", source('c'), source('d'));
    quiet.observed_interval = ObservedInterval {
        from: at(9),
        through: at(9),
    };
    quiet.occurrence = ObservedOccurrence::SourceSighting {
        source_basis: basis(source('d')),
        observed_at: at(9),
    };
    let sighting = observe(&mut f, quiet, 9).unwrap();
    let note = evidence(&mut f.store, &f.work, &f.claim, "runner", "note", 10);
    checkpoint(
        &mut f.store,
        &f.work,
        &f.claim,
        "runner",
        "checkpoint",
        10,
        std::slice::from_ref(&note),
    );
    let before = footprint(&f.store);
    let error = complete(&mut f.store, &f.work, &f.claim, "runner", &note, "done", 11).unwrap_err();
    assert!(
        matches!(error, StoreError::WorkBoundVerificationRefused { ref cause, .. }
        if cause.mismatch == crate::domain::VerificationEvidenceMismatch::StaleSourceRevision
            && cause.stale_source.as_ref().is_some_and(|stale|
                stale.decider == crate::domain::StaleSourceDecider::MeasuredSighting
                && stale.position == sighting.position.position
                && stale.revision.is_none())),
        "{error:?}"
    );
    assert_eq!(footprint(&f.store), before);
    let report = f.store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
}

#[test]
fn producer_samples_record_named_shared_sources_and_explicit_unavailability() {
    // Producer sample shapes use real fixture bindings and minted record IDs.
    let before = serde_json::json!({
        "commit":"1".repeat(40), "tree":"2".repeat(40),
        "submodules":[{"path":"dep", "commit":"3".repeat(40), "changes":[]}], "changes":[]
    });
    let mut after = before.clone();
    after["changes"] = serde_json::json!([{
        "path":"src/a.rs", "status":" M", "origin":null,
        "blob":"4".repeat(40), "mode":"100644"
    }]);
    let measured = serde_json::json!({
        "workspace_id":"workspace-A", "source_id":after,
        "source_root_generation":2, "source_root_state":"named"
    });
    let check_context = serde_json::json!({"workspace_id":"workspace-A", "source_root_generation":2, "source_root_state":"named"});
    let changed = serde_json::json!({
        "kind":"unadmitted_turn", "host_turn_ref":"turn-example", "outcome":"succeeded",
        "source_change":{
            "detection":"content_comparison", "workspace_id":"workspace-A",
            "baseline":{"workspace_id":"workspace-A", "source_id":before, "observed_at":at(4)},
            "sighting":{"source_basis":measured, "observed_at":at(5)}
        },
        "observed_checks":[{
            "host_check_id":"check-example", "check_kind":"test", "observed_result":"passed",
            "started_at":at(4), "finished_at":at(5), "observed_at":at(5),
            "source_basis":check_context, "host_evidence_ref":"termal-run:example"
        }]
    });
    let quiet =
        serde_json::json!({"kind":"source_sighting", "source_basis":measured, "observed_at":at(5)});
    let unavailable = serde_json::json!({
        "kind":"unadmitted_turn", "host_turn_ref":"turn-unavailable", "source_change":null,
        "observed_checks":[{
            "host_check_id":"check-unavailable", "check_kind":"test", "observed_result":"indeterminate",
            "started_at":at(4), "observed_at":at(5)
        }]
    });
    let mut duplicated = changed.clone();
    duplicated["observed_checks"][0]["source_basis"]["source_id"] = after;
    assert!(serde_json::from_value::<ObservedOccurrence>(duplicated).is_err());
    for (index, occurrence) in [changed, quiet, unavailable].into_iter().enumerate() {
        let mut f = fixture();
        refusals::name_root(&mut f, 1, crate::domain::NamedRootBindingKind::Bound, 3, 3);
        let named =
            refusals::name_root(&mut f, 2, crate::domain::NamedRootBindingKind::Bound, 4, 4);
        let mut input = refusals::sighting_under(
            &f,
            "producer-sample",
            NamedRootState::Bound {
                workspace_id: "workspace-A".into(),
                generation: 2,
                named_at: at(4),
            },
            named.event,
            (2, crate::domain::SourceRootState::Named),
        );
        input.policy_basis = accounting::account(&f);
        let mut wire = serde_json::to_value(input).unwrap();
        wire["occurrence"] = occurrence.clone();
        let input: ExecutionObserveInput = serde_json::from_value(wire).unwrap();
        let receipt = observe(&mut f, input.clone(), 7).unwrap();
        let record = stored(&f.store, &receipt.observation);
        assert_eq!(
            serde_json::to_value(record.occurrence.reported()).unwrap(),
            occurrence
        );
        assert_eq!(observe(&mut f, input, 8).unwrap(), receipt);
        assert!(
            receipt
                .observed_checks
                .iter()
                .all(|check| check.credit == ObservedCheckCredit::Uncredited)
        );
        match index {
            0 => assert!(accounting::self_anchored(&receipt)),
            1 => assert_eq!(receipt.accounting, ObservationAccounting::Sighting {}),
            _ => assert_eq!(receipt.accounting, ObservationAccounting::NoSourceChange {}),
        }
        let report = f.store.verify_all().unwrap();
        assert!(report.is_healthy(), "{report:?}");
    }
}

#[test]
fn large_shared_observations_remain_readable_in_bounded_agent_windows() {
    let directory = crate::test_support::temp_home().unwrap();
    let path = directory.path().join("engram.db");
    let mut f = fixture_on(SqliteStore::open(&path).unwrap());
    let mut identity = source('c');
    identity.changes = (0..128)
        .map(|index| Change {
            path: format!("src/file-{index:04}.rs"),
            ..identity.changes[0].clone()
        })
        .collect();
    assert!(
        serde_json::to_vec(&identity).unwrap().len()
            > crate::work_service::MAX_AGENT_WORK_RESPONSE_BYTES
    );
    let mut input = change(&f, "large-source", source('d'), identity.clone());
    let mut check = passed_check("reported-check");
    check.source_basis = Some(check_scope());
    let source_change = match &input.occurrence {
        ObservedOccurrence::InterTurnChange { source_change } => Some(source_change.clone()),
        _ => unreachable!("change fixture"),
    };
    input.occurrence = ObservedOccurrence::UnadmittedTurn {
        host_turn_ref: "large-turn".into(),
        outcome: None,
        source_change,
        observed_checks: vec![check],
    };
    let receipt = observe(&mut f, input, 7).unwrap();
    let record = stored(&f.store, &receipt.observation);
    assert_eq!(
        record
            .occurrence
            .measured_sighting()
            .unwrap()
            .source_basis
            .source_id(),
        Some(&identity)
    );
    let words = crate::verbs::AgentVerbs::new(
        path,
        f.work.project_id.clone(),
        "peer".into(),
        SessionId("peer".into()),
        None,
    );
    let shown = words
        .show_records(
            &f.work.short_ref,
            &crate::verbs::ShowInput {
                observations: true,
                ..Default::default()
            },
            at(8),
        )
        .unwrap();
    let rows = shown.value["observations"]
        .as_array()
        .or_else(|| shown.value["rows"].as_array())
        .unwrap();
    let row = &rows[0];
    assert_eq!(row["source_id_summary"]["commit"], identity.commit);
    assert_eq!(row["source_id_summary"]["changes"], identity.changes.len());
    assert_eq!(
        row["unadmitted"]["checks"][0]["source_workspace"],
        "workspace-A"
    );
    assert!(row["revision"].is_null());
    assert!(
        serde_json::to_vec(&shown.value).unwrap().len()
            <= crate::work_service::MAX_AGENT_WORK_RESPONSE_BYTES
    );
}
