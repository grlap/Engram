use super::*;
use crate::work_service::test_support::{at, completion_input, proposed_root, root_input};
use crate::work_service::{MAX_CRITERION_LINKS, WorkCriterionLinkInput, WorkUpdateInput};

#[test]
fn criterion_links_recovery_matches_unlinked_acceptance_recovery() {
    let home = crate::test_support::temp_home().unwrap();
    let path = home.path().join("work.db");
    let service = LocalWorkService::new(
        path.clone(),
        crate::ProjectId("recovery-shape".into()),
        "agent".into(),
        crate::SessionId("agent".into()),
        None,
    );
    let work = proposed_root(
        service
            .work_propose(root_input("Recovery", "root"), at(0))
            .unwrap(),
    );
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "claim".into(),
            },
            at(1),
        )
        .unwrap();
    let store = SqliteStore::open(path).unwrap();
    let mut item = store.get_work_item(work.work_id).unwrap();
    let claim = store.current_work_claim(work.work_id).unwrap().unwrap();
    // Native planning normalizes this shape. Exercise the defensive boundary
    // on an in-memory value, without manufacturing invalid durable history.
    item.acceptance = vec![" unnormalized criterion ".into()];
    let actor = service.actor("work_complete", "complete ambient local work");
    let mut input = completion_input("Delivered", "complete");
    let ordinary = validated_acceptance(&store, &item, &claim, &input, &actor, &[]).unwrap_err();
    input.link_basis = Some(item.revision);
    input.links = vec![WorkCriterionLinkInput {
        criterion: 1,
        locator: "not-reached".into(),
    }];
    let linked = validated_acceptance(&store, &item, &claim, &input, &actor, &[]).unwrap_err();
    for error in [ordinary, linked] {
        assert!(matches!(error, StoreError::WorkCompletionRecoveryRequired {
            work: id, cause: crate::WorkCompletionRecoveryCause::MissingAcceptance { criterion }
        } if id == work.work_id && criterion == " unnormalized criterion "));
    }
}

#[test]
fn criterion_links_bound_index_work_and_input_count() {
    let home = crate::test_support::temp_home().unwrap();
    let path = home.path().join("work.db");
    let project = crate::ProjectId("link-cost".into());
    let service = LocalWorkService::new(
        path.clone(),
        project,
        "agent".into(),
        crate::SessionId("agent".into()),
        None,
    );
    let work = proposed_root(
        service
            .work_propose(root_input("Measured", "root"), at(0))
            .unwrap(),
    );
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "claim".into(),
            },
            at(1),
        )
        .unwrap();
    let hash: ObjectHash = serde_json::from_value(
        service
            .work_update(
                WorkUpdateInput::Evidence {
                    summary: "Measured artifact".into(),
                    refs: vec![],
                    attach: None,
                    idempotency_key: "note".into(),
                },
                at(2),
            )
            .unwrap()
            .receipt
            .result,
    )
    .unwrap();
    let store = SqliteStore::open(&path).unwrap();
    let item = store.get_work_item(work.work_id).unwrap();
    let claim = store.current_work_claim(work.work_id).unwrap().unwrap();
    let actor = service.actor("work_complete", "complete ambient local work");
    let mut request = completion_input("Delivered", "complete");
    request.link_basis = Some(item.revision);
    request.links = vec![
        WorkCriterionLinkInput {
            criterion: 1,
            locator: hash.to_string()
        };
        MAX_CRITERION_LINKS
    ];
    validate_shape(&request).unwrap();
    crate::canonical::reset_canonical_decode_count();
    let result = acceptance(
        &store,
        &item,
        &claim,
        &request,
        &actor,
        std::slice::from_ref(&hash),
    )
    .unwrap()
    .unwrap();
    let decodes = crate::canonical::canonical_decode_count();
    // load_note and work_evidence_kind_on each validate the native object;
    // the whole-record index's item decode happens once, not per citation.
    assert!(
        decodes <= 2 * request.links.len() + 4,
        "{decodes} canonical decodes"
    );
    assert_eq!(result[0].evidence, vec![hash.to_string()]);
    let connection = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    request.links.push(request.links[0].clone());
    assert!(matches!(service.work_complete(request, at(3)),
        Err(StoreError::WorkCriterionLinkInvalid { criterion: None, reason }) if reason.contains("at most 64")));
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        before
    );
}
