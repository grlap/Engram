use super::*;
use crate::domain::{GitTreeId, VerificationApplicability, VerificationEvidenceMismatch};

fn tree_basis(tree: Option<char>) -> ExecutionSourceBasis {
    let mut source = basis("workspace-B", "equal-content", Some(9));
    source.git_tree = tree.map(|c| GitTreeId::parse(c.to_string().repeat(40)).unwrap());
    source
}

fn record(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    key: &str,
    second: i64,
    tree: char,
    restricted: bool,
) -> ObjectId {
    let source = tree_basis(Some(tree));
    let applicability = restricted.then(|| VerificationApplicability::GitTree {
        tree: source.git_tree.clone().unwrap(),
    });
    let transaction = begin(store);
    let producer = host_check_producer(
        &transaction,
        work,
        claim,
        "runner",
        key,
        second,
        source.clone(),
    );
    let id = host_verification_of_producer_with_applicability(
        &transaction,
        work,
        claim,
        "runner",
        key,
        second,
        second,
        source,
        producer,
        applicability,
    );
    transaction.commit().unwrap();
    id
}

fn quiet(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    tree: Option<char>,
    second: i64,
) {
    let transaction = begin(store);
    host_check_producer(
        &transaction,
        work,
        claim,
        "runner",
        "quiet-sighting",
        second,
        tree_basis(tree),
    );
    transaction.commit().unwrap();
}

#[test]
fn git_tree_newest_restricted_check_requires_the_current_quiet_sighting_tree() {
    for (tree, mismatch) in [
        (Some('b'), VerificationEvidenceMismatch::GitTreeMismatch),
        (None, VerificationEvidenceMismatch::GitTreeMissing),
    ] {
        let (mut store, work, claim, _) = named_work();
        let check = record(&mut store, &work, &claim, "restricted-a", 5, 'a', true);
        quiet(&mut store, &work, &claim, tree, 6);
        checkpoint(
            &mut store,
            &work,
            &claim,
            "runner",
            "ack",
            7,
            std::slice::from_ref(&check),
        );
        let error =
            complete(&mut store, &work, &claim, "runner", &check, "complete", 8).unwrap_err();
        assert!(
            matches!(error, StoreError::WorkBoundVerificationRefused { cause, .. } if cause.mismatch == mismatch)
        );
        let evidence = store.load_verification_evidence(&check).unwrap();
        assert_eq!(
            store
                .verification_applicability_assessment(&evidence)
                .unwrap(),
            Some(mismatch)
        );
    }
}

#[test]
fn git_tree_unused_old_restriction_does_not_veto_newer_sufficient_local_check() {
    let (mut store, work, claim, _) = named_work();
    let old = record(&mut store, &work, &claim, "restricted-a", 5, 'a', true);
    let newest = record(&mut store, &work, &claim, "local-b", 6, 'b', false);
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "ack",
        7,
        &[old, newest.clone()],
    );
    complete(&mut store, &work, &claim, "runner", &newest, "complete", 8).unwrap();
    let report = store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
}

#[test]
fn git_tree_matching_quiet_sighting_qualifies_without_commit_identity() {
    let (mut store, work, claim, _) = named_work();
    let check = record(&mut store, &work, &claim, "restricted-a", 5, 'a', true);
    quiet(&mut store, &work, &claim, Some('a'), 6);
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "ack",
        7,
        std::slice::from_ref(&check),
    );
    complete(&mut store, &work, &claim, "runner", &check, "complete", 8).unwrap();
}

#[test]
fn git_tree_restricted_writer_refuses_barrier_removed_after_open_without_partial_writes() {
    let (mut store, work, claim, _) = named_work();
    let check = record(&mut store, &work, &claim, "restricted-a", 5, 'a', true);
    let evidence = store.load_verification_evidence(&check).unwrap();
    store
        .connection
        .execute_batch("DROP INDEX objects_verification_git_tree_applicability_v1")
        .unwrap();
    let before = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
    let transaction = begin(&mut store);
    let error = crate::storage::work::completion::append_control_verification_evidence_on(
        &transaction,
        &evidence,
    )
    .unwrap_err();
    assert_eq!(
        crate::host::store_error_code(&error),
        "control_projection_invalid"
    );
    drop(transaction);
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).unwrap(),
        before
    );
}

