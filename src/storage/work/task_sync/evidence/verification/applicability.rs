//! Explicit local use decisions; retaining a pack never calls this writer.

use super::super::{
    ADMISSION_KIND, Connection, IMPORTED_ORIGINALS, OptionalExtension, Result, StoreError, invalid,
    is_imported_only, object_on, params, stored_id,
};
use super::{ExecutionKey, TerminalFact, union_on};
use crate::domain::{
    ControlWorkBinding, RegisteredHostRun, VerificationEvidence, VerificationProducerFacts,
    VerificationRequirement,
};
use crate::storage::work::{feeds, planning, query};
use crate::task_sync::evidence::EvidenceError;
use crate::task_sync::evidence::verification::{ApplicabilityRequest, ApplicabilitySummary};
use crate::{ActorContext, CanonicalObject, ObjectId, ProjectId, SessionId};
use chrono::{DateTime, Utc};

pub const APPLICABILITY_KIND: &str = "task_sync_verification_applicability";

mod consumption;
pub(crate) use consumption::qualify_on as qualify_consumption_on;

impl crate::storage::SqliteStore {
    /// Read a bounded summary, keeping the complete source only in storage.
    ///
    /// # Errors
    /// Refuses a missing decision or a damaged current-eligibility projection.
    pub fn imported_verification_summary(&self, id: &ObjectId) -> Result<ApplicabilitySummary> {
        self.imported_verification_summary_in(None, id)
    }

    pub(crate) fn imported_verification_summary_for_project(
        &self,
        project: &ProjectId,
        id: &ObjectId,
    ) -> Result<ApplicabilitySummary> {
        self.imported_verification_summary_in(Some(project), id)
    }

    fn imported_verification_summary_in(
        &self,
        project: Option<&ProjectId>,
        id: &ObjectId,
    ) -> Result<ApplicabilitySummary> {
        let (kind, bytes) = object_on(&self.connection, id)?
            .ok_or_else(|| invalid("applicability decision is unavailable"))?;
        if kind != APPLICABILITY_KIND {
            return Err(invalid("object is not an applicability decision"));
        }
        if is_imported_only(&self.connection, id)? {
            return Err(invalid(
                "applicability decision is retained foreign evidence",
            ));
        }
        let decision: Decision = serde_json::from_slice(&bytes)?;
        if project.is_some_and(|project| project != &decision.project_id) {
            return Err(invalid("applicability decision belongs to another project"));
        }
        let (eligible,invalidated): (bool,bool)=self.connection.query_row("SELECT eligible,invalidated FROM work_task_sync_evidence_applicability WHERE decision_id=?1",[id.as_str()],|row|Ok((row.get(0)?,row.get(1)?)))?;
        Ok(ApplicabilitySummary {
            decision: id.clone(),
            original: decision.original,
            recorded_eligible: eligible,
            terminal_conflicted_since: invalidated,
            source: decision.source.summary(),
            equivalent_contributors_at_decision: decision.contributors.len(),
        })
    }
    /// Prepare explicit applicability using the existing source owner, normal
    /// live destination binding and immutable retained producer facts. This
    /// writes no verification credit; completion wiring owns any later use.
    ///
    /// # Errors
    /// Refuses stale local authority, missing provenance and changed retry intent.
    pub fn associate_imported_verification(
        &mut self,
        project: &ProjectId,
        session: &SessionId,
        actor: &ActorContext,
        request: &ApplicabilityRequest,
        now: DateTime<Utc>,
    ) -> Result<ObjectId> {
        if let Some(decision) = replay_on(&self.connection, project, session, actor, request)? {
            return Ok(decision);
        }
        let measured = measure_destination(&self.connection, request)?;
        let mut tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(decision) = replay_on(&tx, project, session, actor, request)? {
            return Ok(decision);
        }
        let result =
            record_applicability_on(&mut tx, project, session, actor, request, &measured, now)?;
        tx.commit()?;
        Ok(result)
    }
}

