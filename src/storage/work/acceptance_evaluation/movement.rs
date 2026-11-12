//! One source/check movement rule for native and retained origin feed facts.

use crate::ObjectId;
use crate::domain::{AcceptanceSourceBasis, SourceObservation, VerificationSourceIdentity};
use chrono::{DateTime, Utc};
use std::collections::BTreeSet;

pub(crate) enum BasisMove {
    Source(i64, Box<SourceObservation>),
    Check,
}

pub(crate) struct BasisMoveState<'a> {
    declared: Option<&'a AcceptanceSourceBasis>,
    judged: Option<&'a VerificationSourceIdentity>,
    git_input: bool,
    witnessed_after: Option<DateTime<Utc>>,
    live_commit: bool,
    seen: BTreeSet<ObjectId>,
    check: bool,
    quiet_move: Option<(i64, SourceObservation)>,
}

impl<'a> BasisMoveState<'a> {
    pub(crate) fn new(
        declared: Option<&'a AcceptanceSourceBasis>,
        judged: Option<&'a VerificationSourceIdentity>,
        git_input: bool,
        witnessed_after: Option<DateTime<Utc>>,
        live_commit: bool,
    ) -> Self {
        Self {
            declared,
            judged,
            git_input,
            witnessed_after,
            live_commit,
            seen: BTreeSet::new(),
            check: false,
            quiet_move: None,
        }
    }

    /// The first actual barrier is terminal, even if a later sighting reverts.
    /// Callers supply accounted observations within the selected named root.
    pub(crate) fn observe(
        &mut self,
        at: i64,
        observation: SourceObservation,
    ) -> Option<(i64, SourceObservation)> {
        if self.live_commit {
            self.seen.insert(observation.record);
            return None;
        }
        if !observation.source_changed {
            if self.witnessed_after.is_some_and(|checked_at| {
                super::source::quiet_git_input_conflicts(
                    self.declared.map(|basis| &basis.identity),
                    checked_at,
                    &observation,
                )
            }) {
                return Some((at, observation));
            }
            // Quiet sightings never replace a judged Git input.
            if self.git_input {
                return None;
            }
            if let (Some(judged), Some(sighting)) = (self.judged, observation.source_basis.as_ref())
            {
                self.quiet_move =
                    (!sighting.identity.same_source(judged)).then_some((at, observation));
            }
            return None;
        }
        if self.git_input
            || !observation.admitted
            || !super::source::judged_revision(self.declared, &observation)
        {
            return Some((at, observation));
        }
        self.quiet_move = None;
        self.seen.insert(observation.record);
        None
    }

    pub(crate) fn obligation(&mut self, triggering_observation: &ObjectId) {
        if !self.seen.contains(triggering_observation) {
            self.check = true;
        }
    }

    pub(crate) fn check(&mut self) {
        self.check = true;
    }

    /// A current quiet source difference takes precedence over later checks.
    pub(crate) fn finish(self) -> Option<BasisMove> {
        if let Some((at, observation)) = self.quiet_move {
            return Some(BasisMove::Source(at, Box::new(observation)));
        }
        self.check.then_some(BasisMove::Check)
    }
}
