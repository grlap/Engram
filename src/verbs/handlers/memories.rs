//! Project memory word handlers and listing receipts.

use super::{
    AgentVerbs, DateTime, ForgetInput, Guidance, MAX_AGENT_WORK_RESPONSE_BYTES, MemoriesInput,
    Receipt, RememberInput, StoreError, Utc, VerbError, parse_retiring_target, short,
    terminal_safe_actor_label, terminal_safe_multiline, wording,
};

impl AgentVerbs {
    /// `remember`: create one attributed project episode.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when authorization, key, size, redaction, or
    /// revision-basis or terminal lifecycle admission fails.
    pub fn remember(&self, input: RememberInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let retiring_target =
            parse_retiring_target(input.retires_with.as_deref(), input.clear_retires_with)?;
        let edit = super::super::memory_change::edit(input.append, input.section.as_deref())?;
        let receipt = self.service.revise_project_memory(
            input.text,
            input.key,
            input.revise,
            input.expected_revision,
            retiring_target,
            &edit,
            now,
        )?;
        let (changed, reads) = super::super::memory_change::lines(&receipt);
        let mut guidance = Guidance {
            reminders: Vec::new(),
            next: vec![format!("engram work memories {} --full", receipt.key)],
        };
        guidance.next.extend(reads);
        guidance.next.push("engram work memories".into());
        let replay = if receipt.duplicate { " (replayed)" } else { "" };
        let line = match receipt.replaced_revision {
            Some(previous) => format!(
                "revised project memory {}: revision {previous} → {}{replay}",
                receipt.key, receipt.revision
            ),
            None => format!(
                "remembered project memory {} (revision {}){replay}",
                receipt.key, receipt.revision
            ),
        };
        let lines = std::iter::once(line).chain(changed).collect();
        Ok(self.finish_mutation(Receipt::assemble(
            lines,
            guidance,
            serde_json::to_value(receipt)?,
            false,
        )))
    }

    /// `memories`: list/search compact rows or return one dedicated full body.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when the query/full-read shape is invalid or the
    /// core refuses authorization, key resolution, or projection validation.
    pub fn memories(
        &self,
        input: &MemoriesInput,
        now: DateTime<Utc>,
    ) -> Result<Receipt, VerbError> {
        self.with_claim_lapse(now, MAX_AGENT_WORK_RESPONSE_BYTES, |budget| {
            self.memories_within(input, now, budget)
        })
    }

