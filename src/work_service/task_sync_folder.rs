//! The folder route of the explicit task exchange: an export that carries
//! its basis, and an import against a destination basis the caller selects.

use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

use super::LocalWorkService;
use crate::{
    CanonicalObject, ProjectId,
    storage::{StoreError, TaskSyncFolderApply, TaskSyncFolderExport, TaskSyncImportResult},
    task_sync::{
        FORMAT, ProjectDocument, TaskSyncRefusal, TaskTree,
        directory::{DestinationBasis, FolderExchange},
        progress::{ExportProfile, documents_fingerprint},
    },
};

impl LocalWorkService {
    /// Captures both explicit folder components on the same read snapshot.
    ///
    /// # Errors
    /// Refuses missing opt-in, invalid profile or inconsistent stored planning.
    pub fn export_task_sync_folder_with_memories(
        &self,
        profile: &ExportProfile,
        selected: &std::collections::BTreeMap<String, crate::task_sync::memories::KeyHistory>,
        resolutions: &std::collections::BTreeMap<String, crate::task_sync::memories::ImportOptions>,
        now: DateTime<Utc>,
    ) -> Result<
        (
            TaskSyncFolderExport,
            crate::task_sync::memories::ExportOutcome,
        ),
        StoreError,
    > {
        self.require_task_sync_enabled()?;
        self.read_store_at(now)?
            .export_task_sync_folder_with_memories(&self.project_id, profile, selected, resolutions)
    }

    /// Exports shared planning for a Git folder from one read snapshot,
    /// together with the basis this host keeps for its next import.
    ///
    /// # Errors
    ///
    /// Refuses a missing opt-in, invalid disclosure options, an inconsistent
    /// exchange baseline and shared state that forms no valid task tree.
    pub fn export_task_sync_folder(
        &self,
        profile: &ExportProfile,
        now: DateTime<Utc>,
    ) -> Result<TaskSyncFolderExport, StoreError> {
        self.require_task_sync_enabled()?;
        self.read_store_at(now)?
            .export_task_sync_folder(&self.project_id, profile)
    }

    /// Plans one folder export on one snapshot: the named bootstrap when no
    /// folder head is recorded, otherwise only the tasks changed since it.
    ///
    /// # Errors
    ///
    /// Refuses a missing opt-in and every refusal of the storage plan.
    pub fn plan_task_sync_folder_export(
        &self,
        profile: &ExportProfile,
        rebuild: bool,
        now: DateTime<Utc>,
    ) -> Result<crate::storage::FolderExportPlan, StoreError> {
        self.require_task_sync_enabled()?;
        self.read_store_at(now)?
            .plan_task_sync_folder_export(&self.project_id, profile, rebuild)
    }

    /// [`Self::plan_task_sync_folder_export`] and the memory component's
    /// capture on one snapshot.
    ///
    /// # Errors
    ///
    /// Refuses a missing opt-in and every refusal of the plan or the capture.
    pub fn plan_task_sync_folder_export_with_memories(
        &self,
        profile: &ExportProfile,
        rebuild: bool,
        selected: &BTreeMap<String, crate::task_sync::memories::KeyHistory>,
        resolutions: &BTreeMap<String, crate::task_sync::memories::ImportOptions>,
        now: DateTime<Utc>,
    ) -> Result<
        (
            crate::storage::FolderExportPlan,
            crate::task_sync::memories::ExportOutcome,
        ),
        StoreError,
    > {
        self.require_task_sync_enabled()?;
        self.read_store_at(now)?
            .plan_task_sync_folder_export_with_memories(
                &self.project_id,
                profile,
                rebuild,
                selected,
                resolutions,
            )
    }

    /// The recorded folder head of this project under `profile`, if any.
    ///
    /// # Errors
    ///
    /// Refuses a missing opt-in and a damaged projected head.
    pub fn task_sync_folder_head(
        &self,
        profile: &ExportProfile,
        now: DateTime<Utc>,
    ) -> Result<Option<(crate::ObjectId, crate::storage::FolderHead)>, StoreError> {
        self.require_task_sync_enabled()?;
        self.read_store_at(now)?
            .task_sync_folder_head(&self.project_id, profile)
    }

