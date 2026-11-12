//! Read retained historical proof; record a separate, explicitly requested use.

use super::{Connection, ObjectId, ProjectId, Result, invalid, object_on, params};
use crate::domain::{
    AcceptanceEvaluation, AcceptanceEvaluationPolicy, NamedRootBindingEvent, SourceObservation,
    VerificationSourceBasis,
};
use crate::task_sync::evidence::acceptance::{
    AcceptanceUseRefusal as Refusal, AcceptanceUseRequest, HistoricalProof,
};
use crate::task_sync::evidence::witnesses::{
    GoverningBasis, GoverningSnapshotWitness, OriginFeedCut,
};
use crate::{CanonicalObject, FeedId, WorkItem};
use serde::de::DeserializeOwned;

mod evaluation;
mod imported;
mod mark;
mod movement;
mod seal;
mod use_decision;
mod witness;

pub(crate) use use_decision::frozen_results_on;
pub(crate) use use_decision::qualify_consumption_on;
pub(crate) use use_decision::validate_retirement_on;
pub(super) use use_decision::{create_schema, rebuild, verify};

/// All originals are resolved in one explicit origin, never by UUID alone.
struct History<'a> {
    connection: &'a Connection,
    project: &'a ProjectId,
    request: &'a AcceptanceUseRequest,
    cuts: Vec<OriginFeedCut>,
}

impl<'a> History<'a> {
    fn new(
        connection: &'a Connection,
        project: &'a ProjectId,
        request: &'a AcceptanceUseRequest,
    ) -> Result<Self> {
        let mut history = Self {
            connection,
            project,
            request,
            cuts: Vec::new(),
        };
        let mut feeds = Vec::new();
        for id in &request.origin_cuts {
            let cut: OriginFeedCut = history.load(id, "evidence_origin_feed_cut")?;
            if cut.source.origin != request.origin
                || cut.source.project_id != *project
                || feeds.contains(&cut.source.feed)
            {
                return Err(invalid(
                    "historical proof repeats or changes its origin feed scope",
                ));
            }
            feeds.push(cut.source.feed.clone());
            history.cuts.push(cut);
        }
        Ok(history)
    }

    fn bytes(&self, id: &ObjectId, expected: &str) -> Result<Vec<u8>> {
        let (kind, bytes) = super::verification::applicability::qualified_bytes(
            self.connection,
            self.project,
            &self.request.origin,
            id,
        )?;
        if kind != expected {
            return Err(invalid("historical proof dependency kind differs"));
        }
        Ok(bytes)
    }

    fn load<T: DeserializeOwned>(&self, id: &ObjectId, kind: &str) -> Result<T> {
        Ok(serde_json::from_slice(&self.bytes(id, kind)?)?)
    }

    fn cut(&self, feed: &FeedId) -> Option<&OriginFeedCut> {
        self.cuts.iter().find(|cut| &cut.source.feed == feed)
    }

    fn known_head(&self, feed: &FeedId) -> Result<i64> {
        Ok(self.connection.query_row("SELECT COALESCE(MAX(position),0) FROM work_task_sync_evidence_feed_members WHERE project_id=?1 AND origin_id=?2 AND feed=?3",params![self.project.0,self.request.origin.as_str(),serde_json::to_string(feed)?],|row|row.get(0))?)
    }

    fn source_at(
        &self,
        feed: &FeedId,
        through: i64,
        root: &NamedRootBindingEvent,
    ) -> Result<Option<VerificationSourceBasis>> {
        if self
            .cut(feed)
            .is_none_or(|cut| through < 0 || through > cut.through)
        {
            return Err(invalid("historical source exceeds its witnessed feed cut"));
        }
        self.source_on_rows(feed, through, root)
    }

    fn current_source(
        &self,
        feed: &FeedId,
        root: &NamedRootBindingEvent,
    ) -> Result<Option<VerificationSourceBasis>> {
        if self.cut(feed).is_none() {
            return Err(invalid("historical source has no witnessed feed scope"));
        }
        self.source_on_rows(feed, self.known_head(feed)?, root)
    }

    fn source_on_rows(
        &self,
        feed: &FeedId,
        through: i64,
        root: &NamedRootBindingEvent,
    ) -> Result<Option<VerificationSourceBasis>> {
        let mut basis = None;
        let complete = self.walk_rows(feed, through, |_, id, kind| {
            let observation = match kind {
                "execution_observation" => Some(SourceObservation::admitted(
                    id.clone(),
                    &self.load(id, kind)?,
                )),
                "unadmitted_execution_observation" => {
                    SourceObservation::unadmitted(id.clone(), &self.load(id, kind)?)
                }
                _ => None,
            };
            if let Some(observation) = observation
                && observation.project_id == *self.project
                && FeedId::RunExecution(observation.binding.run_id) == *feed
                && observation.source_basis.as_ref().is_some_and(|source| {
                    crate::storage::work::acceptance_evaluation::source::within_named_root(
                        root, source,
                    )
                })
            {
                basis = observation.source_basis;
            }
            Ok(())
        })?;
        if !complete {
            return Err(invalid("historical source feed contains an interior gap"));
        }
        Ok(basis)
    }

