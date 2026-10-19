//! The folder route of the explicit task exchange: an export that carries
//! its basis, and an import against a destination basis the caller selects.

use chrono::{DateTime, Utc};

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
                (recorded.export_basis.clone(), recorded.profile.clone())
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
        if dry_run {
            store.dry_run_task_sync_folder(request, &key)
        } else {
            store.apply_task_sync_folder(request, &key)
        }
    }
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
