//! Deterministic verification matching and work-obligation rules.

use crate::domain::{
    BuiltinObligationRuleRef, BuiltinObligationTrigger, OBLIGATION_RULE_SET_SCHEMA_VERSION,
    ObligationRuleDefinition, ObligationRuleSet, SourceObservation, SourceRootState,
    StaleSourceDecider, VerificationEvidence, VerificationEvidenceMismatch,
    VerificationRequirement, VerificationResult, WorkEvidenceKind, WorkObligation,
};
use serde::Serialize;

/// Whether this selected check applies to the actual judged sighting.
///
/// # Errors
/// A missing or different tree leaves it attributed but non-qualifying.
pub fn check_verification_applicability(
    applicability: Option<&crate::domain::VerificationApplicability>,
    judged: Option<&crate::domain::ExecutionSourceBasis>,
) -> Result<(), VerificationEvidenceMismatch> {
    let Some(crate::domain::VerificationApplicability::GitTree { tree }) = applicability else {
        return Ok(());
    };
    let actual = judged
        .and_then(|basis| basis.git_tree.as_ref())
        .ok_or(VerificationEvidenceMismatch::GitTreeMissing)?;
    if actual != tree {
        return Err(VerificationEvidenceMismatch::GitTreeMismatch);
    }
    Ok(())
}

