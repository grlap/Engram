//! A passed host check on the source an evaluation judged does not ask for a
//! resubmission. A host that starts an evaluator while the requesting turn is
//! still running reports that turn's checks after the evaluator's cut, and
//! the evaluator cannot re-read. A check that passed on the very revision the
//! evaluation declared it judged can only add support, so no verdict it would
//! contradict records through it. Exempt, and nothing else: the verification
//! itself, the environment record it links, and an obligation resolution it
//! satisfied. A failed or indeterminate check, a check on another revision,
//! or any other record after the cut still asks for a resubmission, as does a
//! passed check when the run was last sighted at another revision.

use std::collections::BTreeSet;

use rusqlite::{Connection, params};

use super::{
    NamedEvaluationRoot, ObjectId, SourceRootState, StoreError, VerificationEvidence,
    VerificationResult, WorkRunId, load_typed_work_object, revision_seen_through,
};
use crate::domain::{
    AcceptanceSourceBasis, WorkObligationResolution, WorkObligationResolutionEvent,
};

/// The records after an evaluation's cut that do not ask it to resubmit.
#[derive(Default)]
pub(super) struct ExemptChecks {
    verifications: BTreeSet<ObjectId>,
    environments: BTreeSet<ObjectId>,
}

impl ExemptChecks {
    /// The passed host checks after `position` on the declared revision, and
    /// the environment records they link. None without a declared revision,
    /// or when the run's newest sighting (within the named root, if any) is
    /// not at that revision: a check reported late for an older source must
    /// not exempt itself.
    pub(super) fn after(
        connection: &Connection,
        run_id: WorkRunId,
        position: i64,
        declared: Option<&AcceptanceSourceBasis>,
        root: Option<&NamedEvaluationRoot>,
    ) -> Result<Self, StoreError> {
        let mut exempt = Self::default();
        let Some(declared) = declared else {
            return Ok(exempt);
        };
        if revision_seen_through(connection, run_id, i64::MAX, root)?.as_deref()
            != Some(declared.fingerprint.as_str())
        {
            return Ok(exempt);
        }
        let mut statement = connection.prepare(
            "SELECT object_id FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND position > ?2
               AND object_kind = 'verification_evidence'
             ORDER BY position",
        )?;
        let rows = statement
            .query_map(params![run_id.0.to_string(), position], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for stored in rows {
            let hash = ObjectId::from_stored(stored.clone())
                .ok_or(StoreError::InvalidStoredKey(stored))?;
            let check: VerificationEvidence =
                load_typed_work_object(connection, &hash, "verification_evidence")?;
            if check.result != VerificationResult::Passed || !on_declared(&check, declared, root) {
                continue;
            }
            if let Some(environment) = check.environment.clone() {
                exempt.environments.insert(environment);
            }
            exempt.verifications.insert(hash);
        }
        Ok(exempt)
    }

    /// Whether the run-feed record `hash` of `kind` is exempt.
    pub(super) fn covers(
        &self,
        connection: &Connection,
        kind: &str,
        hash: &ObjectId,
    ) -> Result<bool, StoreError> {
        Ok(match kind {
            "verification_evidence" => self.verifications.contains(hash),
            "environment_evidence" => self.environments.contains(hash),
            "work_obligation_resolution" if !self.verifications.is_empty() => {
                let event: WorkObligationResolutionEvent =
                    load_typed_work_object(connection, hash, "work_obligation_resolution")?;
                matches!(
                    &event.resolution,
                    WorkObligationResolution::Satisfied { evidence, .. }
                        if self.verifications.contains(evidence)
                )
            }
            _ => false,
        })
    }
}

/// Whether `check` ran on the declared revision: the same content revision,
/// in the declared workspace when one is named, and within the claim's named
/// root when it has one.
fn on_declared(
    check: &VerificationEvidence,
    declared: &AcceptanceSourceBasis,
    root: Option<&NamedEvaluationRoot>,
) -> bool {
    let basis = &check.source_basis;
    basis.source_revision == declared.fingerprint
        && declared
            .workspace_id
            .as_ref()
            .is_none_or(|workspace| *workspace == basis.workspace_id)
        && root.is_none_or(|root| {
            basis.workspace_id == root.event.workspace_id
                && basis.source_root_generation == Some(root.event.generation)
                && basis.source_root_state == Some(SourceRootState::Named)
        })
}
