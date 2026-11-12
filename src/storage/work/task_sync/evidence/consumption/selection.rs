//! One source-owner measurement outside the writer for explicit native uses.

use rusqlite::{Connection, params};

use super::{
    ConsumedProof, ImportedEvidenceConsumption, MeasuredDestination, ObjectId, Result, current_on,
    invalid, load_on, measure_binding_destination, object_on, store_error,
};
use crate::domain::{AcceptanceResult, ControlWorkBinding, WorkId, WorkRunId};
use crate::task_sync::evidence::consumption::CONSUMPTION_KIND;

pub(crate) struct LiveUses {
    measured: Option<MeasuredDestination>,
    records: Vec<ImportedEvidenceConsumption>,
}

#[derive(Default)]
pub(crate) struct SelectedUses {
    pub acceptance: Option<SelectedAcceptance>,
    pub current_verification: bool,
    pub retired_imports: Vec<ObjectId>,
}

pub(crate) struct SelectedAcceptance {
    pub results: Vec<AcceptanceResult>,
}

fn selected_records_on(
    connection: &Connection,
    work: WorkId,
    run: WorkRunId,
    citations: &[ObjectId],
    include_satisfied: bool,
) -> Result<Vec<ImportedEvidenceConsumption>> {
    let mut ids = citations.to_vec();
    if include_satisfied {
        let mut statement = connection.prepare(
            "SELECT evidence_id FROM work_run_obligations
             WHERE run_id=?1 AND state='satisfied' AND evidence_id IS NOT NULL",
        )?;
        for row in statement.query_map([run.0.to_string()], |row| row.get::<_, String>(0))? {
            ids.push(super::super::stored_id(row?).map_err(store_error)?);
        }
    }
    ids.sort();
    ids.dedup();
    let mut records = Vec::new();
    for id in ids {
        if object_on(connection, &id)?.is_some_and(|(kind, _)| kind == CONSUMPTION_KIND) {
            let record = load_on(connection, &id)?;
            if record.request.binding.work_id != work || record.request.binding.run_id != run {
                return Err(invalid("selected imported use belongs to another run"));
            }
            let native: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM work_run_evidence WHERE evidence_id=?1 AND run_id=?2 AND evidence_kind='imported')",
                params![id.as_str(),run.0.to_string()], |row| row.get(0),
            )?;
            if !native {
                return Err(invalid("foreign consumption is inert, not a local use"));
            }
            records.push(record);
        }
    }
    Ok(records)
}

pub(crate) fn measure_selected_on(
    connection: &Connection,
    work: WorkId,
    run: WorkRunId,
    citations: &[ObjectId],
    include_satisfied: bool,
) -> Result<Option<LiveUses>> {
    // Capture the database selection together. Source-owner I/O must run only
    // after this snapshot ends; measurement requires an autocommit connection.
    let captured = crate::storage::work::query::on_one_snapshot(connection, |connection| {
        let records = selected_records_on(connection, work, run, citations, include_satisfied)?;
        let Some(first) = records.first() else {
            return Ok(None);
        };
        let binding = if include_satisfied {
            let item = crate::storage::work::query::load_work_item(connection, work)?;
            let current_run = crate::storage::work::query::load_work_run(connection, run)?;
            let claim = crate::storage::work::query::load_work_claim_optional(connection, run)?
                .ok_or_else(|| invalid("imported use requires a current claim"))?;
            ControlWorkBinding {
                root_execution_id: current_run.root_execution_id,
                work_id: work,
                run_id: run,
                work_revision: item.revision,
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
            }
        } else {
            first.request.binding.clone()
        };
        let root = crate::storage::work::feeds::latest_named_root_binding_on(
            connection,
            run,
            binding.claim_id,
            i64::MAX,
        )?;
        let bound = root
            .is_some_and(|(_, _, event)| event.kind == crate::domain::NamedRootBindingKind::Bound);
        Ok(Some((records, bound.then_some(binding))))
    })?;
    let Some((records, binding)) = captured else {
        return Ok(None);
    };
    let measured = binding
        .as_ref()
        .map(|binding| measure_binding_destination(connection, binding).map_err(store_error))
        .transpose()?;
    Ok(Some(LiveUses { measured, records }))
}

/// Recheck selection completeness at the caller's writer snapshot, including
/// an initially empty selection, before qualifying any captured use.
pub(crate) fn reselect_on<'a>(
    connection: &Connection,
    work: WorkId,
    run: WorkRunId,
    citations: &[ObjectId],
    include_satisfied: bool,
    measured: Option<&'a LiveUses>,
) -> Result<Option<&'a LiveUses>> {
    let records = selected_records_on(connection, work, run, citations, include_satisfied)?;
    let captured = measured.map_or(&[][..], |uses| uses.records.as_slice());
    if records != captured {
        return Err(invalid(
            "selected imported uses changed; read the current evidence and retry",
        ));
    }
    Ok(measured)
}

impl LiveUses {
    pub(crate) fn validate_on(
        &self,
        connection: &Connection,
        binding: &ControlWorkBinding,
        selected: &[ObjectId],
        declared: Option<&crate::source::SourceId>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<SelectedUses> {
        let mut selected_uses = SelectedUses::default();
        for record in &self.records {
            let explicitly_selected = selected.contains(&record.request.consumption);
            // Check immutable context and the terminal union even when a local
            // movement retires implicit credit. A conflict is never currency.
            if let ConsumedProof::Verification {
                execution: Some(key),
                ..
            } = &record.proof
                && super::super::verification::union_on(connection, key)
                    .map_err(store_error)?
                    .conflicted
            {
                return Err(invalid("imported verification terminal facts conflict"));
            }
            let current = match &self.measured {
                Some(measured) => {
                    super::destination_current_on(connection, record, binding, measured)?
                }
                None => false,
            };
            if !explicitly_selected && !current {
                if matches!(record.proof, ConsumedProof::Acceptance { .. }) {
                    super::super::acceptance::validate_retirement_on(connection, record)
                        .map_err(store_error)?;
                }
                selected_uses
                    .retired_imports
                    .push(record.request.consumption.clone());
                continue;
            }
            let measured = self
                .measured
                .as_ref()
                .ok_or_else(|| invalid("selected imported use has no current named root"))?;
            if declared.is_some_and(|declared| declared != &measured.source) {
                return Err(invalid(
                    "completion declaration differs from its imported proof source",
                ));
            }
            let results = current_on(connection, record, binding, measured, now)?;
            if matches!(record.proof, ConsumedProof::Verification { .. }) {
                selected_uses.current_verification = true;
            }
            if let Some(mut results) = results
                && explicitly_selected
            {
                if selected_uses.acceptance.is_some() {
                    return Err(invalid("select only one imported acceptance proof"));
                }
                for result in &mut results {
                    result.evidence = vec![record.request.consumption.clone()];
                }
                selected_uses.acceptance = Some(SelectedAcceptance { results });
            }
        }
        Ok(selected_uses)
    }
}
