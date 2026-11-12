//! Exact replay precedes current files; fresh imports retain construction first.

use super::{
    Cancellation, RunLimits,
    admission::{GitSelection, SelectedHeadChanged, observe_git_selection},
    remaining,
};
use crate::{
    ObjectId,
    storage::SqliteStore,
    task_sync::{
        evidence::{EvidenceError, EvidencePack},
        ledger::{
            Binding, CompletedExchange, EvidenceImportRecipe, EvidenceIntent, EvidenceRefusal,
            EvidenceStop,
        },
        retention,
    },
};
use anyhow::{Context, Result, ensure};

pub(crate) struct InspectedEvidence {
    pub(crate) binding_id: ObjectId,
    pub(crate) binding: Binding,
    pub(crate) recipe: EvidenceImportRecipe,
    pub(crate) packs: Vec<EvidencePack>,
}

impl InspectedEvidence {
    pub(crate) fn recheck_recipe(&self) -> Result<()> {
        ensure!(
            retention::evidence::read_recipe(
                &self.binding_id,
                &self.binding,
                self.recipe.intent()?
            )?
            .as_ref()
                == Some(&self.recipe),
            "retained evidence construction changed after inspection"
        );
        ensure!(
            retention::commit_selection(
                &self.binding.repository.git_directory,
                self.recipe.intent()?.operation
            )?
            .as_ref()
                == Some(&self.recipe.intent()?.source),
            "retained evidence carrier selection changed after inspection"
        );
        Ok(())
    }
}

