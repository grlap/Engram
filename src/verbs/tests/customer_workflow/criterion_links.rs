use super::*;
use crate::work_service::{WorkCriterionLinkInput, WorkUpdateInput};

mod corrections;

fn setup(verbs: &AgentVerbs) -> String {
    let added = verbs
        .add(
            AddInput {
                title: "Explicit evidence links".into(),
                acceptance: vec![
                    "First outcome".into(),
                    "Second outcome".into(),
                    "Third outcome".into(),
                ],
                ..Default::default()
            },
            at(0),
        )
        .unwrap();
    let reference = added.value["work"]["short_ref"]
        .as_str()
        .unwrap()
        .to_owned();
    verbs
        .claim(
            ClaimInput {
                work_ref: reference.clone(),
                ttl_seconds: Some(3600),
                recover: None,
            },
            at(1),
        )
        .unwrap();
    reference
}

fn basis(verbs: &AgentVerbs, reference: &str) -> i64 {
    verbs.show(reference, at(4)).unwrap().value["acceptance_basis"]
        .as_i64()
        .unwrap()
}

fn input(reference: &str, basis: i64, criterion: usize, locator: &str) -> DoneInput {
    DoneInput {
        work_ref: Some(reference.into()),
        summary: Some("Delivered outcomes".into()),
        links: vec![WorkCriterionLinkInput {
            criterion,
            locator: locator.into(),
        }],
        link_basis: Some(basis),
        ..Default::default()
    }
}

fn locators(verbs: &AgentVerbs, reference: &str) -> Vec<String> {
    verbs
        .show_records(
            reference,
            &ShowInput {
                notes: true,
                gates: true,
                ..Default::default()
            },
            at(4),
        )
        .unwrap()
        .value["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["locator"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn criterion_links_reuse_holder_note_and_gate_with_frozen_readback_and_replay() {
    let (_home, verbs, path, project) = fixture();
    let reference = setup(&verbs);
    note(
        &verbs,
        &reference,
        "First outcome measured against original artifact",
        2,
    );
    verbs
        .gate(
            GateInput {
                work_ref: Some(reference.clone()),
                name: "third-check".into(),
                failed: vec![],
                evidence_ref: Some("opaque-artifact".into()),
            },
            at(3),
        )
        .unwrap();
    let ids = locators(&verbs, &reference);
    assert_eq!(ids.len(), 2);
    let mut request = input(&reference, basis(&verbs, &reference), 1, &ids[0][..12]);
    request.links.push(WorkCriterionLinkInput {
        criterion: 3,
        locator: ids[1].clone(),
    });
    let done = verbs.done(request.clone(), at(5)).unwrap();
    assert!(!done.owed);
    let facts = &done.value["acceptance_evidence"];
    assert_eq!(facts["unlinked_positions"], json!([2]));
    assert_eq!(facts["link_count"], 2);
    assert_eq!(facts["links_omitted"].as_u64().unwrap_or(0), 0);
    assert_eq!(facts["links"][0]["locator"], ids[0]);
    assert_eq!(facts["links"][1]["locator"], ids[1]);
    assert!(
        done.text()
            .contains("author-linked evidence; not verification")
    );
    assert!(done.text().contains("First outcome measured"));
    assert!(done.text().contains("gate third-check: passed"));
    assert_eq!(
        verbs.show(&reference, at(6)).unwrap().value["acceptance_evidence"],
        *facts
    );
    assert_eq!(
        verbs.done(request, at(6)).unwrap().value["acceptance_evidence"],
        *facts
    );
    let store = SqliteStore::open(path).unwrap();
    let work = store.resolve_work_ref(&project, &reference).unwrap();
    let run = store.latest_work_run(work.work_id).unwrap().unwrap();
    let seal: crate::CompletionSeal = store
        .get(run.completion_seal.as_ref().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(seal.acceptance[0].evidence[0].as_str(), ids[0]);
    assert!(seal.acceptance[1].evidence.is_empty());
    assert_eq!(seal.acceptance[2].evidence[0].as_str(), ids[1]);
    let sealed_objects = store.work_run_evidence(run.run_id).unwrap();
    assert_eq!(
        sealed_objects.len(),
        3,
        "only the optional done summary is a new capture"
    );
    let error = verbs
        .done(input(&reference, work.revision, 2, &ids[0]), at(7))
        .unwrap_err();
    assert!(
        matches!(error.error, StoreError::WorkCriterionLinkInvalid { reason, .. } if reason.contains("frozen"))
    );
    assert_eq!(store.work_run_evidence(run.run_id).unwrap(), sealed_objects);
    note(&verbs, &reference, "Late finding cannot amend the seal", 8);
    let after_late_note = store.work_run_evidence(run.run_id).unwrap();
    assert_eq!(after_late_note.len(), sealed_objects.len() + 1);
    assert!(
        sealed_objects
            .iter()
            .all(|hash| after_late_note.contains(hash))
    );
    let late = locators(&verbs, &reference).pop().unwrap();
    let error = verbs
        .done(input(&reference, work.revision, 2, &late), at(9))
        .unwrap_err();
    assert!(
        matches!(error.error, StoreError::WorkCriterionLinkInvalid { reason, .. }
        if reason.contains("frozen"))
    );
    assert_eq!(
        store.work_run_evidence(run.run_id).unwrap(),
        after_late_note
    );
    assert_eq!(
        store
            .get::<crate::CompletionSeal>(run.completion_seal.as_ref().unwrap())
            .unwrap()
            .unwrap(),
        seal
    );
}

#[test]
fn criterion_links_shape_refusals_precede_any_protocol_write() {
    let (_home, verbs, path, project) = fixture();
    let reference = setup(&verbs);
    let store = SqliteStore::open(&path).unwrap();
    let work = store.resolve_work_ref(&project, &reference).unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    for (links, acceptance, reason) in [
        (Vec::new(), None, "only meaningful with explicit links"),
        (
            vec![WorkCriterionLinkInput {
                criterion: 1,
                locator: "not-resolved".into(),
            }],
            Some(Vec::new()),
            "explicit acceptance and positional links cannot be combined",
        ),
    ] {
        let error = verbs
            .service
            .work_complete_on(
                Some(&reference),
                crate::work_service::WorkCompleteInput {
                    capture: None,
                    evidence: Vec::new(),
                    acceptance,
                    note: None,
                    links,
                    link_basis: Some(work.revision),
                    idempotency_key: String::new(),
                },
                at(2),
            )
            .unwrap_err();
        assert!(
            matches!(error, StoreError::WorkCriterionLinkInvalid { reason: actual, .. }
            if actual.contains(reason))
        );
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&connection).unwrap(),
            before
        );
    }
}

