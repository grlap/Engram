//! Facts from one immutable acceptance vector, not inferred evidence support.

use super::{CompletionSeal, ObjectHash, SqliteStore, StoreError, WorkId, WorkRun, WorkRunId};

/// Valid projections keep `unlinked_positions.len() <= unlinked_count <= criteria_count`;
/// byte shedding removes positions only, never changes the frozen totals.
#[derive(Clone, Debug)]
pub(crate) struct WorkAcceptanceEvidence {
    pub criteria_count: usize,
    pub unlinked_count: usize,
    /// One-based positions in the seal's acceptance vector. Text is not identity.
    pub unlinked_positions: Vec<usize>,
}

impl WorkAcceptanceEvidence {
    /// One-based positions index the seal's acceptance vector. They remain
    /// aligned with item criteria because sealing rebuilds that vector in item
    /// order and completed work cannot be revised. A future acceptance-revision
    /// path must preserve or explicitly replace that alignment contract.
    pub(super) fn from_seal(seal: &CompletionSeal) -> Self {
        let unlinked_positions: Vec<_> = seal
            .acceptance
            .iter()
            .enumerate()
            .filter_map(|(index, result)| result.evidence.is_empty().then_some(index + 1))
            .collect();
        Self {
            criteria_count: seal.acceptance.len(),
            unlinked_count: unlinked_positions.len(),
            unlinked_positions,
        }
    }
}

pub(super) fn for_completed_run(
    store: &SqliteStore,
    run: Option<&WorkRun>,
    work: WorkId,
) -> Result<WorkAcceptanceEvidence, StoreError> {
    let (run_id, hash) = run
        .and_then(|run| run.completion_seal.as_ref().map(|hash| (run.run_id, hash)))
        .ok_or_else(|| {
            StoreError::InvalidWorkProjection("completed work has no completion seal".into())
        })?;
    for_seal(store, hash, work, run_id)
}

pub(super) fn for_seal(
    store: &SqliteStore,
    hash: &ObjectHash,
    work: WorkId,
    run: WorkRunId,
) -> Result<WorkAcceptanceEvidence, StoreError> {
    let seal: CompletionSeal = store.get(hash)?.ok_or_else(|| {
        StoreError::InvalidWorkProjection("acceptance disclosure has no canonical seal".into())
    })?;
    if seal.work_id != work || seal.run_id != run {
        return Err(StoreError::InvalidWorkProjection(
            "acceptance disclosure seal crosses its work or run binding".into(),
        ));
    }
    Ok(WorkAcceptanceEvidence::from_seal(&seal))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_service::test_support::at;

    #[test]
    fn criterion_disclosure_requires_seal_for_every_native_completed_run() {
        let directory = crate::test_support::temp_home().unwrap();
        let store = SqliteStore::open(directory.path().join("work.db")).unwrap();
        let work = WorkId(uuid::Uuid::now_v7());
        let run = WorkRun {
            schema_version: crate::domain::SCHEMA_VERSION,
            run_id: WorkRunId(uuid::Uuid::now_v7()),
            root_execution_id: crate::RootExecutionId(uuid::Uuid::now_v7()),
            work_id: work,
            generation: 1,
            executor: None,
            state: crate::WorkRunState::Completed,
            revision: 1,
            last_checkpoint: None,
            completion_seal: None,
            created_at: at(0),
            updated_at: at(1),
        };
        // Exercise the service's defensive branch without manufacturing an
        // invalid canonical history that storage rightly refuses first.
        for selected in [None, Some(&run)] {
            assert!(matches!(for_completed_run(&store, selected, work),
                Err(StoreError::InvalidWorkProjection(reason)) if reason == "completed work has no completion seal"));
        }
    }

    #[test]
    fn criterion_disclosure_for_seal_refuses_missing_and_each_cross_binding() {
        use crate::work_service::test_support::{completion_input, proposed_root, root_input};
        use crate::work_service::{LocalWorkService, WorkCompleteResult, WorkUpdateInput};
        let directory = crate::test_support::temp_home().unwrap();
        let path = directory.path().join("work.db");
        let service = LocalWorkService::new(
            path.clone(),
            crate::ProjectId("seal-binding".into()),
            "agent".into(),
            crate::SessionId("agent".into()),
            None,
        );
        let work = proposed_root(
            service
                .work_propose(root_input("Binding", "binding-root"), at(0))
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
        let WorkCompleteResult::Completed(completed) = service
            .work_complete(completion_input("delivered", "complete"), at(2))
            .unwrap()
        else {
            panic!("expected completion")
        };
        let mut store = SqliteStore::open(&path).unwrap();
        let seal: CompletionSeal = store.get(&completed.seal).unwrap().unwrap();
        let valid = for_seal(&store, &completed.seal, work.work_id, completed.run_id).unwrap();
        assert_eq!(valid.criteria_count, seal.acceptance.len());
        let mut other = seal.clone();
        other.work_id = WorkId(uuid::Uuid::now_v7());
        other.run_id = WorkRunId(uuid::Uuid::now_v7());
        let missing = crate::CanonicalObject::freeze(&other).unwrap();
        assert!(
            matches!(for_seal(&store, missing.hash(), work.work_id, completed.run_id),
            Err(StoreError::InvalidWorkProjection(reason)) if reason == "acceptance disclosure has no canonical seal")
        );
        let inserted = store.append("completion_seal", &other).unwrap();
        assert_eq!(inserted.hash(), missing.hash());
        for (expected_work, expected_run) in [
            (work.work_id, other.run_id),
            (other.work_id, completed.run_id),
            (work.work_id, completed.run_id),
        ] {
            assert!(
                matches!(for_seal(&store, inserted.hash(), expected_work, expected_run),
                Err(StoreError::InvalidWorkProjection(reason)) if reason == "acceptance disclosure seal crosses its work or run binding")
            );
        }
    }
}
