//! Current native checks replace stale implicit credit without rewriting history.
use super::*;

#[path = "retirement/acceptance.rs"]
mod acceptance;
#[path = "retirement/uncited_native.rs"]
mod uncited_native;

fn move_destination(data: &mut Selected, fault: &str) -> (String, i64) {
    let work = data
        .target
        .store
        .get_work_item(data.target.binding.work_id)
        .unwrap();
    match fault {
        "source" => {
            std::fs::write(data.root.path().join("file"), "new source\n").unwrap();
            data.target.report_source(data.root.path(), 24, true);
        }
        "revision" => {
            data.target
                .store
                .revise_work(
                    &ReviseWorkRequest {
                        work_id: work.work_id,
                        expected_revision: work.revision,
                        patch: WorkRevisionPatch {
                            title: Some("revised destination".into()),
                            ..Default::default()
                        },
                        authority: WorkPlanningAuthority::Claim {
                            run_id: data.target.binding.run_id,
                            holder: session(),
                            claim_id: data.target.binding.claim_id,
                            claim_fence: data.target.binding.claim_fence,
                        },
                        actor: data.target.local_actor(),
                        idempotency_key: "retirement-revision".into(),
                        updated_at: at(24),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
        }
        "root" => {
            data.target
                .store
                .bind_named_root(
                    &project(),
                    &session(),
                    &data.target.token,
                    HostSessionAuthority::Work,
                    data.target.binding.claim_id,
                    data.target.binding.claim_fence,
                    &data.target.workspace,
                    2,
                    at(24),
                    NamedRootBindingKind::Bound,
                    None,
                    &mut actor("runner"),
                    "retirement-root",
                    at(24),
                )
                .unwrap();
        }
        "recovery" => {
            claim(
                &mut data.target.store,
                &work,
                "runner",
                "retirement-recovery",
                4000,
                3600,
            );
            return ("runner".into(), 4001);
        }
        "handoff" => {
            let offer = data
                .target
                .store
                .offer_work_handoff(
                    &OfferWorkHandoffRequest {
                        work_id: work.work_id,
                        run_id: data.target.binding.run_id,
                        expected_work_revision: work.revision,
                        from: session(),
                        to: SessionId("second".into()),
                        claim_id: data.target.binding.claim_id,
                        claim_fence: data.target.binding.claim_fence,
                        ttl_seconds: 300,
                        checkpoint_summary: "transfer after imported use".into(),
                        actor: actor("runner"),
                        idempotency_key: "retirement-offer".into(),
                        offered_at: at(24),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
            data.target
                .store
                .accept_work_handoff(
                    &AcceptWorkHandoffRequest {
                        work_id: work.work_id,
                        offer_id: offer.offer_id,
                        to: SessionId("second".into()),
                        actor: actor("second"),
                        idempotency_key: "retirement-accept".into(),
                        accepted_at: at(25),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
            return ("second".into(), 26);
        }
        _ => unreachable!(),
    }
    ("runner".into(), 26)
}

fn fresh_native(data: &mut Selected, holder: &str, second: i64) -> ObjectId {
    let item = data
        .target
        .store
        .get_work_item(data.target.binding.work_id)
        .unwrap();
    let held = data
        .target
        .store
        .current_work_claim(item.work_id)
        .unwrap()
        .unwrap();
    data.target.binding.work_revision = item.revision;
    data.target.binding.claim_id = held.claim_id;
    data.target.binding.claim_fence = held.fence;
    let token = data
        .target
        .store
        .resume_work_connection(&SessionId(holder.into()), at(second))
        .unwrap();
    let root = data
        .target
        .store
        .read_named_root(
            &project(),
            &SessionId(holder.into()),
            &token,
            HostSessionAuthority::Work,
            held.run_id,
            held.claim_id,
        )
        .unwrap();
    let NamedRootState::Bound { generation, .. } = root.named_root else {
        panic!("bound root")
    };
    // A new native remote check names its actual current holder/fence. The
    // old imported use keeps its historical binding and remains retired.
    let generation = if holder != "runner" || second >= 4000 {
        let generation = generation + 1;
        data.target
            .store
            .bind_named_root(
                &project(),
                &SessionId(holder.into()),
                &token,
                HostSessionAuthority::Work,
                held.claim_id,
                held.fence,
                &data.target.workspace,
                generation,
                at(second),
                NamedRootBindingKind::Bound,
                None,
                &mut actor(holder),
                "fresh-native-root",
                at(second),
            )
            .unwrap();
        generation
    } else {
        generation
    };
    let source = source(data.root.path());
    let mut request = data.target.request.clone();
    request.operation_key = "fresh-native".into();
    request.completion.run_id = "fresh-native-execution".into();
    request.completion.input = RemoteRunInput::SourceId(Box::new(RemoteSourceIdInput::new(
        Some(source.clone()),
        Some(source.clone()),
    )));
    request.completion.finished_at = Some(at(second));
    request.association.binding = data.target.binding.clone();
    request.association.root.generation = generation;
    request.association.intended_use = RemoteVerificationUse::CommittedInput {
        expected_commit: source.commit,
        purpose: RemoteVerificationPurpose::Verification,
    };
    let receipt = data
        .target
        .store
        .record_remote_verification(
            &project(),
            &SessionId(holder.into()),
            &token,
            HostSessionAuthority::Work,
            &actor(holder),
            &request,
            at(second),
        )
        .unwrap();
    assert!(
        receipt.qualification.credited,
        "fresh native check: {:?}",
        receipt.qualification
    );
    receipt
        .verifications
        .first()
        .expect("credited native check has verification")
        .record_id
        .clone()
}

fn finish(
    data: &mut Selected,
    holder: &str,
    evidence: &ObjectId,
    second: i64,
) -> std::result::Result<CompletionSeal, crate::storage::StoreError> {
    let item = data
        .target
        .store
        .get_work_item(data.target.binding.work_id)
        .unwrap();
    let held = data
        .target
        .store
        .current_work_claim(item.work_id)
        .unwrap()
        .unwrap();
    checkpoint(
        &mut data.target.store,
        &item,
        &held,
        holder,
        "retirement-checkpoint",
        second,
        std::slice::from_ref(evidence),
    );
    let mut request = completion_request(
        &item,
        &held,
        holder,
        evidence,
        "retirement-complete",
        second + 1,
    );
    request.source_id = Some(source(data.root.path()));
    data.target
        .store
        .complete_work(&request, &DevelopmentNoopRedactor)
}

#[test]
fn stale_uncited_verification_retires_and_fresh_native_checks_complete_after_each_movement() {
    for fault in ["source", "revision", "root", "recovery", "handoff"] {
        let mut data = selected(false);
        let used = consume(&mut data.target, &data.verification.decision, false);
        let before = object_on(&data.target.store.connection, &used.consumption)
            .unwrap()
            .unwrap()
            .1;
        let (holder, now) = move_destination(&mut data, fault);
        let fresh = fresh_native(&mut data, &holder, now);
        let seal = finish(&mut data, &holder, &fresh, now + 1)
            .unwrap_or_else(|error| panic!("{fault}: {error}"));
        assert!(!seal.evidence.contains(&used.consumption), "{fault}");
        assert_eq!(
            object_on(&data.target.store.connection, &used.consumption)
                .unwrap()
                .unwrap()
                .1,
            before
        );
        healthy(&data.target.store);
    }
}

#[test]
fn retired_implicit_credit_needs_a_fresh_native_check_and_keeps_conflicts_visible() {
    for conflict in [false, true] {
        let mut data = selected(false);
        consume(&mut data.target, &data.verification.decision, false);
        let (holder, now) = move_destination(&mut data, "source");
        if conflict {
            data.origin.request.operation_key = "retired-conflict".into();
            data.origin.request.completion.result.verdict = RemoteRunVerdict::Fail;
            let record = data.origin.record(24);
            let conflicting = pack(
                &data.acceptance.origin,
                vec![envelope(
                    &data.origin.store,
                    &data.acceptance.origin,
                    &record.operation,
                )],
            );
            retain(&mut data.target.store, &conflicting);
            let fresh = fresh_native(&mut data, &holder, now);
            let error = finish(&mut data, &holder, &fresh, now + 1).unwrap_err();
            assert!(
                error.to_string().contains("terminal facts conflict"),
                "{error}"
            );
        } else {
            let item = data
                .target
                .store
                .get_work_item(data.target.binding.work_id)
                .unwrap();
            let held = data
                .target
                .store
                .current_work_claim(item.work_id)
                .unwrap()
                .unwrap();
            let note = data
                .target
                .store
                .record_work_note(
                    &RecordWorkNoteRequest {
                        status: false,
                        work_id: item.work_id,
                        run_id: held.run_id,
                        expected_work_revision: item.revision,
                        holder: held.holder,
                        claim_id: held.claim_id,
                        claim_fence: held.fence,
                        summary: "ordinary note, no fresh verification".into(),
                        refs: vec![],
                        actor: actor(&holder),
                        idempotency_key: "retirement-note".into(),
                        recorded_at: at(now),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
            let error = finish(&mut data, &holder, &note.evidence, now + 1).unwrap_err();
            assert!(error.to_string().contains("passing check"), "{error}");
        }
    }
}

#[test]
fn concurrent_exact_applicability_and_consumption_requests_replay_one_retained_identity() {
    for consumption in [false, true] {
        let data = selected(false);
        let database_home = crate::test_support::temp_home().unwrap();
        let database = database_home.path().join("concurrent.sqlite3");
        data.target
            .store
            .connection
            .execute("VACUUM INTO ?1", [database.to_str().unwrap()])
            .unwrap();
        let first = SqliteStore::open(&database).unwrap();
        let second = SqliteStore::open(&database).unwrap();
        let mut intent = data.verification.clone();
        intent.decision = ObjectId::mint();
        let use_intent = ConsumptionRequest {
            consumption: ObjectId::mint(),
            decision: data.verification.decision.clone(),
            binding: data.target.binding.clone(),
        };
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = [first, second]
            .into_iter()
            .map(|mut store| {
                let intent = intent.clone();
                let request = use_intent.clone();
                let actor = data.target.local_actor();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    if consumption {
                        store
                            .consume_imported_verification(
                                &project(),
                                &session(),
                                &actor,
                                &request,
                                at(24),
                            )
                            .map(|r| r.replayed)
                    } else {
                        store
                            .associate_imported_verification(
                                &project(),
                                &session(),
                                &actor,
                                &intent,
                                at(24),
                            )
                            .map(|_| false)
                            .map_err(crate::storage::work::task_sync::evidence::store_error)
                    }
                })
            })
            .collect();
        let results: Vec<_> = handles
            .into_iter()
            .map(|h| h.join().unwrap().unwrap())
            .collect();
        if consumption {
            assert_eq!(results.iter().filter(|replayed| **replayed).count(), 1);
        }
        let mut store = SqliteStore::open(&database).unwrap();
        let id = if consumption {
            &use_intent.consumption
        } else {
            &intent.decision
        };
        let count: i64 = store
            .connection
            .query_row(
                "SELECT count(*) FROM objects WHERE object_id=?1",
                [id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        let mut actor = data.target.local_actor();
        actor.actor_id.push_str("-changed");
        if consumption {
            assert!(
                store
                    .consume_imported_verification(
                        &project(),
                        &session(),
                        &actor,
                        &use_intent,
                        at(25)
                    )
                    .is_err()
            );
        } else {
            assert!(
                store
                    .associate_imported_verification(
                        &project(),
                        &session(),
                        &actor,
                        &intent,
                        at(25)
                    )
                    .is_err()
            );
        }
        healthy(&store);
    }
}
