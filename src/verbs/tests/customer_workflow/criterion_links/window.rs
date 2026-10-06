use super::*;
use std::fmt::Write as _;

fn window(verbs: &AgentVerbs, reference: &str, after: Option<String>) -> Receipt {
    verbs
        .show_records(
            reference,
            &ShowInput {
                criterion_links: true,
                after,
                ..Default::default()
            },
            at(1000),
        )
        .unwrap()
}

fn finish(verbs: &AgentVerbs, reference: &str, count: usize, evaluated: bool) {
    if count == 0 || evaluated {
        note(verbs, reference, "Completed with no criterion citations", 2);
    }
    let mut ids = Vec::new();
    for index in 0..count.div_ceil(2) {
        if evaluated {
            let result = verbs
                .service
                .work_gate_on(
                    Some(reference),
                    &format!("passing-citation-{index}"),
                    &[],
                    None,
                    at(2),
                )
                .unwrap();
            ids.push(result.receipt.result.as_str().unwrap().to_owned());
            continue;
        }
        let receipt = verbs
            .note(
                &NoteInput {
                    work_ref: Some(reference.into()),
                    text: format!("Evidence {index} {}", "\\\"🙂\u{202e}\u{1b}\n".repeat(80)),
                    refs: Vec::new(),
                    status: false,
                },
                at(2),
            )
            .unwrap();
        ids.push(receipt.value["evidence"].as_str().unwrap().to_owned());
    }
    note(verbs, reference, "Checkpoint all fixture evidence", 3);
    if evaluated {
        let shown = verbs.show(reference, at(4)).unwrap();
        verbs
            .evaluate(
                EvaluateInput {
                    work_ref: Some(reference.into()),
                    mode: "same_session".into(),
                    acceptance_basis: shown.value["acceptance_basis"].as_i64().unwrap(),
                    evidence_basis: shown.value["evidence_basis"].as_i64().unwrap(),
                    verdicts: (1..=3)
                        .map(|criterion| crate::WorkCriterionVerdictInput {
                            criterion,
                            verdict: "pass".into(),
                            basis: "asserted".into(),
                            rationale: "Fixture evidence".into(),
                            evidence: if criterion == 3 {
                                ids[..1].to_vec()
                            } else {
                                ids.clone()
                            },
                        })
                        .collect(),
                    attempt: None,
                    source_fingerprint: None,
                    model: None,
                    execution_identity: None,
                    parent_session: None,
                    supersedes: None,
                },
                at(4),
            )
            .unwrap();
    }
    let shown_basis = basis(verbs, reference);
    let done = verbs
        .done(
            DoneInput {
                work_ref: Some(reference.into()),
                summary: Some("Fixture completion".into()),
                links: if evaluated {
                    Vec::new()
                } else {
                    (0..count)
                        .map(|index| WorkCriterionLinkInput {
                            criterion: if index < count.div_ceil(2) { 1 } else { 2 },
                            locator: ids[index % ids.len()].clone(),
                        })
                        .collect()
                },
                link_basis: (!evaluated && count > 0).then_some(shown_basis),
                ..Default::default()
            },
            at(5),
        )
        .unwrap();
    assert!(!done.owed);
}

fn sealed(
    store: &SqliteStore,
    project: &ProjectId,
    reference: &str,
) -> (crate::WorkRun, crate::CompletionSeal) {
    let item = store.resolve_work_ref(project, reference).unwrap();
    let run = store.latest_work_run(item.work_id).unwrap().unwrap();
    let seal = store
        .get(run.completion_seal.as_ref().unwrap())
        .unwrap()
        .unwrap();
    (run, seal)
}

fn tuples(seal: &crate::CompletionSeal) -> Vec<(usize, usize, String)> {
    seal.acceptance
        .iter()
        .enumerate()
        .flat_map(|(criterion, result)| {
            result
                .evidence
                .iter()
                .enumerate()
                .map(move |(member, id)| (criterion + 1, member + 1, id.to_string()))
        })
        .collect()
}