/// Immutable inputs for matching typed verification evidence at one exact
/// dense run-feed cut.
#[derive(Clone, Copy)]
pub struct VerificationEvidenceMatchInput<'a> {
    /// Relevant host-measured sighting at the cut, never a substituted producer.
    pub judged_basis: Option<&'a crate::domain::ExecutionSourceBasis>,
    pub candidate_kind: WorkEvidenceKind,
    pub evidence: Option<&'a VerificationEvidence>,
    pub producer: Option<&'a dyn crate::domain::VerificationProducer>,
    /// The newest source mutation the run observed at the cut, with its
    /// run-feed position, or `None` when the run observed no mutation: then
    /// the verification is of the run's source as it stands, and the checks
    /// that compare it with a mutation do not apply.
    pub latest_mutation: Option<(&'a SourceObservation, i64)>,
    /// The claim's active named root at this cut. Without one, historical
    /// cross-workspace content matching retains its original behavior.
    pub named_root: Option<NamedRootEvidenceMatch<'a>>,
    pub evidence_position: i64,
    /// The run-feed position of the check's producer observation. Under a
    /// named root the check itself, not only its later verification record,
    /// must follow the binding and any change whose root is unknown, and
    /// after an unadmitted change it must follow that change.
    pub producer_position: Option<i64>,
    /// When the latest change is an unadmitted record and no named root
    /// holds: the newest measured sighting at the cut in that change's
    /// workspace, with its run-feed position. Its revision, not the
    /// unadmitted record's, is the one a check must carry. `None` keeps the
    /// change's own revision.
    pub measured_sighting: Option<(&'a crate::domain::ExecutionSourceBasis, i64)>,
    pub requirement: &'a VerificationRequirement,
}

/// Host-recorded source context for a named-root verification.
#[derive(Clone, Copy)]
pub struct NamedRootEvidenceMatch<'a> {
    pub workspace_id: &'a str,
    pub generation: i64,
    pub binding_position: i64,
    pub latest_sighting: Option<(&'a SourceObservation, i64)>,
    pub unknown_change_position: Option<i64>,
}

/// Applies the anti-stale verification rule without performing I/O.
///
/// `source_revision` is a host-computed fingerprint of complete workspace
/// content (committed state plus dirty-tree content). Without a named root,
/// workspace identity is retained for audit but does not participate in
/// equality: a peer worktree may verify the same exact content fingerprint.
/// Under a named root the check, its producer and the newest sighting must
/// carry the root's exact workspace, generation and `named` state, and the
/// check must run after the binding and after any change whose root is
/// unknown.
///
/// # Errors
///
/// Returns the first typed mismatch that prevents the candidate from
/// satisfying the exact verification requirement at this run-feed cut.
pub fn match_verification_evidence(
    input: &VerificationEvidenceMatchInput<'_>,
) -> Result<(), VerificationEvidenceMismatch> {
    explain_verification_evidence(input).map_err(|(mismatch, _)| mismatch)
}

/// [`match_verification_evidence`], with the source record that decided a
/// `stale_source_revision` mismatch beside it. The decision is the same; the
/// decider is `Some` exactly when the mismatch is stale.
///
/// # Errors
///
/// Returns the first typed mismatch, as [`match_verification_evidence`]
/// does, and for a stale one the record that decided it.
pub fn explain_verification_evidence(
    input: &VerificationEvidenceMatchInput<'_>,
) -> Result<(), (VerificationEvidenceMismatch, Option<StaleSourceDecider>)> {
    let plain = |mismatch: VerificationEvidenceMismatch| (mismatch, None);
    let stale = |decider: StaleSourceDecider| {
        (
            VerificationEvidenceMismatch::StaleSourceRevision,
            Some(decider),
        )
    };
    if input.candidate_kind != WorkEvidenceKind::Verification {
        return Err(plain(VerificationEvidenceMismatch::WrongKind));
    }
    let evidence = input
        .evidence
        .ok_or(plain(VerificationEvidenceMismatch::WrongKind))?;
    let producer = input
        .producer
        .ok_or(plain(VerificationEvidenceMismatch::InvalidProducer))?
        .verification_facts();
    if evidence.check_kind != input.requirement.check_kind {
        return Err(plain(VerificationEvidenceMismatch::CheckKindMismatch));
    }
    let registered_git = producer.registered_git_basis(evidence).is_some();
    let mutation = input
        .latest_mutation
        .map(|(latest_mutation, position)| {
            let basis = latest_mutation
                .source_basis
                .as_ref()
                .ok_or(plain(VerificationEvidenceMismatch::InvalidProducer))?;
            let observed_at = latest_mutation
                .observed_at
                .ok_or(plain(VerificationEvidenceMismatch::InvalidProducer))?;
            Ok((latest_mutation, position, basis, observed_at))
        })
        .transpose()?;
    // A bound record's check ran on the original's run: its producer belongs
    // there, while the record and this run's changes belong here. Its position
    // on this run stands in for the producer's in every comparison with this
    // run, and its binding time for the check's time; the original's own
    // facts were validated on the original's run when it was bound.
    let bound = evidence.bound_from.as_ref();
    let producer_position = if bound.is_some() {
        Some(input.evidence_position)
    } else {
        input.producer_position
    };
    let producer_matches = match bound {
        Some(source) => {
            producer.project_id == evidence.project_id
                && producer.binding.work_id == source.work_id
                && producer.binding.run_id == source.run_id
        }
        None => {
            producer.project_id == evidence.project_id
                && producer.binding == evidence.binding
                && producer.session_id == evidence.session_id
        }
    };
    let same_run = producer_matches
        && input.latest_mutation.is_none_or(|(latest_mutation, _)| {
            evidence.project_id == latest_mutation.project_id
                && evidence.binding.root_execution_id == latest_mutation.binding.root_execution_id
                && evidence.binding.work_id == latest_mutation.binding.work_id
                && evidence.binding.run_id == latest_mutation.binding.run_id
        });
    if !same_run {
        return Err(plain(VerificationEvidenceMismatch::WrongRun));
    }
    // Under a named root the newest sighting in the root decides the revision
    // a check must carry. A quiet move after the root's newest known change,
    // or a change whose root is unknown, leaves that change's revision
    // behind; the check must still run after the change.
    let mut revision_from_root = false;
    if let Some(root) = input.named_root {
        let correct_root = |basis: &crate::domain::ExecutionSourceBasis| {
            basis.workspace_id == root.workspace_id
                && basis.source_root_generation == Some(root.generation)
                && basis.source_root_state == Some(SourceRootState::Named)
        };
        let producer_position =
            producer_position.ok_or(plain(VerificationEvidenceMismatch::InvalidProducer))?;
        if !correct_root(&evidence.source_basis)
            || (bound.is_none() && !producer.source_basis.as_ref().is_some_and(correct_root))
            || input.evidence_position <= root.binding_position
            || producer_position <= root.binding_position
        {
            return Err(stale(StaleSourceDecider::RootBinding));
        }
        if !registered_git && let Some((sighting, _)) = root.latest_sighting {
            let basis = sighting
                .source_basis
                .as_ref()
                .ok_or(plain(VerificationEvidenceMismatch::InvalidProducer))?;
            if !correct_root(basis)
                || evidence.source_basis.source_revision != basis.source_revision
            {
                return Err(stale(StaleSourceDecider::RootSighting));
            }
            revision_from_root = true;
        }
        if root
            .unknown_change_position
            .is_some_and(|unknown| producer_position <= unknown)
        {
            return Err(plain(VerificationEvidenceMismatch::NotAfterMutation));
        }
    }
    // An unadmitted change records where the source was when the host saw
    // it, which may be older than sightings recorded since. Without a named
    // root the newest measured sighting decides the revision instead, so a
    // late report of an old revision asks for a check after it without
    // pinning that revision.
    let measured = input
        .measured_sighting
        .filter(|_| !revision_from_root && mutation.is_some_and(|(change, ..)| !change.admitted));
    if let Some((latest_mutation, _, latest_basis, _)) = mutation {
        if !latest_mutation.source_changed {
            return Err(stale(StaleSourceDecider::LatestChange));
        }
        if !registered_git && !revision_from_root {
            match measured {
                Some((sighting, _))
                    if evidence.source_basis.source_revision != sighting.source_revision =>
                {
                    return Err(stale(StaleSourceDecider::MeasuredSighting));
                }
                None if evidence.source_basis.source_revision != latest_basis.source_revision => {
                    return Err(stale(StaleSourceDecider::LatestChange));
                }
                _ => {}
            }
        }
    }
    if evidence.check_fingerprint != producer.action_fingerprint
        || input
            .requirement
            .check_fingerprint
            .as_ref()
            .is_some_and(|required| required != &evidence.check_fingerprint)
    {
        return Err(plain(
            VerificationEvidenceMismatch::CheckFingerprintMismatch,
        ));
    }
    if evidence.result != VerificationResult::Passed
        || producer
            .run_outcome
            .is_some_and(|outcome| outcome != crate::domain::ExecutionOutcome::Succeeded)
    {
        return Err(plain(VerificationEvidenceMismatch::ResultNotPassed));
    }
    if mutation.is_some_and(|(latest_mutation, latest_mutation_position, _, _)| {
        input.evidence_position <= latest_mutation_position
            || ((input.named_root.is_some() || !latest_mutation.admitted)
                && producer_position.is_none_or(|position| position <= latest_mutation_position))
    }) {
        return Err(plain(VerificationEvidenceMismatch::NotAfterMutation));
    }
    let actor_matches = evidence.actor.session_id.as_ref() == Some(&evidence.session_id)
        && evidence.actor.run_id.as_deref() == Some(evidence.binding.run_id.0.to_string().as_str())
        && producer.actor.session_id.as_ref() == Some(&producer.session_id)
        && producer.actor.run_id.as_deref() == Some(producer.binding.run_id.0.to_string().as_str());
    // A check completed before an unadmitted change was recorded may be an
    // old result whose records arrived late: it cannot follow that change,
    // whenever its record was written, so that floor holds the check's own
    // completion. A bound record stands on this run at its binding time
    // against an admitted change, whose revision it was matched to.
    let run_time = evidence.run_time();
    let times_are_monotone = mutation.is_none_or(|(latest_mutation, _, _, latest_observed_at)| {
        run_time >= latest_observed_at
            && (latest_mutation.admitted || evidence.completed_at >= latest_mutation.recorded_at)
    }) && evidence.completed_at <= evidence.recorded_at
        && producer.observed_at == Some(evidence.completed_at)
        && producer.recorded_at >= evidence.completed_at;
    if !actor_matches || !times_are_monotone {
        return Err(plain(VerificationEvidenceMismatch::InvalidTime));
    }
    if registered_git {
        // Obligation reconstruction has no judged input: it validates the
        // producer's recorded input. Admission and evaluated completion supply
        // the separately declared input to compare against it.
        let judged = input.judged_basis.unwrap_or(&evidence.source_basis);
        if producer.matches_registered_git_input(evidence, &judged.source_revision) != Some(true) {
            return Err(stale(StaleSourceDecider::LatestChange));
        }
    }
    check_verification_applicability(evidence.applicability.as_ref(), input.judged_basis)
        .map_err(plain)
}

/// Rule id of the stock rule that asks for a passing test after each source
/// change. It records rather than blocks: completion resolves an obligation
/// it opened that no matching passing test followed as an attributed waiver,
/// and the item discloses that change as untested.
pub const SOURCE_CHANGE_RULE_ID: &str = "source_mutation_requires_test";

/// Whether an obligation opened for `rule` with `requirement` is the stock
/// source-change rule's, whose open obligations completion records as
/// untested instead of refusing. It matches the stock definition exactly, so
/// an operator-selected rule that reuses the id with another version or a
/// pinned check still blocks.
#[must_use]
pub fn is_stock_source_change_obligation(
    rule: &BuiltinObligationRuleRef,
    requirement: &VerificationRequirement,
) -> bool {
    builtin_obligation_rule_set()
        .rules
        .iter()
        .any(|definition| {
            definition.trigger == BuiltinObligationTrigger::SourceChanged
                && definition.rule == *rule
                && definition.requirement == *requirement
        })
}

/// Whether `rule` opened its obligation from a source change, under the stock
/// rule or an operator-selected one, rather than from an acceptance binding.
/// Every rule a rule set holds is triggered by a source change.
#[must_use]
pub fn is_source_change_obligation(rule: &BuiltinObligationRuleRef) -> bool {
    acceptance_binding_criterion(rule).is_none()
}

/// Stock immutable V1 rule table installed by project-policy bootstrap.
#[must_use]
pub fn builtin_obligation_rule_set() -> ObligationRuleSet {
    ObligationRuleSet {
        schema_version: OBLIGATION_RULE_SET_SCHEMA_VERSION,
        rules: vec![ObligationRuleDefinition {
            rule: BuiltinObligationRuleRef {
                rule_id: SOURCE_CHANGE_RULE_ID.into(),
                rule_version: 1,
            },
            trigger: BuiltinObligationTrigger::SourceChanged,
            requirement: VerificationRequirement {
                check_kind: crate::domain::VerificationKind::Test,
                check_fingerprint: None,
            },
        }],
    }
}

/// Rule id prefix of an obligation an acceptance criterion's binding opens.
/// The bound criterion's one-based position follows it, so a run holds at
/// most one such obligation per criterion and trigger.
pub const ACCEPTANCE_BINDING_RULE_PREFIX: &str = "acceptance_criterion_requires_verification:";
const ACCEPTANCE_BINDING_RULE_VERSION: u16 = 1;

/// The rule an acceptance binding on the criterion at `criterion` opens.
#[must_use]
pub fn acceptance_binding_rule(criterion: usize) -> BuiltinObligationRuleRef {
    BuiltinObligationRuleRef {
        rule_id: format!("{ACCEPTANCE_BINDING_RULE_PREFIX}{criterion}"),
        rule_version: ACCEPTANCE_BINDING_RULE_VERSION,
    }
}

/// The criterion position an obligation's rule names, when an acceptance
/// binding opened it.
#[must_use]
pub fn acceptance_binding_criterion(rule: &BuiltinObligationRuleRef) -> Option<usize> {
    rule.rule_id
        .strip_prefix(ACCEPTANCE_BINDING_RULE_PREFIX)?
        .parse()
        .ok()
}

/// Evaluates one exact immutable rule set against one host source record,
/// read as a change or not.
#[must_use]
pub fn evaluate_obligation_rules(
    rule_set: &ObligationRuleSet,
    source_changed: bool,
) -> Vec<(BuiltinObligationRuleRef, VerificationRequirement)> {
    rule_set
        .rules
        .iter()
        .filter(|definition| match definition.trigger {
            BuiltinObligationTrigger::SourceChanged => source_changed,
            BuiltinObligationTrigger::Unknown => false,
        })
        .map(|definition| (definition.rule.clone(), definition.requirement.clone()))
        .collect()
}

/// Complete immutable inputs for deciding what one evidence candidate does to
/// an obligation at an exact dense run-feed cut.
pub struct ObligationSatisfactionInput<'a> {
    pub judged_basis: Option<&'a crate::domain::ExecutionSourceBasis>,
    pub evidence: &'a VerificationEvidence,
    pub producer: &'a dyn crate::domain::VerificationProducer,
    /// The newest source mutation at the cut with its run-feed position, or
    /// `None` when the run observed none. A builtin rule is triggered by a
    /// mutation and is never satisfied without one; a binding's obligation
    /// is satisfied by verification of the source as it stands.
    pub latest_mutation: Option<(&'a SourceObservation, i64)>,
    pub named_root: Option<NamedRootEvidenceMatch<'a>>,
    pub evidence_position: i64,
    /// The run-feed position of `producer`; see
    /// [`VerificationEvidenceMatchInput::producer_position`].
    pub producer_position: Option<i64>,
    /// See [`VerificationEvidenceMatchInput::measured_sighting`].
    pub measured_sighting: Option<(&'a crate::domain::ExecutionSourceBasis, i64)>,
    /// The source record that triggered the obligation, when a source change
    /// did. A check accounts for an unadmitted trigger only when its producer
    /// and its record follow the trigger and it completed no earlier than
    /// the trigger was recorded.
    pub trigger: Option<&'a SourceObservation>,
    pub evaluated_cut: &'a crate::domain::FeedPosition,
}

/// Why an obligation was left out before the typed matcher ran. Control
/// decides the first three from positions and source context; storage adds the
/// two it reads from the store: an obligation already closed at the cut, and one
/// its named root holds as foreign or displaced.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObligationSkip {
    /// Bound to another run or feed than the evidence.
    OtherRun,
    /// Not yet defined when the check ran: opened at or after the record, or,
    /// under a named root, by a change the check did not run after.
    NotYetDefined,
    /// No source mutation, root sighting or binding for the check to account for.
    NoSourceContext,
    /// Already resolved at the cut.
    AlreadyClosed,
    /// Held by the named root as a foreign or displaced change.
    ForeignOrDisplaced,
}

/// What one verification fact does to one obligation at one cut.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObligationAssessment {
    /// The record satisfies the obligation.
    Matches,
    /// The typed matcher's first mismatch.
    Mismatch(VerificationEvidenceMismatch),
    /// Left out before matching.
    Skipped(ObligationSkip),
}

