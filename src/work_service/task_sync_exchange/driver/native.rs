//! Thin native-capture and whole-pack-page adapters; bodies stay out of receipts.

use super::{LocalWorkService, Result, StepReceipt, StepRequest};
use crate::{DevelopmentNoopRedactor, task_sync::data_worktree::DataWorktree};

impl LocalWorkService {
    pub(super) fn step_native(
        &self,
        request: &StepRequest,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<StepReceipt> {
        match request {
            StepRequest::CaptureNative { binding, request } => {
                // Existing history is required; do not create a store for a
                // missing or misdirected capture request.
                let record = {
                    let store = self.read_store_at(now)?;
                    self.step_binding(&store, binding)?
                };
                anyhow::ensure!(
                    record.profile == request.profile,
                    "native evidence capture disclosure profile differs"
                );
                let actor = self.actor("task_sync_step", "explicit native evidence capture");
                let mut store = self.step_store()?;
                let capture = store.capture_native_evidence(
                    &self.project_id,
                    &self.session_id,
                    &actor,
                    request,
                    &DevelopmentNoopRedactor,
                )?;
                Ok(StepReceipt::NativeCapture {
                    capture: Box::new(capture),
                })
            }
            StepRequest::ExportNative {
                binding,
                project_file,
                branch,
                cursor,
                maximum_packs,
            } => {
                let selected =
                    DataWorktree::observe_project(project_file, &self.project_id, branch)?;
                let page = {
                    let store = self.read_store_at(now)?;
                    let binding = self.step_binding(&store, binding)?;
                    anyhow::ensure!(
                        selected == DataWorktree::from_binding(&binding)?,
                        "native evidence export selects another data worktree"
                    );
                    store.export_native_evidence_capture(
                        &self.project_id,
                        cursor,
                        *maximum_packs,
                        &binding.profile,
                    )?
                };
                let files = selected.stage_evidence_packs(&self.project_id, &page.packs)?;
                Ok(StepReceipt::ExportedNative {
                    files,
                    next: page.next,
                })
            }
            _ => anyhow::bail!("invalid internal native evidence step selection"),
        }
    }
}
