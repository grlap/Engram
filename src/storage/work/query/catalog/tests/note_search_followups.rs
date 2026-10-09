use super::*;
use crate::storage::work::record_windows::{WorkRecordContent, WorkRecordKind};

#[test]
fn note_search_verification_reads_producer_once_and_show_retains_facts() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let work = store
        .create_work(
            &root_request("producer-search", "Unrelated", 0),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let held = claim(&mut store, &work, "holder", "claim", 1, 600);
    let evidence = host_verification(
        &mut store,
        &work,
        &held,
        "holder",
        "producer-search-token",
        VerificationKind::Test,
        VerificationResult::Passed,
        2,
    );
    // A second note matches the same summary; the first locator wins and
    // the item remains a single catalog member.
    host_verification(
        &mut store,
        &work,
        &held,
        "holder",
        "producer-search-token-later",
        VerificationKind::Test,
        VerificationResult::Passed,
        3,
    );
    let index = store
        .work_record_index(
            &work.project_id,
            work.work_id,
            WorkRecordKind::NotesWithGates,
        )
        .unwrap();
    let entry = index
        .iter()
        .find(|entry| entry.address.hash == evidence)
        .unwrap();
    for text in [
        "host observed producer-search-token",
        "command:producer-search-token",
    ] {
        cost::start();
        let (page, total, _, _, _, matches) = store
            .query_work_catalog_continuation(
                &work.project_id,
                at(4),
                &WorkCatalogQuery {
                    search: Some(text.into()),
                    limit: 10,
                    ..WorkCatalogQuery::default()
                },
                None,
            )
            .unwrap();
        let measured = cost::finish();
        assert_eq!(total, 1);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].work.work_id, work.work_id);
        assert_eq!(matches[&work.work_id.0].locator, entry.locator);
        assert_eq!(matches[&work.work_id.0].family, entry.record_family);
        assert_eq!(
            measured["total"]["typed_work_object_decodes"]["execution_observation"],
            1
        );
    }
    cost::start();
    let content = store
        .work_record_content(&work.project_id, work.work_id, entry)
        .unwrap();
    let show_cost = cost::finish();
    assert_eq!(
        show_cost["total"]["typed_work_object_decodes"]["execution_observation"],
        2
    );
    let WorkRecordContent::Note(note) = content else {
        panic!("verification note")
    };
    let facts = note.verification.unwrap();
    assert_eq!(facts.result, VerificationResult::Passed);
    assert_eq!(facts.check_kind, VerificationKind::Test);
    assert_eq!(facts.source_revision, "revision-as-it-stands");
    assert_eq!(facts.producer_outcome, ExecutionOutcome::Succeeded);

    let verification: crate::domain::VerificationEvidence =
        crate::storage::work::feeds::load_typed_work_object(
            &store.connection,
            &evidence,
            "verification_evidence",
        )
        .unwrap();
    store
        .connection
        .execute_batch("SAVEPOINT corrupt_producer")
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
            rusqlite::params![b"{}".as_slice(), verification.producer_observation.as_str()],
        )
        .unwrap();
    assert!(
        store
            .query_work_catalog(
                &work.project_id,
                at(4),
                &WorkCatalogQuery {
                    search: Some("unrelated negative search".into()),
                    limit: 10,
                    ..WorkCatalogQuery::default()
                }
            )
            .is_err(),
        "search still validates the producer even when no note matches"
    );
    assert!(
        store
            .work_record_content(&work.project_id, work.work_id, entry)
            .is_err()
    );
    store
        .connection
        .execute_batch("ROLLBACK TO corrupt_producer; RELEASE corrupt_producer")
        .unwrap();
    assert!(
        store
            .work_record_content(&work.project_id, work.work_id, entry)
            .is_ok()
    );
}