    fn member(&self, feed: &FeedId, id: &ObjectId, kind: &str, through: i64) -> Result<bool> {
        let Some(cut) = self.cut(feed) else {
            return Ok(false);
        };
        if through < 0 || through > cut.through {
            return Ok(false);
        }
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_task_sync_evidence_feed_members WHERE project_id=?1 AND origin_id=?2 AND feed=?3 AND object_id=?4 AND object_kind=?5 AND position<=?6)",
            params![self.project.0,self.request.origin.as_str(),serde_json::to_string(feed)?,id.as_str(),kind,through], |row|row.get(0))?)
    }

    /// Stream every interior row. A witness is not replaced by its endpoint.
    fn walk(
        &self,
        feed: &FeedId,
        through: i64,
        visit: impl FnMut(i64, &ObjectId, &str) -> Result<()>,
    ) -> Result<bool> {
        let Some(cut) = self.cut(feed) else {
            return Ok(false);
        };
        if through < 0 || cut.through < through {
            return Ok(false);
        }
        self.walk_rows(feed, through, visit)
    }

    /// Later admitted facts can invalidate current use; an old requested cut
    /// cannot hide a later evaluator affiliation or a superseding verdict.
    fn walk_current(
        &self,
        feed: &FeedId,
        visit: impl FnMut(i64, &ObjectId, &str) -> Result<()>,
    ) -> Result<bool> {
        if self.cut(feed).is_none() {
            return Ok(false);
        }
        self.walk_rows(feed, self.known_head(feed)?, visit)
    }

    fn walk_rows(
        &self,
        feed: &FeedId,
        through: i64,
        mut visit: impl FnMut(i64, &ObjectId, &str) -> Result<()>,
    ) -> Result<bool> {
        let mut statement = self.connection.prepare("SELECT position,object_id,object_kind FROM work_task_sync_evidence_feed_members WHERE project_id=?1 AND origin_id=?2 AND feed=?3 AND position<=?4 ORDER BY position")?;
        let rows = statement.query_map(
            params![
                self.project.0,
                self.request.origin.as_str(),
                serde_json::to_string(feed)?,
                through
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;
        let mut previous = 0;
        for row in rows {
            let (position, id, kind) = row?;
            if position != previous + 1 {
                return Ok(false);
            }
            let id = super::stored_id(id)?;
            self.bytes(&id, &kind)?;
            visit(position, &id, &kind)?;
            previous = position;
        }
        Ok(previous == through)
    }

    fn governing(
        &self,
        evaluation: &ObjectId,
        record: &AcceptanceEvaluation,
        witness: &ObjectId,
    ) -> Result<Option<WorkItem>> {
        let witness: GoverningSnapshotWitness =
            self.load(witness, "evidence_governing_snapshot")?;
        let expected = GoverningBasis {
            evaluation: evaluation.clone(),
            project_id: record.project_id.clone(),
            work_id: record.work_id,
            work_revision: record.work_revision,
            content_fingerprint: record.work_revision_hash.clone(),
        };
        Ok(witness.validate(&expected)?.cloned())
    }
}

/// The full independently retained governing object is compared canonically.
fn same_governing(original: &WorkItem, destination: &WorkItem) -> Result<bool> {
    // Revision numbers and local run generations are host-local, not proof of
    // equality. The independently identified original is validated before this.
    let mut destination = destination.clone();
    destination.revision = original.revision;
    destination.active_run_id = original.active_run_id;
    destination.lifecycle = original.lifecycle;
    destination.updated_at = original.updated_at;
    destination.restored = original.restored;
    // A restored task has local creation attribution. It does not change the
    // governing criteria, bindings or policy selected for this use.
    destination.created_by.clone_from(&original.created_by);
    destination.created_at = original.created_at;
    Ok(
        CanonicalObject::freeze(original)?.bytes()
            == CanonicalObject::freeze(&destination)?.bytes(),
    )
}

fn inspect(
    history: &History<'_>,
    destination: &super::verification::applicability::Destination,
    policy: &AcceptanceEvaluationPolicy,
    source: &crate::source::SourceId,
) -> Result<Option<Refusal>> {
    inspect_at(history, destination, policy, source, 0)
}

fn inspect_at(
    history: &History<'_>,
    destination: &super::verification::applicability::Destination,
    policy: &AcceptanceEvaluationPolicy,
    source: &crate::source::SourceId,
    depth: usize,
) -> Result<Option<Refusal>> {
    if depth > 1_024 {
        return Ok(Some(Refusal::IncompleteClosure));
    }
    match &history.request.proof {
        HistoricalProof::Evaluation {
            evaluation,
            governing_witness,
        } => evaluation::inspect(
            history,
            destination,
            policy,
            source,
            evaluation,
            governing_witness,
        ),
        HistoricalProof::NativeCompletion {
            seal,
            governing_witness,
        } => seal::inspect(
            history,
            destination,
            policy,
            source,
            seal,
            governing_witness.as_ref(),
            depth,
        ),
    }
}
