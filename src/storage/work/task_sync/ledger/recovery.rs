//! Explicit origin recovery preserves old exchange facts without settling them.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::{Binding, ObjectId, StoreError, require, view};
use crate::ProjectId;
use crate::task_sync::origin::{
    Enrollment, PRODUCING_ORIGIN_KIND,
    recovery::{RECOVERY_KIND, RecoveryBinding, RecoveryReceipt, RecoveryRecord},
};

pub(in crate::storage::work) mod projection;

fn invalid(message: &str) -> StoreError {
    super::super::invalid(message)
}

/// Selected canonical lookup precedes private recipes, current heads and files.
pub(in crate::storage::work) fn origin_recovery_on(
    connection: &Connection,
    project: &ProjectId,
    operation: uuid::Uuid,
) -> Result<Option<RecoveryReceipt>, StoreError> {
    let ids = select(connection, "operation", &operation.to_string())?;
    require(ids.len() <= 1, "origin recovery operation collides")?;
    let Some(id) = ids.first() else {
        return Ok(None);
    };
    let record = read(connection, id)?;
    require(
        record.project_id == *project && record.request.operation == operation,
        "origin recovery operation belongs to another intent",
    )?;
    Ok(Some(RecoveryReceipt {
        recovery: id.clone(),
        record,
    }))
}

pub(super) fn select(
    connection: &Connection,
    key: &str,
    value: &str,
) -> Result<Vec<ObjectId>, StoreError> {
    let (index, expression) = match key {
        "operation" => ("operation", "$.request.operation"),
        "predecessor" => ("predecessor", "$.request.predecessor"),
        _ => return Err(invalid("unknown origin recovery selector")),
    };
    let mut query = connection.prepare(&format!(
        "SELECT object_id FROM objects INDEXED BY objects_task_sync_origin_recovery_{index}
         WHERE object_kind='task_sync_origin_recovery' AND json_valid(canonical_json)
           AND json_extract(canonical_json,'{expression}')=?1 ORDER BY object_id LIMIT 2"
    ))?;
    query
        .query_map([value], |row| row.get::<_, String>(0))?
        .map(|row| {
            ObjectId::from_stored(row?).ok_or_else(|| invalid("invalid origin recovery identity"))
        })
        .collect()
}

pub(super) fn read(connection: &Connection, id: &ObjectId) -> Result<RecoveryRecord, StoreError> {
    let mut budget = view::read::ReadBudget::default();
    let record: RecoveryRecord = view::read::object(connection, id, RECOVERY_KIND, &mut budget)?;
    record.validate()?;
    let predecessor: Enrollment = view::read::object(
        connection,
        &record.request.predecessor,
        PRODUCING_ORIGIN_KIND,
        &mut budget,
    )?;
    let successor: Enrollment = view::read::object(
        connection,
        &record.successor_origin,
        PRODUCING_ORIGIN_KIND,
        &mut budget,
    )?;
    require(
        predecessor.project_id == record.project_id
            && successor.project_id == record.project_id
            && successor.predecessor_origin_id.as_ref() == Some(&record.request.predecessor)
            && successor.enrollment_operation_id == record.request.operation
            && successor.actor == record.actor
            && successor.enrolled_at == record.recovered_at,
        "origin recovery enrollment provenance differs",
    )?;
    super::super::producing_origin::check_cut_on(
        connection,
        &record.project_id,
        &record.predecessor_cut,
    )?;
    let position: i64 = connection.query_row(
        "SELECT position FROM work_feed_entries WHERE feed_kind='project' AND feed_id=?1
         AND object_kind=?2 AND object_id=?3 AND work_id IS NULL",
        params![record.project_id.0, RECOVERY_KIND, id.as_str()],
        |row| row.get(0),
    )?;
    require(
        position > record.predecessor_cut.position,
        "origin recovery feed order differs",
    )?;
    for captured in &record.bindings {
        validate_binding(connection, &record.project_id, captured, &mut budget)?;
    }
    Ok(record)
}

fn validate_binding(
    connection: &Connection,
    project: &ProjectId,
    captured: &RecoveryBinding,
    budget: &mut view::read::ReadBudget,
) -> Result<(), StoreError> {
    let binding = view::read::binding_on(connection, &captured.binding, budget)?;
    let head: super::Event =
        view::read::object(connection, &captured.head, super::EVENT_KIND, budget)?;
    require(
        binding.project == *project
            && binding.repository == captured.repository
            && binding.branch == captured.branch
            && binding.target == captured.target
            && head.binding == captured.binding
            && head.sequence == captured.sequence
            && captured.unresolved != super::settled_tail(&head),
        "origin recovery binding provenance differs",
    )
}

/// Cold capture is bounded by the existing explicit recovery contract. No task
/// payload, current planning export, retained Git file or remote is read here.
pub(in crate::storage::work) fn recovery_bindings_on(
    connection: &Connection,
    project: &ProjectId,
) -> Result<Vec<RecoveryBinding>, StoreError> {
    let mut budget = view::read::ReadBudget::default();
    let mut result = Vec::new();
    for id in view::select::bindings(connection, view::select::Bindings::Project(project))? {
        let binding = view::read::binding_on(connection, &id, &mut budget)?;
        if quarantined(connection, &id, &binding)?
            || view::select::successor(connection, &id)?.is_some()
        {
            continue;
        }
        require(
            binding.project == *project,
            "origin recovery project differs",
        )?;
        let tail = view::read::tail_on(connection, &id, &binding, &mut budget)?;
        result.push(RecoveryBinding {
            binding: id,
            head: tail.id,
            sequence: tail.event.sequence,
            repository: binding.repository,
            branch: binding.branch,
            target: binding.target,
            unresolved: !super::settled_tail(&tail.event),
        });
        require(result.len() <= 64, "origin recovery has too many bindings")?;
    }
    Ok(result)
}