/// The deterministic rule and anti-stale decision for one obligation: the
/// position and source-context prefilter, then the typed matcher. The
/// satisfaction path and the reads that explain it share this decision.
#[must_use]
pub fn assess_obligation_satisfaction(
    input: &ObligationSatisfactionInput<'_>,
    obligation: &WorkObligation,
) -> ObligationAssessment {
    explain_obligation_satisfaction(input, obligation).0
}

/// [`assess_obligation_satisfaction`], with the source record that decided a
/// `stale_source_revision` mismatch beside it, `Some` exactly then.
#[must_use]
pub fn explain_obligation_satisfaction(
    input: &ObligationSatisfactionInput<'_>,
    obligation: &WorkObligation,
) -> (ObligationAssessment, Option<StaleSourceDecider>) {
    let registered_git = input
        .producer
        .verification_facts()
        .registered_git_basis(input.evidence)
        .is_some();
    let git_source_rule = registered_git
        && (acceptance_binding_criterion(&obligation.rule).is_some()
            || is_stock_source_change_obligation(&obligation.rule, &obligation.requirement));
    let expected_feed = crate::domain::FeedId::RunExecution(input.evidence.binding.run_id);
    if input.evaluated_cut.feed != expected_feed
        || obligation.run_id != input.evidence.binding.run_id
        || obligation.trigger_position.feed != expected_feed
    {
        return (
            ObligationAssessment::Skipped(ObligationSkip::OtherRun),
            None,
        );
    }
    // A bound record's producer is on another run; on this one the record
    // itself stands in for it.
    let producer_position = if input.evidence.bound_from.is_some() {
        Some(input.evidence_position)
    } else {
        input.producer_position
    };
    if input.evidence_position > input.evaluated_cut.position
        || obligation.trigger_position.position > input.evaluated_cut.position
        || input.evidence_position <= obligation.trigger_position.position
        // Under a named root a check accounts only for a source change it ran
        // after, wherever that change was recorded.
        || (input.named_root.is_some()
            && acceptance_binding_criterion(&obligation.rule).is_none()
            && producer_position
                .is_some_and(|producer| producer <= obligation.trigger_position.position))
    {
        return (
            ObligationAssessment::Skipped(ObligationSkip::NotYetDefined),
            None,
        );
    }
    // Under a named root a check of the root's newest sighting stands for the
    // source as it is, so it can account for a change the root holds from
    // before its binding too.
    if !git_source_rule
        && input.latest_mutation.is_none()
        && !input.named_root.is_some_and(|root| {
            root.unknown_change_position.is_some() || root.latest_sighting.is_some()
        })
        && acceptance_binding_criterion(&obligation.rule).is_none()
    {
        return (
            ObligationAssessment::Skipped(ObligationSkip::NoSourceContext),
            None,
        );
    }
    if let Err((mismatch, decider)) =
        explain_verification_evidence(&VerificationEvidenceMatchInput {
            judged_basis: input.judged_basis,
            candidate_kind: WorkEvidenceKind::Verification,
            evidence: Some(input.evidence),
            producer: Some(input.producer),
            latest_mutation: input.latest_mutation,
            named_root: input.named_root,
            evidence_position: input.evidence_position,
            producer_position,
            measured_sighting: input.measured_sighting,
            requirement: &obligation.requirement,
        })
    {
        return (ObligationAssessment::Mismatch(mismatch), decider);
    }
    if let Some(trigger) = input.trigger.filter(|trigger| !trigger.admitted) {
        if producer_position.is_none_or(|producer| producer <= obligation.trigger_position.position)
        {
            return (
                ObligationAssessment::Mismatch(VerificationEvidenceMismatch::NotAfterMutation),
                None,
            );
        }
        // The unadmitted floor holds the check's own completion.
        if input.evidence.completed_at < trigger.recorded_at {
            return (
                ObligationAssessment::Mismatch(VerificationEvidenceMismatch::InvalidTime),
                None,
            );
        }
    }
    (ObligationAssessment::Matches, None)
}

