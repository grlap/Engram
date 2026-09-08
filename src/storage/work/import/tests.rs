use super::*;
use crate::DevelopmentNoopRedactor;
use crate::storage::work::test_support::{actor, at};

mod corrections;

fn input() -> WorkImportInput {
    let payload = CanonicalObject::freeze(&serde_json::json!({"plan": "draft"})).unwrap();
    WorkImportInput {
        snapshot: WorkSourceSnapshot {
            schema_version: SCHEMA_VERSION,
            adapter_kind: "planner".into(),
            canonical_ref: "plan/item-1".into(),
            projected: crate::WorkSourceProjection {
                title: Some("External title".into()),
                body: Some("External body".into()),
                status: Some("closed".into()),
                owner: Some("foreign-owner".into()),
            },
            captured_at: now(),
            source_revision: Some("1".into()),
            fingerprint: "planner-revision-1".into(),
            canonical_url: None,
            payload_hash: payload.hash().clone(),
            raw: std::collections::BTreeMap::default(),
        },
        draft: Some(crate::domain::WorkImportDraft {
            title: "Authored local title".into(),
            outcome: "Authored local outcome".into(),
            acceptance: Vec::new(),
        }),
    }
}

#[test]
fn import_preview_guard_and_refresh_preserve_authored_work() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let project = ProjectId("import-test".into());
    let actor = actor("author");
    let input = input();
    let before = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
    let first = store.preview_work_import(&project, &input, now()).unwrap();
    assert_eq!(
        before,
        crate::storage::test_database_shape_snapshot(&store.connection).unwrap()
    );
    assert_eq!(first.effect, WorkImportEffect::Create);
    let receipt = store
        .apply_work_import(
            &project,
            &input,
            &first.preview_token,
            &actor,
            now(),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let work = store.get_work_item(receipt.work_id).unwrap();
    assert_eq!(work.title, "Authored local title");
    assert!(work.acceptance.is_empty());
    assert_eq!(work.origin, WorkOrigin::Imported);
    assert_eq!(work.lifecycle, crate::WorkLifecycle::Open);
    assert_eq!(work.assigned_to, None);
    let committed = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
    assert_eq!(
        receipt,
        store
            .apply_work_import(
                &project,
                &input,
                &first.preview_token,
                &actor,
                now(),
                &DevelopmentNoopRedactor
            )
            .unwrap()
    );
    assert_eq!(
        committed,
        crate::storage::test_database_shape_snapshot(&store.connection).unwrap()
    );
    let mut stranger = actor.clone();
    stranger.session_id = Some(crate::SessionId("different-session".into()));
    assert!(
        store
            .apply_work_import(
                &project,
                &input,
                &first.preview_token,
                &stranger,
                now(),
                &DevelopmentNoopRedactor
            )
            .is_err()
    );
    assert_eq!(
        committed,
        crate::storage::test_database_shape_snapshot(&store.connection).unwrap()
    );

    let mut refresh = input.clone();
    refresh.draft = None;
    refresh.snapshot.source_revision = Some("2".into());
    refresh.snapshot.projected.title = Some("Changed outside".into());
    let preview = store
        .preview_work_import(&project, &refresh, now())
        .unwrap();
    assert_eq!(preview.effect, WorkImportEffect::Notify);
    let notice = store
        .apply_work_import(
            &project,
            &refresh,
            &preview.preview_token,
            &actor,
            now(),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    assert!(notice.proposal.is_some());
    assert_eq!(work, store.get_work_item(work.work_id).unwrap());
    let found = store
        .lookup_work_source(&project, &key(&input.snapshot))
        .unwrap()
        .unwrap();
    assert_eq!(found.cited_snapshot, receipt.snapshot);
    assert_eq!(found.notice_count, 1);
    assert_eq!(
        found.latest_notice.unwrap().proposed_snapshot,
        notice.snapshot
    );
    assert_eq!(
        store
            .preview_work_import(&project, &refresh, now())
            .unwrap()
            .effect,
        WorkImportEffect::AlreadyKnown
    );
    let report = store.verify_all().unwrap();
    assert!(
        report.invalid_objects.is_empty(),
        "{:?}",
        report.invalid_objects
    );
    assert!(
        report.invalid_work_records.is_empty(),
        "{:?}",
        report.invalid_work_records
    );
}

#[test]
fn import_input_refuses_missing_draft_blank_criteria_and_duplicate_json() {
    let store = SqliteStore::open_in_memory().unwrap();
    let project = ProjectId("input-test".into());
    let mut input = input();
    input.draft.as_mut().unwrap().acceptance = vec![" ".into()];
    assert!(store.preview_work_import(&project, &input, now()).is_err());
    input.draft = None;
    assert!(store.preview_work_import(&project, &input, now()).is_err());
    assert!(
        crate::work_service::parse_work_import_input(br#"{"snapshot":null,"snapshot":null}"#)
            .is_err()
    );
}

fn now() -> DateTime<Utc> {
    at(0)
}

#[test]
fn import_notices_survive_two_planning_recoveries_once_without_execution() {
    let project = ProjectId("source-recovery".into());
    let mut source = SqliteStore::open_in_memory().unwrap();
    let original = input();
    let preview = source
        .preview_work_import(&project, &original, at(1))
        .unwrap();
    let receipt = source
        .apply_work_import(
            &project,
            &original,
            &preview.preview_token,
            &actor("importer"),
            at(1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    for revision in [2, 3] {
        let mut next = original.clone();
        next.draft = None;
        next.snapshot.source_revision = Some(revision.to_string());
        let preview = source
            .preview_work_import(&project, &next, at(revision))
            .unwrap();
        source
            .apply_work_import(
                &project,
                &next,
                &preview.preview_token,
                &actor("notifier"),
                at(revision),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
    }
    let expected = source
        .lookup_work_source(&project, &key(&original.snapshot))
        .unwrap()
        .unwrap();
    assert_eq!(expected.notice_count, 2);
    for round in [1, 2] {
        let saved = source
            .save_work_graph_snapshot(
                &project,
                &actor("saver"),
                None,
                crate::WorkGraphSnapshotDestinationKind::Stdout,
                at(10 + round),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        assert_eq!(saved.document.body.sources.len(), 3);
        let mut destination = SqliteStore::open_in_memory().unwrap();
        destination
            .load_work_graph_snapshot(
                &project,
                &actor("loader"),
                &serde_json::to_vec(&saved.document).unwrap(),
                false,
                at(20 + round),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        let found = destination
            .lookup_work_source(&project, &key(&original.snapshot))
            .unwrap()
            .unwrap();
        assert_eq!(found, expected);
        let item = destination.get_work_item(receipt.work_id).unwrap();
        assert!(item.restored);
        assert_eq!(item.active_run_id, None);
        let native_proposals: i64 = destination
            .connection
            .query_row(
                "SELECT COUNT(*) FROM objects WHERE object_kind = 'work_source_proposal'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(native_proposals, 0);
        assert!(destination.verify_all().unwrap().is_healthy());
        source = destination;
    }
}

#[test]
fn import_local_revision_race_and_refresh_draft_refuse_without_writes() {
    let project = ProjectId("import-race".into());
    let mut store = SqliteStore::open_in_memory().unwrap();
    let original = input();
    let preview = store
        .preview_work_import(&project, &original, at(1))
        .unwrap();
    let receipt = store
        .apply_work_import(
            &project,
            &original,
            &preview.preview_token,
            &actor("author"),
            at(1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut next = original.clone();
    next.snapshot.source_revision = Some("2".into());
    let before = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
    assert!(store.preview_work_import(&project, &next, at(2)).is_err());
    assert_eq!(
        before,
        crate::storage::test_database_shape_snapshot(&store.connection).unwrap()
    );
    next.draft = None;
    let preview = store.preview_work_import(&project, &next, at(2)).unwrap();
    store
        .revise_work(
            &crate::ReviseWorkRequest {
                work_id: receipt.work_id,
                expected_revision: 1,
                patch: crate::WorkRevisionPatch {
                    title: Some("Local authored change".into()),
                    ..Default::default()
                },
                authority: crate::WorkPlanningAuthority::Project,
                actor: actor("author"),
                idempotency_key: "local-revision".into(),
                updated_at: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let revised = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
    let error = store
        .apply_work_import(
            &project,
            &next,
            &preview.preview_token,
            &actor("author"),
            at(4),
            &DevelopmentNoopRedactor,
        )
        .unwrap_err();
    assert!(matches!(error, StoreError::InvalidWork(reason) if reason.contains("preview changed")));
    assert_eq!(
        revised,
        crate::storage::test_database_shape_snapshot(&store.connection).unwrap()
    );
}

#[test]
fn import_parser_rejects_unknown_source_fields_but_retains_bounded_raw() {
    let original = input();
    let mut value = serde_json::to_value(&original).unwrap();
    value["snapshot"]["unrecognised"] = serde_json::json!(true);
    assert!(
        crate::work_service::parse_work_import_input(&serde_json::to_vec(&value).unwrap()).is_err()
    );
    value["snapshot"]
        .as_object_mut()
        .unwrap()
        .remove("unrecognised");
    value["snapshot"]["raw"]["planner_data"] = serde_json::json!({"nested": [1, true, "context"]});
    let parsed =
        crate::work_service::parse_work_import_input(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(
        parsed.snapshot.raw["planner_data"],
        value["snapshot"]["raw"]["planner_data"]
    );
    value["snapshot"]["projected"]["acceptance"] =
        serde_json::json!(["Do not silently promote this"]);
    assert!(
        crate::work_service::parse_work_import_input(&serde_json::to_vec(&value).unwrap()).is_err()
    );
}

#[test]
fn import_summary_decodes_only_the_latest_native_capture() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let project = ProjectId("bounded-notices".into());
    let mut original = input();
    original.draft.as_mut().unwrap().acceptance = vec!["Z".into(), "A".into(), "Z".into()];
    let preview = store
        .preview_work_import(&project, &original, at(1))
        .unwrap();
    assert_eq!(preview.draft.as_ref().unwrap().acceptance, ["A", "Z"]);
    let receipt = store
        .apply_work_import(
            &project,
            &original,
            &preview.preview_token,
            &actor("importer"),
            at(1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    assert_eq!(
        store.get_work_item(receipt.work_id).unwrap().acceptance,
        preview.draft.unwrap().acceptance
    );
    let mut small_history_decodes = None;
    for revision in 2..=33 {
        let mut next = original.clone();
        next.draft = None;
        next.snapshot.source_revision = Some(revision.to_string());
        let preview = store
            .preview_work_import(&project, &next, at(revision))
            .unwrap();
        store
            .apply_work_import(
                &project,
                &next,
                &preview.preview_token,
                &actor("notifier"),
                at(revision),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        if revision == 3 {
            crate::canonical::reset_canonical_decode_count();
            let detail = store
                .work_source_detail(&project, &key(&original.snapshot))
                .unwrap()
                .unwrap();
            assert_eq!(detail.lookup.notice_count, 2);
            small_history_decodes = Some(crate::canonical::canonical_decode_count());
        }
    }
    let before = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
    crate::canonical::reset_canonical_decode_count();
    let detail = store
        .work_source_detail(&project, &key(&original.snapshot))
        .unwrap()
        .unwrap();
    let decodes = crate::canonical::canonical_decode_count();
    assert_eq!(
        Some(decodes),
        small_history_decodes,
        "decoding must not grow from two to thirty-two native notices"
    );
    // One selected notice permits its proposal, basis and two source bodies;
    // allow two decodes per object. Eight more cover item/citation resolution
    // and the two returned detail bodies. Omitted native notices add no budget.
    let selected = usize::from(detail.lookup.latest_notice.is_some());
    let decode_bound = 8 + 8 * selected;
    assert!(
        decodes <= decode_bound,
        "ordinary lookup decoded {decodes} canonical bodies"
    );
    assert_eq!(detail.lookup.notice_count, 32);
    assert_eq!(detail.notices_omitted, 31);
    assert_eq!(
        detail
            .latest_proposed_source
            .unwrap()
            .source_revision
            .as_deref(),
        Some("33")
    );
    crate::canonical::reset_canonical_decode_count();
    assert_eq!(
        store
            .work_source_for_item(receipt.work_id)
            .unwrap()
            .unwrap()
            .notice_count,
        32
    );
    assert!(crate::canonical::canonical_decode_count() <= decode_bound);
    assert_eq!(
        before,
        crate::storage::test_database_shape_snapshot(&store.connection).unwrap()
    );
}

#[test]
fn import_preview_is_read_only_under_writer_and_apply_rechecks_other_connection() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("engram.db");
    let project = ProjectId("import-contention".into());
    let mut first = SqliteStore::open(&database).unwrap();
    let mut second = SqliteStore::open(&database).unwrap();
    let service = crate::LocalWorkService::new(
        database.clone(),
        project.clone(),
        "reader".into(),
        crate::SessionId("reader".into()),
        None,
    );
    let original = input();
    second.connection.execute_batch("BEGIN IMMEDIATE").unwrap();
    let before = crate::storage::test_database_shape_snapshot(&first.connection).unwrap();
    let preview = service.preview_work_import(&original, at(1)).unwrap();
    assert!(
        service
            .lookup_work_source(&key(&original.snapshot), at(1))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        before,
        crate::storage::test_database_shape_snapshot(&first.connection).unwrap()
    );
    second.connection.execute_batch("ROLLBACK").unwrap();
    let winner = second
        .apply_work_import(
            &project,
            &original,
            &preview.preview_token,
            &actor("winner"),
            at(2),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let committed = crate::storage::test_database_shape_snapshot(&first.connection).unwrap();
    assert!(
        first
            .apply_work_import(
                &project,
                &original,
                &preview.preview_token,
                &actor("reader"),
                at(3),
                &DevelopmentNoopRedactor
            )
            .is_err()
    );
    assert_eq!(
        committed,
        crate::storage::test_database_shape_snapshot(&first.connection).unwrap()
    );
    assert_eq!(
        service
            .lookup_work_source(&key(&original.snapshot), at(3))
            .unwrap()
            .unwrap()
            .lookup
            .work_id,
        winner.work_id
    );
    let absent = home.path().join("absent.db");
    let missing = crate::LocalWorkService::new(
        absent.clone(),
        project,
        "reader".into(),
        crate::SessionId("reader".into()),
        None,
    );
    assert!(missing.preview_work_import(&original, at(1)).is_err());
    assert!(!absent.exists());
}

#[test]
fn import_recovery_refuses_duplicate_and_misbound_notices_without_writes() {
    let project = ProjectId("source-corruption".into());
    let mut source = SqliteStore::open_in_memory().unwrap();
    let mut input = input();
    input.snapshot.captured_at = at(1);
    let preview = source.preview_work_import(&project, &input, at(1)).unwrap();
    source
        .apply_work_import(
            &project,
            &input,
            &preview.preview_token,
            &actor("importer"),
            at(1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    input.draft = None;
    input.snapshot.captured_at = at(0);
    input.snapshot.source_revision = Some("2".into());
    let preview = source.preview_work_import(&project, &input, at(2)).unwrap();
    source
        .apply_work_import(
            &project,
            &input,
            &preview.preview_token,
            &actor("notifier"),
            at(2),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("saver"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    for (case, message) in [
        (0, "duplicate source-change notice across history layers"),
        (1, "source notice disagrees with its cited item"),
        (2, "source notice has a missing snapshot"),
        (
            3,
            "source notice changes source identity or predates capture",
        ),
        (
            4,
            "source notice changes source identity or predates capture",
        ),
    ] {
        let mut document = saved.document.clone();
        let crate::WorkGraphSnapshotRecordPayload::Native { history } =
            &mut document.body.records[0].payload
        else {
            panic!("native fixture")
        };
        let notice = history.source_notices[0].clone();
        match case {
            0 => history.source_notices.push(notice),
            1 => {
                history.source_notices[0].cited_snapshot =
                    CanonicalObject::freeze(&"different citation")
                        .unwrap()
                        .hash()
                        .clone();
            }
            2 => {
                history.source_notices[0].proposed_snapshot =
                    CanonicalObject::freeze(&"missing proposed source")
                        .unwrap()
                        .hash()
                        .clone();
            }
            3 => history.source_notices[0].recorded_at = at(-1),
            4 => history.source_notices[0].recorded_at = at(0),
            _ => unreachable!(),
        }
        // Rehash the edited body: the test must reach semantic binding checks,
        // not merely fail the outer document checksum.
        document.manifest.body_sha256 = CanonicalObject::freeze(&document.body)
            .unwrap()
            .hash()
            .clone();
        let mut destination = SqliteStore::open_in_memory().unwrap();
        let before = crate::storage::test_database_shape_snapshot(&destination.connection).unwrap();
        for dry_run in [true, false] {
            let error = destination
                .load_work_graph_snapshot(
                    &project,
                    &actor("loader"),
                    &serde_json::to_vec(&document).unwrap(),
                    dry_run,
                    at(4),
                    &DevelopmentNoopRedactor,
                )
                .unwrap_err();
            assert!(
                matches!(error, StoreError::InvalidGraphSnapshot(ref reason) if reason == message),
                "{error:?}"
            );
            assert_eq!(
                before,
                crate::storage::test_database_shape_snapshot(&destination.connection).unwrap()
            );
        }
    }
}
