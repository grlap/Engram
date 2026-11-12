//! Real custody queries continue after thirty seconds and refuse expired input.

use super::*;
use crate::task_sync::{delta::CustodyAuditReceipt, git::PendingQuery, origin::private, retention};
use std::time::Duration;

pub(super) fn expires_then_completes(
    store: &mut SqliteStore,
    binding: &Binding,
    request: &CustodyAuditRequest,
    command: &'static str,
) -> CustodyAuditReceipt {
    let before = state(store);
    let witness = private::read(&binding.store.database, &binding.project).unwrap();
    let inventory = retention::inventory(&binding.repository.git_directory).unwrap();
    {
        let delayed = PendingQuery::at(command, Duration::from_secs(61));
        let error = store
            .audit_task_sync_custody(request, &binding.actor, at(28))
            .unwrap_err();
        assert!(delayed.hits() > 0, "selected query was not reached");
        assert!(
            error.to_string().contains("exceeded its deadline"),
            "{error}"
        );
    }
    assert_eq!(state(store), before);
    assert_eq!(
        private::read(&binding.store.database, &binding.project).unwrap(),
        witness
    );
    assert_eq!(
        retention::inventory(&binding.repository.git_directory).unwrap(),
        inventory
    );
    let original = {
        let delayed = PendingQuery::at(command, Duration::from_secs(31));
        let receipt = store
            .audit_task_sync_custody(request, &binding.actor, at(30))
            .expect("a pending real audit query must survive thirty seconds");
        assert!(delayed.hits() > 0, "selected query was not reached");
        receipt
    };
    let repeated = store
        .audit_task_sync_custody(request, &binding.actor, at(31))
        .unwrap();
    assert_eq!(original.checkpoint, repeated.checkpoint);
    assert!(repeated.repeated && repeated.acknowledged);
    assert!(store.verify_all().unwrap().is_healthy());
    original
}

#[tokio::test]
async fn incoming_data_pin_query_survives_thirty_seconds_and_preserves_expiry() {
    let (_home, mut store, binding, _, request) = super::data_bootstrap::fixture().await;
    expires_then_completes(&mut store, &binding, &request, "show-ref");
}

#[tokio::test]
async fn abandoned_carrier_queries_survive_thirty_seconds_and_preserve_expiry() {
    for command in ["for-each-ref", "cat-file"] {
        let (_home, mut store, binding, id, mut request) = super::data_bootstrap::fixture().await;
        let CustodySeedSelection::IncomingDataCommit { commit } = request.seed else {
            unreachable!()
        };
        request.reason = CheckpointReason::NamedAudit;
        request.seed = CustodySeedSelection::AbandonFetchedCommit {
            binding: id,
            commit,
        };
        expires_then_completes(&mut store, &binding, &request, command);
    }
}