#[test]
fn criterion_links_many_bindings_keep_exact_omissions_in_both_twins() {
    let (_home, verbs, path, project) = fixture();
    let reference = setup(&verbs);
    verbs
        .service
        .work_update_on(
            Some(&reference),
            WorkUpdateInput::Revise {
                patch: crate::WorkRevisionPatch {
                    acceptance: Some(
                        (1..=40)
                            .map(|index| format!("Distinct outcome {index}"))
                            .collect(),
                    ),
                    ..Default::default()
                },
                idempotency_key: "many-criteria".into(),
            },
            at(2),
        )
        .unwrap();
    note(
        &verbs,
        &reference,
        &"Original evidence body ".repeat(100),
        3,
    );
    let locator = locators(&verbs, &reference).remove(0);
    let mut request = input(&reference, basis(&verbs, &reference), 1, &locator);
    request.links = (1..=39)
        .map(|criterion| WorkCriterionLinkInput {
            criterion,
            locator: locator.clone(),
        })
        .collect();
    let done = verbs.done(request, at(5)).unwrap();
    for receipt in [done, verbs.show(&reference, at(6)).unwrap()] {
        let facts = &receipt.value["acceptance_evidence"];
        assert_eq!(facts["unlinked_positions"], json!([40]));
        assert_eq!(facts["link_count"], 39);
        let rows = facts["links"].as_array().unwrap();
        assert!(!rows.is_empty());
        assert_eq!(
            rows.len() as u64 + facts["links_omitted"].as_u64().unwrap(),
            39
        );
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row["criterion"], index + 1);
            assert_eq!(row["locator"], locator);
        }
        assert!(receipt.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
        assert!(
            receipt
                .text()
                .contains("full frozen mapping continuation is not available on this surface")
        );
        assert!(!receipt.text().contains("more links not shown; inspect"));
        assert!(
            serde_json::to_vec_pretty(&receipt.value).unwrap().len()
                < MAX_AGENT_WORK_RESPONSE_BYTES
        );
        assert!(
            receipt
                .text()
                .contains("author-linked evidence; not verification")
        );
    }
    let store = SqliteStore::open(path).unwrap();
    let work = store.resolve_work_ref(&project, &reference).unwrap();
    let run = store.latest_work_run(work.work_id).unwrap().unwrap();
    let seal: crate::CompletionSeal = store
        .get(run.completion_seal.as_ref().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        seal.acceptance
            .iter()
            .map(|row| row.evidence.len())
            .sum::<usize>(),
        39
    );
    assert!(seal.acceptance[39].evidence.is_empty());
}

