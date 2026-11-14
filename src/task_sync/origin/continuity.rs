//! Private custody prepared outside SQLite and checked inside its writer.

use std::path::{Path, PathBuf};

use super::private::{self, PrivateState};
use crate::storage::TaskSyncCustodyObservation as Observed;
use crate::task_sync::{
    invalid,
    settings::files::{Directory, OperationLock},
};
use crate::{ProjectId, storage::StoreError};
use std::collections::BTreeMap;

/// Holding this reservation serializes cooperating installation-local writers.
/// It is neither a file-identity proof nor cross-host execution authority.
pub(crate) struct PreparedContinuity {
    _lock: OperationLock,
    database: PathBuf,
    project: ProjectId,
    state: PrivateState,
    custody: BTreeMap<PathBuf, Observed>,
    request: Option<crate::ObjectId>,
    operation: Option<uuid::Uuid>,
    recovered_activation: Option<RecoveredActivation>,
}

struct RecoveredActivation {
    recovery: crate::ObjectId,
    successor: crate::ObjectId,
    request: crate::ObjectId,
}

impl PreparedContinuity {
    pub(crate) fn recovered_activation(
        &self,
    ) -> Option<(&crate::ObjectId, &crate::ObjectId, &crate::ObjectId)> {
        self.recovered_activation
            .as_ref()
            .map(|context| (&context.recovery, &context.successor, &context.request))
    }

    /// Local metadata only. The writer revalidates these exact canonical links;
    /// this context never exempts an external exchange from Git custody checks.
    pub(crate) fn capture_recovered_activation(
        &mut self,
        receipt: &crate::task_sync::origin::recovery::RecoveryReceipt,
        request: &crate::ObjectId,
    ) {
        self.custody.clear();
        self.request = Some(request.clone());
        self.operation = None;
        self.recovered_activation = Some(RecoveredActivation {
            recovery: receipt.recovery.clone(),
            successor: receipt.record.successor_origin.clone(),
            request: request.clone(),
        });
    }
    pub(crate) fn custody(&self) -> &BTreeMap<PathBuf, Observed> {
        &self.custody
    }
    pub(crate) fn admit_delta_resolution(
        &mut self,
        input: &crate::task_sync::exchange::InspectedDeltaResolution,
        request: &crate::ObjectId,
    ) -> Result<(), StoreError> {
        if self.request.as_ref() != Some(request)
            || self.operation
                != Some(
                    input
                        .operation()
                        .map_err(|error| invalid(error.to_string()))?,
                )
        {
            return Err(invalid(
                "delta resolution differs from reserved custody context",
            ));
        }
        self.custody
            .get_mut(&input.original.binding.repository.git_directory)
            .ok_or_else(|| invalid("delta resolution repository custody missing"))?
            .admit_delta_resolution(input, request)
    }
    pub(crate) fn request(&self) -> Option<&crate::ObjectId> {
        self.request.as_ref()
    }
    pub(crate) fn admit_evidence_recipe(
        &mut self,
        binding: &crate::task_sync::ledger::Binding,
        recipe: &crate::task_sync::ledger::EvidenceImportRecipe,
        request: &crate::ObjectId,
    ) -> Result<(), StoreError> {
        if self.request.as_ref() != Some(request)
            || self.operation != Some(recipe.intent()?.operation)
        {
            return Err(invalid(
                "evidence recipe differs from reserved custody context",
            ));
        }
        self.custody
            .get_mut(&binding.repository.git_directory)
            .ok_or_else(|| invalid("evidence recipe repository custody missing"))?
            .admit_evidence_recipe(binding, recipe, request)
    }
    pub(crate) fn operation(&self) -> Option<uuid::Uuid> {
        self.operation
    }

    pub(crate) fn admit_planning_successor(
        &mut self,
        binding: &crate::task_sync::ledger::Binding,
        retained: &crate::task_sync::retention::successor::RetainedSuccessor,
        request: &crate::ObjectId,
    ) -> Result<(), StoreError> {
        if self.request.as_ref() != Some(request)
            || self.operation != Some(retained.recipe.request.intent.operation)
        {
            return Err(invalid("successor differs from reserved custody context"));
        }
        self.custody
            .get_mut(&binding.repository.git_directory)
            .ok_or_else(|| invalid("successor repository custody missing"))?
            .admit_successor(binding, retained, request)
    }