    fn memories_within(
        &self,
        input: &MemoriesInput,
        now: DateTime<Utc>,
        budget: usize,
    ) -> Result<Receipt, VerbError> {
        if input.revision.is_some() && !input.full {
            return Err(StoreError::InvalidProjectMemory(
                wording::REVISION_NEEDS_FULL_REFUSAL.cli.into(),
            )
            .into());
        }
        crate::storage::validate_context_generation(input.context_generation.as_deref()).map_err(
            |error| VerbError::from(error).retrying(super::super::receipts::ReadRetry::Memories),
        )?;
        if input.full {
            if input.after.is_some() {
                return Err(StoreError::InvalidProjectMemory(
                    wording::FULL_WITH_AFTER_REFUSAL.cli.into(),
                )
                .into());
            }
            let key = input.query.as_deref().ok_or_else(|| {
                StoreError::InvalidProjectMemory(wording::FULL_NEEDS_KEY_REFUSAL.cli.into())
            })?;
            let envelope =
                self.service
                    .project_memory_full(key, input.revision, self.argument_names, now)?;
            let lines = envelope.terminal_lines();
            return Ok(Receipt::assemble(
                lines,
                Guidance {
                    reminders: envelope.reminders.clone(),
                    next: envelope.next.clone(),
                },
                serde_json::to_value(envelope)?,
                false,
            ));
        }
        let filtered = input
            .query
            .as_deref()
            .is_some_and(|query| !query.trim().is_empty());
        // Only the start of the unfiltered listing, carrying the generation a
        // peek printed, is the recovery read that peek asks for, and only once
        // it has been rendered. Every other form stays a read that records
        // nothing and reads no recorded position.
        let records = input.context_generation.is_some() && !filtered && input.after.is_none();
        // Every page of the unfiltered listing that carries a generation is
        // the compact recovery listing, and its continuation keeps the
        // generation; only its first page records.
        let recovery = input.context_generation.as_deref().filter(|_| !filtered);
        let (mut result, listing) = self.service.project_memories_at_cut(
            input.query.as_deref(),
            input.after.as_deref(),
            records,
            recovery.is_some(),
            now,
        )?;
        loop {
            let receipt = match recovery {
                Some(generation) => recovery_list_receipt(&result, generation)?,
                None => project_memory_list_receipt(&result, filtered)?,
            };
            if super::super::receipts::agent_receipt_fits(&receipt, budget)? {
                if let (Some(generation), Some(listing)) = (&input.context_generation, listing) {
                    self.service
                        .acknowledge_project_memory_listing(listing, generation, now);
                }
                return Ok(receipt);
            }
            if result.memories.pop().is_none() {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "project-memory list response cannot fit the {MAX_AGENT_WORK_RESPONSE_BYTES}-byte agent protocol limit"
                ))
                .into());
            }
            if filtered {
                result.omitted_count = result.omitted_count.saturating_add(1);
            }
            result.exhausted = false;
            if !filtered {
                if result.memories.is_empty() {
                    return Err(StoreError::InvalidWorkProjection(format!(
                        "one project-memory list row cannot fit the {MAX_AGENT_WORK_RESPONSE_BYTES}-byte agent protocol limit"
                    ))
                    .into());
                }
                result.next_after = result.memories.last().map(|row| row.key.clone());
            }
        }
    }

    /// `forget`: append an attributed terminal tombstone.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when authorization, key resolution, or terminal
    /// lifecycle validation fails.
    pub fn forget(&self, input: ForgetInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let receipt = self.service.forget_project_memory(input.key, now)?;
        let replay = if receipt.duplicate { " (replayed)" } else { "" };
        let lines = vec![format!("forgot project memory {}{replay}", receipt.key)];
        Ok(self.finish_mutation(Receipt::assemble(
            lines,
            Guidance {
                reminders: vec!["forget is a tombstone, not erasure".into()],
                next: vec!["engram work memories".into()],
            },
            serde_json::to_value(receipt)?,
            false,
        )))
    }
}

/// A recovery listing's compact row: what a session needs to decide which
/// entries to read in full. Author and time stay with the full read.
#[derive(serde::Serialize)]
struct RecoveryRow<'a> {
    key: &'a str,
    revision: u64,
    first_line: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    retiring_target: Option<&'a crate::domain::ProjectMemoryRetiringTarget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retiring_state: Option<&'a crate::domain::ProjectMemoryRetiringState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retiring_target_dropped: Option<&'a crate::domain::ProjectMemoryRetiringTargetDropped>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workaround: Option<bool>,
}

#[derive(serde::Serialize)]
struct RecoveryList<'a> {
    memories: Vec<RecoveryRow<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_after: Option<&'a str>,
    exhausted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    changes: Option<&'a crate::domain::ProjectMemoryKeyChanges>,
}

/// The longest preview a recovery row gives of a memory's first line.
const RECOVERY_FIRST_LINE_BYTES: usize = 120;

/// A sentence end this close to the start is more likely a list number or an
/// abbreviation ("1.", "e.g.") than the end of a useful preview.
const RECOVERY_SENTENCE_MIN_BYTES: usize = 24;

/// The first sentence of a memory's first line when it ends within the
/// preview budget, otherwise the line cut at a character boundary. A '.',
/// '?' or '!' ends a sentence only when whitespace or the line's end follows,
/// it lies past the first few bytes, and a digit or another '.' does not
/// precede it.
fn recovery_first_line(line: &str) -> String {
    let bytes = line.as_bytes();
    let sentence_end = line.char_indices().find_map(|(index, character)| {
        let ends = matches!(character, '.' | '?' | '!')
            && index + 1 >= RECOVERY_SENTENCE_MIN_BYTES
            && !bytes[index - 1].is_ascii_digit()
            && bytes[index - 1] != b'.'
            && bytes.get(index + 1).is_none_or(u8::is_ascii_whitespace);
        ends.then_some(index + 1)
    });
    match sentence_end {
        Some(end) if end <= RECOVERY_FIRST_LINE_BYTES => line[..end].to_owned(),
        _ if line.len() <= RECOVERY_FIRST_LINE_BYTES => line.to_owned(),
        _ => {
            let mut end = RECOVERY_FIRST_LINE_BYTES;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…", line[..end].trim_end())
        }
    }
}

