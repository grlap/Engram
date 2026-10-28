use super::*;
use crate::domain::{
    ExecutionSourceBasis, GitTreeId, VerificationApplicability, VerificationEvidenceMismatch,
};

#[test]
fn git_tree_no_mutation_shortcut_still_checks_the_newest_candidate() {
    for newest_local in [false, true] {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let work = bound_root(&mut store, "no-mutation-tree");
        let claim = claim(&mut store, &work, "runner", "claim", 2, 300);
        let source = |tree: char| ExecutionSourceBasis {
            workspace_id: "workspace".into(),
            source_revision: "equal-content".into(),
            source_root_generation: None,
            source_root_state: None,
            git_tree: Some(GitTreeId::parse(tree.to_string().repeat(40)).unwrap()),
        };
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let a = source('a');
        let producer = host_check_producer(
            &transaction,
            &work,
            &claim,
            "runner",
            "restricted",
            5,
            a.clone(),
        );
        let old = host_verification_of_producer_with_applicability(
            &transaction,
            &work,
            &claim,
            "runner",
            "restricted",
            5,
            5,
            a.clone(),
            producer,
            Some(VerificationApplicability::GitTree {
                tree: a.git_tree.unwrap(),
            }),
        );
        transaction.commit().unwrap();
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let b = source('b');
        let producer = host_check_producer(
            &transaction,
            &work,
            &claim,
            "runner",
            "newest",
            6,
            b.clone(),
        );
        let check = if newest_local {
            host_verification_of_producer(
                &transaction,
                &work,
                &claim,
                "runner",
                "newest",
                6,
                6,
                b,
                producer,
            )
        } else {
            old
        };
        transaction.commit().unwrap();
        checkpoint(
            &mut store,
            &work,
            &claim,
            "runner",
            "ack",
            7,
            std::slice::from_ref(&check),
        );
        let result = complete(&mut store, &work, &claim, "runner", &check, "complete", 8);
        if newest_local {
            result.unwrap();
        } else {
            assert!(
                matches!(result.unwrap_err(), StoreError::WorkBoundVerificationRefused { cause, .. } if cause.mismatch == VerificationEvidenceMismatch::GitTreeMismatch)
            );
        }
    }
}
