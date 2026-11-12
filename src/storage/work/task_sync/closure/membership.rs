//! Selected source/export reads. A destination baseline is never substituted.

use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension, params};

use super::{
    super::{ledger::view::read, origin},
    component,
};
use crate::{
    ObjectId, WorkId,
    storage::{SqliteStore, StoreError},
    task_sync::{TaskMembershipComponent, TaskSyncMembershipView, TaskSyncRefusal},
};

pub(in crate::storage::work::task_sync) mod head;

impl TaskSyncMembershipView {
    pub(super) fn kind(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Export => "export",
        }
    }
}

fn invalid(message: &str) -> StoreError {
    StoreError::InvalidWorkProjection(format!(
        "task-sync membership {message}; repair projections explicitly"
    ))
}

impl SqliteStore {
    /// Read connected members of one admitted binding-scoped source or export.
    /// Preparation is not confirmed publication. Missing membership is never
    /// inferred from the local baseline, native work or an empty inventory.
    /// Like other selected reads this relies on the admitted projection's
    /// coherence; doctor compares the complete canonical derivation.
    ///
    /// # Errors
    /// Refuses unavailable membership, a different canonical head/profile and
    /// damaged selected members or adjacency. Reads no retained file.
    pub fn capture_task_sync_membership_component(
        &self,
        binding: &ObjectId,
        view: TaskSyncMembershipView,
        seeds: impl IntoIterator<Item = WorkId>,
    ) -> Result<TaskMembershipComponent, StoreError> {
        let seeds = seeds.into_iter().collect();
        self.work_read_snapshot(|store| {
            origin::with_origin_scope(&store.connection, || {
                capture_on(&store.connection, binding, view, seeds)
            })
        })
    }
}

// This same connection-level owner is used by the atomic admission writer.
pub(in crate::storage::work::task_sync) fn capture_on(
    connection: &Connection,
    binding_id: &ObjectId,
    view: TaskSyncMembershipView,
    seeds: BTreeSet<WorkId>,
) -> Result<TaskMembershipComponent, StoreError> {
    component::check_capacity(seeds.len())?;
    let binding = read::binding_on(connection, binding_id, &mut read::ReadBudget::default())?;
    let projected: Option<(String, String, Vec<u8>)> = connection
        .query_row(
            "SELECT checkpoint_id,inventory_root,profile_json FROM work_task_sync_source_heads
         WHERE binding_id=?1 AND role=?2",
            params![binding_id.as_str(), view.kind()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let (head, root, profile) = projected.ok_or_else(|| {
        TaskSyncRefusal::IncrementalBasisUnavailable
            .invalid("no admitted binding membership; bootstrap or rebuild explicitly")
    })?;
    let head = ObjectId::from_stored(head).ok_or_else(|| invalid("head id is invalid"))?;
    let canonical = head::read_on(connection, &head, binding_id, &binding, view)?;
    let membership = canonical.output().clone();
    let projected_profile =
        serde_json::from_slice::<crate::task_sync::progress::ExportProfile>(&profile)
            .map_err(|_| invalid("projected profile is invalid"))?;
    if root != membership.inventory_root.hex()
        || projected_profile != membership.profile
        || membership.profile.include_notes != binding.profile.include_notes
        || (view == TaskSyncMembershipView::Export && membership.profile != binding.profile)
    {
        return Err(invalid("head root or profile differs"));
    }
    let component = component::capture_scope(
        connection,
        component::Scope {
            project: &binding.project,
            kind: view.kind(),
            id: binding_id.as_str(),
            view: None,
            baseline: None,
            notes: membership.profile.include_notes,
        },
        seeds,
    )?;
    Ok(TaskMembershipComponent {
        head,
        membership,
        component,
    })
}
