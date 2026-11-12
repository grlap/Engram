//! Read enrollment under the caller's snapshot or writer, without opening a store.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

mod committed;

use crate::task_sync::origin::{EnrollRequest, continuity::PreparedContinuity, private};
use crate::task_sync::origin::{Enrollment, PRODUCING_ORIGIN_KIND, ProducingOrigin};
use crate::task_sync::progress::{ExportCut, FeedAnchor};
use crate::{CanonicalObject, ObjectId, ProjectId, storage::StoreError};

mod recovery;
pub(in crate::storage::work) mod view;

/// Opaque proof valid only while the caller's checked writer remains borrowed.
/// A loaded origin, alias, pathname or caller-supplied boolean cannot mint it.
/// The pointer checks the connection; the borrow binds the transaction lifetime.
pub(crate) struct CheckedProducingOrigin<'tx> {
    origin: ProducingOrigin,
    connection: &'tx Connection,
}

impl CheckedProducingOrigin<'_> {
    pub(crate) fn origin(&self) -> &ProducingOrigin {
        &self.origin
    }

    pub(crate) fn require_on(&self, transaction: &Transaction<'_>) -> Result<(), StoreError> {
        if !std::ptr::eq(self.connection, &raw const **transaction) {
            return Err(super::invalid("origin proof belongs to another connection"));
        }
        Ok(())
    }
}

/// Check logical continuity under the SAME writer as host registration or E1.
/// The private files are read and reserved before this transaction is begun.
pub(crate) fn check_on<'tx>(
    transaction: &'tx Transaction<'_>,
    project: &ProjectId,
    prepared: &PreparedContinuity,
) -> Result<CheckedProducingOrigin<'tx>, StoreError> {
    check_inner(transaction, project, prepared, None)
}

pub(super) fn check_exchange_on(
    transaction: &Transaction<'_>,
    project: &ProjectId,
    prepared: Option<&PreparedContinuity>,
    request: &ObjectId,
) -> Result<(), StoreError> {
    if let Some(prepared) = prepared {
        let checked = check_inner(transaction, project, prepared, Some(request))?;
        checked.require_on(transaction)?;
    } else if load_on(transaction, project)?.is_some() {
        return Err(super::invalid(
            "origin enrollment changed before exchange writer",
        ));
    }
    Ok(())
}

fn check_inner<'tx>(
    transaction: &'tx Transaction<'_>,
    project: &ProjectId,
    prepared: &PreparedContinuity,
    request: Option<&ObjectId>,
) -> Result<CheckedProducingOrigin<'tx>, StoreError> {
    if prepared.request() != request || (request.is_none() && prepared.operation().is_some()) {
        return Err(super::invalid(
            "origin custody observation belongs to another operation",
        ));
    }
    if let Some(audit) = &prepared.state().audit {
        return Err(private::pending_audit_error(audit));
    }
    let (origin, witness, actual) = origin_facts_on(transaction, project, prepared)?;
    let fresh_reservation =
        witness.pending_exchange.is_none() && request.is_some() && actual == witness.exchange;
    if witness.pending_exchange.as_ref() != request && !fresh_reservation {
        if witness.pending_exchange.is_none() {
            return Err(super::invalid(
                "origin exchange witness is behind the ledger without a retained intent; quarantine",
            ));
        }
        return Err(super::invalid(
            "origin exchange intent remains pending; exact recovery required",
        ));
    }
    let recovered = if let Some(request) = request {
        super::ledger::matches_origin_recovery_on(
            transaction,
            project,
            &origin.origin_id,
            witness.exchange.as_ref(),
            request,
        )?
    } else {
        false
    };
    if actual != witness.exchange && !recovered {
        return Err(super::invalid(
            "origin exchange witness differs; quarantine pending exchange",
        ));
    }
    if let Some((recovery, successor, activation_request)) = prepared.recovered_activation() {
        let recorded = super::ledger::origin_recovery_on(
            transaction,
            project,
            origin.enrollment.enrollment_operation_id,
        )?
        .ok_or_else(|| super::invalid("local activation recovery is missing"))?;
        crate::task_sync::ledger::require(
            recorded.recovery == *recovery
                && recorded.record.successor_origin == origin.origin_id
                && *successor == origin.origin_id
                && request == Some(activation_request)
                && prepared.request() == Some(activation_request)
                && prepared.operation().is_none(),
            "local activation differs from the exact recovered origin or request",
        )?;
    } else {
        super::ledger::check_origin_custody_on(
            transaction,
            project,
            prepared.custody(),
            prepared.operation(),
            prepared.request(),
            None,
        )?;
    }
    Ok(CheckedProducingOrigin {
        origin,
        connection: transaction,
    })
}

type OriginFacts = (
    ProducingOrigin,
    private::Witness,
    Option<private::ExchangeAnchor>,
);

