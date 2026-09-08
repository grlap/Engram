//! Full note/history windows share one final-representation fitting path.
//! Detail is an explicit unbounded-body read, never an implicit window escape.

use super::{
    AgentVerbs, DateTime, Deserialize, Guidance, MAX_AGENT_WORK_RESPONSE_BYTES, Receipt, Serialize,
    StoreError, Utc, Value, VerbError, WorkFocusView, WorkSectionOmissionReason, json,
};
use crate::storage::{WorkRecordFamily, WorkRecordKind};
use crate::work_service::identity::DisplayIdentity;
use crate::work_service::{WorkRecordRow, WorkRecordWindow};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
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
        if (input.notes && input.history)
            || (input.gates && !input.notes)
            || (input.note.is_some() && (input.notes || input.history || input.after.is_some()))
            || (input.after.is_some() && !input.notes && !input.history)
        {
            return Err(VerbError::at(
                StoreError::InvalidWork(
                    "choose --notes [--gates] or --history with optional --after, or --note LOCATOR alone"
                        .into(),
                ),
                work_ref,
            ));
        }
        if let Some(locator) = &input.note {
            let (work_ref, row) = self
                .service
                .work_note_detail(work_ref, locator, now)
                .map_err(|error| VerbError::at(error, work_ref))?;
            let value = row_value(&row, false, &work_ref, self.service.display_identity());
            let mut lines = vec![format!(
                "note {}: {} UTF-8 body bytes (complete detail)",
                row.locator, row.body_bytes
            )];
            append_row_lines(&mut lines, &value);
            return Ok(Receipt::assemble(
                lines,
                Guidance {
                    reminders: Vec::new(),
                    next: vec![format!("engram work show {work_ref} --notes")],
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
        if candidate.text().len() < budget
            && serde_json::to_vec_pretty(&candidate.value)?.len() < budget
        {
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
    for row in &rows {
        append_row_lines(&mut receipt.lines, row);
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
    value
}

fn append_row_lines(lines: &mut Vec<String>, row: &Value) {
    lines.push(format!(
        "  - {} {} by {} at {} ({} UTF-8 body bytes){}:",
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
    if let Some(body) = row["summary"].as_str() {
        lines.push(
            super::terminal_data_block(body)
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