fn traverse(
    verbs: &AgentVerbs,
    reference: &str,
    after: Option<String>,
) -> Vec<(usize, usize, String)> {
    let mut after = after;
    let mut rows = Vec::new();
    let mut previous_after = None;
    for _ in 0..100 {
        let page = window(verbs, reference, after);
        assert!(
            super::super::super::super::receipts::agent_receipt_fits(
                &page,
                MAX_AGENT_WORK_RESPONSE_BYTES
            )
            .unwrap()
        );
        let counts = &page.value["criterion_links_window"];
        let total = counts["total"].as_u64().unwrap();
        let earlier = counts["earlier"].as_u64().unwrap();
        let shown = counts["shown"].as_u64().unwrap();
        let remaining = counts["remaining"].as_u64().unwrap();
        assert_eq!(total, earlier + shown + remaining);
        let values = page.value["criterion_links"].as_array().unwrap();
        assert_eq!(shown, values.len() as u64);
        rows.extend(values.iter().map(|row| {
            (
                usize::try_from(row["criterion"].as_u64().unwrap()).unwrap(),
                usize::try_from(row["evidence_member"].as_u64().unwrap()).unwrap(),
                row["locator"].as_str().unwrap().into(),
            )
        }));
        after = counts["after"].as_str().map(str::to_owned);
        if after.is_none() {
            assert_eq!(remaining, 0);
            return rows;
        }
        assert!(shown > 0 && remaining > 0);
        assert_ne!(after, previous_after);
        previous_after = after.clone();
    }
    panic!("criterion links traversal did not reach EOF");
}

#[test]
fn criterion_links_window_traverses_real_seals_and_preserves_all_state() {
    for (count, evaluated) in [
        (0, false),
        (16, false),
        (17, false),
        (64, false),
        (70, true),
    ] {
        let (_home, verbs, path, project) = fixture();
        let reference = setup(&verbs);
        if evaluated {
            crate::verbs::tests::evaluate::enable(
                &path,
                &[crate::AcceptanceEvaluationMode::SameSession],
                1,
            );
        }
        finish(&verbs, &reference, count, evaluated);
        let store = SqliteStore::open(&path).unwrap();
        let (_, seal) = sealed(&store, &project, &reference);
        let expected = tuples(&seal);
        if evaluated {
            assert!(expected.len() > 64);
        }
        let connection = rusqlite::Connection::open(&path).unwrap();
        let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
        assert_eq!(traverse(&verbs, &reference, None), expected);
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&connection).unwrap(),
            before
        );
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

fn decode(token: &str) -> Value {
    let hex = token.strip_prefix("cl1-").unwrap();
    let bytes = hex
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect::<Vec<_>>();
    serde_json::from_slice(&bytes).unwrap()
}

fn encode(value: &Value) -> String {
    let mut token = "cl1-".to_owned();
    for byte in serde_json::to_vec(value).unwrap() {
        write!(token, "{byte:02x}").unwrap();
    }
    token
}