fn replay_on(
    connection: &Connection,
    project: &ProjectId,
    session: &SessionId,
    actor: &ActorContext,
    request: &ApplicabilityRequest,
) -> Result<Option<ObjectId>> {
    if let Some((kind, bytes)) = object_on(connection, &request.decision)? {
        if kind != APPLICABILITY_KIND || is_imported_only(connection, &request.decision)? {
            return Err(invalid("applicability identity conflicts"));
        }
        let old: Decision = serde_json::from_slice(&bytes)?;
        if old.project_id != *project
            || old.original != request.original
            || old.origin != request.origin
            || old.origin_cut != request.origin_cut
            || old.binding != request.binding
            || old.requirement != request.requirement
            || old.criterion != request.criterion
            || old.local_actor != *actor
            || actor.session_id.as_ref() != Some(session)
        {
            return Err(invalid("applicability retry intent differs"));
        }
        return Ok(Some(request.decision.clone()));
    }
    Ok(None)
}

/// An actual source-owner measurement taken outside the SQLite writer.
/// Fields are private so ordinary callers cannot substitute a Git carrier id.
pub(crate) struct MeasuredDestination {
    root: ObjectId,
    pub(crate) source: crate::source::SourceId,
}

/// Use the existing source owner, without holding a writer during filesystem I/O.
pub(crate) fn measure_destination(
    connection: &Connection,
    request: &ApplicabilityRequest,
) -> Result<MeasuredDestination> {
    measure_binding_destination(connection, &request.binding)
}

pub(crate) fn measure_binding_destination(
    connection: &Connection,
    binding: &ControlWorkBinding,
) -> Result<MeasuredDestination> {
    if !connection.is_autocommit() {
        return Err(invalid(
            "source currency must be measured outside the writer",
        ));
    }
    let (_, root, event) = feeds::latest_named_root_binding_on(
        connection,
        binding.run_id,
        binding.claim_id,
        i64::MAX,
    )?
    .filter(|(_, _, event)| event.kind == crate::domain::NamedRootBindingKind::Bound)
    .ok_or_else(|| invalid("destination has no active named root"))?;
    let measured = crate::source::inspect(
        std::path::Path::new(&event.workspace_id),
        crate::source::MAX_BUDGET,
    );
    let source = measured
        .source_id
        .ok_or_else(|| invalid("destination source identity is unavailable"))?;
    Ok(MeasuredDestination { root, source })
}

pub use crate::task_sync::evidence::verification::ApplicabilityRefusal as Refusal;
use crate::task_sync::evidence::verification::VerificationApplicabilityDecision as Decision;

/// Reuse the recorded common qualifier; this read creates no new decision.
pub(crate) struct VerificationUse<'a> {
    pub(crate) project: &'a ProjectId,
    pub(crate) origin: &'a ObjectId,
    pub(crate) binding: &'a ControlWorkBinding,
    pub(crate) original: &'a ObjectId,
    pub(crate) criterion: Option<usize>,
    pub(crate) requirement: Option<&'a VerificationRequirement>,
    pub(crate) governing: &'a ObjectId,
    pub(crate) root: &'a ObjectId,
    pub(crate) source: &'a crate::source::SourceId,
}

pub(crate) fn eligible_for_on(
    connection: &Connection,
    id: &ObjectId,
    context: &VerificationUse<'_>,
) -> Result<bool> {
    let Some((kind, bytes)) = object_on(connection, id)? else {
        return Ok(false);
    };
    // A retained foreign decision records another host's use, never this one's.
    if kind != APPLICABILITY_KIND || is_imported_only(connection, id)? {
        return Ok(false);
    }
    let decision: Decision = serde_json::from_slice(&bytes)?;
    if decision.project_id != *context.project
        || decision.origin != *context.origin
        || decision.binding != *context.binding
        || decision.original != *context.original
        || decision.criterion != context.criterion
        || context
            .requirement
            .is_some_and(|requirement| *requirement != decision.requirement)
        || decision.governing_fingerprint != *context.governing
        || decision.named_root != *context.root
        || decision.source != *context.source
        || decision.refusal.is_some()
    {
        return Ok(false);
    }
    Ok(match decision.execution {
        Some(key) => !union_on(connection, &key)?.conflicted,
        None => true,
    })
}

