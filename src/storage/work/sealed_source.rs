//! Read-only source diagnostics anchored to a native completion seal. This
//! module never re-assesses completion or changes obligation accounting.

use super::current_run_feed_cut_on;
use crate::ObjectId;
use crate::domain::{CompletionSeal, VerificationSourceBasis, WorkId, WorkRunId};
use crate::storage::{SqliteStore, StoreError};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

mod query;

/// Agent-facing projection only. Comparison retains the complete basis separately.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct SealedSourceBasis {
    pub workspace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id_summary: Option<crate::source::SourceIdSummary>,
    pub source_root_generation: Option<i64>,
    pub source_root_state: Option<crate::domain::SourceRootState>,
}

impl From<&VerificationSourceBasis> for SealedSourceBasis {
    fn from(basis: &VerificationSourceBasis) -> Self {
        Self {
            workspace_id: basis.workspace_id.clone(),
            source_revision: basis.legacy_revision().map(str::to_owned),
            source_id_summary: basis.source_id().map(crate::source::SourceId::summary),
            source_root_generation: basis.source_root_generation,
            source_root_state: basis.source_root_state,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SealedSourceFact {
    pub position: i64,
    pub observation: ObjectId,
    pub comparison: String,
    pub source_basis: Option<SealedSourceBasis>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SealedSourceDiagnostic {
    pub project: crate::ProjectId,
    pub seal: ObjectId,
    pub evaluation: Option<ObjectId>,
    pub work: WorkId,
    pub run: WorkRunId,
    pub root_execution: crate::domain::RootExecutionId,
    pub claim: crate::domain::WorkClaimId,
    pub claim_fence: i64,
    pub evaluated_cut: Option<i64>,
    pub named_root_binding: Option<ObjectId>,
    pub completion_cut: i64,
    pub horizon: i64,
    pub judged: Option<SealedSourceBasis>,
    pub unavailable: Option<String>,
    pub total: usize,
    pub differences: usize,
    pub agreements: usize,
    pub incomparable: usize,
    /// Selected independently of the latest endpoint, so agreement cannot
    /// erase a previously reported difference.
    pub first_difference: Option<SealedSourceFact>,
    pub latest_endpoint: Option<SealedSourceFact>,
}

impl SealedSourceDiagnostic {
    pub(crate) const JUDGED_BASIS_UNAVAILABLE: &str =
        "judged source or retained matching root basis is unavailable";
}

pub(crate) struct SealedSourcePage {
    pub diagnostic: SealedSourceDiagnostic,
    pub rows: Vec<SealedSourceFact>,
    pub newer: usize,
}

impl SqliteStore {
    /// Derive a bounded diagnostic from existing records on one original run.
    /// A supplied horizon keeps historical pagination stable across appends
    /// and reopen. No current claim or current evaluation replaces the seal.
    pub(crate) fn sealed_source_page(
        &self,
        seal_id: &ObjectId,
        horizon: Option<i64>,
        before: Option<i64>,
        limit: usize,
    ) -> Result<SealedSourcePage, StoreError> {
        self.work_read_snapshot(|store| {
            let seal = store.completion_seal_at(seal_id)?;
            let head = current_run_feed_cut_on(&store.connection, seal.run_id)?.position;
            let horizon = horizon.unwrap_or(head);
            if horizon < seal.completion_cut.position || horizon > head {
                return Err(StoreError::InvalidWorkProjection(
                    "sealed source cursor has an invalid run horizon".into(),
                ));
            }
            let evaluation = store.completion_seal_evaluation(&seal)?;
            let judged = evaluation
                .as_ref()
                .map(|evaluation| {
                    super::acceptance_evaluation::sealed_source_basis_on(
                        &store.connection,
                        &seal,
                        evaluation,
                    )
                })
                .transpose()?
                .flatten();
            let unavailable = if evaluation.is_none() {
                Some("seal has no bound evaluation".into())
            } else if judged.is_none() {
                Some(SealedSourceDiagnostic::JUDGED_BASIS_UNAVAILABLE.into())
            } else {
                None
            };
            let mut diagnostic = SealedSourceDiagnostic {
                project: seal.root_execution.project_id.clone(),
                seal: seal_id.clone(),
                evaluation: seal.acceptance_evaluation.clone(),
                work: seal.work_id,
                run: seal.run_id,
                root_execution: seal.root_execution_id,
                claim: seal.claim_id,
                claim_fence: seal.claim_fence,
                evaluated_cut: evaluation.as_ref().map(|e| e.evaluated_cut.position),
                named_root_binding: evaluation
                    .as_ref()
                    .and_then(|e| e.named_root_binding.clone()),
                completion_cut: seal.completion_cut.position,
                horizon,
                judged: judged.as_ref().map(SealedSourceBasis::from),
                unavailable,
                total: 0,
                differences: 0,
                agreements: 0,
                incomparable: 0,
                first_difference: None,
                latest_endpoint: None,
            };
            let query = query::SourceQuery::new(&seal, &diagnostic, judged.as_ref())?;
            query.count(&store.connection, &mut diagnostic)?;
            diagnostic.first_difference = query
                .select(&store.connection, None, Some("differs"), false, 1)?
                .pop();
            diagnostic.latest_endpoint = query
                .select(&store.connection, None, Some("endpoint"), true, 1)?
                .pop();
            for fact in [
                &mut diagnostic.first_difference,
                &mut diagnostic.latest_endpoint,
            ]
            .into_iter()
            .flatten()
            {
                let records =
                    store.source_observations(&[(fact.position, fact.observation.clone())])?;
                fact.source_basis = records
                    .into_iter()
                    .next()
                    .and_then(|(_, _, record)| match record {
                        super::SourceObservationRecord::Admitted(observation) => {
                            observation.source_basis.map(VerificationSourceBasis::from)
                        }
                        super::SourceObservationRecord::Unadmitted(observation) => observation
                            .occurrence
                            .measured_sighting()
                            .map(|sighting| sighting.source_basis.clone()),
                    })
                    .as_ref()
                    .map(SealedSourceBasis::from);
            }
            let rows = query.select(&store.connection, before, None, true, limit.min(32))?;
            let newer = before
                .map(|before| query.newer(&store.connection, before))
                .transpose()?
                .unwrap_or(0);
            Ok(SealedSourcePage {
                diagnostic,
                rows,
                newer,
            })
        })
    }

    /// The canonical native seal belonging to the observation's original
    /// run, including when that item has subsequently reopened.
    pub(crate) fn source_observation_seal(
        &self,
        observation: &ObjectId,
    ) -> Result<Option<ObjectId>, StoreError> {
        self.connection.query_row(
            "SELECT completion.seal_id FROM objects observation
             JOIN work_feed_entries sighting
               ON sighting.feed_kind = 'run_execution'
              AND sighting.feed_id = json_extract(observation.canonical_json, '$.binding.run_id')
              AND sighting.object_id = observation.object_id
             JOIN work_completion_seals completion
               ON completion.run_id = sighting.feed_id
             JOIN objects seal ON seal.object_id = completion.seal_id
             WHERE observation.object_id = ?1
               AND sighting.feed_kind = 'run_execution'
               AND sighting.object_kind IN
                 ('execution_observation', 'unadmitted_execution_observation')
               AND sighting.position > json_extract(seal.canonical_json, '$.completion_cut.position')
             LIMIT 1",
            params![observation.as_str()], |row| row.get::<_, String>(0),
        ).optional()?.map(|id| ObjectId::from_stored(id.clone())
            .ok_or(StoreError::InvalidStoredKey(id))).transpose()
    }
}
