use super::*;

#[test]
fn git_tree_execution_observe_preserves_the_actual_sighting_without_minting_credit() {
    for length in [40, 64] {
        for eligible in [false, true] {
            let mut fixture = fixture();
            let tree = crate::domain::GitTreeId::parse("a".repeat(length)).unwrap();
            let mut input =
                super::accounting::accounted(&fixture, "tree-sighting", "rev-a", "rev-b");
            if !eligible {
                input.policy_basis = ObservationPolicyBasis::AuditOnly {};
            }
            let ObservedOccurrence::InterTurnChange {
                source_change: ObservedSourceChange::ContentComparison { sighting, .. },
            } = &mut input.occurrence
            else {
                panic!("content comparison fixture");
            };
            sighting.source_basis.git_tree = Some(tree.clone());
            let first = observe(&mut fixture, input.clone(), 7).unwrap();
            assert_eq!(
                stored(&fixture.store, &first.observation)
                    .occurrence
                    .reported(),
                input.occurrence
            );
            assert_eq!(super::accounting::self_anchored(&first), eligible);
            let sighting = crate::storage::work::feeds::newest_measured_source_basis_on(
                &fixture.store.connection,
                fixture.claim.run_id,
                Some("workspace-A"),
                i64::MAX,
            )
            .unwrap();
            if eligible {
                assert_eq!(sighting.unwrap().1.git_tree, Some(tree));
            } else {
                assert!(
                    sighting.is_none(),
                    "audit-only records are not source sightings"
                );
            }
            let before = footprint(&fixture.store);
            assert_eq!(observe(&mut fixture, input, 8).unwrap(), first);
            assert_eq!(footprint(&fixture.store), before);
            assert_eq!(
                count(
                    &fixture.store,
                    "SELECT COUNT(*) FROM objects WHERE object_kind='verification_evidence'"
                ),
                0
            );
            let report = fixture.store.verify_all().unwrap();
            assert!(report.is_healthy(), "{report:?}");
        }
    }
}
