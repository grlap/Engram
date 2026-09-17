use super::{
    ChildRequirement, DateTime, LocalWorkService, MAX_CHILD_OBLIGATION_REFS, SessionId,
    SqliteStore, StoreError, Utc, WorkChildObligations, WorkFocusView, WorkId, WorkItem,
    WorkLifecycle, WorkRun, work_item_summary,
};

/// Canonical authored contract for an explicit `show --full` read. Not a core
/// wire type and not a focus packet.
#[derive(Clone, Debug)]
pub(crate) struct WorkAuthoredContract {
    pub short_ref: String,
    pub revision: i64,
    pub title: String,
    pub outcome: String,
    pub acceptance: Vec<String>,
    /// The complete newest acceptance evaluation of an open item under an
    /// evaluated policy: every verdict with its full rationale and citations.
    pub evaluation: Option<WorkAuthoredEvaluation>,
}

/// Complete newest evaluation for the `show --full` read.
#[derive(Clone, Debug)]
pub(crate) struct WorkAuthoredEvaluation {
    pub hash: String,
    pub mode: &'static str,
    pub work_revision: i64,
    pub evaluated_cut: i64,
    pub stale: Option<&'static str>,
    pub verdicts: Vec<WorkAuthoredVerdict>,
}

/// One complete verdict of the `show --full` read.
#[derive(Clone, Debug)]
pub(crate) struct WorkAuthoredVerdict {
    pub position: usize,
    pub criterion: String,
    pub verdict: &'static str,
    pub basis: &'static str,
    pub rationale: String,
    pub citations: Vec<String>,
}

impl LocalWorkService {
    /// Current direct open optional children, independent of the rich focus
    /// fitter. All rows and admission diagnostics share one read snapshot.
    pub(crate) fn remaining_optional_children(
        &self,
        parent: WorkId,
        limit: usize,
        now: DateTime<Utc>,
    ) -> Result<super::WorkChildFollowupPage, StoreError> {
        let store = self.store_at(now)?;
        store.work_read_snapshot(|store| {
            store.resolve_work_ref(&self.project_id, &parent.0.to_string())?;
            let children = store
                .work_children(parent)?
                .into_iter()
                .filter(|child| {
                    child.lifecycle == super::WorkLifecycle::Open
                        && child.child_requirement == super::ChildRequirement::Optional
                })
                .collect::<Vec<_>>();
            let total = children.len();
            let items = children
                .into_iter()
                .take(limit)
                .map(|child| {
                    let refusal = match store.check_work_detach_admission(child.work_id, now) {
                        Ok(()) => None,
                        Err(StoreError::WorkDetachRefused { reason, remedy, .. }) => {
                            Some((reason, remedy))
                        }
                        Err(error) => return Err(error),
                    };
                    Ok(super::WorkChildFollowup {
                        work: super::work_item_summary(&child),
                        refusal,
                    })
                })
                .collect::<Result<Vec<_>, StoreError>>()?;
            Ok(super::WorkChildFollowupPage { items, total })
        })
    }

    /// Makes `work_ref` the session's ambient focus without inspecting it, so a
    /// mutation can name its target in the same call.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the reference is absent or outside the project.
    pub fn select_work(&self, work_ref: &str, now: DateTime<Utc>) -> Result<(), StoreError> {
        let mut store = self.store_at(now)?;
        self.bind_target(&mut store, Some(work_ref), now)?;
        Ok(())
    }