    /// Observe existing E1-owned custody before acquiring SQLite's writer.
    /// Its immutable fields cannot be replaced by a caller-provided inventory.
    pub(crate) fn capture_custody(
        &mut self,
        connection: &rusqlite::Connection,
        repository: Option<&Path>,
        request: Option<&crate::ObjectId>,
        operation: Option<uuid::Uuid>,
    ) -> Result<(), StoreError> {
        self.capture_custody_inner(connection, repository, request, operation, false)
    }

    pub(crate) fn capture_cold_custody(
        &mut self,
        connection: &rusqlite::Connection,
        repository: Option<&Path>,
        request: Option<&crate::ObjectId>,
        operation: Option<uuid::Uuid>,
    ) -> Result<(), StoreError> {
        self.capture_custody_inner(connection, repository, request, operation, true)
    }

    fn capture_custody_inner(
        &mut self,
        connection: &rusqlite::Connection,
        repository: Option<&Path>,
        request: Option<&crate::ObjectId>,
        operation: Option<uuid::Uuid>,
        cold: bool,
    ) -> Result<(), StoreError> {
        if !connection.is_autocommit() {
            return Err(invalid(
                "observe origin custody outside the SQLite transaction",
            ));
        }
        let mut roots =
            crate::storage::task_sync_origin_repositories_on(connection, &self.project)?;
        if let Some(repository) = repository {
            roots.insert(repository.to_owned());
        }
        let mut custody = BTreeMap::new();
        for root in roots {
            let mut observed = if cold {
                Observed::capture_cold(&root)?
            } else {
                Observed::capture_for(&root, operation)?
            };
            if !cold {
                observed.capture_related_on(connection)?;
            }
            custody.insert(root.clone(), observed);
        }
        self.custody = custody;
        self.request = request.cloned();
        self.operation = operation;
        self.recovered_activation = None;
        Ok(())
    }

    pub(crate) fn admit_retained_construction(
        &mut self,
        connection: &rusqlite::Connection,
        request: &crate::task_sync::delta::CustodyAuditRequest,
    ) -> Result<(), StoreError> {
        if !connection.is_autocommit()
            || self.request.as_ref() != Some(&request.fingerprint()?)
            || self.operation != Some(request.operation)
        {
            return Err(invalid(
                "construction retirement differs from reserved audit context",
            ));
        }
        let mut admitted = false;
        for observed in self.custody.values_mut() {
            admitted |= observed.admit_retirement(connection, request)?;
        }
        if !admitted {
            return Err(invalid(
                "construction retirement repository custody missing",
            ));
        }
        Ok(())
    }
    pub(crate) fn database(&self) -> &Path {
        &self.database
    }
    pub(crate) fn project(&self) -> &ProjectId {
        &self.project
    }
    pub(crate) fn state(&self) -> &PrivateState {
        &self.state
    }

    /// Refresh only from the capability-checked private owner, never caller data.
    pub(crate) fn refresh(&mut self) -> Result<(), StoreError> {
        self.state = private::read(&self.database, &self.project)?;
        Ok(())
    }
    pub(crate) fn acquire(database: &Path, project: &ProjectId) -> Result<Self, StoreError> {
        crate::project_home_of(database, project)
            .ok_or_else(|| invalid("origin enrollment requires a project-local store"))?;
        let parent = database
            .parent()
            .ok_or_else(|| invalid("origin store has no parent"))?;
        let directory = Directory::open(parent, false)
            .map_err(|_| invalid("origin private directory unavailable"))?
            .ok_or_else(|| invalid("origin private directory missing"))?;
        let lock = directory
            .operation_lock()
            .map_err(|_| invalid("origin continuity is busy or its lock is unsafe"))?;
        Ok(Self {
            _lock: lock,
            database: database.to_owned(),
            project: project.clone(),
            state: private::read(database, project)?,
            custody: BTreeMap::new(),
            request: None,
            operation: None,
            recovered_activation: None,
        })
    }

    /// Bounded private-state comparison before writer admission and immediately
    /// before explicit enrollment cancellation under its unchanged SQL writer.
    pub(crate) fn recheck(&self) -> Result<(), StoreError> {
        if private::read(&self.database, &self.project)? != self.state {
            return Err(invalid(
                "origin private custody changed before writer admission",
            ));
        }
        Ok(())
    }
}
