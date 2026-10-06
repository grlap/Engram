//! Recorded criterion associations from one immutable native completion seal.
//! Navigation retains that seal after reopen; it grants no execution authority.

use super::*;

const MAX_WINDOW_ROWS: usize = 16;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CriterionLinksCursor {
    project: ProjectId,
    work: WorkId,
    run: WorkRunId,
    seal: ObjectId,
    criterion: usize,
    evidence_member: usize,
    locator: ObjectId,
}

#[derive(Clone)]
pub(crate) struct WorkCriterionLinkRow {
    pub criterion: usize,
    pub evidence_member: usize,
    pub locator: ObjectId,
    pub preview: Option<String>,
    pub preview_error_class: Option<&'static str>,
}

pub(crate) struct WorkCriterionLinksWindow {
    pub short_ref: String,
    pub rows: Vec<WorkCriterionLinkRow>,
    pub total: usize,
    pub earlier: usize,
    pub project: ProjectId,
    pub work: WorkId,
    pub run: WorkRunId,
    pub seal: ObjectId,
}

impl WorkCriterionLinksWindow {
    /// Only the last emitted association advances the readable navigation token.
    pub(crate) fn continuation(&self, shown: usize) -> Result<Option<String>, StoreError> {
        if self.earlier + shown == self.total {
            return Ok(None);
        }
        let row = shown
            .checked_sub(1)
            .and_then(|index| self.rows.get(index))
            .ok_or_else(|| invalid("criterion links continuation must advance a member"))?;
        super::continuation::encode(
            "cl1-",
            &CriterionLinksCursor {
                project: self.project.clone(),
                work: self.work,
                run: self.run,
                seal: self.seal.clone(),
                criterion: row.criterion,
                evidence_member: row.evidence_member,
                locator: row.locator.clone(),
            },
        )
        .map(Some)
        .ok_or_else(|| invalid("criterion links cursor exceeds its budget"))
    }
}

impl LocalWorkService {
    /// Reads one seal in one snapshot without registering a session or changing
    /// focus, claims or delivery. A continuation follows its historical run.
    pub(crate) fn work_criterion_links_window(
        &self,
        work_ref: &str,
        after: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<WorkCriterionLinksWindow, StoreError> {
        let cursor = after
            .map(|token| {
                super::continuation::decode::<CriterionLinksCursor>("cl1-", token)
                    .ok_or_else(|| invalid("invalid criterion links cursor; start a fresh window"))
            })
            .transpose()?;
        if cursor
            .as_ref()
            .is_some_and(|cursor| cursor.project != self.project_id)
        {
            return Err(invalid("criterion links cursor belongs to another project"));
        }
        let store = self.read_store_at(now)?;
        store.work_read_snapshot(|store| {
            let item = store.resolve_work_ref(&self.project_id, work_ref)?;
            if cursor.as_ref().is_some_and(|cursor| cursor.work != item.work_id) {
                return Err(invalid("criterion links cursor belongs to another item"));
            }
            let run = if let Some(cursor) = &cursor {
                let run = store.find_work_run(cursor.run)?
                    .ok_or_else(|| invalid("criterion links cursor names no stored run"))?;
                if run.work_id != item.work_id || run.state != WorkRunState::Completed
                    || run.completion_seal.as_ref() != Some(&cursor.seal) {
                    return Err(invalid("criterion links cursor does not name this run's seal"));
                }
                run
            } else {
                if item.lifecycle != WorkLifecycle::Completed {
                    return Err(StoreError::InvalidWork("no current frozen criterion mapping; this work is not completed".into()));
                }
                if store.work_completed_by_restored_record(item.work_id)? {
                    return Err(StoreError::InvalidWork("this store holds no native per-criterion evidence record for this completed work".into()));
                }
                store.latest_work_run(item.work_id)?.ok_or_else(|| broken("completed work has no native run"))?
            };
            let seal_id = run.completion_seal.as_ref().ok_or_else(|| broken("completed native run has no completion seal"))?;
            let seal = store.completion_seal_at(seal_id)?;
            if run.work_id != item.work_id || run.state != WorkRunState::Completed
                || seal.work_id != item.work_id || seal.run_id != run.run_id
                || seal.run_generation != run.generation
                || seal.root_execution_id != run.root_execution_id
                || seal.root_execution.project_id != self.project_id {
                return Err(broken("criterion mapping differs from its canonical run or project"));
            }
            let associations = associations(&seal.acceptance);
            let total = seal.acceptance.iter().map(|result| result.evidence.len()).sum();
            let earlier = if let Some(cursor) = &cursor {
                associations.clone().position(|(criterion, member, locator)|
                    criterion == cursor.criterion && member == cursor.evidence_member && locator == &cursor.locator)
                    .map(|index| index + 1)
                    .ok_or_else(|| invalid("criterion links cursor does not match a sealed member"))?
            } else { 0 };
            let rows = associations.skip(earlier).take(MAX_WINDOW_ROWS).map(|(criterion, evidence_member, locator)| {
                let (preview, preview_error_class) = match store.criterion_evidence_preview(&self.project_id, item.work_id, locator) {
                    Ok(body) => (body.map(|body| compact_text(&body)), None),
                    Err(error) => (None, Some(super::advisory_error_class(&error))),
                };
                WorkCriterionLinkRow { criterion, evidence_member, locator: locator.clone(), preview, preview_error_class }
            }).collect();
            Ok(WorkCriterionLinksWindow { short_ref: item.short_ref, rows, total, earlier,
                project: self.project_id.clone(), work: item.work_id, run: run.run_id, seal: seal_id.clone() })
        })
    }
}

fn associations(
    acceptance: &[crate::AcceptanceResult],
) -> impl Clone + Iterator<Item = (usize, usize, &ObjectId)> {
    acceptance
        .iter()
        .enumerate()
        .flat_map(|(criterion, result)| {
            result
                .evidence
                .iter()
                .enumerate()
                .map(move |(member, locator)| (criterion + 1, member + 1, locator))
        })
}

fn invalid(reason: &str) -> StoreError {
    StoreError::WorkShowCursorInvalid {
        reason: reason.into(),
    }
}

fn broken(reason: &str) -> StoreError {
    StoreError::InvalidWorkProjection(reason.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn criterion_links_recorded_positions_preserve_repeated_members_and_empty_criteria() {
        // Current completion writers deduplicate within one criterion. The
        // reader still preserves every member of the stored representation.
        let id = ObjectId::mint();
        let result = |evidence| crate::AcceptanceResult {
            criterion: "historical".into(),
            satisfied: true,
            evidence,
            assurance: crate::domain::AssuranceLevel::Asserted,
            note: String::new(),
        };
        let acceptance = vec![
            result(vec![id.clone(), id.clone()]),
            result(Vec::new()),
            result(vec![id.clone()]),
        ];
        assert_eq!(
            associations(&acceptance).collect::<Vec<_>>(),
            vec![(1, 1, &id), (1, 2, &id), (3, 1, &id)]
        );
    }
}