fn origin_facts_on(
    transaction: &Transaction<'_>,
    project: &ProjectId,
    prepared: &PreparedContinuity,
) -> Result<OriginFacts, StoreError> {
    if prepared.project() != project
        || transaction.is_autocommit()
        || transaction
            .path()
            .map(|path| crate::storage::store_path_text(std::path::Path::new(path)))
            != Some(crate::storage::store_path_text(prepared.database()))
    {
        return Err(super::invalid(
            "origin proof requires its reserved project writer",
        ));
    }
    let origin = load_on(transaction, project)?
        .ok_or_else(|| super::invalid("producing origin is not explicitly enrolled"))?;
    let witness = prepared.state().witness.as_ref().ok_or_else(|| {
        super::invalid("origin witness missing; copied or restored store is quarantined")
    })?;
    if witness.origin != origin.origin_id
        || witness.project != *project
        || witness.enrollment_operation != origin.enrollment.enrollment_operation_id
    {
        return Err(super::invalid(
            "origin witness differs; quarantine pending exchange",
        ));
    }
    check_cut_on(transaction, project, &witness.cut)?;
    let actual = super::ledger::origin_anchor_on(transaction, project, &origin.origin_id)?;
    if let Some(anchor) = &actual {
        check_cut_on(transaction, project, &anchor.cut)?;
    }
    Ok((origin, witness.clone(), actual))
}

/// Audit-specific reservation shares the origin owner and proof primitives,
/// but never performs E1's uncommitted-request release after an uncertain effect.
pub(super) fn with_custody_audit<T>(
    store: &mut crate::storage::SqliteStore,
    project: &ProjectId,
    request: &ObjectId,
    incoming_bootstrap: Option<(&std::path::Path, uuid::Uuid)>,
    retirement: Option<&crate::task_sync::delta::CustodyAuditRequest>,
    effect: impl FnOnce(
        &mut crate::storage::SqliteStore,
        &mut PreparedContinuity,
        &std::collections::BTreeMap<
            std::path::PathBuf,
            crate::task_sync::retention::audit::AuditReservation,
        >,
    ) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    let database = std::path::PathBuf::from(
        store
            .connection
            .path()
            .ok_or_else(|| super::invalid("custody audit requires a file-backed store"))?,
    );
    let mut guard = PreparedContinuity::acquire(&database, project)?;
    if let Some(pending) = &guard.state().pending {
        return Err(private::pending_enrollment_error(pending));
    }
    let witness =
        guard.state().witness.as_ref().ok_or_else(|| {
            super::invalid("origin witness missing; custody audit is quarantined")
        })?;
    if witness.pending_exchange.is_some() {
        return Err(super::invalid(
            "exchange intent remains pending; recover it before custody audit",
        ));
    }
    // Keep the established origin-before-repository lock order. These same
    // reservations remain borrowed through capture, commit and acknowledgement.
    let roots = super::ledger::origin_repositories_on(&store.connection, project)?;
    let reservations = roots
        .into_iter()
        .map(|root| {
            crate::task_sync::retention::audit::AuditReservation::acquire(&root)
                .map(|reservation| (root, reservation))
                .map_err(|error| super::invalid(error.to_string()))
        })
        .collect::<Result<std::collections::BTreeMap<_, _>, _>>()?;
    if let Some(audit) = &guard.state().audit {
        if &audit.request != request {
            return Err(private::pending_audit_error(audit));
        }
    } else {
        if let Some((repository, operation)) = incoming_bootstrap {
            guard.capture_custody(
                &store.connection,
                Some(repository),
                Some(request),
                Some(operation),
            )?;
        } else {
            guard.capture_custody(&store.connection, None, None, None)?;
        }
        if let Some(retirement) = retirement {
            guard.admit_retained_construction(&store.connection, retirement)?;
        }
        let transaction = store.begin_work_mutation()?;
        if incoming_bootstrap.is_some() {
            check_exchange_on(&transaction, project, Some(&guard), request)?;
        } else {
            check_on(&transaction, project, &guard)?.require_on(&transaction)?;
        }
    }
    effect(store, &mut guard, &reservations)
}

fn audit_after_on(
    transaction: &Transaction<'_>,
    project: &ProjectId,
    prepared: &PreparedContinuity,
    pending: &private::PendingAudit,
) -> Result<Option<private::Witness>, StoreError> {
    use crate::task_sync::{
        ledger::{EVENT_KIND, Event, Transition, require},
        retention::audit::RetainedAudit,
    };
    if prepared.state().audit.as_ref() != Some(pending)
        || prepared.request() != Some(&pending.request)
        || prepared.operation() != Some(pending.operation)
    {
        return Err(super::invalid(
            "custody audit observation differs from its private reservation",
        ));
    }
    let (origin, witness, actual) = origin_facts_on(transaction, project, prepared)?;
    require(
        origin.origin_id == pending.expected_witness.origin && witness.pending_exchange.is_none(),
        "custody audit origin or exchange reservation changed",
    )?;
    let exists: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM objects WHERE object_id=?1)",
        [pending.event_id.as_str()],
        |row| row.get(0),
    )?;
    let after = if exists {
        let mut budget = super::ledger::view::read::ReadBudget::default();
        let event: Event = super::ledger::view::read::object(
            transaction,
            &pending.event_id,
            EVENT_KIND,
            &mut budget,
        )?;
        require(
            pending.matches_retained(&RetainedAudit {
                event_id: pending.event_id.clone(),
                event: event.clone(),
            })?,
            "canonical custody audit differs from exact pending event",
        )?;
        let Transition::CustodyCheckpoint { checkpoint } = event.transition else {
            return Err(super::invalid("custody audit event kind differs"));
        };
        let before = pending
            .expected_witness
            .exchange
            .as_ref()
            .ok_or_else(|| super::invalid("custody audit predecessor anchor missing"))?;
        require(
            before.binding == pending.binding
                && before.head == pending.expected_tail
                && event.sequence
                    == before
                        .sequence
                        .checked_add(1)
                        .ok_or_else(|| super::invalid("custody audit sequence exhausted"))?
                && actual.as_ref().is_some_and(|anchor| {
                    anchor.binding == pending.binding
                        && anchor.head == pending.event_id
                        && anchor.sequence == event.sequence
                }),
            "custody audit recovery is not the exact next canonical event",
        )?;
        let mut after = pending.expected_witness.clone();
        after.cut = ExportCut {
            position: checkpoint.local_cut.position.position,
            anchor: checkpoint.local_cut.anchor,
        };
        require(
            after.cut.position >= pending.expected_witness.cut.position,
            "custody audit cut regresses its predecessor",
        )?;
        after.exchange = actual;
        Some(after)
    } else {
        require(
            actual == pending.expected_witness.exchange,
            "custody audit predecessor changed before commit",
        )?;
        None
    };
    require(
        witness == pending.expected_witness || after.as_ref() == Some(&witness),
        "custody audit witness diverged or SQL was rewound; preserve uncertainty",
    )?;
    super::ledger::check_origin_custody_on(
        transaction,
        project,
        prepared.custody(),
        Some(pending.operation),
        None,
        Some(pending),
    )?;
    Ok(after)
}