pub(super) fn create_schema(connection: &Connection) -> std::result::Result<(), StoreError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS work_task_sync_evidence_applicability (
            decision_id TEXT PRIMARY KEY REFERENCES objects(object_id),
            eligible INTEGER NOT NULL CHECK(eligible IN (0,1)),
            invalidated INTEGER NOT NULL CHECK(invalidated IN (0,1)),
            consumed_by TEXT REFERENCES objects(object_id)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS work_task_sync_evidence_applicability_keys (
            decision_id TEXT PRIMARY KEY REFERENCES work_task_sync_evidence_applicability(decision_id),
            project_id TEXT NOT NULL, host_id TEXT NOT NULL, run_id TEXT NOT NULL
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_task_sync_evidence_applicability_execution
            ON work_task_sync_evidence_applicability_keys(project_id,host_id,run_id,decision_id);",
    )?;
    Ok(())
}

pub(crate) fn qualified_bytes(
    connection: &Connection,
    project: &ProjectId,
    origin: &ObjectId,
    id: &ObjectId,
) -> Result<(String, Vec<u8>)> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_task_sync_evidence_members member JOIN work_task_sync_evidence_packs pack ON pack.pack_id=member.pack_id
         WHERE pack.project_id=?1 AND member.origin_id=?2 AND member.object_id=?3)",
        params![project.0,origin.as_str(),id.as_str()],|row|row.get(0),
    )?;
    if !exists {
        return Err(invalid(
            "verification dependency lacks qualified origin membership",
        ));
    }
    object_on(connection, id)?.ok_or_else(|| invalid("verification original is unavailable"))
}

fn candidate_on(
    connection: &Connection,
    project: &ProjectId,
    request: &ApplicabilityRequest,
) -> Result<(
    VerificationEvidence,
    VerificationProducerFacts,
    Option<ExecutionKey>,
)> {
    candidate_in_context(
        connection,
        project,
        &request.origin,
        &request.original,
        &request.origin_cut,
    )
}

pub(crate) fn candidate_in_context(
    connection: &Connection,
    project: &ProjectId,
    origin: &ObjectId,
    original: &ObjectId,
    origin_cut: &ObjectId,
) -> Result<(
    VerificationEvidence,
    VerificationProducerFacts,
    Option<ExecutionKey>,
)> {
    let (kind, bytes) = qualified_bytes(connection, project, origin, original)?;
    if kind != "verification_evidence" {
        return Err(invalid("selected original is not observed verification"));
    }
    let evidence: VerificationEvidence = serde_json::from_slice(&bytes)?;
    let (kind, cut_bytes) = qualified_bytes(connection, project, origin, origin_cut)?;
    if kind != crate::task_sync::evidence::RecordKind::OriginFeedCut.as_str() {
        return Err(invalid("verification context does not name a feed cut"));
    }
    let cut: crate::task_sync::evidence::witnesses::OriginFeedCut =
        serde_json::from_slice(&cut_bytes)?;
    if cut.source.origin != *origin
        || cut.source.project_id != *project
        || cut.source.feed != crate::FeedId::RunExecution(evidence.binding.run_id)
    {
        return Err(invalid("verification origin cut belongs to another scope"));
    }
    let feed = serde_json::to_string(&cut.source.feed)?;
    for (id, kind) in [
        (original, "verification_evidence"),
        (&evidence.producer_observation, ""),
    ] {
        let position: Option<(i64,String)>=connection.query_row(
            "SELECT position,object_kind FROM work_task_sync_evidence_feed_members WHERE project_id=?1 AND origin_id=?2 AND feed=?3 AND object_id=?4 ORDER BY position LIMIT 1",
            params![project.0,origin.as_str(),feed,id.as_str()],|row|Ok((row.get(0)?,row.get(1)?)),
        ).optional()?;
        if position.is_none_or(|(position, actual)| {
            position > cut.through || (!kind.is_empty() && actual != kind)
        }) {
            return Err(invalid(
                "verification lacks complete witnessed origin membership",
            ));
        }
    }
    candidate_facts_on(connection, project, origin, original)
}