#[test]
fn criterion_links_window_cursor_refusals_and_legitimate_member_seek_write_nothing() {
    let (_home, verbs, path, project) = fixture();
    let reference = setup(&verbs);
    finish(&verbs, &reference, 17, false);
    let page = window(&verbs, &reference, None);
    let token = page.value["criterion_links_window"]["after"]
        .as_str()
        .unwrap();
    let original = decode(token);
    let store = SqliteStore::open(&path).unwrap();
    let (_, seal) = sealed(&store, &project, &reference);
    let expected = tuples(&seal);
    let mut invalid = vec![
        "bad".into(),
        format!("cl1-{}", "0".repeat(8192)),
        token.replacen("cl1-", "s1-", 1),
    ];
    for (field, value) in [
        ("project", json!("foreign")),
        ("work", json!(crate::WorkId::new())),
        ("run", json!(crate::WorkRunId::new())),
        ("seal", json!(crate::ObjectId::mint())),
        ("criterion", json!(0)),
        ("evidence_member", json!(0)),
        ("locator", json!(crate::ObjectId::mint())),
        ("extra", json!(true)),
    ] {
        let mut changed = original.clone();
        changed[field] = value;
        invalid.push(encode(&changed));
    }
    let other = setup_named(&verbs, "Foreign sealed mapping");
    finish(&verbs, &other, 17, false);
    let foreign = window(&verbs, &other, None).value["criterion_links_window"]["after"]
        .as_str()
        .unwrap()
        .to_owned();
    invalid.push(foreign);
    // A different canonical seal with the same body is not this run's
    // recorded completion seal, even if its work and run fields agree.
    let cloned_id = SqliteStore::open(&path)
        .unwrap()
        .append("completion_seal", &seal)
        .unwrap()
        .key()
        .clone();
    let mut forged = original.clone();
    forged["seal"] = json!(cloned_id);
    invalid.push(encode(&forged));
    let connection = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    for token in invalid {
        let error = verbs
            .show_records(
                &reference,
                &ShowInput {
                    criterion_links: true,
                    after: Some(token),
                    ..Default::default()
                },
                at(1000),
            )
            .unwrap_err();
        assert!(
            matches!(error.error, StoreError::WorkShowCursorInvalid { .. }),
            "{error:?}"
        );
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&connection).unwrap(),
            before
        );
    }
    let mut seek = original;
    seek["criterion"] = json!(expected[0].0);
    seek["evidence_member"] = json!(expected[0].1);
    seek["locator"] = json!(expected[0].2);
    assert_eq!(
        traverse(&verbs, &reference, Some(encode(&seek))),
        expected[1..]
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        before
    );
}

#[test]
fn criterion_links_window_refuses_missing_and_restored_stores_without_mutation() {
    let (home, verbs, path, project) = fixture();
    let absent = home.path().join("absent").join("work.db");
    let reader = AgentVerbs::new(
        absent.clone(),
        project.clone(),
        "unregistered".into(),
        SessionId("unregistered".into()),
        None,
    );
    assert!(matches!(
        reader
            .show_records(
                "w-000000000001",
                &ShowInput {
                    criterion_links: true,
                    ..Default::default()
                },
                at(1000)
            )
            .unwrap_err()
            .error,
        StoreError::StoreNotInitialized
    ));
    assert!(!absent.parent().unwrap().exists());
    let reference = setup(&verbs);
    finish(&verbs, &reference, 17, false);
    let snapshot = super::super::review::snapshot(&path, &project, &reference);
    let (restored, _store, restored_path) = super::super::review::load(home.path(), &snapshot);
    let connection = rusqlite::Connection::open(&restored_path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    let error = restored
        .show_records(
            &reference,
            &ShowInput {
                criterion_links: true,
                ..Default::default()
            },
            at(1000),
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("no native per-criterion"),
        "{error}"
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        before
    );
}

#[test]
fn criterion_links_window_is_read_only_for_an_unregistered_reader_and_lapsed_holder() {
    let (_home, verbs, path, project) = fixture();
    let reference = setup(&verbs);
    finish(&verbs, &reference, 17, false);
    let open = setup_named(&verbs, "Unrelated lapsed claim");
    let reader = AgentVerbs::new(
        path.clone(),
        project,
        "unregistered".into(),
        SessionId("unregistered".into()),
        None,
    );
    let default_reader = AgentVerbs::new_with_attribution(
        path.clone(),
        ProjectId("customer-workflow".into()),
        "unregistered".into(),
        SessionId(format!(
            "local-process-v1-42-{}",
            uuid::Uuid::new_v7(uuid::Timestamp::from_unix(
                uuid::NoContext,
                u64::try_from(at(0).timestamp()).unwrap(),
                0
            ))
        )),
        None,
        None,
        crate::work_service::WorkAttributionDefaults {
            actor: None,
            session: true,
        },
    );
    let connection = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    let _denied = super::super::read_only_reads::UnwritableStoreFiles::deny(&path);
    let bytes = std::fs::read(&path).unwrap();
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    let wal_bytes = std::fs::read(&wal).unwrap();
    assert_eq!(traverse(&reader, &reference, None).len(), 17);
    assert_eq!(traverse(&default_reader, &reference, None).len(), 17);
    let receipt = verbs
        .show_records(
            &reference,
            &ShowInput {
                criterion_links: true,
                ..Default::default()
            },
            at(4000),
        )
        .unwrap();
    assert!(
        receipt
            .reminders
            .iter()
            .any(|reminder| reminder.contains(&open))
    );
    assert!(
        crate::verbs::receipts::agent_receipt_fits(&receipt, MAX_AGENT_WORK_RESPONSE_BYTES)
            .unwrap()
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(std::fs::read(&wal).unwrap(), wal_bytes);
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        before
    );
}

#[test]
fn criterion_links_window_keeps_links_when_preview_fails_but_refuses_a_corrupt_seal() {
    let (_home, verbs, path, project) = fixture();
    let reference = setup(&verbs);
    finish(&verbs, &reference, 17, false);
    let store = SqliteStore::open(&path).unwrap();
    let (run, seal) = sealed(&store, &project, &reference);
    let expected = tuples(&seal);
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE objects SET canonical_json = CAST('{}' AS BLOB) WHERE object_id = ?1",
            [expected[0].2.as_str()],
        )
        .unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    let page = window(&verbs, &reference, None);
    assert!(page.value["criterion_links"][0]["preview_error_class"].is_string());
    assert_eq!(traverse(&verbs, &reference, None), expected);
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        before
    );
    connection
        .execute(
            "UPDATE objects SET canonical_json = CAST('{}' AS BLOB) WHERE object_id = ?1",
            [run.completion_seal.unwrap().as_str()],
        )
        .unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    assert!(
        verbs
            .show_records(
                &reference,
                &ShowInput {
                    criterion_links: true,
                    ..Default::default()
                },
                at(1000)
            )
            .is_err()
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        before
    );
}

