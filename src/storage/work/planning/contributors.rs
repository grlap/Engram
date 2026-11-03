use super::{CompletionWaiver, HashSet, ObjectId, RootContribution, RootExecution, SessionId};

pub(in crate::storage::work) fn unique_hashes(values: &[ObjectId]) -> Vec<ObjectId> {
    let mut seen = HashSet::new();
    values
        .iter()
        .filter(|value| seen.insert((*value).clone()))
        .cloned()
        .collect()
}

pub(in crate::storage::work) fn expect_root_contributor(
    execution: &mut RootExecution,
    participant: &SessionId,
) -> bool {
    if execution.expected_contributors.contains(participant) {
        return false;
    }
    execution.expected_contributors.push(participant.clone());
    execution
        .expected_contributors
        .sort_by(crate::storage::work::root_state::compare_contributors);
    true
}

pub(in crate::storage::work) fn add_root_contribution(
    execution: &mut RootExecution,
    participant: &SessionId,
    object: &ObjectId,
) -> bool {
    let contribution = RootContribution {
        participant: participant.clone(),
        object: object.clone(),
    };
    if execution.contributions.contains(&contribution) {
        return false;
    }
    execution.contributions.push(contribution);
    execution
        .contributions
        .sort_by(crate::storage::work::root_state::compare_contributions);
    true
}

pub(in crate::storage::work) fn waive_root_contributor(
    execution: &mut RootExecution,
    participant: &SessionId,
    waived_by: &str,
    reason: &str,
) -> bool {
    if execution
        .contributions
        .iter()
        .any(|contribution| &contribution.participant == participant)
        || execution
            .waivers
            .iter()
            .any(|waiver| &waiver.participant == participant)
    {
        return false;
    }
    execution.waivers.push(CompletionWaiver {
        participant: participant.clone(),
        waived_by: waived_by.to_owned(),
        reason: reason.trim().to_owned(),
    });
    execution
        .waivers
        .sort_by(crate::storage::work::root_state::compare_waivers);
    true
}

pub(in crate::storage::work) fn first_unaccounted_root_contributor(
    execution: &RootExecution,
) -> Option<&SessionId> {
    execution
        .expected_contributors
        .iter()
        .find(|participant| !root_participant_is_accounted(execution, participant))
}

pub(in crate::storage::work) fn root_participant_is_accounted(
    execution: &RootExecution,
    participant: &SessionId,
) -> bool {
    execution
        .contributions
        .iter()
        .any(|contribution| &contribution.participant == participant)
        || execution
            .waivers
            .iter()
            .any(|waiver| &waiver.participant == participant)
}