/// The caller supplies witnessed membership separately from immutable facts.
pub(crate) fn candidate_facts_on(
    connection: &Connection,
    project: &ProjectId,
    origin: &ObjectId,
    original: &ObjectId,
) -> Result<(
    VerificationEvidence,
    VerificationProducerFacts,
    Option<ExecutionKey>,
)> {
    let (kind, bytes) = qualified_bytes(connection, project, origin, original)?;
    if kind != "verification_evidence" {
        return Err(invalid("selected original is not observed verification"));
    }
    let evidence: VerificationEvidence = serde_json::from_slice(&bytes)?;
    if evidence.schema_version != crate::domain::SCHEMA_VERSION
        || evidence.project_id != *project
        || evidence.actor.session_id.as_ref() != Some(&evidence.session_id)
        || evidence.actor.run_id.as_deref() != Some(evidence.binding.run_id.0.to_string().as_str())
        || evidence.bound_from.is_some()
    {
        return Err(invalid(
            "verification provenance is malformed or unsupported",
        ));
    }
    let (kind, bytes) =
        qualified_bytes(connection, project, origin, &evidence.producer_observation)?;
    let (producer, execution) = match kind.as_str() {
        "registered_host_run" => {
            let producer: RegisteredHostRun = serde_json::from_slice(&bytes)?;
            if producer.schema_version != crate::domain::SCHEMA_VERSION {
                return Err(invalid("registered producer schema is unsupported"));
            }
            crate::storage::work::registered_verification::validate_stored_request(
                &producer.request,
                producer.recorded_at,
            )?;
            (
                crate::storage::work::verification_producer::registered_producer_facts(
                    producer, &evidence,
                )?,
                None,
            )
        }
        crate::domain::REMOTE_VERIFICATION_ASSOCIATION_KIND => {
            let record: crate::domain::RemoteVerificationAssociationRecord =
                serde_json::from_slice(&bytes)?;
            if crate::control::remote_result_disqualification(
                &record.request.completion,
                &record.request.association.intended_use,
                record.associated_at,
            )
            .is_some()
            {
                return Err(invalid(
                    "remote producer does not qualify under the common result rule",
                ));
            }
            let fact = TerminalFact::from_bytes(
                evidence.producer_observation.clone(),
                crate::task_sync::evidence::RecordKind::RemoteVerificationAssociation,
                &bytes,
            )?
            .ok_or_else(|| invalid("remote verification is unsupported or provisional"))?;
            let original = record
                .origin
                .as_ref()
                .ok_or_else(|| invalid("remote origin is unavailable"))?;
            qualified_bytes(connection, project, origin, original)?;
            (
                crate::storage::work::remote_verification::producer_facts(
                    &record,
                    &evidence.producer_observation,
                )?,
                Some(fact.key),
            )
        }
        _ => {
            return Err(invalid(
                "reported or uncredited facts are not verification producers",
            ));
        }
    };
    if producer.project_id != evidence.project_id
        || producer.binding != evidence.binding
        || producer.session_id != evidence.session_id
        || producer.observed_at != Some(evidence.completed_at)
        || producer.actor.session_id.as_ref() != Some(&producer.session_id)
        || producer.actor.run_id.as_deref() != Some(producer.binding.run_id.0.to_string().as_str())
        || producer.source_basis.as_ref() != Some(&evidence.source_basis)
    {
        return Err(invalid("verification differs from its original producer"));
    }
    if evidence.source_basis.source_id().is_some()
        && (evidence.environment.is_some() || evidence.applicability.is_some())
    {
        return Err(invalid(
            "shared verification has unsupported attribution fields",
        ));
    }
    Ok((evidence, producer, execution))
}