pub(super) fn check_audit_on(
    transaction: &Transaction<'_>,
    project: &ProjectId,
    prepared: &PreparedContinuity,
    pending: &private::PendingAudit,
) -> Result<(), StoreError> {
    audit_after_on(transaction, project, prepared, pending).map(|_| ())
}

pub(super) fn acknowledge_audit_origin_on(
    transaction: &Transaction<'_>,
    project: &ProjectId,
    prepared: &PreparedContinuity,
    pending: &private::PendingAudit,
) -> Result<(), StoreError> {
    let after = audit_after_on(transaction, project, prepared, pending)?.ok_or_else(|| {
        super::invalid("custody audit must be committed before origin acknowledgement")
    })?;
    let witness = prepared
        .state()
        .witness
        .as_ref()
        .ok_or_else(|| super::invalid("origin witness missing"))?;
    if witness != &after {
        private::advance(prepared.database(), witness, &after)?;
    }
    Ok(())
}

/// Reserve E1's exact request outside its SQL writer and finish the private
/// witness only after that owner's committed result. Uncertain effects preserve
/// custody; a proven unchanged tail can release its exact private reservation.
#[derive(Clone, Copy)]
enum ExchangeMode<'a> {
    Ordinary,
    ExactRecovery,
    BindingActivation,
    DeltaResolution(&'a crate::task_sync::exchange::InspectedDeltaResolution),
    EvidenceImport(
        &'a crate::task_sync::ledger::Binding,
        &'a crate::task_sync::ledger::EvidenceImportRecipe,
    ),
    PlanningSuccessor(
        &'a crate::task_sync::ledger::Binding,
        &'a crate::task_sync::retention::successor::RetainedSuccessor,
    ),
}

pub(super) fn with_planning_successor<T>(
    store: &mut crate::storage::SqliteStore,
    binding: &crate::task_sync::ledger::Binding,
    retained: &crate::task_sync::retention::successor::RetainedSuccessor,
    request: &ObjectId,
    effect: impl FnOnce(
        &mut crate::storage::SqliteStore,
        Option<&PreparedContinuity>,
    ) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    with_exchange_inner(
        store,
        &binding.project,
        &binding.repository.git_directory,
        Some(retained.recipe.request.intent.operation),
        request,
        ExchangeMode::PlanningSuccessor(binding, retained),
        effect,
    )
}

pub(super) fn with_evidence_import<T>(
    store: &mut crate::storage::SqliteStore,
    binding: &crate::task_sync::ledger::Binding,
    recipe: &crate::task_sync::ledger::EvidenceImportRecipe,
    request: &ObjectId,
    effect: impl FnOnce(
        &mut crate::storage::SqliteStore,
        Option<&PreparedContinuity>,
    ) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    with_exchange_inner(
        store,
        &binding.project,
        &binding.repository.git_directory,
        Some(recipe.intent()?.operation),
        request,
        ExchangeMode::EvidenceImport(binding, recipe),
        effect,
    )
}

pub(super) fn with_delta_resolution<T>(
    store: &mut crate::storage::SqliteStore,
    input: &crate::task_sync::exchange::InspectedDeltaResolution,
    request: &ObjectId,
    effect: impl FnOnce(
        &mut crate::storage::SqliteStore,
        Option<&PreparedContinuity>,
    ) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    with_exchange_inner(
        store,
        &input.original.binding.project,
        &input.original.binding.repository.git_directory,
        Some(
            input
                .operation()
                .map_err(|error| super::invalid(error.to_string()))?,
        ),
        request,
        ExchangeMode::DeltaResolution(input),
        effect,
    )
}

/// Select and retain one fresh exchange construction while holding the origin
/// reservation that every exchange writer holds for its whole effect, under one
/// SQL writer that always rolls back. A construction fixes its chain position,
/// so selecting it in a separate snapshot would let another writer advance the
/// chain before the retained file names a stale predecessor. This reserves no
/// private request and writes no SQL: the effect may only retain private files.
pub(super) fn with_construction<T>(
    store: &mut crate::storage::SqliteStore,
    project: &ProjectId,
    repository: &std::path::Path,
    operation: uuid::Uuid,
    request: &ObjectId,
    effect: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    let database = std::path::PathBuf::from(
        store
            .connection
            .path()
            .ok_or_else(|| super::invalid("exchange construction requires a file-backed store"))?,
    );
    let mut guard = PreparedContinuity::acquire(&database, project)?;
    if let Some(pending) = &guard.state().pending {
        return Err(private::pending_enrollment_error(pending));
    }
    if let Some(audit) = &guard.state().audit {
        return Err(private::pending_audit_error(audit));
    }
    guard.capture_custody(
        &store.connection,
        Some(repository),
        Some(request),
        Some(operation),
    )?;
    guard.recheck()?;
    let transaction = store.begin_work_mutation()?;
    check_exchange_on(&transaction, project, Some(&guard), request)?;
    effect(&transaction)
}

