//! CLI exchange admission; all exchange meaning stays with its existing owner.

use super::{Cancellation, LocalWorkService, RunLimits, exchange};
use crate::task_sync::{
    data_worktree::DataWorktree,
    driver::{
        CommitRequest, DeltaRequest, EventSummary, Outcome, ResolutionRequest, StepReceipt,
        StepRequest,
    },
    ledger::{Binding, CompletedExchange, OutgoingProgress, Transition},
};
use crate::{ActorContext, ObjectId, SqliteStore};
use anyhow::{Context, Result, ensure};

mod imported;
mod native;

impl LocalWorkService {
    /// Execute one explicit exchange step, never a network operation or merge.
    /// Historical reads and exact replay use existing original-outcome owners.
    ///
    /// # Errors
    /// Refuses read-only mutations, missing opt-in, foreign scope, drift and
    /// every custody/graph/retention refusal of the selected existing owner.
    pub async fn task_sync_exchange_step(
        &self,
        request: &StepRequest,
        limits: RunLimits,
        cancel: &Cancellation,
    ) -> Result<StepReceipt> {
        limits.validate_request()?;
        let _warning = crate::operation_time::Warning::start("task exchange service");
        ensure!(
            !cancel.is_cancelled(),
            "exchange step cancelled before admission"
        );
        let started = std::time::Instant::now();
        if !request.is_read_only() {
            self.require_task_sync_writer()?;
            self.require_task_sync_enabled()?;
        }
        let now = chrono::Utc::now();
        self.validate_read_attribution(now)?;
        match request {
            StepRequest::QualifyVerification { .. }
            | StepRequest::VerificationSummary { .. }
            | StepRequest::QualifyAcceptance { .. }
            | StepRequest::AcceptanceSummary { .. }
            | StepRequest::ConsumeVerification { .. }
            | StepRequest::ConsumeAcceptance { .. } => self.step_imported(request, now),
            StepRequest::CaptureNative { .. } | StepRequest::ExportNative { .. } => {
                self.step_native(request, now)
            }
            StepRequest::ReadBinding { .. }
            | StepRequest::CompletedDelta { .. }
            | StepRequest::ReadDeltaProgress { .. }
            | StepRequest::CompletedEvidence { .. }
            | StepRequest::CompletedSuccessor { .. } => self.step_read(request, now),
            StepRequest::InspectTarget { .. }
            | StepRequest::RetainData { .. }
            | StepRequest::Activate { .. }
            | StepRequest::AdvanceData { .. } => {
                self.step_data(request, limits, cancel, started).await
            }
            StepRequest::PrepareDelta { .. }
            | StepRequest::RetainIncomingDelta { .. }
            | StepRequest::RecordExportCommit { .. }
            | StepRequest::RecordPublication { .. }
            | StepRequest::ApplyDelta { .. } => {
                self.step_delta(request, now, limits, cancel, started).await
            }
            StepRequest::ApplyEvidence { .. } | StepRequest::ApplySuccessor { .. } => {
                self.step_incoming(request, now, limits, cancel, started)
                    .await
            }
            StepRequest::RecordResolution { .. } | StepRequest::RecordRetryProgress { .. } => {
                Box::pin(self.step_resolution(request, limits, cancel, started)).await
            }
        }
    }