#[test]
fn criterion_links_window_continues_the_historical_seal_after_reopen_and_later_completion() {
    let (_home, verbs, path, project) = fixture();
    let reference = setup(&verbs);
    finish(&verbs, &reference, 17, false);
    let page = window(&verbs, &reference, None);
    let basis = page.value["criterion_links_window"]["basis"].clone();
    let after = page.value["criterion_links_window"]["after"]
        .as_str()
        .unwrap()
        .to_owned();
    let store = SqliteStore::open(&path).unwrap();
    let (_, seal) = sealed(&store, &project, &reference);
    let expected = tuples(&seal);
    note(
        &verbs,
        &reference,
        "Late note preserves frozen navigation",
        6,
    );
    assert_eq!(
        window(&verbs, &reference, Some(after.clone())).value["criterion_links_window"]["basis"],
        basis
    );
    verbs
        .service
        .work_update_on(
            Some(&reference),
            WorkUpdateInput::Reopen {
                reason: "New execution".into(),
                idempotency_key: "window-reopen".into(),
            },
            at(7),
        )
        .unwrap();
    assert!(
        verbs
            .show_records(
                &reference,
                &ShowInput {
                    criterion_links: true,
                    ..Default::default()
                },
                at(8)
            )
            .unwrap_err()
            .to_string()
            .contains("no current frozen")
    );
    let historical = window(&verbs, &reference, Some(after.clone()));
    assert_eq!(historical.value["criterion_links_window"]["basis"], basis);
    let earlier = usize::try_from(
        historical.value["criterion_links_window"]["earlier"]
            .as_u64()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        traverse(&verbs, &reference, Some(after.clone())),
        expected[earlier..]
    );
    verbs
        .claim(
            ClaimInput {
                work_ref: reference.clone(),
                ttl_seconds: Some(3600),
                recover: None,
            },
            at(9),
        )
        .unwrap();
    note(&verbs, &reference, "Checkpoint in the later run", 9);
    verbs
        .done(
            DoneInput {
                work_ref: Some(reference.clone()),
                ..Default::default()
            },
            at(10),
        )
        .unwrap();
    assert_ne!(
        window(&verbs, &reference, None).value["criterion_links_window"]["basis"],
        basis
    );
    assert_eq!(
        window(&verbs, &reference, Some(after)).value["criterion_links_window"]["basis"],
        basis
    );
    assert_eq!(
        store
            .get::<crate::CompletionSeal>(&store.stored_seal_id(&seal))
            .unwrap()
            .unwrap(),
        seal
    );
}
