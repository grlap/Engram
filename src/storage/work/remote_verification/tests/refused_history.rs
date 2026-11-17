use super::*;
use crate::storage::{WorkRecordContent, WorkRecordFamily, WorkRecordKind};

fn refuse_destination(f: &mut Fixture, unsupported: bool) -> RemoteVerificationReason {
    if unsupported {
        f.request.completion.schema_version = REMOTE_COMPLETION_SCHEMA_VERSION + 1;
        RemoteVerificationReason::ProducerUnsupported
    } else {
        f.request.association.binding.work_revision += 1;
        RemoteVerificationReason::AssociationStale
    }
}

#[test]
fn remote_refused_history_existing_item_is_visible_without_run_credit() {
    for unsupported in [false, true] {
        let mut f = fixture();
        let work = f.request.association.binding.work_id;
        let root = f.store.get_work_item(work).unwrap().root_id;
        let root_feed = FeedId::RootWork(root);
        let root_before = f.store.work_feed_head(&root_feed).unwrap();
        let run_before = f.run_head();
        let reason = refuse_destination(&mut f, unsupported);
        let receipt = f.assert_uncredited(reason);
        assert_eq!(f.run_head(), run_before);
        assert_eq!(f.store.work_feed_head(&root_feed).unwrap(), root_before + 1);
        assert!(f.association(&receipt.operation).actor.run_id.is_none());
        assert!(producer_facts_on(&f.store.connection, &receipt.operation).is_err());

        let index = f
            .store
            .work_record_index(&f.project, work, WorkRecordKind::NotesWithGates)
            .unwrap();
        let rows = index
            .iter()
            .filter(|row| row.address.hash == receipt.operation)
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 1, "one item observation per delivery");
        let row = rows[0];
        assert_eq!(row.record_family, WorkRecordFamily::Observations);
        let WorkRecordContent::Note(note) =
            f.store.work_record_content(&f.project, work, row).unwrap()
        else {
            panic!("remote receipt is an observation note");
        };
        assert!(note.summary.contains(reason.word()));
        assert!(
            note.summary
                .contains(receipt.qualification.detail.as_ref().unwrap())
        );
        assert!(note.gate.is_none());
        assert!(note.verification.is_none());
        assert!(
            f.store
                .resolve_criterion_evidence_classified(
                    &f.project,
                    work,
                    f.request.association.binding.run_id,
                    receipt.operation.as_str(),
                    &index,
                )
                .unwrap()
                .is_err()
        );
        let snapshot = f.snapshot();
        assert_eq!(f.record_at(11).unwrap(), receipt);
        assert_eq!(f.snapshot(), snapshot, "exact retries append nothing");
        f.assert_healthy();
    }
}

#[test]
fn remote_refused_history_show_renders_reason_detail_and_original_times() {
    for unsupported in [false, true] {
        let directory = crate::test_support::temp_home().unwrap();
        let path = directory.path().join("remote-history.db");
        let store =
            SqliteStore::open_with_host_path_policy(&path, crate::HostPathPolicy::host_default())
                .unwrap();
        let mut f = fixture_in(store, 0, None);
        let work = f.request.association.binding.work_id;
        let reason = refuse_destination(&mut f, unsupported);
        let receipt = f.assert_uncredited(reason);
        let verbs = crate::verbs::AgentVerbs::new(
            path,
            f.project.clone(),
            "reader".into(),
            SessionId("reader".into()),
            None,
        );
        let show = verbs
            .show_records(
                &work.0.to_string(),
                &crate::verbs::ShowInput {
                    notes: true,
                    gates: true,
                    ..crate::verbs::ShowInput::default()
                },
                at(12),
            )
            .unwrap();
        let rows = show.value["notes"].as_array().unwrap();
        let row = rows
            .iter()
            .find(|row| row["locator"].as_str() == Some(receipt.operation.as_str()))
            .expect("show lists a refused delivery on its existing destination");
        let summary = row["summary"].as_str().unwrap();
        for fact in [
            "not credited:",
            reason.word(),
            receipt.qualification.detail.as_ref().unwrap(),
            &format!("finished_at={}", at(9).to_rfc3339()),
            &format!("associated_at={}", at(10).to_rfc3339()),
        ] {
            assert!(summary.contains(fact), "missing {fact}: {summary}");
        }
        assert_eq!(row["family"], "observations");
        assert!(show.text().contains(reason.word()));
        f.assert_healthy();
    }
}