/// This decision writes no native evidence, obligation satisfaction or feed event.
/// The caller holds the normal local claim and retains its exact prepared UUID.
pub(crate) fn record_applicability_on(
    transaction: &mut rusqlite::Transaction<'_>,
    project: &ProjectId,
    session: &SessionId,
    actor: &ActorContext,
    request: &ApplicabilityRequest,
    measured: &MeasuredDestination,
    now: DateTime<Utc>,
) -> Result<ObjectId> {
    let savepoint = transaction.savepoint()?;
    let result = record_on(&savepoint, project, session, actor, request, measured, now)?;
    savepoint.commit()?;
    Ok(result)
}

fn record_on(
    connection: &Connection,
    project: &ProjectId,
    session: &SessionId,
    actor: &ActorContext,
    request: &ApplicabilityRequest,
    measured: &MeasuredDestination,
    now: DateTime<Utc>,
) -> Result<ObjectId> {
    // This core expects the exchange operation owner to replay its terminal
    // receipt before measuring. A prepared UUID can never overwrite a decision.
    if object_on(connection, &request.decision)?.is_some() {
        return Err(invalid("applicability identity is already retained"));
    }
    if request.decision.as_str().len() != 32
        || actor.session_id.as_ref() != Some(session)
        || actor.run_id.as_deref() != Some(request.binding.run_id.0.to_string().as_str())
    {
        return Err(invalid(
            "local applicability actor or prepared identity differs",
        ));
    }
    let Destination {
        item,
        cut,
        root,
        basis,
    } = destination_on(connection, project, session, request, measured, now)?;
    let (evidence, producer, execution) = candidate_on(connection, project, request)?;
    let mut refusal =
        crate::control::check_verification_result(&evidence, &producer, &request.requirement)
            .err()
            .map(Refusal::Mismatch);
    if !evidence.source_basis.identity.same_source(&basis.identity) {
        refusal = Some(Refusal::SourceDiffers);
    }
    if producer.matches_registered_source(&evidence, &basis.identity) != Some(true) {
        refusal = Some(Refusal::SourceDiffers);
    }
    if let Err(mismatch) = crate::control::check_verification_applicability(
        evidence.applicability.as_ref(),
        basis.legacy_basis().as_ref(),
    ) {
        refusal = Some(Refusal::Mismatch(mismatch));
    }
    let mut contributors = Vec::new();
    if let Some(key) = &execution {
        let union = union_on(connection, key)?;
        if union.contributors.is_empty() {
            return Err(invalid("remote execution union is unavailable"));
        }
        if union.conflicted {
            refusal = Some(Refusal::TerminalConflict);
        }
        contributors = union.contributors;
    }
    let known_import = connection.query_row("SELECT admission_id FROM work_task_sync_evidence_packs WHERE project_id=?1 ORDER BY position DESC LIMIT 1",[&project.0],|row|row.get::<_,String>(0)).optional()?.map(stored_id).transpose()?;
    let decision = Decision {
        project_id: project.clone(),
        original: request.original.clone(),
        origin: request.origin.clone(),
        origin_cut: request.origin_cut.clone(),
        binding: request.binding.clone(),
        requirement: request.requirement.clone(),
        criterion: request.criterion,
        governing_fingerprint: CanonicalObject::freeze(&item)?.key().clone(),
        named_root: root,
        source: measured.source.clone(),
        run_cut: cut,
        known_import,
        execution,
        contributors,
        refusal,
        origin_actor: evidence.actor,
        local_actor: actor.clone(),
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
    .map_err(EvidenceError::from)?;
    crate::storage::SqliteStore::insert_object(connection, APPLICABILITY_KIND, &object)?;
    append_on(connection, &request.decision, &decision)?;
    Ok(request.decision.clone())
}

fn append_on(connection: &Connection, id: &ObjectId, decision: &Decision) -> Result<()> {
    let conflicted = decision.execution.as_ref().map(|key| connection.query_row(
        "SELECT conflicted FROM work_task_sync_evidence_execution_keys WHERE project_id=?1 AND host_id=?2 AND run_id=?3",
        params![key.project.0,key.host,key.run],|row|row.get::<_,bool>(0))).transpose()?.unwrap_or(false);
    connection.execute("INSERT INTO work_task_sync_evidence_applicability(decision_id,eligible,invalidated,consumed_by) VALUES (?1,?2,?3,NULL)",params![id.as_str(),decision.refusal.is_none(),conflicted])?;
    if let Some(key) = &decision.execution {
        connection.execute("INSERT INTO work_task_sync_evidence_applicability_keys(decision_id,project_id,host_id,run_id) VALUES (?1,?2,?3,?4)",params![id.as_str(),key.project.0,key.host,key.run])?;
    }
    Ok(())
}

pub(super) fn clear(connection: &Connection) -> Result<()> {
    connection.execute_batch("DELETE FROM work_task_sync_evidence_applicability_keys; DELETE FROM work_task_sync_evidence_applicability;")?;
    Ok(())
}

/// Rebuild local decisions only; a retained foreign decision gets no row.
pub(super) fn rebuild(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare(&format!(
        "SELECT object_id,canonical_json FROM objects WHERE object_kind=?1
         AND object_id NOT IN ({IMPORTED_ORIGINALS}) ORDER BY object_id"
    ))?;
    for row in statement.query_map(params![APPLICABILITY_KIND, ADMISSION_KIND], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })? {
        let (id, bytes) = row?;
        append_on(
            connection,
            &stored_id(id)?,
            &serde_json::from_slice(&bytes)?,
        )?;
    }
    Ok(())
}