pub(super) fn with_exchange<T>(
    store: &mut crate::storage::SqliteStore,
    project: &ProjectId,
    repository: &std::path::Path,
    operation: Option<uuid::Uuid>,
    request: &ObjectId,
    effect: impl FnOnce(
        &mut crate::storage::SqliteStore,
        Option<&PreparedContinuity>,
    ) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    with_exchange_inner(
        store,
        project,
        repository,
        operation,
        request,
        ExchangeMode::Ordinary,
        effect,
    )
}

/// Binding activation is local bookkeeping. Only a recorded recovered successor
/// can defer predecessor Git custody to the separate external-effect guard.
pub(super) fn with_binding_activation<T>(
    store: &mut crate::storage::SqliteStore,
    project: &ProjectId,
    repository: &std::path::Path,
    request: &ObjectId,
    effect: impl FnOnce(
        &mut crate::storage::SqliteStore,
        Option<&PreparedContinuity>,
    ) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    with_exchange_inner(
        store,
        project,
        repository,
        None,
        request,
        ExchangeMode::BindingActivation,
        effect,
    )
}

/// Recovery can acknowledge only its exact retained request. It must never
/// release another operation's reservation or create a new reservation.
pub(super) fn with_exchange_recovery<T>(
    store: &mut crate::storage::SqliteStore,
    project: &ProjectId,
    repository: &std::path::Path,
    operation: uuid::Uuid,
    request: &ObjectId,
    effect: impl FnOnce(
        &mut crate::storage::SqliteStore,
        Option<&PreparedContinuity>,
    ) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    with_exchange_inner(
        store,
        project,
        repository,
        Some(operation),
        request,
        ExchangeMode::ExactRecovery,
        effect,
    )
}

fn with_exchange_inner<T>(
    store: &mut crate::storage::SqliteStore,
    project: &ProjectId,
    repository: &std::path::Path,
    operation: Option<uuid::Uuid>,
    request: &ObjectId,
    mode: ExchangeMode<'_>,
    effect: impl FnOnce(
        &mut crate::storage::SqliteStore,
        Option<&PreparedContinuity>,
    ) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    let recovery_only = matches!(mode, ExchangeMode::ExactRecovery);
    let origin = load_on(&store.connection, project)?;
    let Some(origin) = origin else {
        if let Some(path) = store.connection.path() {
            let path = std::path::Path::new(path);
            if crate::project_home_of(path, project).is_some() {
                let state = private::read(path, project)?;
                if let Some(audit) = &state.audit {
                    return Err(private::pending_audit_error(audit));
                }
                if state.witness.is_none()
                    && let Some(pending) = &state.pending
                {
                    return Err(private::pending_enrollment_error(pending));
                }
                if state.witness.is_some() || state.pending.is_some() {
                    return Err(super::invalid(
                        "origin private custody is ahead of the database; quarantine",
                    ));
                }
            }
        }
        return effect(store, None);
    };
    let database = std::path::PathBuf::from(
        store
            .connection
            .path()
            .ok_or_else(|| super::invalid("origin exchange requires file-backed store"))?,
    );
    let mut guard = PreparedContinuity::acquire(&database, project)?;
    if let Some(pending) = &guard.state().pending {
        return Err(private::pending_enrollment_error(pending));
    }
    if let Some(audit) = &guard.state().audit {
        return Err(private::pending_audit_error(audit));
    }
    let local_activation = if matches!(mode, ExchangeMode::BindingActivation)
        && guard.state().witness.as_ref().is_some_and(|witness| {
            witness
                .pending_exchange
                .as_ref()
                .is_none_or(|pending| pending == request)
        }) {
        super::ledger::origin_recovery_on(
            &store.connection,
            project,
            origin.enrollment.enrollment_operation_id,
        )?
        .filter(|receipt| receipt.record.successor_origin == origin.origin_id)
    } else {
        None
    };
    if recovery_only
        && guard.state().witness.as_ref().is_some_and(|witness| {
            witness
                .pending_exchange
                .as_ref()
                .is_some_and(|pending| pending != request)
        })
    {
        return Err(super::invalid(
            "another origin exchange intent remains pending; exact recovery required",
        ));
    }
    capture_exchange_context(
        &mut guard,
        &store.connection,
        repository,
        request,
        operation,
        local_activation.as_ref(),
        mode,
    )?;
    let witness = guard
        .state()
        .witness
        .as_ref()
        .ok_or_else(|| super::invalid("origin witness missing; quarantine pending exchange"))?
        .clone();
    if recovery_only && witness.pending_exchange.is_none() {
        // A repeated completed recovery observes the receipt under the checked
        // writer, without writing a new private request or resetting a witness.
        return effect(store, Some(&guard));
    }
    if witness
        .pending_exchange
        .as_ref()
        .is_some_and(|old| old != request)
    {
        // A previous effect may have rolled back but lost its cleanup (for
        // example, writer contention). A different request gets no artifact
        // exception: all retained custody must already be accounted for.
        let pending = witness
            .pending_exchange
            .as_ref()
            .ok_or_else(|| super::invalid("origin exchange reservation disappeared"))?;
        guard.capture_custody(&store.connection, Some(repository), Some(pending), None)?;
        if !release_uncommitted_exchange(store, project, &guard, &witness)? {
            return Err(super::invalid(
                "previous origin exchange step committed; retry that exact step to finish its witness before this request",
            ));
        }
        guard.refresh()?;
        capture_exchange_context(
            &mut guard,
            &store.connection,
            repository,
            request,
            operation,
            local_activation.as_ref(),
            mode,
        )?;
    }
    let witness = guard
        .state()
        .witness
        .as_ref()
        .ok_or_else(|| super::invalid("origin witness missing"))?
        .clone();
    // Check the base before retaining a new request. An exact interrupted
    // request may already be committed and is checked against the actual tail.
    {
        let transaction = store.begin_work_mutation()?;
        // Before reserving a fresh request, its private witness still has no
        // pending request. The captured custody nevertheless belongs to the
        // exact upcoming request, so compare that request separately below.
        let checked = check_inner(&transaction, project, &guard, Some(request))?;
        checked.require_on(&transaction)?;
    }
    let mut reserved = witness.clone();
    reserved.pending_exchange = Some(request.clone());
    if reserved != witness {
        private::advance(&database, &witness, &reserved)?;
        guard.refresh()?;
    }
    guard.recheck()?;
    let result = match effect(store, Some(&guard)) {
        Ok(result) => result,
        Err(error) => {
            // Reobserve after rollback. Never erase import/publication custody
            // or a committed transition, even when its caller returned Err.
            if capture_exchange_context(
                &mut guard,
                &store.connection,
                repository,
                request,
                operation,
                local_activation.as_ref(),
                mode,
            )
            .is_ok()
            {
                let _ = release_uncommitted_exchange(store, project, &guard, &reserved);
            }
            return Err(error);
        }
    };
    // The writer succeeded; independently derive its actual committed tail.
    // A failed private write leaves the exact pending request recoverable.
    capture_exchange_context(
        &mut guard,
        &store.connection,
        repository,
        request,
        operation,
        local_activation.as_ref(),
        mode,
    )?;
    let transaction = store.begin_work_mutation()?;
    let checked = check_inner(&transaction, project, &guard, Some(request))?;
    checked.require_on(&transaction)?;
    let mut next = reserved.clone();
    next.cut = cut_on(&transaction, project)?;
    next.exchange = super::ledger::origin_anchor_on(&transaction, project, &origin.origin_id)?;
    next.pending_exchange = None;
    drop(checked);
    drop(transaction);
    private::advance(&database, &reserved, &next)?;
    Ok(result)
}