    /// Records the next folder head if its predecessor is still current.
    ///
    /// # Errors
    ///
    /// Refuses a read-only service, a missing opt-in, another project and a
    /// moved head.
    pub fn record_task_sync_folder_head(
        &self,
        head: &crate::storage::FolderHead,
        now: DateTime<Utc>,
    ) -> Result<crate::ObjectId, StoreError> {
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        if head.project_id != self.project_id {
            return Err(TaskSyncRefusal::OriginMismatch
                .invalid("task folder head belongs to another project"));
        }
        self.lock_store_at(now)?.record_task_sync_folder_head(head)
    }

    /// Applies a resolved folder against the destination basis the caller
    /// selected, never against the basis the folder itself carries. The note
    /// selection is shared by both hosts; the origin-machine alias is
    /// host-local, so the destination is compared under its own basis
    /// profile. A dry run performs the whole application under the same
    /// idempotency key and rolls it back.
    ///
    /// # Errors
    ///
    /// Refuses a read-only service, a missing opt-in, a basis for another
    /// project or note selection, local planning changed since the basis, and
    /// every refusal of the folder application itself.
    pub fn import_task_sync_folder(
        &self,
        folder: &FolderExchange,
        basis: &DestinationBasis,
        dry_run: bool,
        now: DateTime<Utc>,
    ) -> Result<TaskSyncImportResult, StoreError> {
        Ok(self
            .import_task_sync_folder_with_memories(
                folder,
                basis,
                &BTreeMap::default(),
                &BTreeMap::default(),
                dry_run,
                now,
            )?
            .tasks)
    }

    /// Imports task targets and the explicitly selected memory component on
    /// the same connection/transaction, including a single final dry-run rollback.
    ///
    /// # Errors
    /// Refuses the same admission/basis failures as task-only import and an
    /// invalid memory manifest; individual key refusals remain visible.
    pub fn import_task_sync_folder_with_memories(
        &self,
        folder: &FolderExchange,
        basis: &DestinationBasis,
        memories: &std::collections::BTreeMap<String, crate::task_sync::memories::KeyHistory>,
        options: &std::collections::BTreeMap<String, crate::task_sync::memories::ImportOptions>,
        dry_run: bool,
        now: DateTime<Utc>,
    ) -> Result<crate::task_sync::memories::FolderImportOutcome, StoreError> {
        self.import_task_sync_folder_with_head(folder, basis, memories, options, dry_run, now, None)
            .map(|(outcome, _)| outcome)
    }