    fn step_read(
        &self,
        request: &StepRequest,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<StepReceipt> {
        match request {
            StepRequest::ReadBinding { binding } => {
                let store = self.read_store_at(now)?;
                Ok(StepReceipt::Binding {
                    binding: binding.clone(),
                    record: Box::new(self.step_binding(&store, binding)?),
                })
            }
            StepRequest::CompletedDelta { request } => {
                let store = self.read_store_at(now)?;
                self.step_binding(&store, &request.binding)?;
                outcome(store.completed_task_sync_delta_import(&request.binding, &request.intent)?)
            }
            StepRequest::ReadDeltaProgress { request } => {
                let store = self.read_store_at(now)?;
                self.step_binding(&store, &request.binding)?;
                let events =
                    store.task_sync_delta_outgoing_events(&request.binding, &request.intent)?;
                let events = events
                    .into_iter()
                    .map(|(event, record)| {
                        Ok(EventSummary {
                            event,
                            sequence: record.sequence,
                            transition: transition_word(&record.transition)?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(StepReceipt::Progress { events })
            }
            StepRequest::CompletedEvidence { request } => {
                let store = self.read_store_at(now)?;
                self.step_binding(&store, &request.binding)?;
                outcome(store.completed_task_sync_evidence(&request.binding, &request.intent)?)
            }
            StepRequest::CompletedSuccessor { request } => {
                let store = self.read_store_at(now)?;
                self.step_binding(&store, &request.binding)?;
                outcome(
                    store.completed_task_sync_planning_successor(
                        &request.binding,
                        &request.request,
                    )?,
                )
            }
            _ => anyhow::bail!("invalid internal read step selection"),
        }
    }

    async fn step_data(
        &self,
        request: &StepRequest,
        limits: RunLimits,
        cancel: &Cancellation,
        started: std::time::Instant,
    ) -> Result<StepReceipt> {
        match request {
            StepRequest::InspectTarget {
                project_file,
                branch,
                remote,
                reference,
            } => {
                let data = DataWorktree::observe_project(project_file, &self.project_id, branch)?;
                let target = exchange::ExchangeTarget::capture_remaining(
                    &exchange::TargetSelection {
                        repository: data.checkout.clone(),
                        remote: remote.clone(),
                        reference: reference.clone(),
                    },
                    exchange::remaining(limits, started)?,
                    cancel,
                )
                .await?;
                data.revalidate()?;
                Ok(StepReceipt::Target {
                    configuration_fingerprint: target.fingerprint()?,
                    checkout: data.checkout,
                    git_directory: data.git_directory,
                    branch: data.reference,
                    head: data.head,
                })
            }
            StepRequest::RetainData {
                project_file,
                branch,
                operation,
                commit,
            } => {
                let data = DataWorktree::observe_project(project_file, &self.project_id, branch)?;
                data.check_commit(commit)?;
                let retained = exchange::retain_commit_remaining(
                    &data.checkout,
                    *operation,
                    commit,
                    exchange::remaining(limits, started)?,
                    cancel,
                )
                .await?;
                data.revalidate()?;
                Ok(StepReceipt::Retained {
                    commit: Box::new(retained),
                })
            }
            StepRequest::Activate {
                project_file,
                binding,
            } => {
                self.step_actor(&binding.actor)?;
                ensure!(
                    binding.project == self.project_id,
                    "exchange binding belongs to another project"
                );
                ensure!(
                    binding.target.reference == binding.branch,
                    "exchange target must select the data branch"
                );
                let branch = binding
                    .branch
                    .strip_prefix("refs/heads/")
                    .context("data branch ref required")?;
                let selected =
                    DataWorktree::observe_project(project_file, &self.project_id, branch)?;
                ensure!(
                    selected == DataWorktree::from_binding(binding)?,
                    "binding selects another project data worktree"
                );
                let mut store = self.step_store()?;
                Ok(StepReceipt::Activated {
                    binding: store.activate_task_sync_binding(binding)?,
                })
            }
            StepRequest::AdvanceData {
                project_file,
                branch,
                retained,
                expected_head,
            } => {
                let selected =
                    DataWorktree::observe_project(project_file, &self.project_id, branch)?;
                let advanced = selected.advance_retained(retained, expected_head.as_deref())?;
                Ok(StepReceipt::Advanced {
                    reference: advanced.reference,
                    head: advanced.head,
                })
            }
            _ => anyhow::bail!("invalid internal data step selection"),
        }
    }

    async fn step_delta(
        &self,
        request: &StepRequest,
        now: chrono::DateTime<chrono::Utc>,
        limits: RunLimits,
        cancel: &Cancellation,
        started: std::time::Instant,
    ) -> Result<StepReceipt> {
        match request {
            StepRequest::RetainIncomingDelta { request } => {
                let (mut store, binding) = self.step_delta_store(request)?;
                let candidate = exchange::retain_incoming_delta_remaining(
                    &mut store,
                    &request.binding,
                    &binding,
                    &request.intent,
                    now,
                    exchange::remaining(limits, started)?,
                    cancel,
                )
                .await?;
                Ok(StepReceipt::DeltaRetained {
                    candidate: Box::new(candidate),
                })
            }
            StepRequest::PrepareDelta { request } => {
                // Preserve the existing service's pre-store project/feed admission.
                let event =
                    self.prepare_task_sync_delta_export(&request.binding, &request.intent, now)?;
                Ok(recorded(event, None, false))
            }
            StepRequest::RecordExportCommit { request } => {
                let (mut store, binding) = self.step_delta_store(&request.delta)?;
                let event = exchange::record_delta_export_commit_remaining(
                    &mut store,
                    &request.delta.binding,
                    &binding,
                    &request.delta.intent,
                    &request.selection,
                    exchange::remaining(limits, started)?,
                    cancel,
                )
                .await?;
                Ok(recorded(event, None, false))
            }
            StepRequest::RecordPublication { request, progress } => {
                let (mut store, binding) = self.step_delta_store(&request.delta)?;
                let selection = publication(request, &binding, progress.as_ref().clone());
                let (event, fresh) = exchange::record_delta_publication_remaining(
                    &mut store,
                    &selection,
                    exchange::remaining(limits, started)?,
                    cancel,
                )
                .await?;
                Ok(recorded(
                    event,
                    Some(fresh),
                    fresh && matches!(progress.as_ref(), OutgoingProgress::PushStarted),
                ))
            }
            StepRequest::ApplyDelta { request } => {
                let (mut store, binding) = self.step_delta_store(request)?;
                let result = Box::pin(exchange::apply_delta_exchange_remaining(
                    &mut store,
                    &request.binding,
                    &binding,
                    &request.intent,
                    now,
                    exchange::remaining(limits, started)?,
                    cancel,
                ))
                .await?;
                outcome(Some(result))
            }
            _ => anyhow::bail!("invalid internal delta step selection"),
        }
    }

    async fn step_incoming(
        &self,
        request: &StepRequest,
        now: chrono::DateTime<chrono::Utc>,
        limits: RunLimits,
        cancel: &Cancellation,
        started: std::time::Instant,
    ) -> Result<StepReceipt> {
        match request {
            StepRequest::ApplyEvidence { request } => {
                self.step_actor(&request.intent.actor)?;
                self.step_cut(&request.binding, &request.intent.local_cut)?;
                let (mut store, binding) = self.step_bound_store(&request.binding)?;
                let result = Box::pin(exchange::apply_evidence_exchange_remaining(
                    &mut store,
                    &request.binding,
                    &binding,
                    &request.intent,
                    now,
                    exchange::remaining(limits, started)?,
                    cancel,
                ))
                .await?;
                outcome(Some(result))
            }
            StepRequest::ApplySuccessor { request } => {
                self.step_actor(&request.request.intent.actor)?;
                self.step_cut(&request.binding, &request.request.intent.local_cut)?;
                let (mut store, binding) = self.step_bound_store(&request.binding)?;
                let result = exchange::apply_planning_successor(
                    &mut store,
                    &request.binding,
                    &binding,
                    &request.request,
                    &request.payload,
                    now,
                    &crate::DevelopmentNoopRedactor,
                )?;
                outcome(Some(result))
            }
            _ => anyhow::bail!("invalid internal incoming step selection"),
        }
    }

    async fn step_resolution(
        &self,
        request: &StepRequest,
        limits: RunLimits,
        cancel: &Cancellation,
        started: std::time::Instant,
    ) -> Result<StepReceipt> {
        match request {
            StepRequest::RecordResolution { request } => {
                let (mut store, binding) = self.step_delta_store(&request.original.delta)?;
                let selected = resolution(request, &binding);
                let input = Box::pin(exchange::inspect_delta_resolution_remaining(
                    &store,
                    &selected,
                    exchange::remaining(limits, started)?,
                    cancel,
                ))
                .await?;
                let event = exchange::record_delta_resolution_remaining(
                    &mut store,
                    &input,
                    exchange::remaining(limits, started)?,
                    cancel,
                )
                .await?;
                Ok(recorded(event, None, false))
            }
            StepRequest::RecordRetryProgress { request, progress } => {
                let (mut store, binding) = self.step_delta_store(&request.original.delta)?;
                let selected = resolution(request, &binding);
                let input = Box::pin(exchange::inspect_delta_resolution_remaining(
                    &store,
                    &selected,
                    exchange::remaining(limits, started)?,
                    cancel,
                ))
                .await?;
                let (event, fresh) = exchange::record_delta_retry_progress_remaining(
                    &mut store,
                    &input,
                    progress,
                    exchange::remaining(limits, started)?,
                    cancel,
                )
                .await?;
                Ok(recorded(
                    event,
                    Some(fresh),
                    fresh && matches!(progress.as_ref(), OutgoingProgress::PushStarted),
                ))
            }
            _ => anyhow::bail!("invalid internal resolution step selection"),
        }
    }

    fn step_actor(&self, actor: &ActorContext) -> Result<()> {
        ensure!(
            actor.actor_id == self.actor_id && actor.session_id.as_ref() == Some(&self.session_id),
            "exchange step belongs to another actor or session"
        );
        Ok(())
    }

    fn step_store(&self) -> Result<SqliteStore> {
        let mut store = SqliteStore::open_unresolved(&self.database)?;
        self.register_on(&mut store, chrono::Utc::now())?;
        Ok(store)
    }

    fn step_binding(&self, store: &SqliteStore, id: &ObjectId) -> Result<Binding> {
        let binding = store
            .task_sync_binding(id)?
            .context("exchange binding is missing")?;
        ensure!(
            binding.project == self.project_id,
            "exchange binding belongs to another project"
        );
        Ok(binding)
    }

    fn step_delta_store(&self, request: &DeltaRequest) -> Result<(SqliteStore, Binding)> {
        self.step_actor(&request.intent.actor)?;
        self.step_cut(&request.binding, &request.intent.local_cut)?;
        self.step_bound_store(&request.binding)
    }

    fn step_cut(
        &self,
        binding: &ObjectId,
        cut: &crate::task_sync::delta::OriginQualifiedCut,
    ) -> Result<()> {
        ensure!(
            cut.position.feed == crate::FeedId::Project(self.project_id.clone())
                && &cut.binding == binding,
            "exchange step belongs to another project or binding"
        );
        Ok(())
    }

    fn step_bound_store(&self, id: &ObjectId) -> Result<(SqliteStore, Binding)> {
        let binding = {
            let store = self.read_store_at(chrono::Utc::now())?;
            self.step_binding(&store, id)?
        };
        let store = self.step_store()?;
        ensure!(
            store.task_sync_binding(id)?.as_ref() == Some(&binding),
            "exchange binding changed before admission"
        );
        Ok((store, binding))
    }
}

fn publication<'a>(
    request: &'a CommitRequest,
    binding: &'a Binding,
    progress: OutgoingProgress,
) -> exchange::DeltaPublicationRequest<'a> {
    exchange::DeltaPublicationRequest {
        binding_id: &request.delta.binding,
        binding,
        intent: &request.delta.intent,
        selection: &request.selection,
        progress,
    }
}

fn resolution<'a>(
    request: &'a ResolutionRequest,
    binding: &'a Binding,
) -> exchange::DeltaResolutionRequest<'a> {
    exchange::DeltaResolutionRequest {
        binding_id: &request.original.delta.binding,
        binding,
        original: &request.original.delta.intent,
        selection: &request.original.selection,
        recipe: &request.recipe,
    }
}

fn recorded(event: ObjectId, newly_recorded: Option<bool>, start_recorded: bool) -> StepReceipt {
    StepReceipt::Recorded {
        event,
        newly_recorded,
        start_recorded,
    }
}

fn transition_word(transition: &Transition) -> Result<String> {
    // Serialize only inside the service; the response selects the closed tag,
    // never the potentially large retained preparation or receipt body.
    let value = serde_json::to_value(transition)?;
    Ok(value
        .get("transition")
        .and_then(serde_json::Value::as_str)
        .context("exchange transition tag is missing")?
        .to_owned())
}

fn outcome(result: Option<CompletedExchange>) -> Result<StepReceipt> {
    let outcome = result
        .map(|result| {
            let transition = match &result.event.transition {
                Transition::DeltaImportApplied { .. } => "planning_applied",
                Transition::PlanningDeferred { .. } => "planning_deferred",
                Transition::PlanningSuccessorApplied { .. } => "planning_successor_applied",
                Transition::EvidenceImportApplied { .. } => "evidence_retained",
                Transition::EvidenceStopped { .. } => "evidence_stopped",
                _ => anyhow::bail!("unexpected exchange completion transition"),
            };
            Ok::<_, anyhow::Error>(Outcome {
                prepared: result.prepared,
                outcome: result.outcome,
                transition: transition.into(),
            })
        })
        .transpose()?;
    Ok(StepReceipt::Outcome { outcome })
}