/// Parity in both directions over local decisions; a row for a retained
/// foreign decision is extra.
pub(super) fn verify(connection: &Connection) -> Result<()> {
    let difference: bool = connection.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM objects original LEFT JOIN work_task_sync_evidence_applicability projection ON projection.decision_id=original.object_id
         WHERE original.object_kind=?1 AND original.object_id NOT IN ({IMPORTED_ORIGINALS})
         AND (projection.decision_id IS NULL OR projection.eligible IS NOT (json_extract(original.canonical_json,'$.refusal') IS NULL)))
         OR EXISTS(SELECT 1 FROM work_task_sync_evidence_applicability projection LEFT JOIN objects original ON original.object_id=projection.decision_id
         WHERE original.object_kind IS NOT ?1 OR projection.decision_id IN ({IMPORTED_ORIGINALS}))
         OR EXISTS(SELECT 1 FROM objects original
         LEFT JOIN work_task_sync_evidence_applicability_keys dependency ON dependency.decision_id=original.object_id
         LEFT JOIN work_task_sync_evidence_execution_keys execution ON execution.project_id=dependency.project_id AND execution.host_id=dependency.host_id AND execution.run_id=dependency.run_id
         JOIN work_task_sync_evidence_applicability projection ON projection.decision_id=original.object_id
         WHERE original.object_kind=?1 AND (
           ((json_extract(original.canonical_json,'$.execution') IS NULL) != (dependency.decision_id IS NULL))
           OR (dependency.decision_id IS NOT NULL AND (
             dependency.project_id IS NOT json_extract(original.canonical_json,'$.execution.project')
             OR dependency.host_id IS NOT json_extract(original.canonical_json,'$.execution.host')
             OR dependency.run_id IS NOT json_extract(original.canonical_json,'$.execution.run')
             OR execution.representative_id IS NULL))
           OR projection.invalidated IS NOT COALESCE(execution.conflicted,0)))
         OR EXISTS(SELECT 1 FROM work_task_sync_evidence_applicability_keys dependency LEFT JOIN objects original ON original.object_id=dependency.decision_id
         WHERE original.object_kind IS NOT ?1 OR dependency.decision_id IN ({IMPORTED_ORIGINALS}))"),
        params![APPLICABILITY_KIND, ADMISSION_KIND],|row|row.get(0),
    )?;
    if difference {
        return Err(invalid("local applicability projection differs"));
    }
    // These are this host's writer invariants, not a retained original's.
    let mut statement = connection.prepare(&format!(
        "SELECT canonical_json FROM objects WHERE object_kind=?1
         AND object_id NOT IN ({IMPORTED_ORIGINALS})"
    ))?;
    for row in statement.query_map(params![APPLICABILITY_KIND, ADMISSION_KIND], |row| {
        row.get::<_, Vec<u8>>(0)
    })? {
        let decision: Decision = serde_json::from_slice(&row?)?;
        if decision.source.validate().is_err()
            || decision
                .contributors
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
            || decision.local_actor.run_id.as_deref()
                != Some(decision.binding.run_id.0.to_string().as_str())
        {
            return Err(invalid("local applicability record is malformed"));
        }
    }
    Ok(())
}