#[test]
fn remote_refused_history_unknown_and_foreign_items_stay_project_only() {
    for unsupported in [false, true] {
        for foreign in [false, true] {
            let mut f = fixture();
            let original = f.request.association.binding.work_id;
            let other = f
                .store
                .create_work(
                    &root_request("private-project", "private destination", 1),
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
            f.request.association.binding.work_id = if foreign {
                other.work_id
            } else {
                WorkId::new()
            };
            if unsupported {
                f.request.completion.schema_version = REMOTE_COMPLETION_SCHEMA_VERSION + 1;
            }
            let reason = if unsupported {
                RemoteVerificationReason::ProducerUnsupported
            } else {
                RemoteVerificationReason::AssociationStale
            };
            let receipt = f.assert_uncredited(reason);
            let feeds: Vec<String> = f
                .store
                .connection
                .prepare("SELECT feed_kind FROM work_feed_entries WHERE object_id=?1")
                .unwrap()
                .query_map([receipt.operation.as_str()], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(feeds, ["project"]);
            for (project, work) in [(&f.project, original), (&other.project_id, other.work_id)] {
                let index = f
                    .store
                    .work_record_index(project, work, WorkRecordKind::NotesWithGates)
                    .unwrap();
                assert!(
                    !index
                        .iter()
                        .any(|row| row.address.hash == receipt.operation)
                );
            }
        }
    }
}

#[test]
fn remote_refused_history_old_project_only_records_are_not_backfilled() {
    let mut f = fixture();
    refuse_destination(&mut f, true);
    let record = RemoteVerificationAssociationRecord {
        schema_version: SCHEMA_VERSION,
        project_id: f.project.clone(),
        session_id: f.session.clone(),
        request: f.request.clone(),
        origin: None,
        qualification: RemoteVerificationQualification::refused(
            RemoteVerificationReason::ProducerUnsupported,
            Some("historical unsupported producer".into()),
        ),
        actor: actor("runner"),
        associated_at: at(10),
    };
    let object = CanonicalObject::mint(&record).unwrap();
    let tx = f.store.connection.transaction().unwrap();
    SqliteStore::insert_object(&tx, REMOTE_VERIFICATION_ASSOCIATION_KIND, &object).unwrap();
    super::super::super::feeds::append_to_project_feed(
        &tx,
        &f.project,
        REMOTE_VERIFICATION_ASSOCIATION_KIND,
        &object,
    )
    .unwrap();
    tx.commit().unwrap();
    let before = f.snapshot();
    let index = f
        .store
        .work_record_index(
            &f.project,
            f.request.association.binding.work_id,
            WorkRecordKind::NotesWithGates,
        )
        .unwrap();
    assert!(!index.iter().any(|row| row.address.hash == *object.key()));
    f.assert_healthy();
    assert_eq!(f.snapshot(), before);
}

#[test]
fn remote_refused_history_survives_store_import_and_projection_repair() {
    for unsupported in [false, true] {
        let directory = crate::test_support::temp_home().unwrap();
        let source = directory.path().join("source.db");
        let target = directory.path().join("target.db");
        let export = directory.path().join("store.jsonl");
        let store =
            SqliteStore::open_with_host_path_policy(&source, crate::HostPathPolicy::host_default())
                .unwrap();
        let mut f = fixture_in(store, 0, None);
        let work = f.request.association.binding.work_id;
        let reason = refuse_destination(&mut f, unsupported);
        let receipt = f.assert_uncredited(reason);
        let before = f
            .store
            .work_record_index(&f.project, work, WorkRecordKind::NotesWithGates)
            .unwrap();
        assert!(
            before
                .iter()
                .any(|row| row.address.hash == receipt.operation)
        );
        drop(f.store);
        crate::storage::migration::export_json(&source, &export).unwrap();
        crate::storage::migration::import_json(&export, &target).unwrap();
        assert!(
            SqliteStore::repair_rebuildable_projections(&target)
                .unwrap()
                .is_healthy()
        );
        f.store =
            SqliteStore::open_with_host_path_policy(&target, crate::HostPathPolicy::host_default())
                .unwrap();
        let after = f
            .store
            .work_record_index(&f.project, work, WorkRecordKind::NotesWithGates)
            .unwrap();
        assert_eq!(
            before
                .iter()
                .map(|row| (&row.address, &row.order, &row.locator, &row.record_family))
                .collect::<Vec<_>>(),
            after
                .iter()
                .map(|row| (&row.address, &row.order, &row.locator, &row.record_family))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            f.association(&receipt.operation).qualification,
            receipt.qualification
        );
        assert_eq!(f.count("verification_evidence"), 0);
        f.assert_healthy();
    }
}

#[test]
fn remote_refused_history_doctor_rejects_invalid_feed_membership() {
    for scenario in ["extra_run", "wrong_root", "unknown_root", "foreign_root"] {
        let mut f = fixture();
        let binding = f.request.association.binding.clone();
        let other = f
            .store
            .create_work(
                &root_request(
                    if scenario == "foreign_root" {
                        "private-project"
                    } else {
                        "project-a"
                    },
                    "another destination",
                    1,
                ),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        match scenario {
            "unknown_root" => f.request.association.binding.work_id = WorkId::new(),
            "foreign_root" => f.request.association.binding.work_id = other.work_id,
            _ => {}
        }
        f.request.completion.schema_version = REMOTE_COMPLETION_SCHEMA_VERSION + 1;
        let receipt = f.assert_uncredited(RemoteVerificationReason::ProducerUnsupported);
        let bad_feed = if scenario == "extra_run" {
            FeedId::RunExecution(binding.run_id)
        } else {
            FeedId::RootWork(other.root_id)
        };
        let tx = f.store.connection.transaction().unwrap();
        let position = super::super::super::feeds::reserve_feed_position(&tx, &bad_feed).unwrap();
        let (kind, id) = match bad_feed {
            FeedId::RunExecution(run) => ("run_execution", run.0.to_string()),
            FeedId::RootWork(root) => ("root_work", root.0.to_string()),
            FeedId::Project(_) => unreachable!(),
        };
        tx.execute(
            "INSERT INTO work_feed_entries(feed_kind,feed_id,position,object_kind,object_id,work_id)
             VALUES(?1,?2,?3,?4,?5,NULL)",
            params![kind, id, position.position, REMOTE_VERIFICATION_ASSOCIATION_KIND, receipt.operation.as_str()],
        ).unwrap();
        tx.commit().unwrap();
        let report = f.store.verify_all().unwrap();
        let label = format!("work_feed:{kind}:{id}:{}", position.position);
        assert!(
            report
                .invalid_work_records
                .iter()
                .any(|finding| finding == &format!("{label}:wrong_membership")
                    || finding == &format!("{label}:unsupported_or_unbound_object")),
            "{scenario}: {report:?}"
        );
    }
}
