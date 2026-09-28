//! Which receipt each work operation stores, and how the doctor reads it.
//!
//! Two stores hold what an operation returned, for exact replay: a core
//! operation keeps its receipt in `work_operation_results`, and an ambient
//! protocol operation keeps its caller-visible result in
//! `work_protocol_attempts`. The tables below name every operation that may
//! store one and the type it stores. Persisting refuses any other name, and
//! the doctor decodes each stored row as its operation's type, so a receipt
//! this build cannot read is found before replay, and before a migration
//! import publishes it. Completion results and the receipts of retired
//! operations are the exceptions: they are registered but not decoded (see
//! [`Receipt::NotDecoded`] and [`Receipt::Retired`]).

use rusqlite::Connection;
use serde::de::DeserializeOwned;

use super::{
    AcceptanceEvaluationReceipt, CompletionSeal, ObjectId, WorkBlocker, WorkClaim,
    WorkHandoffOffer, WorkItem, WorkNoteCapture, WorkObligationResolutionEvent, WorkRun,
};
use crate::{
    domain::{
        NextReadyChildClaim, RejectRequiredChildReceipt, RequiredChildWaiver, WorkDecomposition,
        WorkImportReceipt, WorkPlanReceipt, WorkRelease,
    },
    storage::{StoreError, undecodable_json_reason},
    work_service::{WorkHandoffResult, WorkNoteResult, WorkProposeResult, WorkUpdateResult},
};

#[cfg(test)]
mod tests;

/// Reads one stored receipt; `Err` holds a shape-only reason, never a value
/// from the receipt.
type Decoder = fn(&[u8]) -> Result<(), String>;

/// How the receipts stored under one operation name are read.
#[derive(Clone, Copy)]
pub(super) enum Receipt {
    /// An operation this build runs: every stored receipt decodes as its type.
    Decoded(Decoder),
    /// An operation this build runs whose stored results are not decoded:
    /// older ones lack a member its current result type requires. They stay
    /// as stored, and replaying one fails, until an import conversion, which
    /// this build does not have, rewrites them. Newer results are not
    /// checked either, since the doctor cannot tell them apart by name.
    NotDecoded,
    /// An operation this build no longer runs. Its receipts stay as stored
    /// and are never decoded or reported, and nothing new is stored under
    /// its name.
    Retired,
}

/// Core operations and the receipt each stores in `work_operation_results`.
const CORE_RECEIPTS: &[(&str, Receipt)] = &[
    ("accept_work_handoff", Receipt::Decoded(decode::<WorkClaim>)),
    ("add_work_blocker", Receipt::Decoded(decode::<WorkBlocker>)),
    (
        "add_work_prerequisite",
        Receipt::Decoded(decode::<WorkItem>),
    ),
    (
        "cancel_work_handoff",
        Receipt::Decoded(decode::<WorkHandoffOffer>),
    ),
    ("checkpoint_work", Receipt::Decoded(decode::<ObjectId>)),
    (
        "claim_next_ready_child",
        Receipt::Decoded(decode::<NextReadyChildClaim>),
    ),
    ("claim_work", Receipt::Decoded(decode::<WorkClaim>)),
    ("clear_work_blocker", Receipt::Decoded(decode::<WorkItem>)),
    ("complete_work", Receipt::Decoded(decode::<CompletionSeal>)),
    ("complete_work_recovery", Receipt::Retired),
    ("create_work", Receipt::Decoded(decode::<WorkItem>)),
    (
        "decompose_work",
        Receipt::Decoded(decode::<WorkDecomposition>),
    ),
    ("detach_work", Receipt::Decoded(decode::<WorkItem>)),
    ("dispose_work", Receipt::Decoded(decode::<WorkItem>)),
    ("import_work", Receipt::Decoded(decode::<WorkImportReceipt>)),
    (
        "offer_work_handoff",
        Receipt::Decoded(decode::<WorkHandoffOffer>),
    ),
    (
        crate::storage::PLAN_CORE_OPERATION,
        Receipt::Decoded(decode::<WorkPlanReceipt>),
    ),
    (
        "record_acceptance_evaluation",
        Receipt::Decoded(decode::<AcceptanceEvaluationReceipt>),
    ),
    (
        "record_restored_work_evidence",
        Receipt::Decoded(decode::<ObjectId>),
    ),
    ("record_work_evidence", Receipt::Decoded(decode::<ObjectId>)),
    (
        "record_work_note",
        Receipt::Decoded(decode::<WorkNoteCapture>),
    ),
    (
        "reject_required_child",
        Receipt::Decoded(decode::<RejectRequiredChildReceipt>),
    ),
    ("release_work", Receipt::Decoded(decode::<WorkRelease>)),
    (
        "remove_work_prerequisite",
        Receipt::Decoded(decode::<WorkItem>),
    ),
    ("reopen_work", Receipt::Decoded(decode::<WorkRun>)),
    ("revise_work", Receipt::Decoded(decode::<WorkItem>)),
    (
        "waive_required_child",
        Receipt::Decoded(decode::<RequiredChildWaiver>),
    ),
    (
        "waive_work_obligation",
        Receipt::Decoded(decode::<WorkObligationResolutionEvent>),
    ),
];