#[test]
fn criterion_links_require_the_authors_read_basis_not_a_fresh_internal_read() {
    for change_acceptance in [false, true] {
        let (_home, verbs, path, project) = fixture();
        let reference = setup(&verbs);
        note(&verbs, &reference, "Already recorded evidence", 2);
        let old_basis = basis(&verbs, &reference);
        let locator = locators(&verbs, &reference).remove(0);
        verbs
            .service
            .work_update_on(
                Some(&reference),
                WorkUpdateInput::Revise {
                    patch: crate::WorkRevisionPatch {
                        acceptance: change_acceptance.then(|| vec!["Replacement criterion".into()]),
                        title: (!change_acceptance).then(|| "Different title".into()),
                        ..Default::default()
                    },
                    idempotency_key: "revise-between-read-and-done".into(),
                },
                at(5),
            )
            .unwrap();
        let store = SqliteStore::open(&path).unwrap();
        let work = store.resolve_work_ref(&project, &reference).unwrap();
        assert_ne!(work.revision, old_basis);
        let evidence = store
            .work_run_evidence(work.active_run_id.unwrap())
            .unwrap();
        let error = verbs
            .done(input(&reference, old_basis, 1, &locator), at(6))
            .unwrap_err();
        assert!(
            matches!(error.error, StoreError::WorkCriterionLinkInvalid { reason, .. } if reason.contains("basis changed"))
        );
        assert_eq!(store.get_work_item(work.work_id).unwrap(), work);
        assert_eq!(
            store
                .work_run_evidence(work.active_run_id.unwrap())
                .unwrap(),
            evidence
        );
        let mut missing = input(&reference, work.revision, 1, &locator);
        missing.link_basis = None;
        assert!(matches!(verbs.done(missing, at(6)).unwrap_err().error,
            StoreError::WorkCriterionLinkInvalid { reason, .. } if reason.contains("require link_basis")));
    }
}

#[test]
fn criterion_links_refusals_distinguish_observation_checkpoint_and_wrong_locator() {
    let (_home, verbs, path, project) = fixture();
    let reference = setup(&verbs);
    let peer = AgentVerbs::new(
        path.clone(),
        project.clone(),
        "peer".into(),
        SessionId("peer".into()),
        None,
    );
    note(&peer, &reference, "Observation without execution credit", 2);
    note(&verbs, &reference, "Holder evidence", 3);
    let ids = locators(&verbs, &reference);
    let store = SqliteStore::open(&path).unwrap();
    let work = store.resolve_work_ref(&project, &reference).unwrap();
    let run = store.get_work_run(work.active_run_id.unwrap()).unwrap();
    let evidence = store.work_run_evidence(run.run_id).unwrap();
    let checkpoint = run.last_checkpoint.unwrap().to_string();
    for (position, locator, reason) in [
        (1, ids[0].as_str(), "non-holder or pre-claim observation"),
        (1, checkpoint.as_str(), "checkpoint"),
        (1, "https://artifact.invalid/result", "artifact path or URL"),
        (0, ids[1].as_str(), "outside the acceptance list"),
        (4, ids[1].as_str(), "outside the acceptance list"),
    ] {
        let error = verbs
            .done(input(&reference, work.revision, position, locator), at(5))
            .unwrap_err();
        assert!(
            matches!(&error.error, StoreError::WorkCriterionLinkInvalid { reason: actual, .. } if actual.contains(reason)),
            "{error}"
        );
        assert!(
            error
                .guidance()
                .next
                .iter()
                .any(|command| command.ends_with("--notes --gates"))
        );
        assert_eq!(store.get_work_item(work.work_id).unwrap(), work);
        assert_eq!(store.work_run_evidence(run.run_id).unwrap(), evidence);
    }
}

