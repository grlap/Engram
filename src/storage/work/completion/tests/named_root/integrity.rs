use super::*;

/// The doctor traces the generation a sighting states to an event recorded
/// before it: a generation the claim never bound, or an end that was never
/// recorded, is reported by the sighting's record id.
#[test]
fn the_doctor_reports_a_stated_generation_it_cannot_trace() {
    let (mut store, work, claim, _) = named_work();
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
    let untraced = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "no-such-generation",
        5,
        Some(basis("workspace-B", "B5", Some(12))),
        None,
    );
    let mut ended = basis("workspace-B", "B6", Some(9));
    ended.source_root_state = Some(SourceRootState::Ended);
    let never_ended = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "never-ended",
        6,
        Some(ended),
        None,
    );
    let report = store.verify_all().expect("doctor");
    for id in [&untraced, &never_ended] {
        let label = format!(
            "execution_observation:{id}:source root generation without its recorded binding"
        );
        assert!(
            report.invalid_work_records.contains(&label),
            "{label}: {report:?}"
        );
    }
}

/// A named-root bind receipt must name the event it recorded, at that
/// event's run-feed position and with its workspace and generation. A
/// receipt changed in any of them is reported, and the original is healthy.
#[test]
fn the_doctor_ties_a_named_root_receipt_to_its_event() {
    let (store, _, _, _) = named_work();
    // The work session stores its receipt as the host-receipt envelope in the
    // work-operation result store; the receipt itself is its `result`.
    let (key, stored): (String, Vec<u8>) = store
        .connection
        .query_row(
            "SELECT idempotency_key, result_json FROM work_operation_results
             WHERE operation = 'host_named_root_bind'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("the bind receipt");
    let original: serde_json::Value = serde_json::from_slice(&stored).expect("receipt JSON");
    let write = |receipt: &serde_json::Value| {
        store
            .connection
            .execute(
                "UPDATE work_operation_results SET result_json = ?1
                 WHERE operation = 'host_named_root_bind' AND idempotency_key = ?2",
                rusqlite::params![
                    serde_json::to_vec(receipt).expect("receipt bytes"),
                    key.as_str()
                ],
            )
            .expect("rewrite the receipt");
    };
    let label = format!("work_operation_result:host_named_root_bind:{key}:");
    for (field, value) in [
        ("workspace_id", serde_json::json!("workspace-C")),
        ("event", serde_json::json!(ObjectId::mint())),
        ("generation", serde_json::json!(10)),
    ] {
        let mut altered = original.clone();
        altered["result"][field] = value;
        write(&altered);
        let report = store.verify_all().expect("doctor");
        assert!(
            report
                .invalid_work_records
                .iter()
                .any(|record| record.starts_with(&label)),
            "{field}: {report:?}"
        );
    }
    write(&original);
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

/// The doctor holds a claim's naming history to what the host writer
/// admits: an end of a generation that a later name already superseded is
/// reported, although its fields repeat the bound event it names.
#[test]
fn the_doctor_reports_an_end_of_a_superseded_generation() {
    let (mut store, work, claim, _) = named_work();
    name_root(&mut store, &work, &claim, 10, 5);
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
    let bytes: Vec<u8> = store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects
             WHERE object_kind = 'named_root_binding'
               AND json_extract(canonical_json, '$.generation') = 9",
            [],
            |row| row.get(0),
        )
        .expect("the generation-9 binding");
    let mut ended: crate::domain::NamedRootBindingEvent =
        serde_json::from_slice(&bytes).expect("binding event");
    ended.kind = NamedRootBindingKind::Ended;
    ended.end_reason = Some(crate::domain::NamedRootEndReason::ExplicitClear);
    ended.recorded_at = at(6);
    let transaction = begin(&mut store);
    let (id, _) = crate::storage::work::append_named_root_binding_on(&transaction, &ended)
        .expect("append the end of generation 9");
    transaction.commit().expect("commit the end");
    let report = store.verify_all().expect("doctor");
    assert!(
        report
            .invalid_work_records
            .contains(&format!("named_root_binding:{id}")),
        "{report:?}"
    );
}

/// The doctor re-derives every displacement and named-root satisfaction: a
/// displaced resolution recorded for a foreign change the root keeps open or
/// for a change in the root itself, and a satisfied resolution of a foreign
/// change by a check in the root, are each reported by their obligation.
#[test]
fn the_doctor_reports_forged_named_root_resolutions() {
    type Forge = fn(&ObjectId, &ObjectId, &FeedPosition) -> WorkObligationResolution;
    let cases: [(&str, &str, Forge); 3] = [
        ("workspace-A", "displaced-open-foreign", |binding, _, _| {
            WorkObligationResolution::Displaced {
                binding: binding.clone(),
                trigger_workspace_id: "workspace-A".into(),
            }
        }),
        ("workspace-B", "displaced-in-root", |binding, _, _| {
            WorkObligationResolution::Displaced {
                binding: binding.clone(),
                trigger_workspace_id: "workspace-B".into(),
            }
        }),
        ("workspace-A", "satisfied-foreign", |_, check, cut| {
            WorkObligationResolution::Satisfied {
                evidence: check.clone(),
                evaluated_cut: cut.clone(),
            }
        }),
    ];
    for (workspace, key, forge) in cases {
        let (mut store, work, claim, _) = named_work();
        let check = host_verification_from_basis(
            &mut store,
            &work,
            &claim,
            "runner",
            &format!("{key}-check"),
            VerificationKind::Test,
            VerificationResult::Passed,
            5,
            basis("workspace-B", "B6", Some(9)),
        );
        // Recorded after the check, the change keeps its obligation open.
        let change = source_mutation_from_basis(
            &mut store,
            &work,
            &claim,
            "runner",
            key,
            6,
            Some(basis(workspace, "R5", Some(9))),
            None,
        );
        let binding = super::super::super::named_root_context_on(
            &store.connection,
            claim.run_id,
            claim.claim_id,
            i64::MAX,
        )
        .expect("lookup")
        .expect("root B is active")
        .binding_id;
        let cut = super::super::super::current_run_feed_cut_on(&store.connection, claim.run_id)
            .expect("run cut");
        assert_eq!(
            obligation(&store, &claim, &Pick::TriggeredBy(&change)).0,
            WorkObligationState::Open,
            "{key}: the change's obligation must still be open to forge it"
        );
        let label = forge_resolution(&mut store, &claim, &change, forge(&binding, &check, &cut));
        let report = store.verify_all().expect("doctor");
        assert!(
            report.invalid_work_records.contains(&label),
            "{key}: {label}: {report:?}"
        );
    }
}

/// A seal's disclosed foreign changes must be exactly its displaced
/// resolutions; a seal rewritten to hide one is reported.
#[test]
fn the_doctor_reports_a_seal_that_hides_a_displaced_change() {
    let (mut store, work, claim, _) = named_work();
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-B",
        VerificationKind::Test,
        VerificationResult::Passed,
        5,
        basis("workspace-B", "B5", Some(9)),
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-B",
        6,
        std::slice::from_ref(&checked),
    );
    complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &checked,
        "complete-B",
        7,
    )
    .expect("the B check completes the work");
    let seal_id = store
        .get_work_run(claim.run_id)
        .expect("run")
        .completion_seal
        .expect("the run is sealed");
    let bytes: Vec<u8> = store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id = ?1",
            [seal_id.as_str()],
            |row| row.get(0),
        )
        .expect("the seal");
    let mut seal: serde_json::Value = serde_json::from_slice(&bytes).expect("seal JSON");
    assert!(
        seal.as_object_mut()
            .expect("seal")
            .remove("foreign_workspace_changes")
            .is_some()
    );
    store
        .connection
        .execute(
            "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
            rusqlite::params![
                serde_json_canonicalizer::to_vec(&seal).expect("canonical seal"),
                seal_id.as_str()
            ],
        )
        .expect("rewrite the seal");
    // Its projection is rewritten the same way, so only the disclosure is off.
    store
        .connection
        .execute(
            "UPDATE work_completion_seals SET seal_json = ?1 WHERE seal_id = ?2",
            rusqlite::params![
                serde_json::to_vec(&seal).expect("seal projection"),
                seal_id.as_str()
            ],
        )
        .expect("rewrite the seal projection");
    let report = store.verify_all().expect("doctor");
    let label = format!("completion_seal:{seal_id}:obligation_basis");
    assert!(
        report.invalid_work_records.contains(&label),
        "{label}: {report:?}"
    );
}