/// Ambient protocol operations and the result each stores in
/// `work_protocol_attempts`. Completion results are not decoded: older ones
/// lack the asserted criterion count the current result requires.
const PROTOCOL_RESULTS: &[(&str, Receipt)] = &[
    ("work_complete", Receipt::NotDecoded),
    ("work_handoff:accept", HANDOFF),
    ("work_handoff:cancel", HANDOFF),
    ("work_handoff:offer", HANDOFF),
    (
        crate::storage::DECOMPOSE_PROTOCOL_OPERATION,
        Receipt::Decoded(decomposition_proposal),
    ),
    (
        crate::storage::PLAN_PROTOCOL_OPERATION,
        Receipt::Decoded(plan_proposal),
    ),
    ("work_propose:root", Receipt::Decoded(root_proposal)),
    ("work_update:add_prerequisite", UPDATE),
    ("work_update:block", UPDATE),
    ("work_update:cancel", UPDATE),
    ("work_update:checkpoint", UPDATE),
    ("work_update:claim", UPDATE),
    ("work_update:claim_next_ready", UPDATE),
    ("work_update:detach", UPDATE),
    ("work_update:evidence", UPDATE),
    ("work_update:gate", UPDATE),
    (
        "work_update:note",
        Receipt::Decoded(decode::<WorkNoteResult>),
    ),
    ("work_update:reject", UPDATE),
    ("work_update:release", UPDATE),
    ("work_update:remove_prerequisite", UPDATE),
    ("work_update:reopen", UPDATE),
    ("work_update:revise", UPDATE),
    ("work_update:supersede", UPDATE),
    ("work_update:unblock", UPDATE),
    ("work_update:waive_required_child", UPDATE),
];

const UPDATE: Receipt = Receipt::Decoded(decode::<WorkUpdateResult>);
const HANDOFF: Receipt = Receipt::Decoded(decode::<WorkHandoffResult>);

/// Why a stored receipt is reported: its operation is registered nowhere.
const UNREGISTERED: &str = "an operation this build does not register";
/// Why a stored proposal is reported: it decodes, but as another kind.
const ANOTHER_KIND: &str = "a proposal of another kind";

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<(), String> {
    decode_value::<T>(bytes).map(drop)
}

fn decode_value<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    serde_json::from_slice(bytes).map_err(|error| undecodable_json_reason(&error))
}

fn root_proposal(bytes: &[u8]) -> Result<(), String> {
    match decode_value(bytes)? {
        WorkProposeResult::Root { .. } => Ok(()),
        _ => Err(ANOTHER_KIND.into()),
    }
}

fn decomposition_proposal(bytes: &[u8]) -> Result<(), String> {
    match decode_value(bytes)? {
        WorkProposeResult::Decomposition(_) => Ok(()),
        _ => Err(ANOTHER_KIND.into()),
    }
}

