use super::*;
use crate::domain::GitTreeId;

#[test]
fn git_tree_binding_checks_each_target_and_preserves_the_restriction() {
    for target_tree in [None, Some('b'), Some('a')] {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let mut a = held(&mut store, "tree-original", HOLDER, 9, "R1", 2);
        let mut b = held(&mut store, "tree-target", HOLDER, 10, "R1", 4);
        a.git_tree = Some(GitTreeId::parse("a".repeat(40)).unwrap());
        b.git_tree = target_tree.map(|c| GitTreeId::parse(c.to_string().repeat(40)).unwrap());
        let original = native_check(&mut store, &a, "R1", 20);
        // A native check also records the target's newest actual sighting.
        let target_check = native_check(&mut store, &b, "R1", 21);
        let mut target = target(&store, &b, "R1", vec![1]);
        target.sighting.observation = stored(&store, &target_check).producer_observation;
        let input = request(&original, "R1", vec![target]);
        let before = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
        let outcome = bind(&mut store, &input, 30);
        if target_tree == Some('a') {
            let VerificationBindOutcome::Bound(receipt) = outcome else {
                panic!("matching tree refused");
            };
            let bound = &receipt.bound[0].verification;
            let record = stored(&store, bound);
            assert_eq!(
                record.applicability,
                stored(&store, &original).applicability
            );
            assert_eq!(record.source_basis.git_tree, b.git_tree);
            // This core helper has no host-operation replay wrapper. A
            // second application refuses the existing bound copy.
            let after = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
            let repeated = refused(bind(&mut store, &input, 31));
            assert_eq!(repeated.targets.len(), 1);
            assert_eq!(repeated.targets[0].reason, Reason::AlreadyBound);
            assert_eq!(
                crate::storage::test_database_shape_snapshot(&store.connection).unwrap(),
                after
            );
            assert_eq!(stored(&store, bound), record);
            let report = store.verify_all().unwrap();
            assert!(report.is_healthy(), "{report:?}");
        } else {
            let refusal = refused(outcome);
            assert_eq!(
                refusal.targets[0].reason,
                if target_tree.is_none() {
                    Reason::GitTreeMissing
                } else {
                    Reason::GitTreeMismatch
                }
            );
            assert_eq!(
                crate::storage::test_database_shape_snapshot(&store.connection).unwrap(),
                before
            );
        }
    }
}