fn capture_exchange_context(
    guard: &mut PreparedContinuity,
    connection: &Connection,
    repository: &std::path::Path,
    request: &ObjectId,
    operation: Option<uuid::Uuid>,
    activation: Option<&crate::task_sync::origin::recovery::RecoveryReceipt>,
    mode: ExchangeMode<'_>,
) -> Result<(), StoreError> {
    if let Some(activation) = activation {
        guard.capture_recovered_activation(activation, request);
        Ok(())
    } else {
        guard.capture_custody(connection, Some(repository), Some(request), operation)?;
        match mode {
            ExchangeMode::DeltaResolution(input) => guard.admit_delta_resolution(input, request)?,
            ExchangeMode::EvidenceImport(binding, recipe) => {
                guard.admit_evidence_recipe(binding, recipe, request)?;
            }
            ExchangeMode::PlanningSuccessor(binding, retained) => {
                guard.admit_planning_successor(binding, retained, request)?;
            }
            ExchangeMode::Ordinary
            | ExchangeMode::ExactRecovery
            | ExchangeMode::BindingActivation => {}
        }
        Ok(())
    }
}

fn release_uncommitted_exchange(
    store: &mut crate::storage::SqliteStore,
    project: &ProjectId,
    guard: &PreparedContinuity,
    reserved: &private::Witness,
) -> Result<bool, StoreError> {
    guard.recheck()?;
    let request = reserved
        .pending_exchange
        .as_ref()
        .ok_or_else(|| super::invalid("origin exchange reservation missing"))?;
    let transaction = store.begin_work_mutation()?;
    let checked = check_inner(&transaction, project, guard, Some(request))?;
    checked.require_on(&transaction)?;
    let actual =
        super::ledger::origin_anchor_on(&transaction, project, &checked.origin().origin_id)?;
    if actual != reserved.exchange {
        return Ok(false);
    }
    drop(checked);
    drop(transaction);
    let mut next = reserved.clone();
    next.pending_exchange = None;
    private::advance(guard.database(), reserved, &next)?;
    Ok(true)
}

pub(super) fn cut_on(
    connection: &Connection,
    project: &ProjectId,
) -> Result<ExportCut, StoreError> {
    let position: i64 = connection.query_row(
        "SELECT COALESCE((SELECT position FROM work_feed_heads WHERE feed_kind = 'project' AND feed_id = ?1), 0)",
        [&project.0], |row| row.get(0),
    )?;
    let cut = ExportCut {
        position,
        anchor: if position == 0 {
            None
        } else {
            Some(connection.query_row(
            "SELECT object_kind, object_id FROM work_feed_entries WHERE feed_kind = 'project' AND feed_id = ?1 AND position = ?2",
            params![project.0, position], |row| Ok(FeedAnchor {
                object_kind: row.get(0)?,
                object_id: ObjectId::from_stored(row.get::<_, String>(1)?)
                    .ok_or(rusqlite::Error::InvalidQuery)?,
            }),
        )?)
        },
    };
    check_cut_on(connection, project, &cut)?;
    Ok(cut)
}