#[test]
fn criterion_links_do_not_promote_prior_run_or_inherited_evidence() {
    let (home, verbs, path, project) = fixture();
    let reference = setup(&verbs);
    note(&verbs, &reference, "Original execution evidence", 2);
    let original = locators(&verbs, &reference).remove(0);
    let document = super::review::snapshot(&path, &project, &reference);
    let (restored, restored_store, _) = super::review::load(home.path(), &document);
    restored
        .claim(
            ClaimInput {
                work_ref: reference.clone(),
                ttl_seconds: Some(3600),
                recover: None,
            },
            at(102),
        )
        .unwrap();
    let restored_work = restored_store
        .resolve_work_ref(&project, &reference)
        .unwrap();
    let inherited = restored
        .show_with_notes(&reference, true, at(103))
        .unwrap()
        .value["notes"][0]["locator"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(inherited.contains(':'));
    let error = restored
        .done(
            input(&reference, restored_work.revision, 1, &inherited),
            at(104),
        )
        .unwrap_err();
    assert!(
        matches!(error.error, StoreError::WorkCriterionLinkInvalid { reason, .. } if reason.contains("inherited record member"))
    );
    verbs
        .done(
            DoneInput {
                work_ref: Some(reference.clone()),
                summary: Some("First execution delivered".into()),
                ..Default::default()
            },
            at(5),
        )
        .unwrap();
    verbs
        .service
        .work_update_on(
            Some(&reference),
            WorkUpdateInput::Reopen {
                reason: "Independent execution generation".into(),
                idempotency_key: "reopen".into(),
            },
            at(6),
        )
        .unwrap();
    verbs
        .claim(
            ClaimInput {
                work_ref: reference.clone(),
                ttl_seconds: Some(3600),
                recover: None,
            },
            at(7),
        )
        .unwrap();
    let work = SqliteStore::open(&path)
        .unwrap()
        .resolve_work_ref(&project, &reference)
        .unwrap();
    let error = verbs
        .done(input(&reference, work.revision, 1, &original), at(8))
        .unwrap_err();
    assert!(
        matches!(error.error, StoreError::WorkCriterionLinkInvalid { reason, .. } if reason.contains("earlier run"))
    );
}

#[test]
fn criterion_links_none_and_all_change_only_explicit_bindings_and_stay_bounded() {
    for linked in [false, true] {
        let (_home, verbs, _, _) = fixture();
        let reference = setup(&verbs);
        note(
            &verbs,
            &reference,
            &format!(
                "STOP requires reading the original\n{}",
                "bounded preview ".repeat(200)
            ),
            2,
        );
        let locator = locators(&verbs, &reference).remove(0);
        let mut request = DoneInput {
            work_ref: Some(reference.clone()),
            summary: Some("Delivered".into()),
            ..Default::default()
        };
        if linked {
            request.link_basis = Some(basis(&verbs, &reference));
            request.links = (1..=3)
                .map(|criterion| WorkCriterionLinkInput {
                    criterion,
                    locator: locator.clone(),
                })
                .collect();
        }
        let receipt = verbs.done(request, at(5)).unwrap();
        assert_eq!(
            receipt.value["acceptance_evidence"]["unlinked_count"],
            if linked { 0 } else { 3 }
        );
        assert_eq!(receipt.value["acceptance_criteria_asserted"], 3);
        assert_eq!(receipt.value["acceptance_criteria_changed"], false);
        assert!(receipt.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
        assert!(
            serde_json::to_vec_pretty(&receipt.value).unwrap().len()
                < MAX_AGENT_WORK_RESPONSE_BYTES
        );
        if linked {
            for row in receipt.value["acceptance_evidence"]["links"]
                .as_array()
                .unwrap()
            {
                assert!(
                    row["detail"]
                        .as_str()
                        .unwrap()
                        .ends_with(&format!("--note {locator}"))
                );
                assert!(row["preview"].as_str().unwrap().len() <= 192);
            }
        }
    }
}