/// One page of the compact recovery listing: on its first page, the keys
/// changed since the session's recorded position, then key, revision and a
/// first-line preview per memory, continued with the same generation.
fn recovery_list_receipt(
    result: &crate::domain::ProjectMemoryList,
    context_generation: &str,
) -> Result<Receipt, VerbError> {
    let mut lines = Vec::new();
    if let Some(changes) = result.changes.as_ref().filter(|changes| changes.count > 0) {
        lines.push(format!(
            "changed since this session last listed or was shown them: {}",
            super::super::memory_recovery::changes_summary(changes)
        ));
    }
    lines.push(format!("{} project memory item(s):", result.memories.len()));
    let mut rows = Vec::with_capacity(result.memories.len());
    for row in &result.memories {
        let first_line = recovery_first_line(&row.first_line);
        lines.push(format!(
            "  {} (r{}) — {}",
            row.key,
            row.revision,
            terminal_safe_multiline(&first_line)
        ));
        for line in crate::work_service::retiring_target_lines(
            row.retiring_target.as_ref(),
            row.retiring_state.as_ref(),
            row.retiring_target_dropped.as_ref(),
            true,
        ) {
            lines.push(format!("    {line}"));
        }
        rows.push(RecoveryRow {
            key: &row.key,
            revision: row.revision,
            first_line,
            retiring_target: row.retiring_target.as_ref(),
            retiring_state: row.retiring_state.as_ref(),
            retiring_target_dropped: row.retiring_target_dropped.as_ref(),
            workaround: row.workaround,
        });
    }
    if result.exhausted {
        lines.push("  (end of project memories)".into());
    }
    let mut guidance = Guidance::default();
    if let Some(after) = &result.next_after {
        guidance.next.push(format!(
            "engram work memories --context-generation {context_generation} --after {after}"
        ));
    }
    let value = RecoveryList {
        memories: rows,
        next_after: result.next_after.as_deref(),
        exhausted: result.exhausted,
        changes: result.changes.as_ref(),
    };
    Ok(Receipt::assemble(
        lines,
        guidance,
        serde_json::to_value(value)?,
        false,
    ))
}

fn project_memory_list_receipt(
    result: &crate::domain::ProjectMemoryList,
    filtered: bool,
) -> Result<Receipt, VerbError> {
    let mut lines = vec![format!("{} project memory item(s):", result.memories.len())];
    for row in &result.memories {
        lines.push(format!(
            "  {} (revision {}) — {} — by {} ({})",
            row.key,
            row.revision,
            short(&terminal_safe_multiline(&row.first_line)),
            terminal_safe_actor_label(&row.actor_id, row.actor_context.as_deref()),
            row.remembered_at.format("%Y-%m-%d %H:%M UTC")
        ));
        for line in crate::work_service::retiring_target_lines(
            row.retiring_target.as_ref(),
            row.retiring_state.as_ref(),
            row.retiring_target_dropped.as_ref(),
            true,
        ) {
            // Whole, not shortened: the display form of the target, with a
            // local item's short ref; the full read's reminder gives the
            // restore command by work id. The listing is byte-fitted.
            lines.push(format!("    {line}"));
        }
    }
    if result.exhausted {
        lines.push("  (end of project memories)".into());
    }
    let mut guidance = Guidance::default();
    if let Some(after) = &result.next_after {
        guidance
            .next
            .push(format!("engram work memories --after {after}"));
    }
    if filtered && result.omitted_count > 0 {
        guidance.reminders.push(format!(
            "{} more matches were omitted; refine the memory query",
            result.omitted_count
        ));
    }
    Ok(Receipt::assemble(
        lines,
        guidance,
        serde_json::to_value(result)?,
        false,
    ))
}