pub(in crate::storage::work) fn check_origin_recovery_on(
    transaction: &Transaction<'_>,
    record: &RecoveryRecord,
) -> Result<(), StoreError> {
    record.validate()?;
    let head = super::super::producing_origin::load_on(transaction, &record.project_id)?
        .ok_or_else(|| invalid("origin recovery predecessor is missing"))?;
    require(
        head.origin_id == record.request.predecessor,
        "origin recovery predecessor changed",
    )?;
    require(
        super::super::producing_origin::cut_on(transaction, &record.project_id)?
            == record.predecessor_cut,
        "origin recovery local cut changed",
    )?;
    require(
        recovery_bindings_on(transaction, &record.project_id)? == record.bindings,
        "origin recovery captured binding heads changed",
    )?;
    require(
        origin_recovery_on(transaction, &record.project_id, record.request.operation)?.is_none(),
        "origin recovery operation already exists",
    )?;
    // Ahead-of-DB witness references are retained as reported uncertainty, not
    // required to resolve in this older store and never adopted as a head.
    if let Some(witness) = &record.custody.witness {
        require(
            witness.project == record.project_id && witness.cut.position >= 0,
            "origin recovery reported witness scope differs",
        )?;
    }
    Ok(())
}

/// Quarantine is a distinct canonical relationship, never supersedes or Applied.
pub(super) fn quarantined(
    connection: &Connection,
    id: &ObjectId,
    binding: &Binding,
) -> Result<bool, StoreError> {
    let projected: Option<String> = connection
        .query_row(
            "SELECT recovery_id FROM work_task_sync_origin_recovery_bindings WHERE binding_id=?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    let expected = if let Some(origin) = &binding.store.origin {
        let ids = select(connection, "predecessor", origin.as_str())?;
        require(ids.len() <= 1, "origin recovery lineage forks")?;
        ids.into_iter()
            .next()
            .map(|recovery| {
                let record = read(connection, &recovery)?;
                require(
                    record.project_id == binding.project,
                    "origin recovery project differs",
                )?;
                Ok::<_, StoreError>(
                    record
                        .bindings
                        .iter()
                        .any(|b| &b.binding == id)
                        .then_some(recovery),
                )
            })
            .transpose()?
            .flatten()
    } else {
        None
    };
    if let Some(recovery) = expected {
        require(
            projected.as_deref() == Some(recovery.as_str()),
            "origin recovery binding projection differs; repair projections explicitly",
        )?;
        return Ok(true);
    }
    if let Some(recovery) = projected {
        let recovery = ObjectId::from_stored(recovery)
            .ok_or_else(|| invalid("invalid recovery projection"))?;
        let record = read(connection, &recovery)?;
        require(
            record.project_id == binding.project
                && record.bindings.iter().any(|b| &b.binding == id),
            "origin recovery binding projection differs; repair projections explicitly",
        )?;
        return Ok(true);
    }
    Ok(false)
}

/// External exchange admission preserves predecessor uncertainty. Local setup
/// and passive evidence are independent of this external-effect decision.
pub(super) fn require_scope_on(
    connection: &Connection,
    binding: &Binding,
) -> Result<(), StoreError> {
    let mut origin = binding.store.origin.clone();
    let mut visited = std::collections::BTreeSet::new();
    let mut budget = view::read::ReadBudget::default();
    while let Some(current) = origin {
        require(
            visited.insert(current.clone()) && visited.len() <= 4096,
            "origin recovery lineage exceeds bound",
        )?;
        let enrollment: Enrollment =
            view::read::object(connection, &current, PRODUCING_ORIGIN_KIND, &mut budget)?;
        require(
            enrollment.project_id == binding.project,
            "origin recovery lineage scope differs",
        )?;
        let Some(predecessor) = enrollment.predecessor_origin_id else {
            break;
        };
        for id in select(connection, "predecessor", predecessor.as_str())? {
            let recovery = read(connection, &id)?;
            require(
                recovery.successor_origin == current,
                "origin recovery successor differs",
            )?;
            let custody = &recovery.custody;
            let unknown = custody.registration_uninterpretable
                || custody.enrollment_intent.is_some()
                || custody.audit_intent.is_some()
                || custody.witness.as_ref().is_some_and(|w| {
                    w.pending_exchange.is_some()
                        || w.exchange.as_ref().is_some_and(|anchor| {
                            !recovery.bindings.iter().any(|b| {
                                b.binding == anchor.binding
                                    && b.head == anchor.head
                                    && b.sequence == anchor.sequence
                            })
                        })
                });
            require(
                !unknown,
                "origin recovery retains unknown private-history scope; explicit reconciliation required",
            )?;
            for captured in recovery.bindings.iter().filter(|b| b.unresolved) {
                require(
                    !(super::same_directory(
                        &captured.repository.git_directory,
                        &binding.repository.git_directory,
                    ) && (captured.branch == binding.branch
                        || captured.target.reference == binding.target.reference)),
                    "origin recovery retains unresolved exchange on this target; explicit reconciliation required",
                )?;
            }
        }
        origin = Some(predecessor);
    }
    Ok(())
}
