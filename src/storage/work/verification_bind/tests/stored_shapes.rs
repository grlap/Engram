//! The other shapes the removed operation stored: a tree and a restriction, an
//! original from a registered or remote producer, several targets, and a
//! criterion the check did not suit. Doctor, the host reads, projection repair
//! and migration keep reading them.

use super::*;
use crate::domain::{
    GitTreeId, LegacyRegisteredVerificationPostRun, LegacyRemoteRunInput,
    REMOTE_COMPLETION_SCHEMA_VERSION, RegisteredVerificationAssociation,
    RegisteredVerificationCheck, RegisteredVerificationInput, RegisteredVerificationInputOutcome,
    RegisteredVerificationOutcome, RegisteredVerificationRequest, RegisteredVerificationRoot,
    RegisteredVerificationRun, RemoteRunCompletion, RemoteRunMode, RemoteRunOrigin,
    RemoteRunResult, RemoteRunVerdict, RemoteVerificationAssociation, RemoteVerificationPurpose,
    RemoteVerificationRequest, RemoteVerificationUse,
};

/// Stores a bind of `original` to `b` and holds its record to doctor, both
/// host reads and the assessment, each naming the original's producer.
fn assert_bound_original_reads(
    store: &mut SqliteStore,
    b: &Held,
    original: &ObjectId,
    revision: &str,
) -> ObjectId {
    let input = request(
        original,
        revision,
        vec![target(store, b, revision, vec![1])],
    );
    let receipt = store_bound(store, &input, 30);
    let bound = receipt.bound[0].verification.clone();
    let record = stored(store, &bound);
    let producer = stored(store, original).producer_observation;
    assert_eq!(record.producer_observation, producer);
    assert!(record.bound_from.is_some());

    let read = satisfying(&binding_page(store, b));
    assert_eq!(read.record, bound);
    assert_eq!(read.producer.record, producer);
    assert!(read.bound.is_some());
    let rows = verification_page(store, b).rows;
    assert!(
        rows.iter()
            .any(|row| row.record == bound && row.bound.is_some()),
        "{rows:?}"
    );
    let assessment = store
        .verification_assessment(b.work.work_id, &bound, None, 8)
        .expect("assessment")
        .expect("a verification record");
    assert_eq!(
        assessment.bound.as_ref().map(|bound| &bound.verification),
        Some(original)
    );
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
    bound
}

/// A record bound under a tree keeps the original's restriction and the
/// target's measured tree, through doctor, repair and migration.
#[test]
fn a_tree_carrying_bound_record_keeps_its_restriction() {
    let directory = crate::test_support::temp_home().expect("directory");
    let database = directory.path().join("source.db");
    let tree = GitTreeId::parse("a".repeat(40)).unwrap();
    let bound = {
        let mut store = SqliteStore::open(&database).expect("store");
        let mut a = held(&mut store, "tree-original", HOLDER, 9, "R1", 2);
        let mut b = held(&mut store, "tree-target", HOLDER, 10, "R1", 4);
        a.git_tree = Some(tree.clone());
        b.git_tree = Some(tree.clone());
        let original = native_check(&mut store, &a, "R1", 20);
        // A native check also records the target's newest sighting, at the tree.
        let target_check = native_check(&mut store, &b, "R1", 21);
        let mut bound_target = target(&store, &b, "R1", vec![1]);
        bound_target.sighting.observation = stored(&store, &target_check).producer_observation;
        let input = request(&original, "R1", vec![bound_target]);
        let receipt = store_bound(&mut store, &input, 30);
        let bound = receipt.bound[0].verification.clone();
        let record = stored(&store, &bound);
        let restriction = stored(&store, &original).applicability;
        assert!(restriction.is_some());
        assert_eq!(record.applicability, restriction);
        assert_eq!(
            record.source_basis.legacy_basis().unwrap().git_tree,
            Some(tree.clone())
        );
        let report = store.verify_all().expect("doctor");
        assert!(report.is_healthy(), "{report:?}");
        bound
    };
    let imported = repair_and_migrate(directory.path(), &database);
    let record = stored(&imported, &bound);
    assert!(record.applicability.is_some());
    assert_eq!(
        record.source_basis.legacy_basis().unwrap().git_tree,
        Some(tree)
    );
}

/// A record bound from a registered host run names that run as its producer.
#[test]
fn a_record_bound_from_a_registered_run_reads_through_doctor_and_the_host_reads() {
    let directory = crate::test_support::temp_home().expect("directory");
    let database = directory.path().join("source.db");
    let commit = "c".repeat(40);
    let revision = format!("git-commit:{commit}");
    let bound = {
        let mut store = SqliteStore::open(&database).expect("store");
        let a = held(&mut store, "item-a", HOLDER, 9, &revision, 2);
        let b = held(&mut store, "item-b", HOLDER, 10, &revision, 4);
        let host = open_work_host(&mut store, HOLDER, at(7));
        let command = "cargo test --lib";
        let request = RegisteredVerificationRequest {
            operation_key: "termal-run:test-bound-original".into(),
            registration: RegisteredVerificationAssociation {
                host_run_id: "test-bound-original".into(),
                request_identity: "bound-original".into(),
                registered_at: at(8),
                initiating_session_id: host.session_id.clone(),
                owning_session_id: None,
                binding: binding_of(&store, &a),
                root: RegisteredVerificationRoot {
                    workspace_id: ROOT.into(),
                    generation: a.generation,
                    state: SourceRootState::Named,
                },
            },
            input: RegisteredVerificationInput::Commit { commit },
            run: RegisteredVerificationRun {
                mode: "focused".into(),
                command: command.into(),
                started_at: at(8),
                finished_at: at(9),
                outcome: RegisteredVerificationOutcome::Passed,
            },
            checks: vec![RegisteredVerificationCheck {
                stage: "focused".into(),
                check_kind: VerificationKind::Test,
                command: command.into(),
                check_fingerprint: check_fingerprint(command),
                started_at: Some(at(8)),
                completed_at: Some(at(9)),
                outcome: RegisteredVerificationOutcome::Passed,
                summary: None,
                refs: vec![],
                counts: None,
            }],
            post_run: LegacyRegisteredVerificationPostRun {
                outcome: RegisteredVerificationInputOutcome::Clean,
                clean_before: true,
                clean_after: true,
                dirty_paths: vec![],
                disposition: None,
            }
            .into(),
        };
        let receipt = store
            .record_registered_verification(
                &a.work.project_id,
                &host.session_id,
                &host.connection_token,
                crate::storage::HostSessionAuthority::Work,
                &actor(HOLDER),
                &request,
                at(10),
            )
            .expect("the registered run");
        let original = receipt.verifications[0].record_id.clone();
        assert_eq!(
            stored(&store, &original).producer_observation,
            receipt.producer
        );
        assert_bound_original_reads(&mut store, &b, &original, &revision)
    };
    let imported = repair_and_migrate(directory.path(), &database);
    assert!(stored(&imported, &bound).bound_from.is_some());
}

