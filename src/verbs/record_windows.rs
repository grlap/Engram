//! Full note/history windows share one final-representation fitting path.
//! Detail is an explicit unbounded-body read, never an implicit window escape.

use super::{
    AgentVerbs, DateTime, Deserialize, Guidance, MAX_AGENT_WORK_RESPONSE_BYTES, Receipt, Serialize,
    StoreError, Utc, Value, VerbError, WorkFocusView, WorkSectionOmissionReason, json,
};
use crate::storage::{WorkRecordFamily, WorkRecordKind};
use crate::work_service::identity::DisplayIdentity;
use crate::work_service::{WorkAuthoredContract, WorkRecordRow, WorkRecordWindow};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "exclusive show modes stay independent clap/MCP flags, not a mode enum"
)]
pub struct ShowInput {
    #[serde(default)]
    pub notes: bool,
    /// Include structured gate evidence in an explicit notes window.
    #[serde(default)]
    pub gates: bool,
    #[serde(default)]
    pub history: bool,
    pub after: Option<String>,
    pub note: Option<String>,
    /// Complete stored title, outcome, and acceptance; exclusive of windows.
    #[serde(default)]
    pub full: bool,
    /// The evaluation records of the item's run, oldest to newest within a
    /// bounded window; `after` continues it.
    #[serde(default)]
    pub evaluations: bool,
    /// One evaluation record complete, by its full record id.
    pub evaluation: Option<String>,
    /// The source observations of the item's run in a bounded window,
    /// oldest to newest within it; `after` continues it.
    #[serde(default)]
    pub observations: bool,
}