pub(super) fn check_cut_on(
    connection: &Connection,
    project: &ProjectId,
    cut: &ExportCut,
) -> Result<(), StoreError> {
    if cut.position < 0 || (cut.position == 0) != cut.anchor.is_none() {
        return Err(super::invalid("origin witness feed cut is malformed"));
    }
    if let Some(anchor) = &cut.anchor {
        let actual: Option<(String, String)> = connection.query_row(
            "SELECT object_kind, object_id FROM work_feed_entries WHERE feed_kind = 'project' AND feed_id = ?1 AND position = ?2",
            params![project.0, cut.position], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        if actual != Some((anchor.object_kind.clone(), anchor.object_id.as_str().into())) {
            return Err(super::invalid(
                "origin witness feed anchor missing or divergent; quarantine",
            ));
        }
    }
    Ok(())
}

impl crate::storage::SqliteStore {
    /// Prepare installation and Git custody outside the caller's SQL writer.
    ///
    /// # Errors
    /// Refuses unsafe custody, contention or an inconsistent retained ledger.
    pub(crate) fn prepare_task_sync_origin(
        &self,
        project: &ProjectId,
    ) -> Result<PreparedContinuity, StoreError> {
        let database = std::path::Path::new(
            self.connection
                .path()
                .filter(|path| !path.is_empty())
                .ok_or_else(|| super::invalid("origin requires a file-backed store"))?,
        );
        let mut guard = PreparedContinuity::acquire(database, project)?;
        guard.capture_custody(&self.connection, None, None, None)?;
        Ok(guard)
    }
    /// Read the logical origin without enrolling, adopting or changing files.
    ///
    /// # Errors
    /// Refuses malformed or competing canonical lineage.
    pub fn task_sync_producing_origin(
        &self,
        project: &ProjectId,
    ) -> Result<Option<ProducingOrigin>, StoreError> {
        self.work_read_snapshot(|store| load_on(&store.connection, project))
    }

    /// Cancel an exact private enrollment reservation only after its canonical
    /// absence and unchanged predecessor custody are checked under a writer.
    /// Retained exchange artifacts and the witness are never removed or reset.
    /// Returns false when no reservation remains for an uncommitted operation.
    ///
    /// # Errors
    /// Refuses changed intent, committed enrollment, witness drift or rewind.
    pub fn cancel_task_sync_origin_enrollment(
        &mut self,
        project: &ProjectId,
        actor: &crate::ActorContext,
        request: &EnrollRequest,
    ) -> Result<bool, StoreError> {
        crate::storage::admit_live_actor_session(actor)?;
        if request.operation.is_nil() || actor.actor_id.trim().is_empty() {
            return Err(super::invalid(
                "origin cancellation operation or actor is empty",
            ));
        }
        let database = std::path::PathBuf::from(
            self.connection
                .path()
                .filter(|path| !path.is_empty())
                .ok_or_else(|| {
                    super::invalid("origin cancellation requires a file-backed store")
                })?,
        );
        // Cancellation admits no artifact and therefore does not run the
        // custody-ahead check which may be blocking the enrollment retry.
        let guard = PreparedContinuity::acquire(&database, project)?;
        guard.recheck()?;
        let transaction = self.begin_work_mutation()?;
        if view::operation_exists(&transaction, request.operation)? {
            return Err(super::invalid(
                "origin enrollment committed; retry origin-enroll, never cancel",
            ));
        }
        let current = load_on(&transaction, project)?;
        if current.as_ref().map(|origin| &origin.origin_id) != request.predecessor.as_ref() {
            return Err(super::invalid("origin cancellation predecessor differs"));
        }
        let Some(intent) = &guard.state().pending else {
            return Ok(false);
        };
        if intent.record.project_id != *project
            || intent.record.enrollment_operation_id != request.operation
            || intent.record.predecessor_origin_id != request.predecessor
            || guard.state().witness != intent.expected_witness
        {
            return Err(super::invalid(
                "origin cancellation intent or witness differs",
            ));
        }
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM objects WHERE object_id=?1)",
            [intent.origin.as_str()],
            |row| row.get(0),
        )?;
        if exists {
            return Err(super::invalid(
                "origin enrollment record exists; preserve the retained intent",
            ));
        }
        match (&current, &intent.expected_witness) {
            (None, None) => (),
            (Some(origin), Some(witness))
                if witness.origin == origin.origin_id
                    && witness.enrollment_operation
                        == origin.enrollment.enrollment_operation_id =>
            {
                check_cut_on(&transaction, project, &witness.cut)?;
                if super::ledger::origin_anchor_on(&transaction, project, &origin.origin_id)?
                    != witness.exchange
                {
                    return Err(super::invalid(
                        "origin cancellation exchange anchor differs; preserve custody",
                    ));
                }
                if let Some(anchor) = &witness.exchange {
                    check_cut_on(&transaction, project, &anchor.cut)?;
                }
            }
            _ => {
                return Err(super::invalid(
                    "origin cancellation predecessor witness differs",
                ));
            }
        }
        // Keep both the project reservation and SQL writer through this exact
        // slot removal. No SQL mutation or exchange reservation is cancelled.
        guard.recheck()?;
        private::cancel_after_absence_check(&database, intent)?;
        drop(transaction);
        Ok(true)
    }

    /// Explicit installation enrollment, or an explicitly named logical fork.
    /// Missing private custody never silently adopts a retained database ID.
    ///
    /// # Errors
    /// Refuses contention, changed replay intent, uncertain exchange or lineage.
    pub fn enroll_task_sync_origin(
        &mut self,
        project: &ProjectId,
        actor: &crate::ActorContext,
        request: &EnrollRequest,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<ProducingOrigin, StoreError> {
        crate::storage::admit_live_actor_session(actor)?;
        if request.operation.is_nil() || actor.actor_id.trim().is_empty() {
            return Err(super::invalid(
                "origin enrollment operation or actor is empty",
            ));
        }
        let database = std::path::PathBuf::from(
            self.connection
                .path()
                .filter(|path| !path.is_empty())
                .ok_or_else(|| super::invalid("origin enrollment requires a file-backed store"))?,
        );
        let guard = self.prepare_task_sync_origin(project)?;
        if let Some(current) = load_on(&self.connection, project)?
            && current.enrollment.enrollment_operation_id == request.operation
        {
            if current.enrollment.predecessor_origin_id != request.predecessor {
                return Err(super::invalid(
                    "origin enrollment operation was reused with different intent",
                ));
            }
            if guard
                .state()
                .witness
                .as_ref()
                .is_some_and(|w| w.origin == current.origin_id)
            {
                let transaction = self.begin_work_mutation()?;
                let checked = check_on(&transaction, project, &guard)?;
                checked.require_on(&transaction)?;
                drop(checked);
                drop(transaction);
                if let Some(pending) = &guard.state().pending
                    && pending.record.enrollment_operation_id == request.operation
                    && pending.origin == current.origin_id
                {
                    private::finish(
                        &database,
                        pending,
                        &current,
                        guard
                            .state()
                            .witness
                            .as_ref()
                            .ok_or_else(|| super::invalid("origin witness missing"))?
                            .cut
                            .clone(),
                    )?;
                }
                return Ok(current);
            }
        }
        // Include legacy bindings with no origin: enrollment must not strand
        // an exchange which its newly selected origin would no longer admit.
        if guard
            .state()
            .witness
            .as_ref()
            .is_some_and(|witness| witness.pending_exchange.is_some())
        {
            return Err(super::invalid(
                "origin exchange intent remains pending; recover its exact request before enrollment",
            ));
        }
        super::ledger::require_project_settled_on(&self.connection, project)?;
        super::ledger::check_origin_custody_on(
            &self.connection,
            project,
            guard.custody(),
            None,
            None,
            None,
        )?;
        let intent = if let Some(pending) = &guard.state().pending
            && pending.record.enrollment_operation_id == request.operation
        {
            if pending.record.predecessor_origin_id != request.predecessor
                || &pending.record.project_id != project
            {
                return Err(super::invalid(
                    "origin enrollment operation was reused with different intent",
                ));
            }
            pending.clone()
        } else {
            let current = load_on(&self.connection, project)?;
            if current.as_ref().map(|origin| &origin.origin_id) != request.predecessor.as_ref() {
                return Err(super::invalid(
                    "explicit origin predecessor is required; never adopt an existing origin",
                ));
            }
            if current.is_none() && guard.state().witness.is_some() {
                return Err(super::invalid(
                    "origin witness is ahead of this database; quarantine",
                ));
            }
            private::PendingEnrollment::new(
                Enrollment {
                    project_id: project.clone(),
                    predecessor_origin_id: request.predecessor.clone(),
                    enrollment_operation_id: request.operation,
                    actor: actor.clone(),
                    enrolled_at: now,
                },
                guard.state().witness.clone(),
            )
        };
        let new_intent = guard.state().pending.as_ref() != Some(&intent);
        private::retain(&database, &intent)?;
        // Re-read the retained intent outside the writer; the reservation lives
        // through SQL commit and the post-commit witness replacement.
        let retained = private::read(&database, project)?;
        if retained.pending.as_ref() != Some(&intent) {
            return Err(super::invalid("origin intent changed"));
        }
        let object = intent.object()?;
        let transaction = match self.begin_work_mutation() {
            Ok(transaction) => transaction,
            Err(error) => {
                if new_intent {
                    let _ = private::cancel_uncommitted(&database, &intent);
                }
                return Err(error);
            }
        };
        let staged = (|| {
            let existing = load_on(&transaction, project)?;
            super::ledger::require_project_settled_on(&transaction, project)?;
            // Recheck inside the writer as well as before retaining intent.
            super::ledger::check_origin_custody_on(
                &transaction,
                project,
                guard.custody(),
                None,
                None,
                None,
            )?;
            if let Some(prior) = &existing
                && prior.origin_id != intent.origin
            {
                let checked = check_on(&transaction, project, &guard)?;
                checked.require_on(&transaction)?;
            } else if existing.is_some() {
                check_committed_enrollment_on(&transaction, project, &guard, &intent)?;
            }
            let origin = insert_enrollment_on(&transaction, &object)?;
            let cut = cut_on(&transaction, project)?;
            Ok::<_, StoreError>((origin, cut))
        })();
        let (origin, cut) = match staged {
            Ok(result) => result,
            Err(error) => {
                drop(transaction);
                if new_intent {
                    let _ = private::cancel_uncommitted(&database, &intent);
                }
                return Err(error);
            }
        };
        // Once commit is attempted its outcome can be ambiguous: keep the
        // retained record and let an exact retry validate authoritative state.
        transaction.commit()?;
        private::finish(&database, &intent, &origin, cut)?;
        Ok(origin)
    }
}

