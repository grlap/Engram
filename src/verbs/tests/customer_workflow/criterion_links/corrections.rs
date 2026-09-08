use super::*;

#[test]
fn criterion_links_pending_basis_refusal_never_discloses_derived_key() {
    let (_home, verbs, path, _) = fixture();
    let reference = setup(&verbs);
    note(&verbs, &reference, "Original evidence", 2);
    let request = input(&reference, basis(&verbs, &reference), 1, "not-a-note");
    assert!(verbs.done(request.clone(), at(5)).is_err());
    let connection = rusqlite::Connection::open(path).unwrap();
    let key: String = connection
        .query_row(
            "SELECT idempotency_key FROM work_protocol_attempts WHERE operation = 'work_complete'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    verbs
        .service
        .work_update_on(
            Some(&reference),
            WorkUpdateInput::Revise {
                patch: crate::WorkRevisionPatch {
                    title: Some("Changed work".into()),
                    ..Default::default()
                },
                idempotency_key: "revise".into(),
            },
            at(6),
        )
        .unwrap();
    let error = verbs.done(request, at(7)).unwrap_err();
    assert!(!error.to_string().contains("linked-completion:"), "{error}");
    let rendered = error.to_string();
    // Derive forbidden material from the actual persisted key. Neither a new
    // prefix nor a partially shortened digest may evade the privacy oracle.
    for fragment in key.as_bytes().windows(16) {
        assert!(
            !rendered
                .as_bytes()
                .windows(16)
                .any(|window| window == fragment)
        );
    }
    assert!(matches!(
        error.error,
        StoreError::WorkCriterionLinkInvalid {
            criterion: None,
            ..
        }
    ));
    let mut explicit = crate::work_service::WorkCompleteInput {
        links: vec![WorkCriterionLinkInput {
            criterion: 1,
            locator: "not-a-note".into(),
        }],
        link_basis: Some(basis(&verbs, &reference)),
        capture: None,
        evidence: vec![],
        acceptance: None,
        note: None,
        idempotency_key: "caller-owned-key".into(),
    };
    assert!(matches!(
        verbs
            .service
            .work_complete_on(Some(&reference), explicit.clone(), at(8)),
        Err(StoreError::WorkCriterionLinkInvalid { .. })
    ));
    explicit.note = Some("Materially different acceptance note".into());
    assert!(
        matches!(verbs.service.work_complete_on(Some(&reference), explicit, at(9)),
        Err(StoreError::WorkOperationIdempotencyConflict { operation, key })
        if operation == "work_complete" && key == "caller-owned-key")
    );
}

#[test]
fn criterion_links_refused_intent_cannot_recover_another_completion() {
    for invalid_position in [false, true] {
        let (_home, verbs, path, project) = fixture();
        let reference = setup(&verbs);
        note(&verbs, &reference, "Original evidence", 2);
        let locator = locators(&verbs, &reference).remove(0);
        let request = input(
            &reference,
            basis(&verbs, &reference),
            usize::from(!invalid_position),
            if invalid_position {
                &locator
            } else {
                "not-a-note"
            },
        );
        assert!(verbs.done(request.clone(), at(5)).is_err());
        let completed = verbs
            .done(
                input(&reference, basis(&verbs, &reference), 2, &locator),
                at(6),
            )
            .unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        verbs.service.select_work(&reference, at(7)).unwrap();
        let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
        let error = verbs.done(request, at(7)).unwrap_err();
        assert!(matches!(
            error.error,
            StoreError::WorkCriterionLinkInvalid { .. }
        ));
        assert!(!error.to_string().contains("linked-completion:"));
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&connection).unwrap(),
            before
        );
        assert_eq!(
            verbs.show(&reference, at(8)).unwrap().value["acceptance_evidence"],
            completed.value["acceptance_evidence"]
        );
        let store = SqliteStore::open(path).unwrap();
        assert_eq!(
            store
                .resolve_work_ref(&project, &reference)
                .unwrap()
                .lifecycle,
            crate::WorkLifecycle::Completed
        );
    }
}