pub(crate) struct Destination {
    pub(crate) item: crate::WorkItem,
    pub(crate) cut: i64,
    pub(crate) root: ObjectId,
    pub(crate) basis: crate::domain::VerificationSourceBasis,
}

fn destination_on(
    connection: &Connection,
    project: &ProjectId,
    session: &SessionId,
    request: &ApplicabilityRequest,
    measured: &MeasuredDestination,
    now: DateTime<Utc>,
) -> Result<Destination> {
    binding_destination_on(
        connection,
        project,
        session,
        &request.binding,
        measured,
        now,
        request
            .criterion
            .map(|criterion| (criterion, &request.requirement)),
    )
}

pub(crate) fn binding_destination_on(
    connection: &Connection,
    project: &ProjectId,
    session: &SessionId,
    binding: &ControlWorkBinding,
    measured: &MeasuredDestination,
    now: DateTime<Utc>,
    criterion: Option<(usize, &VerificationRequirement)>,
) -> Result<Destination> {
    planning::validate_control_work_binding_on(connection, project, session, binding, now)?;
    let item = query::load_work_item(connection, binding.work_id)?;
    if item.project_id != *project || item.revision != binding.work_revision {
        return Err(invalid("destination governing basis moved"));
    }
    if let Some((criterion, requirement)) = criterion
        && !item
            .acceptance_bindings
            .iter()
            .any(|binding| binding.criterion == criterion && &binding.requirement == requirement)
    {
        return Err(invalid("destination criterion requirement differs"));
    }
    let cut = feeds::current_run_feed_cut_on(connection, binding.run_id)?.position;
    let (_, root, event) =
        feeds::latest_named_root_binding_on(connection, binding.run_id, binding.claim_id, cut)?
            .filter(|(_, _, event)| event.kind == crate::domain::NamedRootBindingKind::Bound)
            .ok_or_else(|| invalid("destination named root is unavailable"))?;
    if root != measured.root {
        return Err(invalid("destination root moved during measurement"));
    }
    let (_, sighting) = feeds::latest_named_root_sighting_on(
        connection,
        binding.run_id,
        &event.workspace_id,
        event.generation,
        cut,
        false,
    )?
    .ok_or_else(|| invalid("destination has no accepted source sighting"))?;
    let basis = sighting
        .source_basis
        .as_ref()
        .ok_or_else(|| invalid("destination source sighting lacks its basis"))?;
    if basis.source_id() != Some(&measured.source) {
        return Err(invalid("destination source moved since its sighting"));
    }
    Ok(Destination {
        item,
        cut,
        root,
        basis: basis.clone(),
    })
}
