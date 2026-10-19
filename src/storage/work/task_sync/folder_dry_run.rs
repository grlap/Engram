//! A folder application that is always rolled back.

use super::{
    TaskSyncImportResult,
    folder::{TaskSyncFolderApply, apply_task_sync_folder_on},
};
use crate::storage::{SqliteStore, StoreError};

impl SqliteStore {
    /// Runs the whole folder application under the real idempotency key, with
    /// every check, replay and refusal it would make now, in a transaction that
    /// is then rolled back explicitly. A committed intent replays its receipt,
    /// as the real repeat would. Nothing persists: no task, baseline, receipt
    /// or feed entry, and no idempotency intent is used up. A later real
    /// application checks everything again.
    ///
    /// # Errors
    ///
    /// Returns the refusal the committing application would return now, or a
    /// failed rollback when the application itself succeeded.
    pub fn dry_run_task_sync_folder(
        &mut self,
        request: TaskSyncFolderApply<'_>,
        idempotency_key: &str,
    ) -> Result<TaskSyncImportResult, StoreError> {
        let transaction = self.begin_work_mutation()?;
        let applied = apply_task_sync_folder_on(&transaction, request, idempotency_key);
        let rolled_back = transaction.rollback();
        // The application's own refusal explains more than a failed rollback.
        let mut result = applied?;
        rolled_back?;
        result.dry_run = true;
        Ok(result)
    }
}
