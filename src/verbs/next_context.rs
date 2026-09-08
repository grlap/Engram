//! Compact injection only. Rebuild references from retained rows on every fit
//! pass; neither the service view nor the exact staged delivery is rewritten.

use std::collections::HashMap;

use super::receipts::{CompactNextReceipt, compact_row_line};
use super::{Value, json};
use crate::work_service::{WorkCurrentStatus, WorkDiscoverySummary, WorkDiscoveryView};

pub(super) const CLIPPED_STATUS_REMINDER: &str = "read full status via its --note locator before acting on approval or STOP conditions; a clipped prefix grants no permission";

#[derive(Clone)]
pub(super) struct CompactChange {
    pub(super) line: String,
    /// Rendered kind and actor retained even when the body is a reference.
    pub(super) attribution: String,
    /// Work reference and verified immutable capture identity, never body text.
    pub(super) note: Option<(String, String)>,
}

impl From<String> for CompactChange {
    fn from(line: String) -> Self {
        Self {
            line,
            attribution: String::new(),
            note: None,
        }
    }
}

impl From<&str> for CompactChange {
    fn from(line: &str) -> Self {
        line.to_owned().into()
    }
}

pub(super) struct Row {
    pub(super) value: Value,
    pub(super) lines: Vec<String>,
}

#[derive(Default)]
pub(super) struct Context {
    pub(super) held: Vec<Row>,
    pub(super) assigned: Vec<Row>,
    pub(super) participated: Vec<Row>,
    pub(super) changes: Vec<String>,
    clipped: bool,
}

#[derive(Default)]
struct Seen {
    owners: HashMap<String, String>,
    captures: HashMap<String, Vec<(String, String)>>,
}

impl Seen {
    fn remember_status(&mut self, reference: &str, status: Option<&WorkCurrentStatus>) {
        if let Some(status) = status {
            self.remember(reference, &status.locator);
        }
    }

    fn remember(&mut self, reference: &str, identity: &str) {
        if let Some(owner) = self.owners.get(reference) {
            self.remember_at(reference, identity, owner.clone());
        }
    }

    fn remember_at(&mut self, reference: &str, identity: &str, owner: String) {
        if identity.is_empty() {
            return;
        }
        self.captures
            .entry(reference.into())
            .or_default()
            .push((identity.into(), owner));
    }

    fn repeats(&self, reference: &str, identity: Option<&str>) -> bool {
        identity.is_some_and(|identity| self.owner_of(reference, identity).is_some())
    }

    fn owner_of(&self, reference: &str, identity: &str) -> Option<&str> {
        self.captures
            .get(reference)?
            .iter()
            .find(|(capture, _)| capture == identity)
            .map(|(_, owner)| owner.as_str())
    }
}

impl Context {
    pub(super) fn new(compact: &CompactNextReceipt) -> Self {
        let mut context = Self::default();
        let mut seen = Seen::default();
        context.add_held(compact, &mut seen);
        context.add_discovery(compact, &mut seen);
        for (index, change) in compact.changes.iter().enumerate() {
            let line = if let Some((reference, identity)) = &change.note {
                if let Some(owner) = seen.owner_of(reference, identity) {
                    format!("{} — see {owner}", change.attribution)
                } else {
                    seen.remember_at(
                        reference,
                        identity,
                        format!("changes entry {} ({reference})", index + 1),
                    );
                    change.line.clone()
                }
            } else {
                change.line.clone()
            };
            context.changes.push(super::short(&line));
        }
        context
    }

    fn add_held(&mut self, compact: &CompactNextReceipt, seen: &mut Seen) {
        for held in &compact.held {
            let reference = &held.work_ref;
            seen.owners
                .insert(reference.clone(), format!("held {reference}"));
            seen.remember_status(reference, held.current_status.as_ref());
            seen.remember_status(reference, held.status_observation.as_ref());
            self.clipped |= clipped(
                held.current_status.as_ref(),
                held.status_observation.as_ref(),
            );
            let mut row = Row {
                value: json!(held),
                lines: vec![format!("  {}", compact_row_line(held))],
            };
            if let Some(discovery) = compact
                .discovery
                .assigned
                .iter()
                .chain(&compact.discovery.participated)
                .find(|row| row.work_ref == *reference)
                && let Some(note) = discovery
                    .note
                    .as_ref()
                    .filter(|_| !seen.repeats(reference, discovery.note_identity.as_deref()))
            {
                if let Value::Object(fields) = &mut row.value {
                    fields.insert("note".into(), json!(note));
                    if discovery.note_session_id.is_some() {
                        fields.insert("note_by".into(), json!("you"));
                    }
                }
                row.lines.push(format!(
                    "    note:{}",
                    super::receipts::discovery_note_text(discovery)
                ));
                append_note_navigation(&mut row, reference);
                if let Some(identity) = &discovery.note_identity {
                    seen.remember(reference, identity);
                }
            }
            self.held.push(row);
        }
    }