/// Apply selected immutable evidence without granting local verification credit.
/// Completed requests return original canonical metadata before current files,
/// Git or new identity selection. A matching pending local acknowledgement
/// first requires proof of that exact already committed step. The independent
/// completed lookup remains read-only. Fresh requests preserve exact construction
/// before the selected pack bytes are read, then use one admitted SQL writer.
/// A selected head observed at another commit than the request expects ends
/// the request with a known stop. No fetch, commit, branch advance or push is
/// performed.
///
/// # Errors
/// Refuses changed retry intent, unsupported confinement, data-carrier drift,
/// incomplete typed context, private uncertainty or any atomic storage refusal.
pub async fn apply_evidence_exchange(
    store: &mut SqliteStore,
    binding_id: &ObjectId,
    binding: &Binding,
    intent: &EvidenceIntent,
    now: chrono::DateTime<chrono::Utc>,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<CompletedExchange> {
    let started = std::time::Instant::now();
    limits.validate_request()?;
    let limits = crate::task_sync::exchange::remaining(limits, started)?;
    apply_evidence_exchange_remaining(store, binding_id, binding, intent, now, limits, cancel).await
}

// The public ingress admitted the full budget; internal calls spend its remainder.
pub(crate) async fn apply_evidence_exchange_remaining(
    store: &mut SqliteStore,
    binding_id: &ObjectId,
    binding: &Binding,
    intent: &EvidenceIntent,
    now: chrono::DateTime<chrono::Utc>,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<CompletedExchange> {
    let warning = crate::operation_time::Warning::start("task exchange apply evidence exchange");
    if let Some(original) = store.completed_task_sync_evidence(binding_id, intent)? {
        store.acknowledge_committed_task_sync_evidence(binding_id, intent, &original)?;
        return Ok(original);
    }
    ensure!(
        cfg!(windows),
        "evidence exchange requires supported confined Git execution"
    );
    binding.validate()?;
    intent.validate(binding_id, binding)?;
    crate::storage::admit_live_actor_session(&intent.actor)?;
    let started = std::time::Instant::now();
    let retained = retention::evidence::read_recipe(binding_id, binding, intent)?;
    let selected = GitSelection {
        expected_head: intent.expected_head.as_deref(),
        commit: &intent.source,
        common_commit: None,
        published_head: false,
    };
    // Observe the selected Git state before a fresh recipe exists, so any
    // uncertain or unrelated Git refusal leaves nothing new behind. Its chain
    // position is chosen only afterwards, together with its retention, and
    // retention still precedes reading any selected pack bytes.
    let observed = observe_git_selection(
        binding,
        selected,
        remaining(limits, started)?,
        std::time::Instant::now(),
        cancel,
    )
    .await;
    let carrier_retained = || -> Result<()> {
        ensure!(
            retention::commit_selection(&binding.repository.git_directory, intent.operation)?
                .as_ref()
                == Some(&intent.source),
            "selected evidence carrier retention is missing or differs"
        );
        Ok(())
    };
    let mut construct = || -> Result<EvidenceImportRecipe> {
        match &retained {
            Some(original) => {
                retention::evidence::retain_recipe(binding_id, binding, original)?;
                Ok(original.clone())
            }
            None => Ok(store.retain_task_sync_evidence_recipe(binding_id, binding, intent, now)?),
        }
    };
    if head_changed(&observed) {
        // The fetch already retained this operation's carrier, and custody
        // refuses every later exchange while a retained operation is unknown
        // to the ledger. A definite head change is a known outcome whether or
        // not construction was retained yet, so it always ends canonically.
        carrier_retained()?;
        let recipe = construct()?;
        return stop_changed_selection(store, binding_id, binding, recipe);
    }
    observed?;
    carrier_retained()?;
    let recipe = construct()?;
    let (packs, stopped) = match crate::task_sync::git::read_selected_evidence_remaining(
        binding_id,
        binding,
        intent,
        remaining(limits, started)?.deadline,
        &warning,
    ) {
        Ok(batch) => (batch.packs, None),
        Err(error) => {
            let Some(stopped) = known_stop(&error) else {
                return Err(error);
            };
            (Vec::new(), Some(stopped))
        }
    };
    let observed = observe_git_selection(
        binding,
        selected,
        remaining(limits, started)?,
        std::time::Instant::now(),
        cancel,
    )
    .await;
    if head_changed(&observed) {
        return stop_changed_selection(store, binding_id, binding, recipe);
    }
    observed?;
    let input = InspectedEvidence {
        binding_id: binding_id.clone(),
        binding: binding.clone(),
        recipe,
        packs,
    };
    input
        .recheck_recipe()
        .context("evidence construction is not retained exactly")?;
    match stopped {
        Some(stopped) => Ok(store.stop_task_sync_evidence_import(&input, &stopped)?),
        None => Ok(store.apply_task_sync_evidence_import(&input)?),
    }
}

/// The known stop for a selected-evidence read that definitely failed: the
/// immutable carrier disagrees with the frozen request, or its pack carries a
/// classified evidence refusal. An unavailable, interrupted, late or otherwise
/// uncertain read has none, so its retained request stays pending for an exact
/// retry and grants no coverage.
fn known_stop(error: &anyhow::Error) -> Option<EvidenceStop> {
    error.chain().find_map(|cause| {
        if cause
            .downcast_ref::<crate::task_sync::git::SelectionMismatch>()
            .is_some()
        {
            return Some(EvidenceStop::Refused {
                reason: EvidenceRefusal::InvalidPayload,
            });
        }
        cause
            .downcast_ref::<EvidenceError>()
            .and_then(EvidenceStop::from_error)
    })
}

/// Whether an observation definitely read the selected branch at another head.
/// An unavailable, cancelled or otherwise uncertain observation is not a change.
fn head_changed(observed: &Result<()>) -> bool {
    observed
        .as_ref()
        .is_err_and(|error| error.downcast_ref::<SelectedHeadChanged>().is_some())
}

/// Settle a retained construction whose selected head definitely differs from
/// the request's. Its expected head is fixed, so an exact retry could only
/// refuse again while its retained carrier and recipe kept custody from every
/// later exchange. The known stop uses the retained identities, imports no
/// pack and grants no coverage.
fn stop_changed_selection(
    store: &mut SqliteStore,
    binding_id: &ObjectId,
    binding: &Binding,
    recipe: EvidenceImportRecipe,
) -> Result<CompletedExchange> {
    let input = InspectedEvidence {
        binding_id: binding_id.clone(),
        binding: binding.clone(),
        recipe,
        packs: Vec::new(),
    };
    input
        .recheck_recipe()
        .context("evidence construction is not retained exactly")?;
    Ok(store.stop_task_sync_evidence_import(
        &input,
        &EvidenceStop::Refused {
            reason: EvidenceRefusal::SelectionChanged,
        },
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_sync::git::{QueryFailure, SelectionMismatch};

    /// A definite disagreement ends the request even under added context; an
    /// unavailable, late or failed read leaves it pending.
    #[test]
    fn only_a_definite_selected_evidence_failure_ends_a_request() {
        let invalid = Some(EvidenceStop::Refused {
            reason: EvidenceRefusal::InvalidPayload,
        });
        let definite = anyhow::Error::new(SelectionMismatch("declared size differs"))
            .context("reading selected evidence");
        assert_eq!(known_stop(&definite), invalid);
        for uncertain in [
            anyhow::Error::new(QueryFailure::Deadline),
            anyhow::Error::new(QueryFailure::Nonzero {
                command: "cat-file".into(),
                code: Some(128),
            }),
            anyhow::Error::new(EvidenceError::Io(std::io::Error::other("pipe closed"))),
            anyhow::anyhow!("data worktree changed since observation"),
        ] {
            assert_eq!(known_stop(&uncertain), None, "{uncertain:#}");
        }
    }
}
