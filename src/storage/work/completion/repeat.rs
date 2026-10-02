//! The repeat rule: whether a reported source change leaves the source where
//! the run already saw it, and so is no change.

use rusqlite::{Connection, params};

use super::super::feeds::{
    latest_source_mutation_in_on, latest_source_mutation_on, latest_unlocated_source_change_on,
};
use super::named_root::named_root_context_on;
use crate::domain::{ExecutionSourceBasis, WorkClaimId, WorkRunId};
use crate::storage::StoreError;

/// Whether a reported source change leaves the source where the run already
/// saw it. Such a report changed nothing: the revision, not the host's flag,
/// decides a source change.
///
/// The report repeats only when its revision equals the revision of the
/// newest recorded source change, the row test-obligation satisfaction and
/// evaluation freshness key on, and no host record on the run since that
/// change saw any other revision: no execution observation, and no
/// environment evidence, which reports its own source basis. Verification
/// evidence is left out: it copies its producer observation's basis, which the
/// run already holds at the producer's own position, and a late verification
/// of an earlier producer would carry a revision seen before the change. The
/// first condition keeps a repeat from re-anchoring obligations. The second
/// keeps a move that only records claiming no change carried (a check run
/// after someone else's edit, an environment capture) from hiding the next
/// change, whether it moves on, reverts, or goes away and comes back: the
/// content in between is content an evaluation may have judged. When the
/// newest recorded change carries no revision, or the run has none, or the
/// report carries no revision, this answers `false` and the host's flag
/// stands, so a later change with a revision can re-anchor obligations that a
/// revision-less change left waiver-only. Without a named root, the content
/// fingerprint compares across workspaces. Once the host names a root, only
/// that workspace and generation can anchor source freshness.
pub(in crate::storage) fn source_revision_repeats_on(
    connection: &Connection,
    run_id: WorkRunId,
    claim_id: WorkClaimId,
    basis: Option<&ExecutionSourceBasis>,
) -> Result<bool, StoreError> {
    let Some(basis) = basis else {
        return Ok(false);
    };
    Ok(newest_change_repeated_on(connection, run_id, claim_id, basis, i64::MAX, false)?.is_some())
}

/// The newest recorded source change that a report of `basis` would repeat
/// on the run's feed through `through`, with its position, by the rule
/// [`source_revision_repeats_on`] states; `None` when the report is a change.
/// With `workspace_scoped`, the report is compared within its own workspace
/// even without a named root: the newest change and any other revision seen
/// since are read in that workspace only, as an unadmitted report's repeat
/// scope asks; a change with no located source since the anchor still breaks
/// the repeat, since nothing shows it was elsewhere.
pub(in crate::storage) fn newest_change_repeated_on(
    connection: &Connection,
    run_id: WorkRunId,
    claim_id: WorkClaimId,
    basis: &ExecutionSourceBasis,
    through: i64,
    workspace_scoped: bool,
) -> Result<Option<(i64, crate::domain::SourceObservation)>, StoreError> {
    // Under a named root a report is compared within its own workspace: a
    // sighting in the root with the root's newest change, any other with the
    // newest change the host recorded in that workspace.
    let root = named_root_context_on(connection, run_id, claim_id, through)?;
    let newest = match &root {
        Some(root)
            if basis.workspace_id == root.binding.workspace_id
                && basis.source_root_generation == Some(root.binding.generation) =>
        {
            root.latest_mutation.clone()
        }
        Some(_) => {
            latest_source_mutation_in_on(connection, run_id, Some(&basis.workspace_id), through)?
        }
        None if workspace_scoped => {
            latest_source_mutation_in_on(connection, run_id, Some(&basis.workspace_id), through)?
        }
        None => latest_source_mutation_on(connection, run_id, through)?,
    };
    let Some((change_position, newest_change)) = newest else {
        return Ok(None);
    };
    if newest_change
        .source_basis
        .as_ref()
        .is_none_or(|recorded| recorded.source_revision != basis.source_revision)
    {
        return Ok(None);
    }
    // Scoped to a workspace without a named root, a change recorded with no
    // located source since the anchor may have been in this workspace too, so
    // nothing shows the source stayed where the anchor left it.
    if workspace_scoped
        && root.is_none()
        && latest_unlocated_source_change_on(connection, run_id, change_position, through)?
            .is_some()
    {
        return Ok(None);
    }
    // Every host record that carries a revision counts: admitted and
    // accounted unadmitted source records and environment evidence alike.
    let (revision_sql, workspace_sql) = (
        super::super::feeds::source_basis_sql("source_revision"),
        super::super::feeds::source_basis_sql("workspace_id"),
    );
    let moved_since: bool = connection.query_row(
        &format!(
            "SELECT EXISTS (
                 SELECT 1 FROM work_feed_entries entry
                 JOIN objects object ON object.object_id = entry.object_id
                 WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
                   AND entry.position > ?2 AND entry.position <= ?5
                   AND (entry.object_kind = 'environment_evidence'
                        OR {})
                   AND {revision_sql} IS NOT NULL
                   AND {revision_sql} != ?3
                   AND (?4 IS NULL OR {workspace_sql} = ?4)
             )",
            super::super::feeds::SOURCE_RECORD_SQL
        ),
        params![
            run_id.0.to_string(),
            change_position,
            basis.source_revision,
            (root.is_some() || workspace_scoped).then_some(basis.workspace_id.as_str()),
            through
        ],
        |row| row.get(0),
    )?;
    Ok((!moved_since).then_some((change_position, newest_change)))
}