    fn add_discovery(&mut self, compact: &CompactNextReceipt, seen: &mut Seen) {
        for (name, source, target) in [
            ("assigned", &compact.discovery.assigned, &mut self.assigned),
            (
                "participated",
                &compact.discovery.participated,
                &mut self.participated,
            ),
        ] {
            for original in source {
                if let Some(owner) = seen.owners.get(&original.work_ref)
                    && captures_owned_by(original, seen, owner)
                {
                    target.push(Row {
                        value: json!({"ref": original.work_ref, "context_ref": owner}),
                        lines: vec![format!("  {} — see {owner}", original.work_ref)],
                    });
                    continue;
                }
                let mut row = original.clone();
                seen.owners
                    .insert(row.work_ref.clone(), format!("{name} {}", row.work_ref));
                seen.remember_status(&row.work_ref, row.current_status.as_ref());
                seen.remember_status(&row.work_ref, row.status_observation.as_ref());
                self.clipped |=
                    clipped(row.current_status.as_ref(), row.status_observation.as_ref());
                if row
                    .note
                    .as_ref()
                    .is_some_and(|_| seen.repeats(&row.work_ref, row.note_identity.as_deref()))
                {
                    row.note = None;
                    row.note_session_id = None;
                }
                if row.note.is_some()
                    && let Some(identity) = &row.note_identity
                {
                    seen.remember(&row.work_ref, identity);
                }
                target.push(discovery_row(&row));
            }
        }
    }

    pub(super) fn append_discovery_lines(
        &self,
        lines: &mut Vec<String>,
        discovery: &WorkDiscoveryView,
    ) {
        for (name, rows, omitted) in [
            ("assigned", &self.assigned, discovery.assigned_omitted),
            (
                "participated",
                &self.participated,
                discovery.participated_omitted,
            ),
        ] {
            if rows.is_empty() && omitted == 0 {
                continue;
            }
            lines.push(format!("{name} ({} shown):", rows.len()));
            for row in rows {
                lines.extend(row.lines.clone());
            }
            if omitted > 0 {
                lines.push(format!("  ({omitted} more {name} not shown)"));
            }
        }
    }
}

fn discovery_row(row: &WorkDiscoverySummary) -> Row {
    let mut value = json!(row);
    if let Value::Object(fields) = &mut value {
        fields.remove("note_session_id");
        if row.note_session_id.is_some() {
            fields.insert("note_by".into(), json!("you"));
        }
    }
    let mut lines = Vec::new();
    super::receipts::append_discovery_row(&mut lines, row);
    let mut projected = Row { value, lines };
    if row.note.is_some() {
        append_note_navigation(&mut projected, &row.work_ref);
    }
    projected
}

// Repeated work metadata may point at its primary, but every carried capture
// must also be proven present there. Unknown identity keeps its own body.
fn captures_owned_by(row: &WorkDiscoverySummary, seen: &Seen, owner: &str) -> bool {
    let status_owned = row
        .current_status
        .iter()
        .chain(&row.status_observation)
        .all(|status| seen.owner_of(&row.work_ref, &status.locator) == Some(owner));
    status_owned
        && (row.note.is_none()
            || row
                .note_identity
                .as_deref()
                .is_some_and(|identity| seen.owner_of(&row.work_ref, identity) == Some(owner)))
}

fn append_note_navigation(row: &mut Row, reference: &str) {
    let command = format!("engram work show {reference} --notes");
    if let Value::Object(fields) = &mut row.value {
        fields.insert("note_detail".into(), json!(command));
    }
    row.lines.push(format!("    note detail: {command}"));
}

fn clipped(current: Option<&WorkCurrentStatus>, peer: Option<&WorkCurrentStatus>) -> bool {
    current
        .into_iter()
        .chain(peer)
        .any(|status| !status.complete)
}

pub(super) fn refresh_guidance(compact: &mut CompactNextReceipt) {
    let clipped = Context::new(compact).clipped;
    compact
        .guidance
        .reminders
        .retain(|reminder| reminder != CLIPPED_STATUS_REMINDER);
    if clipped {
        compact
            .guidance
            .reminders
            .insert(0, CLIPPED_STATUS_REMINDER.into());
        if compact.guidance.reminders.len() > super::MAX_COMPACT_REMINDER_ITEMS {
            let omitted = compact.guidance.reminders.len() - super::MAX_COMPACT_REMINDER_ITEMS;
            compact
                .guidance
                .reminders
                .truncate(super::MAX_COMPACT_REMINDER_ITEMS);
            if let Some(entry) = compact.omissions.iter_mut().find(|entry| {
                entry.section == "reminders"
                    && entry.reason == super::WorkSectionOmissionReason::CountLimit
            }) {
                entry.omitted_count += omitted;
            } else {
                compact
                    .omissions
                    .push(super::receipts::CompactSectionOmission {
                        section: "reminders".into(),
                        reason: super::WorkSectionOmissionReason::CountLimit,
                        omitted_count: omitted,
                    });
            }
        }
    }
}