    /// [`Self::import_task_sync_folder_with_memories`], recording `head`'s
    /// folder head in the import's own transaction when one is given; a dry
    /// run records none.
    #[allow(
        clippy::too_many_arguments,
        reason = "the complete import's own inputs beside the folder head it records"
    )]
    pub(super) fn import_task_sync_folder_with_head(
        &self,
        folder: &FolderExchange,
        basis: &DestinationBasis,
        memories: &std::collections::BTreeMap<String, crate::task_sync::memories::KeyHistory>,
        options: &std::collections::BTreeMap<String, crate::task_sync::memories::ImportOptions>,
        dry_run: bool,
        now: DateTime<Utc>,
        head: Option<CompleteHead<'_>>,
    ) -> Result<
        (
            crate::task_sync::memories::FolderImportOutcome,
            Option<crate::storage::FolderHead>,
        ),
        StoreError,
    > {
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        let (export_basis, profile) = match basis {
            DestinationBasis::Recorded(recorded) => {
                if recorded.project_id != self.project_id {
                    return Err(TaskSyncRefusal::OriginMismatch
                        .invalid("task folder basis belongs to another project"));
                }
                if recorded.profile.include_notes != folder.profile.include_notes {
                    return Err(TaskSyncRefusal::DivergentPlanning
                        .invalid("task folder basis note selection differs from the folder's"));
                }
                // The basis is this host's recorded folder head with nothing
                // changed since it. Its root and fingerprint include held
                // tasks' incoming bytes, so it is compared with the head, not
                // with local planning, whichever basis it carries.
                let unchanged = match &recorded.incremental {
                    // The proof that nothing changed and the fingerprint it
                    // accepts are read on one snapshot, so a revision made
                    // between them is never accepted as the old basis; the
                    // write still compares that fingerprint with planning.
                    Some(incremental) => self.read_store_at(now)?.work_read_snapshot(|store| {
                        let unchanged_head = store
                            .task_sync_folder_head(&self.project_id, &recorded.profile)?
                            .is_some_and(|(_, head)| {
                                head.inventory_root == incremental.inventory_root
                                    && head.task_count == incremental.task_count
                                    && head.cut == incremental.cut
                            })
                            && store
                                .plan_task_sync_folder_export(
                                    &self.project_id,
                                    &recorded.profile,
                                    false,
                                )
                                .is_ok_and(|plan| plan.changes.is_empty());
                        unchanged_head
                            .then(|| {
                                store
                                    .export_task_sync_folder(&self.project_id, &recorded.profile)
                                    .map(|current| current.export_basis)
                            })
                            .transpose()
                    })?,
                    None => None,
                };
                let export_basis = match (unchanged, &recorded.export_basis, &recorded.incremental)
                {
                    (Some(current), _, _) => current,
                    (None, Some(export_basis), _) => export_basis.clone(),
                    // An incremental export records its membership root, not
                    // the complete fingerprint. The complete route reads every
                    // task anyway, so it proves the destination still has that
                    // membership and compares by the fingerprint it computes.
                    (None, None, Some(incremental)) => {
                        let current = self
                            .read_store_at(now)?
                            .export_task_sync_folder(&self.project_id, &recorded.profile)?;
                        let root = current.tree.tasks.iter().try_fold(
                            crate::task_sync::inventory::InventoryRoot::ZERO,
                            crate::task_sync::inventory::InventoryRoot::including,
                        )?;
                        if root != incremental.inventory_root
                            || current.tree.tasks.len() as u64 != incremental.task_count
                        {
                            return Err(TaskSyncRefusal::DivergentPlanning.invalid(
                                "local planning changed since this host's own export named by --basis",
                            ));
                        }
                        current.export_basis
                    }
                    (None, None, None) => {
                        return Err(TaskSyncRefusal::InvalidGraph
                            .invalid("task folder basis carries no basis"));
                    }
                };
                (export_basis, recorded.profile.clone())
            }
            // An empty destination exports nothing under any alias.
            DestinationBasis::Empty => (
                empty_export_basis(&self.project_id, &folder.profile)?,
                folder.profile.clone(),
            ),
        };
        let actor = self.actor("task-sync:import", "import reported task planning state");
        let request = TaskSyncFolderApply {
            project: &self.project_id,
            actor: &actor,
            tree: &folder.tree,
            export_basis: &export_basis,
            profile: &profile,
            complete: true,
            now,
        };
        // The request's serialization omits its time, so an exact repeat of
        // the same intent derives the same key and replays its first receipt.
        let key = format!(
            "task-sync-folder:{}",
            CanonicalObject::freeze(&request)?.key().as_str()
        );
        let mut store = self.lock_store_at(now)?;
        match head {
            // A dry run rolls everything back, so it records no head either.
            Some(head) if !dry_run => {
                let applied =
                    store.apply_task_sync_folder_complete(crate::storage::FolderCompleteApply {
                        apply: request,
                        idempotency_key: &key,
                        previous: head.previous,
                        commit: head.commit,
                        inventory_root: head.inventory_root,
                        task_count: head.task_count,
                        task_bytes: head.task_bytes,
                        rebuild: head.rebuild,
                        memories,
                        memory_options: options,
                    })?;
                Ok((
                    crate::task_sync::memories::FolderImportOutcome {
                        tasks: applied.result,
                        memories: applied.memories,
                        memory_partial: applied.memory_partial,
                    },
                    applied.head,
                ))
            }
            _ => Ok((
                store.apply_task_sync_folder_with_memories(
                    request, &key, memories, options, dry_run,
                )?,
                None,
            )),
        }
    }
}

/// The folder head a complete import records in its own transaction.
pub(super) struct CompleteHead<'a> {
    pub previous: Option<&'a crate::ObjectId>,
    pub commit: Option<String>,
    pub inventory_root: crate::task_sync::inventory::InventoryRoot,
    pub task_count: u64,
    pub task_bytes: u64,
    pub rebuild: bool,
}

/// The basis of a destination that holds no shared planning: the
/// fingerprint of the empty export it would make under `profile`. It is
/// built from nothing, never read from the store, so the applying
/// transaction's comparison is what proves the destination empty.
fn empty_export_basis(project: &ProjectId, profile: &ExportProfile) -> Result<String, StoreError> {
    let empty = TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: project.clone(),
            notes_included: profile.include_notes,
        },
        tasks: Vec::new(),
    };
    Ok(documents_fingerprint(&empty.folder_documents()?))
}