#[test]
fn criterion_links_same_mapping_does_not_prove_the_same_completion_intent() {
    let (_home, verbs, _, _) = fixture();
    let reference = setup(&verbs);
    note(&verbs, &reference, "Original evidence", 2);
    let locator = locators(&verbs, &reference).remove(0);
    let child = verbs
        .add(
            AddInput {
                title: "Required child".into(),
                under: Some(reference.clone()),
                ..Default::default()
            },
            at(3),
        )
        .unwrap();
    let child = child.value["work"]["short_ref"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut request = input(&reference, basis(&verbs, &reference), 2, &locator);
    request.summary = None;
    // The valid mapping is first refused by a real required-child barrier.
    assert!(verbs.done(request.clone(), at(5)).unwrap().owed);
    verbs
        .claim(
            ClaimInput {
                work_ref: child.clone(),
                ttl_seconds: Some(300),
                recover: None,
            },
            at(6),
        )
        .unwrap();
    assert!(
        !verbs
            .done(
                DoneInput {
                    work_ref: Some(child),
                    summary: Some("Child delivered".into()),
                    ..Default::default()
                },
                at(7)
            )
            .unwrap()
            .owed
    );
    let mut other = request.clone();
    other.summary = Some("Different intent captures and checkpoints".into());
    assert!(!verbs.done(other, at(8)).unwrap().owed);
    assert!(matches!(verbs.done(request, at(9)).unwrap_err().error,
        StoreError::WorkCriterionLinkInvalid { reason, .. } if reason.contains("completion is frozen")));
}

#[test]
fn criterion_links_genuine_interrupted_success_keeps_its_mapping() {
    for capture in [true, false] {
        let (_home, verbs, path, _) = fixture();
        let reference = setup(&verbs);
        note(&verbs, &reference, "Original evidence", 2);
        let locator = locators(&verbs, &reference).remove(0);
        let mut request = input(&reference, basis(&verbs, &reference), 2, &locator);
        if !capture {
            verbs
                .service
                .work_update_on(
                    Some(&reference),
                    WorkUpdateInput::Checkpoint {
                        summary: "Existing checkpoint".into(),
                        evidence: None,
                        idempotency_key: "checkpoint".into(),
                    },
                    at(3),
                )
                .unwrap();
            request.summary = None;
        }
        let connection = rusqlite::Connection::open(&path).unwrap();
        // Fail only the outer receipt publication, after the storage seal commits.
        connection.execute_batch("CREATE TRIGGER interrupt_link_result BEFORE UPDATE OF result_json ON work_protocol_attempts
        WHEN NEW.operation = 'work_complete' AND NEW.result_json IS NOT NULL
        BEGIN SELECT RAISE(ABORT, 'test interrupts outer completion publication'); END;").unwrap();
        let error = verbs.done(request.clone(), at(5)).unwrap_err();
        assert!(matches!(error.error, StoreError::Sqlite(_)));
        connection
            .execute_batch("DROP TRIGGER interrupt_link_result")
            .unwrap();
        let recovered = verbs.done(request.clone(), at(6)).unwrap();
        assert!(!recovered.owed);
        assert_eq!(
            recovered.value["acceptance_evidence"]["links"][0]["criterion"],
            2
        );
        assert_eq!(
            recovered.value["acceptance_evidence"]["links"][0]["locator"],
            locator
        );
        let replay = verbs.done(request, at(7)).unwrap();
        assert_eq!(
            replay.value["acceptance_evidence"],
            recovered.value["acceptance_evidence"]
        );
        assert!(!replay.text().contains("linked-completion:"));
        assert!(
            !serde_json::to_string(&replay.value)
                .unwrap()
                .contains("linked-completion:")
        );
    }
}

#[test]
fn criterion_links_preview_diagnostic_survives_receipt_roundtrip() {
    use crate::verbs::acceptance::AcceptanceEvidence;
    use crate::work_service::{WorkAcceptanceEvidence, WorkAcceptanceLink};
    let evidence = crate::ObjectHash::from_canonical_bytes(b"preview-diagnostic-fixture");
    let facts = WorkAcceptanceEvidence {
        work_id: Some(crate::WorkId(uuid::Uuid::now_v7())),
        link_count: 1,
        links: vec![WorkAcceptanceLink {
            criterion: 2,
            evidence: evidence.clone(),
            preview: None,
            preview_error_class: Some("canonical_object_invalid"),
        }],
        criteria_count: 3,
        unlinked_count: 2,
        unlinked_positions: vec![1, 3],
    };
    let page = AcceptanceEvidence::new(&facts);
    let recovered = page.facts();
    assert_eq!(recovered.links[0].evidence, evidence);
    assert_eq!(
        recovered.links[0].preview_error_class,
        Some("canonical_object_invalid")
    );
    let receipt = page
        .append(&Receipt::assemble(
            vec!["done example".into()],
            Guidance::default(),
            json!({"completed":true}),
            false,
        ))
        .unwrap();
    assert_eq!(
        receipt.value["acceptance_evidence"]["links"][0]["preview_error_class"],
        "canonical_object_invalid"
    );
    assert!(
        receipt.value["acceptance_evidence"]["links"][0]
            .get("preview")
            .is_none()
    );
    assert!(
        receipt
            .text()
            .contains("preview diagnostic class: canonical_object_invalid")
    );
    assert!(!receipt.owed);
}

#[test]
fn criterion_links_membership_is_run_scoped_without_first_row_selection() {
    let (_home, verbs, path, project) = fixture();
    let reference = setup(&verbs);
    note(&verbs, &reference, "Original execution evidence", 2);
    let old = locators(&verbs, &reference).remove(0);
    verbs
        .done(
            DoneInput {
                work_ref: Some(reference.clone()),
                summary: Some("First execution".into()),
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
                reason: "Independent new run".into(),
                idempotency_key: "reopen".into(),
            },
            at(6),
        )
        .unwrap();
    verbs
        .claim(
            ClaimInput {
                work_ref: reference.clone(),
                ttl_seconds: Some(300),
                recover: None,
            },
            at(7),
        )
        .unwrap();
    note(&verbs, &reference, "Current execution evidence", 8);
    let store = SqliteStore::open(path).unwrap();
    let work = store.resolve_work_ref(&project, &reference).unwrap();
    let run = store
        .current_work_claim(work.work_id)
        .unwrap()
        .unwrap()
        .run_id;
    let index = store
        .work_record_index(
            &project,
            work.work_id,
            crate::storage::WorkRecordKind::NotesWithGates,
        )
        .unwrap();
    let current = store.work_run_evidence(run).unwrap().remove(0);
    assert_eq!(
        store
            .resolve_criterion_evidence(&project, work.work_id, run, 1, current.as_str(), &index)
            .unwrap(),
        current
    );
    assert!(
        matches!(store.resolve_criterion_evidence(&project, work.work_id, run, 1, &old, &index),
        Err(StoreError::WorkCriterionLinkInvalid { reason, .. }) if reason.contains("earlier run"))
    );
}
