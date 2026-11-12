use super::super::verification::applicability::{
    MeasuredDestination, binding_destination_on, measure_binding_destination,
};
use crate::storage::work::task_sync::evidence::acceptance::{
    AcceptanceUseRequest, Connection, History, ProjectId, Refusal, Result, inspect, invalid,
    object_on, params,
};
use crate::storage::work::task_sync::evidence::{
    ADMISSION_KIND, IMPORTED_ORIGINALS, is_imported_only,
};
use crate::task_sync::evidence::acceptance::AcceptanceUseSummary;
use crate::{ActorContext, SessionId};
use crate::{CanonicalObject, ObjectId};
use chrono::{DateTime, Utc};

const KIND: &str = "task_sync_acceptance_use";

mod consumption;
pub(crate) use consumption::frozen_results_on;
pub(crate) use consumption::qualify_on as qualify_consumption_on;
pub(crate) use consumption::validate_retirement_on;

use crate::task_sync::evidence::acceptance::AcceptanceUseDecision as Decision;

impl crate::storage::SqliteStore {
    /// Record an explicit historical-use assessment. This is neither a new
    /// evaluation nor a native completion, and never changes project policy.
    ///
    /// # Errors
    /// Refuses stale local authority, unresolved originals and changed retries.
    pub fn associate_imported_acceptance(
        &mut self,
        project: &ProjectId,
        session: &SessionId,
        actor: &ActorContext,
        request: &AcceptanceUseRequest,
        now: DateTime<Utc>,
    ) -> Result<ObjectId> {
        if let Some(id) = replay_on(&self.connection, project, session, actor, request)? {
            return Ok(id);
        }
        let measured = measure_binding_destination(&self.connection, &request.binding)?;
        let mut transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(id) = replay_on(&transaction, project, session, actor, request)? {
            return Ok(id);
        }
        let id = record_on(
            &mut transaction,
            project,
            session,
            actor,
            request,
            &measured,
            now,
        )?;
        transaction.commit()?;
        Ok(id)
    }

    /// Reassess a use against actual current local authority and retained facts.
    /// Complete source values stay in the canonical record, not this projection.
    ///
    /// # Errors
    /// Refuses missing records and malformed retained historical context.
    pub fn imported_acceptance_summary(
        &self,
        id: &ObjectId,
        now: DateTime<Utc>,
    ) -> Result<AcceptanceUseSummary> {
        self.imported_acceptance_summary_in(None, id, now)
    }

    pub(crate) fn imported_acceptance_summary_for_project(
        &self,
        project: &ProjectId,
        id: &ObjectId,
        now: DateTime<Utc>,
    ) -> Result<AcceptanceUseSummary> {
        self.imported_acceptance_summary_in(Some(project), id, now)
    }