/// Recover only the exact retained enrollment whose SQL commit preceded its
/// private witness replacement. A predecessor witness is not a normal replay.
fn check_committed_enrollment_on(
    transaction: &Transaction<'_>,
    project: &ProjectId,
    guard: &PreparedContinuity,
    intent: &private::PendingEnrollment,
) -> Result<(), StoreError> {
    if guard.state().pending.as_ref() != Some(intent)
        || guard.state().witness != intent.expected_witness
    {
        return Err(super::invalid(
            "committed origin recovery has different private custody",
        ));
    }
    if let Some(witness) = &intent.expected_witness {
        let bytes: Vec<u8> = transaction.query_row(
            "SELECT canonical_json FROM objects WHERE object_kind = ?1 AND object_id = ?2",
            params![PRODUCING_ORIGIN_KIND, witness.origin.as_str()],
            |row| row.get(0),
        )?;
        let predecessor: Enrollment = CanonicalObject::stored(&witness.origin, bytes)?.decode()?;
        if predecessor.project_id != *project
            || predecessor.enrollment_operation_id != witness.enrollment_operation
            || witness.pending_exchange.is_some()
        {
            return Err(super::invalid("committed fork predecessor witness differs"));
        }
        check_cut_on(transaction, project, &witness.cut)?;
        if super::ledger::origin_anchor_on(transaction, project, &witness.origin)?
            != witness.exchange
        {
            return Err(super::invalid(
                "committed fork predecessor exchange differs",
            ));
        }
    }
    if super::ledger::origin_anchor_on(transaction, project, &intent.origin)?.is_some() {
        return Err(super::invalid(
            "committed enrollment already has unwitnessed exchange",
        ));
    }
    super::ledger::check_origin_custody_on(transaction, project, guard.custody(), None, None, None)
}

