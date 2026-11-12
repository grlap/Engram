//! Project-bearing fields cannot hide a different project in a transport pack.

use super::{EvidenceError, RecordEnvelope, RecordKind as K, decode, domain};
use crate::ProjectId;

/// Check explicit project fields without inferring project identity from a path.
/// Project-free historical records remain qualified by their original enrollment.
///
/// # Errors
/// Refuses a foreign project or a memory outside the shared work scope.
pub fn check_project(record: &RecordEnvelope, expected: &ProjectId) -> Result<(), EvidenceError> {
    macro_rules! project {
        ($body:ty) => {
            Some(decode::<$body>(record)?.project_id)
        };
    }
    let project = match record.kind() {
        K::ImportedConsumption => project!(super::super::consumption::ImportedEvidenceConsumption),
        K::VerificationApplicability => {
            project!(super::super::verification::VerificationApplicabilityDecision)
        }
        K::AcceptanceUse => {
            Some(decode::<super::super::acceptance::AcceptanceUseDecision>(record)?.project)
        }
        K::ProducingOrigin => project!(crate::task_sync::origin::Enrollment),
        K::RegisteredHostRun => project!(domain::RegisteredHostRun),
        K::VerificationEvidence => project!(domain::VerificationEvidence),
        K::RemoteTerminalOrigin => project!(domain::RemoteTerminalOrigin),
        K::RemoteVerificationAssociation => project!(domain::RemoteVerificationAssociationRecord),
        K::ExecutionObservation => project!(domain::ExecutionObservation),
        K::UnadmittedExecutionObservation => project!(domain::UnadmittedExecutionObservation),
        K::EnvironmentEvidence => project!(domain::EnvironmentEvidence),
        K::AcceptanceEvaluation => project!(domain::AcceptanceEvaluation),
        K::WorkObligation => project!(domain::WorkObligation),
        K::WorkObligationResolution => project!(domain::WorkObligationResolutionEvent),
        K::NamedRootBinding => project!(domain::NamedRootBindingEvent),
        K::WorkObservation => project!(domain::WorkObservation),
        K::WorkRestoredRecord => project!(crate::graph_snapshot::RestoredRecord),
        K::WorkEvent => project!(domain::WorkEvent),
        K::WorkItemRevision => project!(domain::WorkItem),
        K::WorkSourceProposal => project!(domain::WorkSourceProposal),
        K::CompletionSeal => Some(
            decode::<domain::CompletionSeal>(record)?
                .root_execution
                .project_id,
        ),
        K::WorkRootDelta => Some(
            decode::<domain::RootExecutionDelta>(record)?
                .header
                .project_id,
        ),
        K::OriginFeedSegment => Some(
            decode::<super::super::witnesses::OriginFeedSegment>(record)?
                .source
                .project_id,
        ),
        K::OriginFeedCut => Some(
            decode::<super::super::witnesses::OriginFeedCut>(record)?
                .source
                .project_id,
        ),
        K::GoverningSnapshotWitness => Some(
            decode::<super::super::witnesses::GoverningSnapshotWitness>(record)?
                .basis
                .project_id,
        ),
        K::MemoryVersion => match decode::<domain::MemoryVersion>(record)?.scope {
            domain::Scope::Work { project, .. } => Some(project),
            _ => {
                return Err(EvidenceError::InvalidPack(
                    "only shared work memory belongs in evidence exchange",
                ));
            }
        },
        K::WorkEvidence
        | K::WorkCheckpoint
        | K::WorkHandoffOffer
        | K::WorkBlocker
        | K::ControlPolicy
        | K::ObligationRuleSet
        | K::MemoryAssertionEvent
        | K::WorkRestoredEvidence
        | K::WorkSourceSnapshot => None,
    };
    if project.as_ref().is_some_and(|project| project != expected) {
        return Err(EvidenceError::Dependency {
            object: record.object_id().clone(),
            problem: "evidence original belongs to another project",
        });
    }
    Ok(())
}