    fn imported_acceptance_summary_in(
        &self,
        project: Option<&ProjectId>,
        id: &ObjectId,
        now: DateTime<Utc>,
    ) -> Result<AcceptanceUseSummary> {
        let (kind, bytes) = object_on(&self.connection, id)?
            .ok_or_else(|| invalid("acceptance use is unavailable"))?;
        if kind != KIND {
            return Err(invalid("object is not an acceptance use"));
        }
        if is_imported_only(&self.connection, id)? {
            return Err(invalid("acceptance use is retained foreign evidence"));
        }
        let decision: Decision = serde_json::from_slice(&bytes)?;
        if project.is_some_and(|project| project != &decision.project) {
            return Err(invalid("acceptance use belongs to another project"));
        }
        let session = decision
            .local_actor
            .session_id
            .as_ref()
            .ok_or_else(|| invalid("acceptance use actor is unavailable"))?;
        let current = crate::storage::work::query::load_work_item(
            &self.connection,
            decision.request.binding.work_id,
        )?;
        let run = crate::storage::work::query::load_work_run(
            &self.connection,
            decision.request.binding.run_id,
        )?;
        if current.active_run_id != Some(decision.request.binding.run_id)
            || run.generation != decision.local_generation
            || run.is_finished()
        {
            return Ok(summary(
                id,
                &decision,
                Some(Refusal::LocalGenerationDiffers),
            ));
        }
        let mut refusal = decision.refusal;
        if refusal.is_none() {
            let measured = measure_binding_destination(&self.connection, &decision.request.binding);
            let snapshot = self.connection.unchecked_transaction()?;
            refusal = match measured {
                Ok(measured) => match binding_destination_on(
                    &snapshot,
                    &decision.project,
                    session,
                    &decision.request.binding,
                    &measured,
                    now,
                    None,
                ) {
                    Ok(destination) => {
                        let run = crate::storage::work::query::load_work_run(
                            &snapshot,
                            decision.request.binding.run_id,
                        )?;
                        let policy = Self::load_acceptance_evaluation_policy_on(&snapshot)?;
                        if run.generation != decision.local_generation
                            || destination.root != decision.named_root
                            || CanonicalObject::freeze(&destination.item)?.key()
                                != &decision.governing_fingerprint
                        {
                            Some(Refusal::LocalGenerationDiffers)
                        } else if measured.source != decision.source {
                            Some(Refusal::SourceDiffers)
                        } else {
                            inspect(
                                &History::new(&snapshot, &decision.project, &decision.request)?,
                                &destination,
                                &policy,
                                &measured.source,
                            )?
                        }
                    }
                    Err(_) => Some(Refusal::LocalGenerationDiffers),
                },
                Err(_) => Some(Refusal::SourceDiffers),
            };
            snapshot.commit()?;
        }
        Ok(summary(id, &decision, refusal))
    }
}

fn replay_on(
    connection: &Connection,
    project: &ProjectId,
    session: &SessionId,
    actor: &ActorContext,
    request: &AcceptanceUseRequest,
) -> Result<Option<ObjectId>> {
    let Some((kind, bytes)) = object_on(connection, &request.decision)? else {
        return Ok(None);
    };
    if kind != KIND || is_imported_only(connection, &request.decision)? {
        return Err(invalid("acceptance use identity conflicts"));
    }
    let old: Decision = serde_json::from_slice(&bytes)?;
    if old.project != *project
        || old.request != *request
        || old.local_actor != *actor
        || actor.session_id.as_ref() != Some(session)
    {
        return Err(invalid("acceptance use retry intent differs"));
    }
    Ok(Some(request.decision.clone()))
}

fn summary(id: &ObjectId, decision: &Decision, refusal: Option<Refusal>) -> AcceptanceUseSummary {
    AcceptanceUseSummary {
        decision: id.clone(),
        proof: decision.request.proof.clone(),
        recorded_eligible: decision.refusal.is_none(),
        currently_eligible: refusal.is_none(),
        refusal,
        source: decision.source.summary(),
    }
}