impl AgentVerbs {
    /// Explicit note/history window or complete immutable note detail.
    ///
    /// # Errors
    /// Refuses conflicting modes, invalid/stale cursors and unresolved locators.
    pub fn show_records(
        &self,
        work_ref: &str,
        input: &ShowInput,
        now: DateTime<Utc>,
    ) -> Result<Receipt, VerbError> {
        let other_modes =
            input.notes || input.history || input.gates || input.note.is_some() || input.full;
        if input.observations {
            if other_modes || input.evaluations || input.evaluation.is_some() {
                return Err(VerbError::at(
                    StoreError::InvalidWork(
                        "choose --observations with optional --after, alone".into(),
                    ),
                    work_ref,
                ));
            }
            return self.show_observations(work_ref, input.after.as_deref(), now);
        }
        if (input.evaluations && (other_modes || input.evaluation.is_some()))
            || (input.evaluation.is_some() && (other_modes || input.after.is_some()))
        {
            return Err(VerbError::at(
                StoreError::InvalidWork(
                    "choose --evaluations with optional --after, or --evaluation RECORD_ID alone"
                        .into(),
                ),
                work_ref,
            ));
        }
        if input.evaluations {
            return self.show_evaluations(work_ref, input.after.as_deref(), now);
        }
        if let Some(record) = &input.evaluation {
            return self.show_evaluation(work_ref, record, now);
        }
        if (input.notes && input.history)
            || (input.gates && !input.notes)
            || (input.note.is_some() && (input.notes || input.history || input.full))
            || (input.after.is_some() && !input.notes && !input.history && input.note.is_none())
            || (input.full
                && (input.notes
                    || input.history
                    || input.gates
                    || input.after.is_some()
                    || input.note.is_some()))
        {
            return Err(VerbError::at(
                StoreError::InvalidWork(
                    "choose --notes [--gates] or --history with optional --after, --note LOCATOR with optional --after, or --full"
                        .into(),
                ),
                work_ref,
            ));
        }
        if input.full {
            let contract = self
                .service
                .work_authored_contract(work_ref, now)
                .map_err(|error| VerbError::at(error, work_ref))?;
            return Ok(full_contract_receipt(&contract));
        }
        if let Some(locator) = &input.note {
            let (work_ref, row, assessment) = self
                .service
                .work_note_detail(work_ref, locator, input.after.as_deref(), now)
                .map_err(|error| match error {
                    // A refused continuation starts again from the detail.
                    StoreError::WorkShowCursorInvalid { .. } => VerbError::for_listing(
                        error,
                        &format!(
                            "engram work show {} --note {}",
                            safe_reference_argument(work_ref),
                            safe_reference_argument(locator)
                        ),
                    ),
                    error => VerbError::at(error, work_ref),
                })?;
            let continuation = assessment
                .as_ref()
                .and_then(|page| page.continuation.as_ref())
                .map(|token| {
                    format!(
                        "engram work show {work_ref} --note {} --after {token}",
                        row.locator
                    )
                });
            // The continuation names the record's full id, so it stays on the
            // assessment block, like a window row's detail command, not in `next`.
            // An inherited event or completion came from the history window.
            let next = vec![format!(
                "engram work show {work_ref} {}",
                if row.member.is_some() {
                    "--history"
                } else {
                    "--notes"
                }
            )];
            let assessment_value = assessment
                .as_ref()
                .map(|page| super::verification_assessment::value(page, continuation.as_deref()));
            // A continuation page carries the assessment alone; the note itself
            // was on the first page. The service refuses `after` on any other
            // note, so a continued note always has its assessment.
            if let (Some(_), Some(page)) = (&input.after, &assessment) {
                let mut lines = vec![format!("note {}: assessment continued", row.locator)];
                super::verification_assessment::append_lines(
                    &mut lines,
                    page,
                    continuation.as_deref(),
                );
                return Ok(Receipt::assemble(
                    lines,
                    Guidance {
                        reminders: Vec::new(),
                        next,
                    },
                    json!({ "work_ref": work_ref, "locator": row.locator, "assessment": assessment_value }),
                    false,
                ));
            }
            let mut value = row_value(&row, false, &work_ref, self.service.display_identity());
            // An inherited event or completion also carries the complete
            // member, every field as its record stores it, framed as data,
            // except its actor, which reads as the row's display label like
            // every other attribution in these windows.
            let displayed_member = row.member.as_ref().map(|member| {
                let mut member = member.clone();
                if let Some(actor) = member.get_mut("actor") {
                    *actor = value["by"].clone();
                }
                member
            });
            let member = displayed_member
                .as_ref()
                .map(|member| -> Result<_, VerbError> {
                    let compact = serde_json::to_string(member)
                        .map_err(|error| VerbError::at(StoreError::Json(error), &work_ref))?;
                    let pretty = serde_json::to_string_pretty(member)
                        .map_err(|error| VerbError::at(StoreError::Json(error), &work_ref))?;
                    Ok((compact.len(), pretty))
                })
                .transpose()?;
            let mut lines = vec![match &member {
                Some((bytes, _)) => format!(
                    "history {}: complete inherited {} member, {bytes} UTF-8 bytes",
                    row.locator,
                    super::terminal_safe_line(&row.kind)
                ),
                None => format!(
                    "note {}: {} UTF-8 body bytes (complete detail)",
                    row.locator, row.body_bytes
                ),
            }];
            append_row_lines(&mut lines, &value, row.family);
            if let (Some(record), Some((bytes, pretty))) = (&displayed_member, &member) {
                lines.push("    member:".into());
                lines.push(
                    super::terminal_data_block(pretty)
                        .lines()
                        .map(|line| format!("      {line}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
                value["member"] = record.clone();
                value["member_bytes"] = json!(bytes);
            }
            if let (Some(page), Some(assessment)) = (&assessment, assessment_value) {
                super::verification_assessment::append_lines(
                    &mut lines,
                    page,
                    continuation.as_deref(),
                );
                value["assessment"] = assessment;
            }
            return Ok(Receipt::assemble(
                lines,
                Guidance {
                    reminders: Vec::new(),
                    next,
                },
                json!({ "work_ref": work_ref, "note": value }),
                false,
            ));
        }
        if !input.notes && !input.history {
            return self.show(work_ref, now);
        }
        let kind = if input.gates {
            WorkRecordKind::NotesWithGates
        } else if input.notes {
            WorkRecordKind::Notes
        } else {
            WorkRecordKind::History
        };
        // Quote an unresolved caller ref for refusal guidance; success uses the
        // canonical short ref from the same snapshot as the window.
        let command = format!(
            "engram work show {} {}",
            safe_reference_argument(work_ref),
            window_flags(kind)
        );
        let (view, page) = self
            .service
            .work_record_window(work_ref, kind, input.after.as_deref(), now)
            .map_err(|error| VerbError::for_listing(error, &command))?;
        fit_window(
            view,
            &page,
            |view| {
                if input.after.is_some() {
                    Ok(continuation_header(view))
                } else {
                    self.render_show(view, now)
                }
            },
            self.service.display_identity(),
            MAX_AGENT_WORK_RESPONSE_BYTES,
        )
        .map_err(|error| VerbError::for_listing(error.error, &command))
    }
}

/// A continuation is a record read, not another full item inspection. Its
/// context is the same bound item and cut; the explicit read restores detail.
pub(super) fn continuation_header(view: &WorkFocusView) -> Receipt {
    let work = &view.status.work;
    let detail = super::mutation::full_detail(&work.short_ref, "");
    Receipt::assemble(
        vec![
            format!("{} \"{}\"", work.short_ref, super::short(&work.title)),
            format!("full detail: {}", super::terminal_command(&detail)),
        ],
        Guidance {
            reminders: Vec::new(),
            next: vec![detail.clone()],
        },
        json!({"work": {"short_ref": work.short_ref, "title": work.title}, "full_detail": detail}),
        false,
    )
}

fn full_contract_receipt(contract: &WorkAuthoredContract) -> Receipt {
    let mut lines = vec![format!(
        "{} revision {} (complete contract)",
        contract.short_ref, contract.revision
    )];
    append_labeled_block(&mut lines, "title", &contract.title);
    append_labeled_block(&mut lines, "outcome", &contract.outcome);
    lines.push("acceptance:".into());
    for (position, criterion) in contract.acceptance.iter().enumerate() {
        let safe = super::terminal_data_block(criterion);
        let bound = contract
            .acceptance_bindings
            .iter()
            .find(|binding| binding.criterion == position + 1)
            .map(|binding| super::show::binding_note(&binding.requirement));
        let last = safe.split('\n').count().saturating_sub(1);
        for (index, line) in safe.split('\n').enumerate() {
            let prefix = if index == 0 {
                format!("  {}. ", position + 1)
            } else {
                "    ".into()
            };
            let suffix = if index == last {
                bound.as_deref().unwrap_or("")
            } else {
                ""
            };
            lines.push(format!("{prefix}{line}{suffix}"));
        }
    }
    let evaluation = contract.evaluation.as_ref().map(|evaluation| {
        let passed = evaluation
            .verdicts
            .iter()
            .filter(|verdict| verdict.verdict == "pass")
            .count();
        let freshness = match evaluation.stale {
            None => "fresh".to_owned(),
            Some(reason) => format!("stale: {reason}"),
        };
        lines.push(format!(
            "evaluation: {} {} — {passed}/{} pass, {freshness}; evaluated revision {} at run position {} (complete)",
            evaluation.mode,
            &evaluation.hash[..12],
            evaluation.verdicts.len(),
            evaluation.work_revision,
            evaluation.evaluated_cut
        ));
        if let Some(superseded) = &evaluation.supersedes {
            lines.push(format!("  supersedes the carried failure {superseded}"));
        }
        if let Some(observation) = &evaluation.stale_observation {
            lines.push(format!("  {}", observation.line(super::terminal_safe_line)));
        }
        let carried = evaluation
            .carried_failure
            .as_ref()
            .map(super::show::show_carried_failure);
        if let (Some(carried), Some(full)) = (&carried, &evaluation.carried_failure) {
            lines.push(super::show::carried_failure_line(carried));
            // The before side of the revision: the criteria the carried
            // failure judged, with its non-passing verdicts. The newest
            // evaluation below may have judged other criteria since.
            lines.push(format!("  criteria evaluation {} judged:", full.evaluation));
            for (index, criterion) in full.judged_criteria.iter().enumerate() {
                let safe = super::terminal_data_block(criterion);
                for (line_index, line) in safe.split('\n').enumerate() {
                    let prefix = if line_index == 0 {
                        format!("    {}. ", index + 1)
                    } else {
                        "       ".into()
                    };
                    lines.push(format!("{prefix}{line}"));
                }
                if let Some(blocking) = full
                    .blocking
                    .iter()
                    .find(|blocking| blocking.criterion == index + 1)
                {
                    let safe = super::terminal_data_block(&blocking.rationale);
                    for (line_index, line) in safe.split('\n').enumerate() {
                        let prefix = if line_index == 0 {
                            format!("       {}: ", blocking.verdict.word())
                        } else {
                            "       ".into()
                        };
                        lines.push(format!("{prefix}{line}"));
                    }
                }
            }
            // A newer failing evaluation that named it judged other criteria:
            // the middle of the three contracts the next evaluator compares.
            if full.evaluation.as_str() != evaluation.hash {
                lines.push(format!(
                    "  criteria the newest evaluation {} judged:",
                    evaluation.hash
                ));
                for verdict in &evaluation.verdicts {
                    let safe = super::terminal_data_block(&verdict.criterion);
                    for (line_index, line) in safe.split('\n').enumerate() {
                        let prefix = if line_index == 0 {
                            format!("    {}. ", verdict.position)
                        } else {
                            "       ".into()
                        };
                        lines.push(format!("{prefix}{line}"));
                    }
                }
            }
            // A binding-only revision leaves the text unchanged, so the
            // bindings it judged are the before side of the comparison, and
            // the newest failing evaluation's are the middle one.
            lines.push(format!(
                "  judged bindings: {}",
                bindings_summary(&full.judged_bindings)
            ));
            if let Some(newest) = &full.newest_judged_bindings {
                lines.push(format!(
                    "  bindings the newest evaluation judged: {}",
                    bindings_summary(newest)
                ));
            }
        }
        if let Some(cause) = &evaluation.reroll {
            lines.push(super::show::reroll_line(cause));
        }
        for verdict in &evaluation.verdicts {
            lines.push(format!(
                "  {}. {} ({})",
                verdict.position, verdict.verdict, verdict.basis
            ));
            let safe = super::terminal_data_block(&verdict.rationale);
            for line in safe.split('\n') {
                lines.push(format!("     {line}"));
            }
            if !verdict.citations.is_empty() {
                lines.push(format!("     citations: {}", verdict.citations.join(", ")));
            }
        }
        let mut value = json!({
            "hash": evaluation.hash,
            "mode": evaluation.mode,
            "work_revision": evaluation.work_revision,
            "evaluated_cut": evaluation.evaluated_cut,
            "stale": evaluation.stale,
            "passed": passed,
            "verdicts": evaluation
                .verdicts
                .iter()
                .map(|verdict| json!({
                    "position": verdict.position,
                    "criterion": verdict.criterion,
                    "verdict": verdict.verdict,
                    "basis": verdict.basis,
                    "rationale": verdict.rationale,
                    "citations": verdict.citations,
                }))
                .collect::<Vec<_>>(),
        });
        if let Some(object) = value.as_object_mut() {
            if let Some(superseded) = &evaluation.supersedes {
                object.insert("supersedes".into(), json!(superseded));
            }
            if let Some(observation) = &evaluation.stale_observation {
                object.insert("stale_observation".into(), json!(observation));
            }
            if let Some(cause) = &evaluation.reroll {
                object.insert(
                    "reroll".into(),
                    json!(crate::domain::AcceptanceEvaluationAdmissionCause::Reroll(
                        cause.clone()
                    )),
                );
            }
            if let (Some(carried), Some(full)) = (&carried, &evaluation.carried_failure) {
                let mut carried = json!(carried);
                if let Some(fields) = carried.as_object_mut() {
                    fields.insert("judged_criteria".into(), json!(full.judged_criteria));
                    fields.insert("blocking".into(), json!(full.blocking));
                    fields.insert("judged_bindings".into(), json!(full.judged_bindings));
                    if let Some(newest) = &full.newest_judged_bindings {
                        fields.insert("newest_judged_bindings".into(), json!(newest));
                    }
                }
                object.insert("carried_failure".into(), carried);
            }
        }
        value
    });
    let mut work = json!({
        "short_ref": contract.short_ref,
        "revision": contract.revision,
        "title": contract.title,
        "outcome": contract.outcome,
        "acceptance": contract.acceptance,
    });
    if let Some(object) = work.as_object_mut() {
        // The after side of a carried failure's binding comparison, beside
        // the criteria it binds.
        if !contract.acceptance_bindings.is_empty() {
            object.insert(
                "acceptance_bindings".into(),
                json!(contract.acceptance_bindings),
            );
        }
        if let Some(evaluation) = evaluation {
            object.insert("evaluation".into(), evaluation);
        }
    }
    let mut value = json!({ "work": work });
    if contract.acceptance_placeholder.is_some()
        && let Some(object) = value.as_object_mut()
    {
        object.insert("acceptance_placeholder".into(), json!(true));
    }
    Receipt::assemble(
        lines,
        Guidance {
            reminders: contract
                .acceptance_placeholder
                .as_deref()
                .map(super::handlers::placeholder_acceptance_reminder)
                .into_iter()
                .collect(),
            next: vec![format!(
                "engram work show {}",
                super::listing::shell_quote(&contract.short_ref)
            )],
        },
        value,
        false,
    )
}

/// `none`, or each binding as `criterion N [requires host KIND verification]`.
fn bindings_summary(bindings: &[crate::domain::AcceptanceBinding]) -> String {
    if bindings.is_empty() {
        return "none".to_owned();
    }
    bindings
        .iter()
        .map(|binding| {
            format!(
                "criterion {} {}",
                binding.criterion,
                super::show::binding_note(&binding.requirement).trim_start()
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn append_labeled_block(lines: &mut Vec<String>, label: &str, text: &str) {
    let safe = super::terminal_data_block(text);
    let mut parts = safe.split('\n');
    if let Some(first) = parts.next() {
        lines.push(format!("{label}: {first}"));
    }
    for line in parts {
        lines.push(format!("  {line}"));
    }
}

pub(super) fn safe_reference_argument(work_ref: &str) -> String {
    if work_ref
        .chars()
        .any(crate::domain::is_unsafe_rendered_text_char)
    {
        "<ref>".into()
    } else {
        super::listing::shell_quote(work_ref)
    }
}

fn window_flags(kind: WorkRecordKind) -> &'static str {
    match kind {
        WorkRecordKind::Notes => "--notes",
        WorkRecordKind::NotesWithGates => "--notes --gates",
        WorkRecordKind::History => "--history",
    }
}

pub(super) fn fit_window(
    mut view: WorkFocusView,
    page: &WorkRecordWindow,
    render: impl Fn(&WorkFocusView) -> Result<Receipt, VerbError>,
    identity: DisplayIdentity<'_>,
    budget: usize,
) -> Result<Receipt, VerbError> {
    let work_ref = view.status.work.short_ref.clone();
    if page.kind.is_notes() {
        view.evidence_items.clear();
        view.latest_evidence_item = None;
        view.omissions
            .retain(|entry| entry.reason != WorkSectionOmissionReason::EvidenceCountLimit);
    } else {
        view.history.items.clear();
        view.history.total = 0;
        view.history.omitted = 0;
        view.restored_history.items.clear();
        view.restored_history.total = 0;
        view.restored_history.omitted = 0;
    }
    let first = usize::from(!page.rows.is_empty());
    let with_first = |view: &WorkFocusView, placeholder| {
        append_window(
            render(view)?,
            page,
            first,
            placeholder,
            &work_ref,
            identity,
            budget,
        )
    };
    // The first remaining row must be represented, never skipped. Try its
    // complete body first; if even essential metadata cannot coexist with it,
    // use an explicit detail placeholder that still advances by one member.
    let (base, placeholder) =
        match super::show::fit_show_receipt(view.clone(), |view| with_first(view, false), budget) {
            Ok(receipt) => (receipt, false),
            Err(error)
                if first > 0
                    && page.kind.is_notes()
                    && matches!(error.error, StoreError::InvalidWorkProjection(_)) =>
            {
                (
                    super::show::fit_show_receipt(view, |view| with_first(view, true), budget)?,
                    true,
                )
            }
            Err(error) => return Err(error),
        };
    let mut best = base.clone();
    let (mut lower, mut upper) = (first, page.rows.len());
    while lower < upper {
        let visible = lower + (upper - lower).div_ceil(2);
        let candidate = append_window(
            base.clone(),
            page,
            visible,
            placeholder,
            &work_ref,
            identity,
            budget,
        )?;
        if super::receipts::agent_receipt_fits(&candidate, budget)? {
            best = candidate;
            lower = visible;
        } else {
            upper = visible - 1;
        }
    }
    Ok(best)
}

fn append_window(
    mut receipt: Receipt,
    page: &WorkRecordWindow,
    visible: usize,
    placeholder: bool,
    work_ref: &str,
    identity: DisplayIdentity<'_>,
    budget: usize,
) -> Result<Receipt, VerbError> {
    let word = page.kind.word();
    #[cfg(test)]
    super::receipts::SHOW_NOTE_FIT_PROBES.with(|count| count.set(count.get() + 1));
    let marker = format!("{word}: window ");
    if let Some(start) = receipt
        .lines
        .iter()
        .position(|line| line.starts_with(&marker))
    {
        receipt.lines.truncate(start);
    }
    if page.kind.is_notes() {
        receipt.lines.retain(|line| !line.starts_with("notes:"));
    }
    let command = format!("engram work show {work_ref} {}", window_flags(page.kind));
    receipt
        .next
        .retain(|next| next != &command && !next.starts_with(&format!("{command} --after ")));
    let after = page.continuation(visible)?;
    if let Some(after) = &after {
        receipt.next.insert(0, format!("{command} --after {after}"));
    }
    // Full first pages keep a visible parent slot after window navigation and
    // primary recovery. Compact continuation headers carry no parent context.
    if let Some(parent_ref) = receipt.value.get("parent_ref").and_then(Value::as_str) {
        let parent_command = format!("engram work show {parent_ref}");
        if let Some(index) = receipt.next.iter().position(|next| next == &parent_command)
            && index >= super::MAX_TEXT_NEXT_COMMANDS
        {
            let parent_command = receipt.next.remove(index);
            receipt
                .next
                .insert(super::MAX_TEXT_NEXT_COMMANDS - 1, parent_command);
        }
    }
    let older = page.total - page.newer - visible;
    let omitted = page.total - visible;
    let mut window = json!({ "selection": "newest_first", "order": "oldest_first", "newer": page.newer,
        "older": older, "shown": visible, "total": page.total, "after": after,
        "byte_budget": budget, "read_cut": page.read_cut() });
    let rows = page.rows[..visible]
        .iter()
        .enumerate()
        .rev()
        .map(|(index, row)| row_value(row, placeholder && index == 0, work_ref, identity))
        .collect::<Vec<_>>();
    let families = [
        WorkRecordFamily::Notes,
        WorkRecordFamily::Observations,
        WorkRecordFamily::Gates,
        WorkRecordFamily::History,
    ]
    .into_iter()
    .filter(|family| !page.kind.is_notes() || *family != WorkRecordFamily::History)
    .map(|family| {
        let total = page.families.get(&family).copied().unwrap_or(0);
        let shown = page.rows[..visible]
            .iter()
            .filter(|row| row.family == family)
            .count();
        (
            family,
            json!({ "total": total, "shown": shown, "omitted": total - shown }),
        )
    })
    .collect::<std::collections::BTreeMap<_, _>>();
    window["families"] = json!(families);
    if page.kind.is_notes() {
        window["includes_gates"] = json!(page.kind == WorkRecordKind::NotesWithGates);
        if page.kind == WorkRecordKind::Notes
            && page
                .families
                .get(&WorkRecordFamily::Gates)
                .copied()
                .unwrap_or(0)
                > 0
        {
            let gates = format!("engram work show {work_ref} --notes --gates");
            if !receipt.next.contains(&gates) {
                receipt.next.push(gates);
            }
        }
        receipt.value["notes"] = json!(rows);
        receipt.value["notes_omitted"] = json!(omitted);
        receipt.value["notes_window"] = window;
    } else {
        if let Some(fields) = receipt.value.as_object_mut() {
            fields.remove("restored_history");
        }
        receipt.value["history"] =
            json!({ "items": rows, "total": page.total, "omitted": omitted, "window": window });
    }
    receipt.value["next"] = json!(receipt.next);
    receipt.lines.push(format!("{word}: window {visible} of {}; {omitted} omitted ({older} older, {} newer); oldest to newest within window", page.total, page.newer));
    receipt.lines.push(format!(
        "  byte budget: {budget}; read cut: project position {}, observed at {}, valid until ms {}",
        page.read_cut().project_position,
        page.read_cut().observed_at.to_rfc3339(),
        page.read_cut()
            .valid_until_ms
            .map_or_else(|| "none".into(), |until| until.to_string())
    ));
    if page.kind.is_notes() {
        let families = &receipt.value["notes_window"]["families"];
        receipt.lines.push(format!(
            "  families: {} notes, {} observations; gate evidence: {} ({} shown, {} omitted)",
            families["notes"]["total"],
            families["observations"]["total"],
            families["gates"]["total"],
            families["gates"]["shown"],
            families["gates"]["omitted"]
        ));
    } else {
        for (family, counts) in families {
            receipt.lines.push(format!(
                "  family {}: {} ({} shown, {} omitted)",
                match family {
                    WorkRecordFamily::Notes => "notes",
                    WorkRecordFamily::Observations => "observations",
                    WorkRecordFamily::Gates => "gates",
                    WorkRecordFamily::History => "history",
                },
                counts["total"],
                counts["shown"],
                counts["omitted"]
            ));
        }
    }
    for (row, source) in rows.iter().zip(page.rows[..visible].iter().rev()) {
        append_row_lines(&mut receipt.lines, row, source.family);
    }
    Ok(receipt)
}

fn row_value(
    row: &WorkRecordRow,
    placeholder: bool,
    work_ref: &str,
    identity: DisplayIdentity<'_>,
) -> Value {
    let omitted = row.body_omitted || placeholder;
    let mut value = json!({ "locator": row.locator, "kind": row.kind, "family": row.family, "body_bytes": row.body_bytes,
        "by": super::actor_label(&identity.author(&row.actor.actor_id, row.actor.session_id.as_ref()), row.actor.attribution_context()),
        "created_at": row.recorded_at,
        "non_holder": row.actor.provenance_chain.iter().any(crate::domain::is_non_holder_note_marker) });
    if row.family != WorkRecordFamily::History {
        if let Some(role) = crate::domain::status_note_role(&row.actor) {
            value["status_owner"] = json!(role == crate::domain::StatusNoteRole::Owner);
        }
        if let Some(position) = row.project_position() {
            value["feed_position"] = json!(position);
        }
    }
    if omitted {
        value["body_omitted"] = json!(true);
    } else {
        value["summary"] = json!(row.summary);
        value["refs"] = json!(row.refs);
        if row.summary_truncated {
            value["summary_truncated"] = json!(true);
        }
    }
    if omitted || row.summary_truncated {
        value["detail"] = json!(format!(
            "engram work show {work_ref} --note {}",
            row.locator
        ));
    }
    if let Some(gate) = &row.gate {
        value["gate"] = json!({ "name": gate.name, "passed": gate.passed });
    }
    if let Some(facts) = &row.verification {
        value["verification"] = verification_value(facts);
    }
    value
}

/// The typed facts of a native verification record, beside its summary: the
/// summary is the host's attributed prose and never decides the result.
fn verification_value(facts: &crate::storage::VerificationFacts) -> Value {
    let word = |value: Value| value.as_str().unwrap_or_default().to_owned();
    let mut value = json!({
        "result": word(json!(facts.result)),
        "check_kind": word(json!(facts.check_kind)),
        "source_revision": facts.source_revision,
        "producer_outcome": word(json!(facts.producer_outcome)),
    });
    // Why an indeterminate verification does not count, in plain words.
    if facts.result == crate::domain::VerificationResult::Indeterminate {
        value["meaning"] = json!(format!(
            "the host recorded the outcome as {}, so this record cannot satisfy a passing-check requirement",
            value["producer_outcome"].as_str().unwrap_or_default()
        ));
    }
    value
}

fn append_row_lines(lines: &mut Vec<String>, row: &Value, family: WorkRecordFamily) {
    let marker = match family {
        WorkRecordFamily::Notes => "note",
        WorkRecordFamily::Observations => "observation",
        WorkRecordFamily::Gates => "gate",
        WorkRecordFamily::History => "history",
    };
    lines.push(format!(
        "  - {} [{marker}] {} by {} at {} ({} UTF-8 body bytes){}:",
        row["locator"].as_str().unwrap_or_default(),
        super::terminal_safe_line(row["kind"].as_str().unwrap_or_default()),
        super::terminal_safe_line(row["by"].as_str().unwrap_or("another actor")),
        row["created_at"].as_str().unwrap_or_default(),
        row["body_bytes"],
        if row["status_owner"] == false {
            " (peer status observation, no commitment)"
        } else if row["non_holder"] == true {
            " (non-holder)"
        } else {
            ""
        }
    ));
    if let Some(facts) = row["verification"].as_object() {
        let field = |key: &str| {
            super::terminal_safe_line(facts.get(key).and_then(Value::as_str).unwrap_or_default())
        };
        lines.push(format!(
            "    verification: {} {} on source revision {}; producer outcome {}",
            field("result"),
            field("check_kind"),
            field("source_revision"),
            field("producer_outcome")
        ));
        if facts.contains_key("meaning") {
            lines.push(format!("    {}", field("meaning")));
        }
    }
    if let Some(body) = row["summary"].as_str() {
        lines.push(
            super::terminal_note_block(body)
                .lines()
                .map(|line| format!("    {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        if row["summary_truncated"] == true {
            lines.push(format!(
                "    summary shortened; {}",
                super::terminal_command(row["detail"].as_str().unwrap_or_default())
            ));
        }
    } else {
        lines.push(format!(
            "    complete note does not fit this window; {}",
            super::terminal_command(row["detail"].as_str().unwrap_or_default())
        ));
    }
    for reference in row["refs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        lines.push(format!(
            "    ref: {}",
            super::terminal_data_block(reference).replace('\n', "\n         ")
        ));
    }
}