    /// The work this session holds under a live claim, with expiry, read from
    /// the claim projection without building any focus view.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the store cannot be read.
    pub fn held_work(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<(WorkId, DateTime<Utc>)>, StoreError> {
        let store = self.store_at(now)?;
        store.work_held_by(&self.session_id, now)
    }

    /// Every live claim in this project, used only to annotate compact agent
    /// catalog rows without constructing one focus packet per item.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the live-claim projection is invalid.
    pub fn live_work_claims(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<(WorkId, SessionId, DateTime<Utc>)>, StoreError> {
        let store = self.store_at(now)?;
        store.live_work_claims(&self.project_id, now)
    }

    /// Inspects work by reference without changing ambient focus or staging
    /// any delivery.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the reference is absent or projections are invalid.
    pub fn inspect_work(
        &self,
        work_ref: &str,
        now: DateTime<Utc>,
    ) -> Result<WorkFocusView, StoreError> {
        let store = self.store_at(now)?;
        let work = store.resolve_work_ref(&self.project_id, work_ref)?;
        self.focus_view(&store, work.work_id, false, true, now)
    }

    /// Resolves one work reference without projecting or changing ambient
    /// focus. Agent translations use this only to attribute core refusals.
    pub(crate) fn resolve_work_reference(
        &self,
        work_ref: &str,
        now: DateTime<Utc>,
    ) -> Result<WorkItem, StoreError> {
        self.store_at(now)?
            .resolve_work_ref(&self.project_id, work_ref)
    }

    /// Complete stored title, outcome, and acceptance. One read snapshot; no
    /// focus, delivery, or session-registration writes. Does not load or fit a
    /// focus view.
    pub(crate) fn work_authored_contract(
        &self,
        work_ref: &str,
        now: DateTime<Utc>,
    ) -> Result<WorkAuthoredContract, StoreError> {
        let store = self.read_store_at(now)?;
        store.work_read_snapshot(|store| {
            let item = store.resolve_work_ref(&self.project_id, work_ref)?;
            let evaluation = if item.lifecycle == WorkLifecycle::Open
                && store.acceptance_evaluation_policy()?.is_evaluated()
            {
                store
                    .acceptance_evaluation_status(item.work_id, None)?
                    .map(|status| WorkAuthoredEvaluation {
                        hash: status.evaluation.as_str().to_owned(),
                        mode: status.record.mode.word(),
                        work_revision: status.record.work_revision,
                        evaluated_cut: status.record.evaluated_cut.position,
                        stale: status.stale.map(crate::AcceptanceStaleReason::word),
                        verdicts: status
                            .record
                            .verdicts
                            .iter()
                            .enumerate()
                            .map(|(index, verdict)| WorkAuthoredVerdict {
                                position: index + 1,
                                criterion: verdict.criterion.clone(),
                                verdict: verdict.verdict.word(),
                                basis: verdict.basis.word(),
                                rationale: verdict.rationale.clone(),
                                citations: verdict
                                    .evidence
                                    .iter()
                                    .map(|hash| hash.as_str().to_owned())
                                    .collect(),
                            })
                            .collect(),
                    })
            } else {
                None
            };
            Ok(WorkAuthoredContract {
                short_ref: item.short_ref,
                revision: item.revision,
                title: item.title,
                outcome: item.outcome,
                acceptance: item.acceptance,
                evaluation,
            })
        })
    }

    /// Selects and inspects ambient work without implicitly changing its claim.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the reference is absent or projections are invalid.
    pub fn work_focus(
        &self,
        work_ref: &str,
        now: DateTime<Utc>,
    ) -> Result<WorkFocusView, StoreError> {
        let mut store = self.store_at(now)?;
        let item = store.resolve_work_ref(&self.project_id, work_ref)?;
        store.focus_work_session(&self.project_id, &self.session_id, item.work_id, now)?;
        self.focus_view(&store, item.work_id, true, true, now)
    }

    /// The safe agent renderer owns the final byte budget, not this richer
    /// intermediate view. Contract text stays whole; summary and relation
    /// bounds still apply to the other fields.
    pub(crate) fn work_focus_for_agent(
        &self,
        work_ref: &str,
        now: DateTime<Utc>,
    ) -> Result<WorkFocusView, StoreError> {
        let store = self.read_store_at(now)?;
        // Reading never selects ambient focus or discards a staged delivery.
        // Resolve the explicit target and its advisory sections in one cut.
        store.work_read_snapshot(|store| {
            let item = store.resolve_work_ref(&self.project_id, work_ref)?;
            self.focus_view_for_projection(
                store,
                item.work_id,
                // The agent renderer does not emit the focus-bound memory index.
                false,
                true,
                super::service::FocusText::Full,
                now,
            )
        })
    }
}

pub(super) fn child_obligations(
    store: &SqliteStore,
    parent: &WorkItem,
    run: Option<&WorkRun>,
    children: &[WorkItem],
) -> Result<WorkChildObligations, StoreError> {
    let waived = store.work_child_waivers(parent, run)?;
    let mut groups = WorkChildObligations::default();
    for child in children {
        let successor = store.required_child_successor(child)?;
        let page = match child.child_requirement {
            ChildRequirement::Required
                if child.lifecycle != WorkLifecycle::Completed
                    && !waived.contains(&child.work_id)
                    && successor
                        .as_ref()
                        .is_none_or(|state| state.resolution.is_none()) =>
            {
                &mut groups.required_owed
            }
            ChildRequirement::Optional if child.lifecycle == WorkLifecycle::Open => {
                &mut groups.open_optional
            }
            _ => continue,
        };
        page.total += 1;
        // Proposed is not creatable in V1; revisit the default Open-only ls
        // scope when that lifecycle gains a creation path.
        page.includes_disposed |= matches!(
            child.lifecycle,
            WorkLifecycle::Cancelled | WorkLifecycle::Superseded
        );
        if page.items.len() < MAX_CHILD_OBLIGATION_REFS {
            let mut summary = work_item_summary(child);
            summary.required_child_successor = successor;
            page.items.push(summary);
        }
    }
    Ok(groups)
}

#[cfg(test)]
mod tests;