pub(super) fn record_on(
    transaction: &mut rusqlite::Transaction<'_>,
    project: &ProjectId,
    session: &SessionId,
    actor: &ActorContext,
    request: &AcceptanceUseRequest,
    measured: &MeasuredDestination,
    now: DateTime<Utc>,
) -> Result<ObjectId> {
    let savepoint = transaction.savepoint()?;
    if request.decision.as_str().len() != 32
        || actor.session_id.as_ref() != Some(session)
        || actor.run_id.as_deref() != Some(request.binding.run_id.0.to_string().as_str())
        || object_on(&savepoint, &request.decision)?.is_some()
    {
        return Err(invalid("acceptance use actor or prepared identity differs"));
    }
    let destination = binding_destination_on(
        &savepoint,
        project,
        session,
        &request.binding,
        measured,
        now,
        None,
    )?;
    let policy = crate::storage::SqliteStore::load_acceptance_evaluation_policy_on(&savepoint)?;
    let history = History::new(&savepoint, project, request)?;
    let refusal = inspect(&history, &destination, &policy, &measured.source)?;
    let run = crate::storage::work::query::load_work_run(&savepoint, request.binding.run_id)?;
    let decision = Decision {
        project: project.clone(),
        request: request.clone(),
        local_actor: actor.clone(),
        local_generation: run.generation,
        governing_fingerprint: CanonicalObject::freeze(&destination.item)?.key().clone(),
        named_root: destination.root,
        source: measured.source.clone(),
        destination_cut: destination.cut,
        policy,
        refusal,
        decided_at: now,
    };
    let object = CanonicalObject::identified(&request.decision, &decision)?;
    crate::task_sync::evidence::limits::enforce(
        crate::task_sync::evidence::limits::LimitKind::RecordTooLarge,
        Some(&request.decision),
        crate::task_sync::evidence::limits::Dimension::Bytes,
        object.bytes().len(),
        crate::task_sync::evidence::limits::MAX_RECORD_BYTES,
    )
    .map_err(crate::task_sync::evidence::EvidenceError::from)?;
    crate::storage::SqliteStore::insert_object(&savepoint, KIND, &object)?;
    append(&savepoint, &request.decision, &decision)?;
    savepoint.commit()?;
    Ok(request.decision.clone())
}

pub(crate) fn create_schema(
    connection: &Connection,
) -> std::result::Result<(), super::super::StoreError> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS work_task_sync_acceptance_uses (decision_id TEXT PRIMARY KEY REFERENCES objects(object_id),project_id TEXT NOT NULL,eligible INTEGER NOT NULL CHECK(eligible IN (0,1))) STRICT;")?;
    Ok(())
}

fn append(connection: &Connection, id: &ObjectId, decision: &Decision) -> Result<()> {
    connection.execute("INSERT INTO work_task_sync_acceptance_uses(decision_id,project_id,eligible) VALUES (?1,?2,?3)",params![id.as_str(),decision.project.0,decision.refusal.is_none()])?;
    Ok(())
}

/// Rebuild local uses only; a retained foreign use gets no row.
pub(crate) fn rebuild(connection: &Connection) -> Result<()> {
    connection.execute("DELETE FROM work_task_sync_acceptance_uses", [])?;
    let mut statement = connection.prepare(&format!(
        "SELECT object_id,canonical_json FROM objects WHERE object_kind=?1
         AND object_id NOT IN ({IMPORTED_ORIGINALS}) ORDER BY object_id"
    ))?;
    for row in statement.query_map(params![KIND, ADMISSION_KIND], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })? {
        let (id, bytes) = row?;
        append(
            connection,
            &super::super::stored_id(id)?,
            &serde_json::from_slice(&bytes)?,
        )?;
    }
    Ok(())
}

/// Parity in both directions over local uses; a row for a retained foreign
/// use is extra.
pub(crate) fn verify(connection: &Connection) -> Result<()> {
    let differs:bool=connection.query_row(&format!("SELECT EXISTS(SELECT 1 FROM objects object LEFT JOIN work_task_sync_acceptance_uses projection ON projection.decision_id=object.object_id WHERE object.object_kind=?1 AND object.object_id NOT IN ({IMPORTED_ORIGINALS}) AND (projection.decision_id IS NULL OR projection.project_id IS NOT json_extract(object.canonical_json,'$.project') OR projection.eligible IS NOT (json_extract(object.canonical_json,'$.refusal') IS NULL))) OR EXISTS(SELECT 1 FROM work_task_sync_acceptance_uses projection LEFT JOIN objects object ON object.object_id=projection.decision_id WHERE object.object_kind IS NOT ?1 OR projection.decision_id IN ({IMPORTED_ORIGINALS}))"),params![KIND, ADMISSION_KIND],|row|row.get(0))?;
    if differs {
        return Err(invalid("acceptance use projection differs"));
    }
    Ok(())
}