/// A record bound from a credited remote delivery names that delivery as its
/// producer, on the original's run.
#[test]
fn a_record_bound_from_a_credited_remote_check_reads_through_doctor_and_the_host_reads() {
    let directory = crate::test_support::temp_home().expect("directory");
    let database = directory.path().join("source.db");
    let commit = "c".repeat(40);
    let revision = format!("git-commit:{commit}");
    let bound = {
        let mut store = SqliteStore::open(&database).expect("store");
        let a = held(&mut store, "item-a", HOLDER, 9, &revision, 2);
        let b = held(&mut store, "item-b", HOLDER, 10, &revision, 4);
        let delivery = RemoteVerificationRequest {
            operation_key: "remote:item-a".into(),
            completion: RemoteRunCompletion {
                schema_version: REMOTE_COMPLETION_SCHEMA_VERSION,
                origin: RemoteRunOrigin {
                    host_id: "dl560".into(),
                    runner_session_id: "native-root".into(),
                },
                run_id: "test-remote".into(),
                mode: RemoteRunMode::Full,
                result: RemoteRunResult {
                    verdict: RemoteRunVerdict::Pass,
                    terminal: true,
                    exit: Some(0),
                    reason: "completed".into(),
                },
                input: LegacyRemoteRunInput {
                    commit: Some(commit.clone()),
                    ..LegacyRemoteRunInput::default()
                }
                .into(),
                finished_at: Some(at(20)),
            },
            association: RemoteVerificationAssociation {
                binding: binding_of(&store, &a),
                root: RegisteredVerificationRoot {
                    workspace_id: ROOT.into(),
                    generation: 9,
                    state: SourceRootState::Named,
                },
                intended_use: RemoteVerificationUse::CommittedInput {
                    expected_commit: commit,
                    purpose: RemoteVerificationPurpose::Verification,
                },
            },
        };
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("delivery transaction");
        let receipt = super::super::super::remote_verification::record_remote_verification_on(
            &transaction,
            &a.work.project_id,
            &SessionId(HOLDER.into()),
            &actor(HOLDER),
            &delivery,
            at(21),
        )
        .expect("delivery");
        transaction.commit().expect("commit delivery");
        assert!(receipt.qualification.credited, "{receipt:?}");
        let original = receipt.verifications[0].record_id.clone();
        let bound = assert_bound_original_reads(&mut store, &b, &original, &revision);
        assert_eq!(
            stored(&store, &bound).producer_observation,
            receipt.operation
        );
        bound
    };
    let imported = repair_and_migrate(directory.path(), &database);
    assert!(stored(&imported, &bound).bound_from.is_some());
}

/// One stored bind with two targets, one criterion the check did not suit, and
/// the original's claim released since: its receipt stays matched to its
/// records, in order, across a reopen, repair and migration.
#[test]
fn a_stored_bind_of_two_targets_stays_healthy_after_its_original_claim_ended() {
    let directory = crate::test_support::temp_home().expect("directory");
    let database = directory.path().join("source.db");
    let mut store = SqliteStore::open(&database).expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
    let c = held_with(
        &mut store,
        "item-c",
        HOLDER,
        11,
        "R1",
        6,
        VerificationRequirement {
            check_kind: VerificationKind::Build,
            check_fingerprint: None,
        },
    );
    let original = native_check(&mut store, &a, "R1", 20);
    let input = request(
        &original,
        "R1",
        vec![
            target(&store, &b, "R1", vec![1]),
            target(&store, &c, "R1", vec![1]),
        ],
    );
    let receipt = store_bound(&mut store, &input, 30);
    assert_eq!(receipt.bound[0].work_id, b.work.work_id);
    assert_eq!(receipt.bound[1].work_id, c.work.work_id);
    assert!(receipt.bound[0].criteria[0].eligible);
    let unsuited = &receipt.bound[1].criteria[0];
    assert!(!unsuited.eligible);
    assert!(unsuited.binding.is_some());
    assert!(unsuited.reason.is_some());
    release(&mut store, &a, 40);
    assert!(store.verify_all().expect("doctor").is_healthy());
    drop(store);
    let store = SqliteStore::open(&database).expect("reopened");
    assert!(store.verify_all().expect("doctor").is_healthy());
    drop(store);
    let imported = repair_and_migrate(directory.path(), &database);
    for bound in &receipt.bound {
        assert!(stored(&imported, &bound.verification).bound_from.is_some());
    }
}