/// Minimum host assurance that may mediate one material effect class.
///
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EffectClass, ObjectId, ProjectId, SessionId};
    use chrono::{TimeZone, Utc};
    fn hash(seed: &str) -> ObjectId {
        ObjectId::from_canonical_bytes(seed.as_bytes())
    }
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the matcher regression keeps every binding and stale-cut assertion together"
    )]
    fn verification_match_is_kind_safe_cross_workspace_and_stale_at_the_cut() {
        use crate::domain::{
            ActorContext, AssuranceLevel, ControlWorkBinding, ExecutionObservation,
            ExecutionSourceBasis, RootExecutionId, VerificationEvidence, VerificationKind,
            VerificationResult, WorkClaimId, WorkEvidenceKind, WorkId, WorkRunId,
        };

        let run_id = WorkRunId::new();
        let session_id = SessionId("verification-host".into());
        let binding = ControlWorkBinding {
            root_execution_id: RootExecutionId::new(),
            work_id: WorkId::new(),
            run_id,
            work_revision: 3,
            claim_id: WorkClaimId::new(),
            claim_fence: 2,
        };
        let actor = ActorContext {
            actor_id: "host-adapter".into(),
            actor_kind: "system".into(),
            assurance: AssuranceLevel::Asserted,
            run_id: Some(run_id.0.to_string()),
            session_id: Some(session_id.clone()),
            source_tool: Some("host-control:turn_checkpoint".into()),
            source_skill: None,
            provenance_chain: Vec::new(),
            reason: "record host fact".into(),
        };
        let project_id = ProjectId("verification-project".into());
        let mutation_time = Utc.timestamp_millis_opt(10_000).unwrap();
        let verification_time = Utc.timestamp_millis_opt(20_000).unwrap();
        let latest_mutation = ExecutionObservation {
            schema_version: crate::domain::SCHEMA_VERSION,
            project_id: project_id.clone(),
            binding: binding.clone(),
            session_id: session_id.clone(),
            grant_id: "mutation-grant".into(),
            observation_id: "source-mutation".into(),
            action_fingerprint: hash("mutate source"),
            effect: EffectClass::MutateLocal,
            outcome: crate::domain::ExecutionOutcome::Succeeded,
            source_changed: true,
            reported_source_change: None,
            obligation_rule_set: hash("obligation rule set"),
            source_basis: Some(ExecutionSourceBasis {
                git_tree: None,
                workspace_id: "workspace-a".into(),
                source_revision: "content-revision-1".into(),
                source_root_generation: None,
                source_root_state: None,
            }),
            observed_at: Some(mutation_time),
            actor: actor.clone(),
            recorded_at: mutation_time,
        };
        let producer = ExecutionObservation {
            schema_version: crate::domain::SCHEMA_VERSION,
            project_id: project_id.clone(),
            binding: binding.clone(),
            session_id: session_id.clone(),
            grant_id: "verification-grant".into(),
            observation_id: "test-command".into(),
            action_fingerprint: hash("cargo test command"),
            effect: EffectClass::Observe,
            outcome: crate::domain::ExecutionOutcome::Succeeded,
            source_changed: false,
            reported_source_change: None,
            obligation_rule_set: hash("obligation rule set"),
            source_basis: Some(ExecutionSourceBasis {
                git_tree: None,
                workspace_id: "workspace-b".into(),
                source_revision: "content-revision-1".into(),
                source_root_generation: None,
                source_root_state: None,
            }),
            observed_at: Some(verification_time),
            actor: actor.clone(),
            recorded_at: verification_time,
        };
        let evidence = VerificationEvidence {
            applicability: None,
            schema_version: crate::domain::SCHEMA_VERSION,
            project_id,
            binding,
            session_id,
            producer_observation: hash("producer observation"),
            source_basis: producer.source_basis.clone().unwrap(),
            environment: None,
            check_kind: VerificationKind::Test,
            check_fingerprint: producer.action_fingerprint.clone(),
            result: VerificationResult::Passed,
            completed_at: verification_time,
            summary: "host recorded tests".into(),
            refs: Vec::new(),
            actor,
            recorded_at: verification_time,
            bound_from: None,
        };
        let latest_mutation = crate::domain::SourceObservation::admitted(
            ObjectId::from_canonical_bytes(b"latest mutation"),
            &latest_mutation,
        );
        let rules = evaluate_obligation_rules(
            &builtin_obligation_rule_set(),
            latest_mutation.source_changed,
        );
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].0.rule_id, "source_mutation_requires_test");
        assert_eq!(rules[0].1.check_kind, VerificationKind::Test);
        assert_eq!(rules[0].1.check_fingerprint, None);
        let producer_rules =
            evaluate_obligation_rules(&builtin_obligation_rule_set(), producer.source_changed);
        assert!(producer_rules.is_empty(), "{producer_rules:?}");
        let requirement = VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: Some(evidence.check_fingerprint.clone()),
        };
        let exact = VerificationEvidenceMatchInput {
            judged_basis: None,
            candidate_kind: WorkEvidenceKind::Verification,
            evidence: Some(&evidence),
            producer: Some(&producer),
            latest_mutation: Some((&latest_mutation, 1)),
            named_root: None,
            evidence_position: 4,
            producer_position: None,
            measured_sighting: None,
            requirement: &requirement,
        };
        assert_eq!(match_verification_evidence(&exact), Ok(()));
        let kind_only_requirement = VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: None,
        };
        assert_eq!(
            match_verification_evidence(&VerificationEvidenceMatchInput {
                requirement: &kind_only_requirement,
                ..exact
            }),
            Ok(())
        );
        let wrong_check_requirement = VerificationRequirement {
            check_kind: VerificationKind::Build,
            check_fingerprint: None,
        };
        assert_eq!(
            match_verification_evidence(&VerificationEvidenceMatchInput {
                requirement: &wrong_check_requirement,
                ..exact
            }),
            Err(VerificationEvidenceMismatch::CheckKindMismatch)
        );

        let wrong_kind = VerificationEvidenceMatchInput {
            candidate_kind: WorkEvidenceKind::Generic,
            ..exact
        };
        assert_eq!(
            match_verification_evidence(&wrong_kind),
            Err(VerificationEvidenceMismatch::WrongKind)
        );

        let mut later_mutation = latest_mutation.clone();
        later_mutation
            .source_basis
            .as_mut()
            .unwrap()
            .source_revision = "content-revision-2".into();
        let stale = VerificationEvidenceMatchInput {
            judged_basis: None,
            candidate_kind: WorkEvidenceKind::Verification,
            evidence: Some(&evidence),
            producer: Some(&producer),
            latest_mutation: Some((&later_mutation, 3)),
            named_root: None,
            evidence_position: 4,
            producer_position: None,
            measured_sighting: None,
            requirement: &requirement,
        };
        assert_eq!(
            match_verification_evidence(&stale),
            Err(VerificationEvidenceMismatch::StaleSourceRevision)
        );

        // The decision around the matcher: positions and source context
        // first, each named, then the matcher's first mismatch.
        let run_feed = |position| crate::domain::FeedPosition {
            feed: crate::domain::FeedId::RunExecution(run_id),
            position,
        };
        let obligation = crate::domain::WorkObligation {
            schema_version: crate::domain::SCHEMA_VERSION,
            obligation_id: crate::domain::WorkObligationId(uuid::Uuid::now_v7()),
            project_id: evidence.project_id.clone(),
            root_execution_id: evidence.binding.root_execution_id,
            root_id: evidence.binding.work_id,
            work_id: evidence.binding.work_id,
            run_id,
            work_revision: 3,
            rule_set: hash("obligation rule set"),
            rule: rules[0].0.clone(),
            triggering_observation: hash("source mutation"),
            trigger_position: run_feed(1),
            requirement: requirement.clone(),
            opened_at: mutation_time,
        };
        let cut = run_feed(5);
        let at_cut = ObligationSatisfactionInput {
            judged_basis: None,
            evidence: &evidence,
            producer: &producer,
            latest_mutation: Some((&latest_mutation, 1)),
            named_root: None,
            evidence_position: 4,
            producer_position: None,
            measured_sighting: None,
            trigger: None,
            evaluated_cut: &cut,
        };
        let assess = |input: &ObligationSatisfactionInput<'_>, obligation| {
            assess_obligation_satisfaction(input, obligation)
        };
        assert_eq!(assess(&at_cut, &obligation), ObligationAssessment::Matches);
        let other_feed = crate::domain::FeedPosition {
            feed: crate::domain::FeedId::RunExecution(WorkRunId::new()),
            position: 5,
        };
        assert_eq!(
            assess(
                &ObligationSatisfactionInput {
                    evaluated_cut: &other_feed,
                    ..at_cut
                },
                &obligation
            ),
            ObligationAssessment::Skipped(ObligationSkip::OtherRun)
        );
        let later = crate::domain::WorkObligation {
            trigger_position: run_feed(6),
            ..obligation.clone()
        };
        assert_eq!(
            assess(&at_cut, &later),
            ObligationAssessment::Skipped(ObligationSkip::NotYetDefined)
        );
        assert_eq!(
            assess(
                &ObligationSatisfactionInput {
                    latest_mutation: None,
                    ..at_cut
                },
                &obligation
            ),
            ObligationAssessment::Skipped(ObligationSkip::NoSourceContext),
            "a rule's obligation with no mutation, sighting or binding at the cut"
        );
        assert_eq!(
            assess(
                &ObligationSatisfactionInput {
                    latest_mutation: Some((&later_mutation, 3)),
                    ..at_cut
                },
                &obligation
            ),
            ObligationAssessment::Mismatch(VerificationEvidenceMismatch::StaleSourceRevision)
        );
    }

    #[test]
    fn a_verification_requirement_names_no_environment() {
        use crate::domain::{VerificationKind, VerificationRequirement};

        let pinned: VerificationRequirement = serde_json::from_value(serde_json::json!({
            "check_kind": "test",
            "check_fingerprint": hash("cargo test --workspace"),
        }))
        .expect("a kind with a pinned check decodes");
        assert_eq!(pinned.check_kind, VerificationKind::Test);
        assert_eq!(
            pinned.check_fingerprint,
            Some(hash("cargo test --workspace"))
        );
        // An environment is refused by name, whether it carries a value or
        // is null; a member the requirement lacks is never read past.
        for environment in [
            serde_json::json!(hash("environment")),
            serde_json::Value::Null,
        ] {
            let error = serde_json::from_value::<VerificationRequirement>(serde_json::json!({
                "check_kind": "test",
                "required_environment": environment,
            }))
            .expect_err("an environment pin is refused")
            .to_string();
            assert!(
                error.contains("unknown field `required_environment`"),
                "{error}"
            );
        }
    }
}