/// Commit the exact record retained by the private enrollment intent. The
/// caller must own writer admission and continuity/fork authorization. This
/// primitive neither activates the installation witness nor commits SQL.
/// Stored attribution describes the original intent, not the retrying process.
/// The caller admits the live actor; historic sessions may already have expired.
pub(super) fn insert_enrollment_on(
    transaction: &Transaction<'_>,
    object: &CanonicalObject,
) -> Result<ProducingOrigin, StoreError> {
    let record: Enrollment = object.decode()?;
    let current = load_on(transaction, &record.project_id)?;
    let existing: Option<(String, Vec<u8>)> = transaction
        .query_row(
            "SELECT object_kind, canonical_json FROM objects WHERE object_id = ?1",
            [object.key().as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((kind, bytes)) = existing {
        if kind != PRODUCING_ORIGIN_KIND || bytes != object.bytes() {
            return Err(super::invalid(
                "origin enrollment retry differs from its retained record",
            ));
        }
        // An exact historical replay never reactivates a superseded origin.
        if current
            .as_ref()
            .is_none_or(|head| &head.origin_id != object.key())
        {
            return Err(super::invalid("origin enrollment has been superseded"));
        }
        return Ok(ProducingOrigin {
            origin_id: object.key().clone(),
            enrollment: record,
        });
    }
    if current.as_ref().map(|head| &head.origin_id) != record.predecessor_origin_id.as_ref() {
        return Err(super::invalid("origin enrollment predecessor changed"));
    }
    // Validate all proposed fields and operation identity before inserting.
    if record.project_id.0.trim().is_empty()
        || record.actor.actor_id.trim().is_empty()
        || record.enrollment_operation_id.is_nil()
        || object.bytes().len() > MAX_RECORD_BYTES
    {
        return Err(super::invalid("invalid origin enrollment record"));
    }
    let count: i64 = transaction.query_row(
        "SELECT count(*) FROM objects WHERE object_kind = ?1",
        [PRODUCING_ORIGIN_KIND],
        |row| row.get(0),
    )?;
    let count = usize::try_from(count)
        .map_err(|_| super::invalid("origin lineage count exceeds the supported bound"))?;
    if count >= MAX_LINEAGE_RECORDS {
        return Err(super::invalid(
            "origin lineage capacity exceeded; preserve existing records",
        ));
    }
    if view::operation_exists(transaction, record.enrollment_operation_id)? {
        return Err(super::invalid(
            "origin enrollment operation already names another record",
        ));
    }
    crate::storage::SqliteStore::insert_object(transaction, PRODUCING_ORIGIN_KIND, object)?;
    super::super::feeds::append_to_project_feed(
        transaction,
        &record.project_id,
        PRODUCING_ORIGIN_KIND,
        object,
    )?;
    view::record(transaction, object.key(), &record)?;
    Ok(ProducingOrigin {
        origin_id: object.key().clone(),
        enrollment: record,
    })
}

const MAX_LINEAGE_RECORDS: usize = 4096;
const MAX_RECORD_BYTES: usize = 16 * 1024;

pub(super) fn verify_on(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    view::verify(connection, checked, invalid)
}

/// Read one project's projected head and its canonical record/feed proof.
/// This grants no continuity admission or exhaustive history validation.
/// The caller owns the transaction; this function never starts or commits one.
pub(crate) fn load_on(
    connection: &Connection,
    project: &ProjectId,
) -> Result<Option<ProducingOrigin>, StoreError> {
    view::load(connection, project)
}
