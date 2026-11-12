//! Explicit cold recovery creates a successor without adopting prior custody.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

use super::{cut_on, insert_enrollment_on, load_on};
use crate::task_sync::{
    invalid,
    origin::{
        Enrollment,
        private::recovery::{RecoveryRecipe, RecoveryReservation},
        recovery::{RECOVERY_KIND, RecoveryReceipt, RecoveryRecord, RecoveryRequest},
    },
    progress::{ExportCut, FeedAnchor},
};
use crate::{
    ActorContext, ObjectId, ProjectId,
    storage::{SqliteStore, StoreError},
};

#[cfg(test)]
mod tests;

#[cfg(test)]
use crate::CanonicalObject;

#[cfg(test)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum Fault {
    BeforeCommit,
    AfterCommit,
}

#[cfg(test)]
thread_local! { static FAULT: std::cell::Cell<Option<Fault>> = const { std::cell::Cell::new(None) }; }

#[cfg(test)]
fn fail_at(point: Fault) -> Result<(), StoreError> {
    FAULT.with(|fault| {
        if fault.get() == Some(point) {
            fault.set(None);
            Err(invalid("injected origin recovery interruption"))
        } else {
            Ok(())
        }
    })
}

impl SqliteStore {
    /// Cancel only an exact recovery recipe proven absent from canonical history.
    ///
    /// # Errors
    /// Refuses committed enrollment/recovery, changed intent or private custody.
    pub fn cancel_task_sync_origin_recovery(
        &mut self,
        project: &ProjectId,
        actor: &ActorContext,
        request: &RecoveryRequest,
    ) -> Result<bool, StoreError> {
        crate::storage::admit_live_actor_session(actor)?;
        if request.operation.is_nil() {
            return Err(invalid("origin recovery requires a nonempty operation"));
        }
        let database = std::path::PathBuf::from(
            self.connection
                .path()
                .filter(|path| !path.is_empty())
                .ok_or_else(|| {
                    invalid("origin recovery cancellation requires a file-backed store")
                })?,
        );
        let reservation = RecoveryReservation::acquire(&database, project)?;
        let pending = reservation.recipe()?;
        let transaction = self.begin_work_mutation()?;
        if super::super::ledger::origin_recovery_on(&transaction, project, request.operation)?
            .is_some()
            || super::view::operation_exists(&transaction, request.operation)?
        {
            return Err(invalid(
                "origin recovery committed; retry its receipt, never cancel",
            ));
        }
        let Some(recipe) = pending else {
            return Ok(false);
        };
        if recipe.recovery.request != *request {
            return Err(invalid("origin recovery cancellation intent differs"));
        }
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM objects WHERE object_id IN (?1,?2))",
            params![
                recipe.recovery_id.as_str(),
                recipe.recovery.successor_origin.as_str()
            ],
            |row| row.get(0),
        )?;
        if exists
            || load_on(&transaction, project)?
                .is_none_or(|origin| origin.origin_id != request.predecessor)
        {
            return Err(invalid(
                "origin recovery cancellation canonical basis differs",
            ));
        }
        reservation.cancel_after_absence_check(&recipe)?;
        Ok(true)
    }

    /// Recover to a newly minted local successor. A completed retry returns the
    /// original canonical receipt, never recreates lost private custody.
    ///
    /// # Errors
    /// Refuses changed recovery intent, lineage, private custody or ledger basis,
    /// unsafe private files and incomplete post-commit acknowledgement.
    pub fn recover_task_sync_origin(
        &mut self,
        project: &ProjectId,
        actor: &ActorContext,
        request: &RecoveryRequest,
        now: DateTime<Utc>,
    ) -> Result<RecoveryReceipt, StoreError> {
        crate::storage::admit_live_actor_session(actor)?;
        if request.operation.is_nil() {
            return Err(invalid("origin recovery requires a nonempty operation"));
        }
        let database = std::path::PathBuf::from(
            self.connection
                .path()
                .filter(|path| !path.is_empty())
                .ok_or_else(|| invalid("origin recovery requires a file-backed store"))?,
        );
        let reservation = RecoveryReservation::acquire(&database, project)?;
        let pending = reservation.recipe()?;
        let original =
            super::super::ledger::origin_recovery_on(&self.connection, project, request.operation)?;
        if let Some(original) = &original {
            if original.record.request != *request {
                return Err(invalid(
                    "origin recovery operation was reused with different intent",
                ));
            }
            if pending.is_none() {
                // Receipt replay is history only. Normal exchange admission must
                // still prove this installation's current independent witness.
                return Ok(original.clone());
            }
        }
        let recipe = if let Some(recipe) = pending {
            if recipe.recovery.request != *request {
                return Err(invalid("another exact origin recovery remains pending"));
            }
            recipe
        } else {
            let (cut, bindings) = self.work_read_snapshot(|store| {
                let predecessor = load_on(&store.connection, project)?
                    .ok_or_else(|| invalid("origin recovery predecessor is not enrolled"))?;
                if predecessor.origin_id != request.predecessor {
                    return Err(invalid("origin recovery predecessor changed"));
                }
                Ok((
                    cut_on(&store.connection, project)?,
                    super::super::ledger::recovery_bindings_on(&store.connection, project)?,
                ))
            })?;
            let enrollment = Enrollment {
                project_id: project.clone(),
                predecessor_origin_id: Some(request.predecessor.clone()),
                enrollment_operation_id: request.operation,
                actor: actor.clone(),
                enrolled_at: now,
            };
            RecoveryRecipe {
                schema_version: 1,
                enrollment,
                recovery_id: ObjectId::mint(),
                recovery: RecoveryRecord {
                    schema_version: 1,
                    project_id: project.clone(),
                    request: request.clone(),
                    successor_origin: ObjectId::mint(),
                    predecessor_cut: cut,
                    custody: reservation.custody()?,
                    bindings,
                    actor: actor.clone(),
                    recovered_at: now,
                },
            }
        };
        reservation.retain(&recipe)?;
        let transaction = self.begin_work_mutation()?;
        let original =
            super::super::ledger::origin_recovery_on(&transaction, project, request.operation)?;
        let origin = if let Some(original) = original {
            if original.recovery != recipe.recovery_id || original.record != recipe.recovery {
                return Err(invalid(
                    "origin recovery canonical result differs from retained recipe",
                ));
            }
            let origin = load_on(&transaction, project)?
                .ok_or_else(|| invalid("committed origin recovery successor is missing"))?;
            if origin.origin_id != recipe.recovery.successor_origin
                || origin.enrollment != recipe.enrollment
            {
                return Err(invalid("committed origin recovery successor has changed"));
            }
            origin
        } else {
            reservation.check_predecessor(&recipe)?;
            super::super::ledger::check_origin_recovery_on(&transaction, &recipe.recovery)?;
            let origin = insert_enrollment_on(&transaction, &recipe.enrollment_object()?)?;
            let recovery = recipe.recovery_object()?;
            SqliteStore::insert_object(&transaction, RECOVERY_KIND, &recovery)?;
            super::super::super::feeds::append_to_project_feed(
                &transaction,
                project,
                RECOVERY_KIND,
                &recovery,
            )?;
            origin
        };
        let cut = recovery_cut_on(&transaction, project, &recipe.recovery_id)?;
        #[cfg(test)]
        fail_at(Fault::BeforeCommit)?;
        transaction.commit()?;
        #[cfg(test)]
        fail_at(Fault::AfterCommit)?;
        reservation.finish(&recipe, &origin, cut)?;
        Ok(RecoveryReceipt {
            recovery: recipe.recovery_id,
            record: recipe.recovery,
        })
    }
}

fn recovery_cut_on(
    connection: &Connection,
    project: &ProjectId,
    recovery: &ObjectId,
) -> Result<ExportCut, StoreError> {
    let position = connection.query_row(
        "SELECT position FROM work_feed_entries WHERE feed_kind='project' AND feed_id=?1
         AND object_kind=?2 AND object_id=?3 AND work_id IS NULL",
        params![project.0, RECOVERY_KIND, recovery.as_str()],
        |row| row.get(0),
    )?;
    Ok(ExportCut {
        position,
        anchor: Some(FeedAnchor {
            object_kind: RECOVERY_KIND.into(),
            object_id: recovery.clone(),
        }),
    })
}