fn plan_proposal(bytes: &[u8]) -> Result<(), String> {
    match decode_value(bytes)? {
        WorkProposeResult::Plan(_) => Ok(()),
        _ => Err(ANOTHER_KIND.into()),
    }
}

fn find(table: &[(&str, Receipt)], operation: &str) -> Option<Receipt> {
    table
        .iter()
        .find(|(name, _)| *name == operation)
        .map(|(_, receipt)| *receipt)
}

/// Refuses to store a receipt under a name that is registered nowhere, or
/// under a retired one.
fn admit(table: &[(&str, Receipt)], family: &str, operation: &str) -> Result<(), StoreError> {
    match find(table, operation) {
        Some(Receipt::Decoded(_) | Receipt::NotDecoded) => Ok(()),
        Some(Receipt::Retired) | None => Err(StoreError::InvalidWorkProjection(format!(
            "{family} operation {operation} stores no receipt this build registers"
        ))),
    }
}

/// Refuses a core receipt whose operation is not registered or is retired.
pub(super) fn admit_core_receipt(operation: &str) -> Result<(), StoreError> {
    admit(CORE_RECEIPTS, "work", operation)
}

/// Refuses a protocol attempt or result whose operation is not registered
/// or is retired.
pub(super) fn admit_protocol_result(operation: &str) -> Result<(), StoreError> {
    admit(PROTOCOL_RESULTS, "work-protocol", operation)
}

/// Why the doctor reports one stored receipt of a registered operation, if
/// it does: the operation's type cannot read it. A retired or undecoded
/// operation's receipt is never reported. The callers report an
/// unregistered operation themselves.
fn problem(table: &[(&str, Receipt)], operation: &str, bytes: Option<&[u8]>) -> Option<String> {
    match find(table, operation)? {
        Receipt::Decoded(decode) => bytes.and_then(|bytes| decode(bytes).err()),
        Receipt::NotDecoded | Receipt::Retired => None,
    }
}

/// Why the doctor reports a protocol attempt's stored result, if it does.
/// A pending attempt, which holds no result yet, is reported only when its
/// operation is not registered.
pub(super) fn protocol_result_problem(operation: &str, result: Option<&[u8]>) -> Option<String> {
    if find(PROTOCOL_RESULTS, operation).is_none() {
        return Some(UNREGISTERED.into());
    }
    problem(PROTOCOL_RESULTS, operation, result)
}

/// Decodes every stored core receipt as its operation's type, one row at a
/// time, and reports each one that does not decode or whose operation is not
/// registered, as `work_operation_result:<operation>:<key>:<reason>`.
pub(super) fn verify_work_operation_results(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut statement = connection.prepare(
        "SELECT operation, idempotency_key, result_json FROM work_operation_results
         ORDER BY operation, idempotency_key",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        *checked += 1;
        let operation: String = row.get(0)?;
        let key: String = row.get(1)?;
        let reason = if find(CORE_RECEIPTS, &operation).is_none() {
            Some(UNREGISTERED.to_owned())
        } else {
            let receipt: Vec<u8> = row.get(2)?;
            problem(CORE_RECEIPTS, &operation, Some(&receipt))
        };
        if let Some(reason) = reason {
            invalid.push(format!("work_operation_result:{operation}:{key}:{reason}"));
        }
    }
    Ok(())
}

/// Every test build checks, as each receipt is stored, that its operation's
/// registered type reads it back.
#[cfg(test)]
pub(super) fn assert_core_receipt_decodes(operation: &str, bytes: &[u8]) {
    if let Some(reason) = problem(CORE_RECEIPTS, operation, Some(bytes)) {
        panic!("{operation} stored a receipt its registered type cannot read: {reason}");
    }
}

/// [`assert_core_receipt_decodes`] for an ambient protocol result.
#[cfg(test)]
pub(super) fn assert_protocol_result_decodes(operation: &str, bytes: &[u8]) {
    if let Some(reason) = problem(PROTOCOL_RESULTS, operation, Some(bytes)) {
        panic!("{operation} stored a result its registered type cannot read: {reason}");
    }
}