#[test]
fn git_tree_stored_reader_refuses_unknown_members_without_rewriting_legacy_bytes() {
    let (mut store, work, claim, _) = named_work();
    let check = record(&mut store, &work, &claim, "strict-reader", 5, 'a', true);
    let evidence = store.load_verification_evidence(&check).unwrap();
    let mut legacy = evidence.clone();
    legacy.applicability = None;
    legacy.source_basis.git_tree = None;
    let mut old = serde_json::to_value(&legacy).unwrap();
    old.as_object_mut().unwrap().remove("applicability");
    old["source_basis"]
        .as_object_mut()
        .unwrap()
        .remove("git_tree");
    let old = CanonicalObject::freeze(&old).unwrap();
    let decoded: VerificationEvidence = CanonicalObject::decode_bytes(old.bytes()).unwrap();
    assert_eq!(
        CanonicalObject::freeze(&decoded).unwrap().bytes(),
        old.bytes()
    );
    let original: Vec<u8> = store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id = ?1",
            [check.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    for value in [
        serde_json::json!("private-verification-value"),
        serde_json::Value::Null,
    ] {
        let mut changed = serde_json::to_value(&evidence).unwrap();
        changed["unsupported_member"] = value;
        let changed = CanonicalObject::freeze(&changed).unwrap();
        store
            .connection
            .execute(
                "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
                rusqlite::params![changed.bytes(), check.as_str()],
            )
            .unwrap();
        let error = store
            .load_verification_evidence(&check)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("unknown field `unsupported_member`"),
            "{error}"
        );
        let report = store.verify_all().unwrap();
        assert!(
            report
                .invalid_control_records
                .iter()
                .any(|finding| finding.starts_with(&format!(
                    "verification_evidence:{check}:unknown field `unsupported_member`"
                )))
        );
        assert!(!format!("{report:?}").contains("private-verification-value"));
        store
            .connection
            .execute(
                "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
                rusqlite::params![&original, check.as_str()],
            )
            .unwrap();
        assert_eq!(store.load_verification_evidence(&check).unwrap(), evidence);
    }
}

#[test]
fn git_tree_repair_and_migration_preserve_restricted_record_ids_and_bytes() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("source.db");
    let (mut store, work, claim, _) = named_work_in(SqliteStore::open(&database).unwrap());
    let check = record(&mut store, &work, &claim, "transfer", 5, 'a', true);
    let evidence = store.load_verification_evidence(&check).unwrap();
    let original: Vec<u8> = store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id = ?1",
            [check.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    drop(store);
    let report = SqliteStore::repair_rebuildable_projections(&database).unwrap();
    assert!(report.is_healthy(), "{report:?}");
    let export = directory.path().join("transfer.jsonl");
    crate::storage::migration::export_json(&database, &export).unwrap();
    let target = directory.path().join("target.db");
    crate::storage::migration::import_json(&export, &target).unwrap();
    for path in [&database, &target] {
        let store = SqliteStore::open(path).unwrap();
        assert!(
            store
                .verification_git_tree_applicability_available()
                .unwrap()
        );
        assert_eq!(store.load_verification_evidence(&check).unwrap(), evidence);
        let actual: Vec<u8> = store
            .connection
            .query_row(
                "SELECT canonical_json FROM objects WHERE object_id = ?1",
                [check.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(actual, original);
        let report = store.verify_all().unwrap();
        assert!(report.is_healthy(), "{report:?}");
    }
}

#[test]
fn git_tree_attach_discloses_nonqualification_without_granting_it() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("attach.db");
    let (mut store, work, claim, _) = named_work_in(SqliteStore::open(&database).unwrap());
    let check = record(&mut store, &work, &claim, "attach", 5, 'a', true);
    quiet(&mut store, &work, &claim, Some('b'), 6);
    let evidence = store.load_verification_evidence(&check).unwrap();
    let count = || {
        store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM objects WHERE object_kind = 'verification_evidence'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
    };
    let before = count();
    let service = LocalWorkService::new(
        database,
        work.project_id.clone(),
        "runner".into(),
        SessionId("runner".into()),
        None,
    );
    let attached = service
        .work_update_on(
            Some(&work.short_ref),
            crate::work_service::WorkUpdateInput::Evidence {
                summary: String::new(),
                refs: vec![],
                attach: Some(crate::work_service::WorkEvidenceAttachInput {
                    evidence: check.to_string(),
                }),
                idempotency_key: "attach-restricted".into(),
            },
            at(7),
        )
        .unwrap();
    assert_eq!(attached.receipt.result["attribution_only"], true);
    assert_eq!(
        attached.receipt.result["applicability"],
        serde_json::to_value(&evidence.applicability).unwrap()
    );
    assert_eq!(
        attached.receipt.result["applicability_mismatch"],
        "git_tree_mismatch"
    );
    assert_eq!(count(), before);
    assert_eq!(
        store
            .verification_applicability_assessment(&evidence)
            .unwrap(),
        Some(VerificationEvidenceMismatch::GitTreeMismatch)
    );
}
