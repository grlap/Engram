//! Successful strict obligation reads shared only within one verification snapshot.

use super::{
    Connection, HashMap, ObjectId, StoreError, WorkId, WorkObligationId, WorkObligationRecord,
    WorkRunId, load_work_obligation_records_on, work_obligation_run_id_on,
};

#[derive(Clone)]
pub(in crate::storage::work) struct VerifiedObligationBinding {
    pub definition: ObjectId,
    pub resolution: Option<ObjectId>,
    pub project: crate::ProjectId,
    pub root: WorkId,
    pub run: WorkRunId,
}

impl From<WorkObligationRecord> for VerifiedObligationBinding {
    fn from(record: WorkObligationRecord) -> Self {
        Self {
            definition: record.definition_id,
            resolution: record.resolution_id,
            project: record.obligation.project_id,
            root: record.obligation.root_id,
            run: record.obligation.run_id,
        }
    }
}

/// Constructed inside the work verifier's transaction and dropped before it ends.
/// Full records are transient; only their validated identity/membership facts live
/// across the two audit traversals. Failed loads publish no facts.
pub(in crate::storage::work) struct ObligationAudit<'a> {
    connection: &'a Connection,
    runs: HashMap<WorkRunId, HashMap<WorkObligationId, VerifiedObligationBinding>>,
    #[cfg(test)]
    reuse: bool,
}

impl<'a> ObligationAudit<'a> {
    pub fn new(connection: &'a Connection) -> Self {
        debug_assert!(!connection.is_autocommit());
        Self {
            connection,
            runs: HashMap::new(),
            #[cfg(test)]
            reuse: true,
        }
    }

    #[cfg(test)]
    pub fn without_reuse(connection: &'a Connection) -> Self {
        Self {
            reuse: false,
            ..Self::new(connection)
        }
    }

    pub fn binding(
        &mut self,
        obligation_id: WorkObligationId,
    ) -> Result<VerifiedObligationBinding, StoreError> {
        #[cfg(test)]
        if !self.reuse {
            return super::load_work_obligation_by_id_on(self.connection, obligation_id)
                .map(Into::into);
        }

        // Preserve the ordinary loader's projection-based run lookup, including
        // its missing-id and malformed-run errors, even for a previously read id.
        let run_id = work_obligation_run_id_on(self.connection, obligation_id)?;
        let bindings = match self.runs.entry(run_id) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let records = load_work_obligation_records_on(self.connection, run_id, None);
                #[cfg(test)]
                super::super::cost::obligation_audit_run_loaded(run_id, records.is_ok());
                let bindings = records?
                    .into_iter()
                    .map(|record| (record.obligation.obligation_id, record.into()))
                    .collect();
                entry.insert(bindings)
            }
        };
        bindings.get(&obligation_id).cloned().ok_or_else(|| {
            StoreError::InvalidWorkProjection(format!(
                "obligation {} disappeared during its verified load",
                obligation_id.0
            ))
        })
    }
}