/// The doctor holds a satisfaction recorded while the claim was unbound to
/// the same history: neither a change foreign to the root bound when it was
/// recorded nor a change whose root cannot be established, recorded while
/// that root was bound, can be satisfied by a check after that root ended,
/// even one that accounts for a later change by the rules without a root.
#[test]
fn the_doctor_reports_a_bound_change_satisfied_while_unbound() {
    for (key, change_basis) in [
        ("foreign-under-9", Some(basis("workspace-A", "A5", Some(9)))),
        ("unknown-under-9", None),
    ] {
        let (mut store, work, claim, _) = named_work();
        let change = source_mutation_from_basis(
            &mut store,
            &work,
            &claim,
            "runner",
            key,
            5,
            change_basis,
            None,
        );
        end_root(&mut store, &work, &claim, "workspace-B", 9, 4, 6);
        source_mutation_from_basis(
            &mut store,
            &work,
            &claim,
            "runner",
            "unbound-change",
            7,
            Some(basis("workspace-B", "B7", None)),
            None,
        );
        let check = host_verification_from_basis(
            &mut store,
            &work,
            &claim,
            "runner",
            "test-unbound",
            VerificationKind::Test,
            VerificationResult::Passed,
            8,
            basis("workspace-B", "B7", None),
        );
        assert_eq!(
            obligation(&store, &claim, &Pick::TriggeredBy(&change)).0,
            WorkObligationState::Open,
            "{key}"
        );
        let cut = super::super::super::current_run_feed_cut_on(&store.connection, claim.run_id)
            .expect("run cut");
        let label = forge_resolution(
            &mut store,
            &claim,
            &change,
            WorkObligationResolution::Satisfied {
                evidence: check,
                evaluated_cut: cut,
            },
        );
        let report = store.verify_all().expect("doctor");
        assert!(
            report.invalid_work_records.contains(&label),
            "{key}: {label}: {report:?}"
        );
    }
}
