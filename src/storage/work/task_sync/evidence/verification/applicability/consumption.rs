//! Fresh destination checks for an explicitly selected existing B decision.

use super::{
    APPLICABILITY_KIND, Connection, ControlWorkBinding, Decision, Destination, ObjectId, Result,
    VerificationUse, candidate_in_context, eligible_for_on, invalid, object_on, union_on,
};
use crate::task_sync::evidence::consumption::ConsumedProof;

pub(crate) struct QualifiedVerification {
    pub proof: ConsumedProof,
}

pub(crate) fn qualify_on(
    connection: &Connection,
    id: &ObjectId,
    binding: &ControlWorkBinding,
    destination: &Destination,
    source: &crate::source::SourceId,
) -> Result<QualifiedVerification> {
    let (kind, bytes) = object_on(connection, id)?
        .ok_or_else(|| invalid("imported verification decision is unavailable"))?;
    if kind != APPLICABILITY_KIND {
        return Err(invalid("selected use is not a verification decision"));
    }
    let decision: Decision = serde_json::from_slice(&bytes)?;
    if let Some(criterion) = decision.criterion
        && !destination
            .item
            .acceptance_bindings
            .iter()
            .any(|bound| bound.criterion == criterion && bound.requirement == decision.requirement)
    {
        return Err(invalid("imported verification criterion binding changed"));
    }
    let governing = crate::CanonicalObject::freeze(&destination.item)?
        .key()
        .clone();
    let context = VerificationUse {
        project: &destination.item.project_id,
        origin: &decision.origin,
        binding,
        original: &decision.original,
        criterion: decision.criterion,
        requirement: Some(&decision.requirement),
        governing: &governing,
        root: &destination.root,
        source,
    };
    if !eligible_for_on(connection, id, &context)? {
        return Err(invalid("imported verification is not currently applicable"));
    }
    // Reuse the origin-qualified reader, including its complete producer/cut
    // context. Retaining a decision never substitutes for the original graph.
    let (evidence, producer, execution) = candidate_in_context(
        connection,
        &destination.item.project_id,
        &decision.origin,
        &decision.original,
        &decision.origin_cut,
    )?;
    crate::control::check_verification_result(&evidence, &producer, &decision.requirement)
        .map_err(|_| invalid("imported verification no longer qualifies"))?;
    let contributors = execution.as_ref().map_or_else(
        || Ok(Vec::new()),
        |key| {
            let union = union_on(connection, key)?;
            if union.conflicted || union.contributors.is_empty() {
                return Err(invalid("imported verification terminal union conflicts"));
            }
            Ok(union.contributors)
        },
    )?;
    Ok(QualifiedVerification {
        proof: ConsumedProof::Verification {
            origin: decision.origin,
            original: decision.original,
            origin_cut: decision.origin_cut,
            requirement: decision.requirement,
            criterion: decision.criterion,
            execution,
            contributors,
        },
    })
}
