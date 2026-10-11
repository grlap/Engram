//! Typed shared prose and conservative, whole-note disclosure exclusions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::{SharedActor, invalid, validate_shared_actor};
use crate::{ObjectId, storage::StoreError};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SharedNoteKind {
    Generic,
    Status,
}

/// Original identity is shared data, separate from locally minted history ids.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SharedNote {
    pub id: ObjectId,
    pub kind: SharedNoteKind,
    pub body: String,
    pub author: SharedActor,
    pub recorded_at: DateTime<Utc>,
    pub origin_machine: String,
}

/// Local disclosure accounting, never part of the deterministic shared files.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct NoteOmissions {
    pub counts: BTreeMap<String, usize>,
}

impl NoteOmissions {
    pub(crate) fn omit(&mut self, reason: &str) {
        *self.counts.entry(reason.into()).or_default() += 1;
    }
}

/// The operator supplies an alias, never a path or automatically detected host name.
///
/// # Errors
/// Refuses empty, oversized or non-alias values.
pub fn validate_machine_alias(value: &str) -> Result<(), StoreError> {
    if !(1..=64).contains(&value.len())
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(invalid(
            "origin-machine must be 1-64 ASCII letters, digits, '.', '_' or '-'",
        ));
    }
    Ok(())
}

pub(crate) fn restricted(labels: &[String]) -> bool {
    labels.iter().any(|label| {
        matches!(
            label.to_ascii_lowercase().as_str(),
            "restricted" | "secret" | "secret-ref"
        )
    })
}

pub(crate) fn validate_notes(notes: &[SharedNote], labels: &[String]) -> Result<(), StoreError> {
    validate_note_structure(notes)?;
    for note in notes {
        if restricted(labels) {
            return Err(invalid(
                "shared notes on restricted or secret tasks are refused",
            ));
        }
        if let Some(reason) = prose_exclusion(&note.body)
            .or_else(|| author_exclusion(&note.author.actor_id))
            .or_else(|| author_exclusion(&note.author.actor_kind))
        {
            return Err(invalid(format!("shared note refused: {reason}")));
        }
    }
    Ok(())
}

pub(crate) fn validate_note_structure(notes: &[SharedNote]) -> Result<(), StoreError> {
    if notes
        .windows(2)
        .any(|pair| pair[0].id.as_str() >= pair[1].id.as_str())
    {
        return Err(invalid("shared notes must be id ordered and unique"));
    }
    for note in notes {
        validate_machine_alias(&note.origin_machine)?;
        validate_shared_actor(&note.author)?;
        if note.body.len() > crate::domain::MAX_WORK_NOTE_BYTES {
            return Err(StoreError::WorkNoteTooLarge {
                bytes: note.body.len(),
                limit: crate::domain::MAX_WORK_NOTE_BYTES,
            });
        }
        if ObjectId::from_stored(note.id.as_str().to_owned()).is_none()
            || note.body.trim().is_empty()
            || note.body.trim() != note.body
        {
            return Err(invalid(
                "shared note requires an original id and nonblank body",
            ));
        }
    }
    Ok(())
}

/// Conservative syntax, not DLP. Commit hashes also count as possible locators.
pub(crate) fn prose_exclusion(text: &str) -> Option<&'static str> {
    text_exclusion(text, true)
}

pub(crate) fn author_exclusion(text: &str) -> Option<&'static str> {
    text_exclusion(text, false)
}

fn text_exclusion(text: &str, slash_paths: bool) -> Option<&'static str> {
    if text.contains('\\') {
        return Some("machine or user path");
    }
    let lower = text.to_ascii_lowercase();
    // Keep path syntax together, but inspect starts after assignments and prose
    // punctuation too. Inspect colon pairs before splitting their syntax so
    // drive and URI suffixes remain distinguishable from labels and principals.
    for token in lower.split(|c: char| !c.is_ascii_alphanumeric() && !"/.-_:$%~".contains(c)) {
        let mut parts = token.split_inclusive(':').peekable();
        while let Some(part) = parts.next() {
            if let Some(name) = part.strip_suffix(':') {
                let name = name
                    .rsplit(['-', '_', '.', '%', '/'])
                    .next()
                    .unwrap_or_default();
                if (8..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Some("possible locator");
                }
                if parts.peek().is_some_and(|rest| !rest.starts_with(':'))
                    && name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"+.".contains(&b))
                {
                    return Some("machine or user path");
                }
            }
        }
        for token in std::iter::once(token).chain(token.split([':', '-', '_', '%'])) {
            let token = token.trim_matches('_').trim_end_matches('.');
            let bytes = token.as_bytes();
            if token.starts_with("./") || token.starts_with("../") {
                return Some("repository or evidence path");
            }
            if token.split_once(':').is_some_and(|(id, _)| {
                (8..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit())
            }) {
                return Some("possible locator");
            }
            let scheme = token.split_once(':').is_some_and(|(name, rest)| {
                !rest.is_empty()
                    && !rest.starts_with(':')
                    && name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"+.-".contains(&b))
            });
            let variable = token.match_indices('%').any(|(start, _)| {
                bytes[start + 1..]
                    .iter()
                    .position(|b| *b == b'%')
                    .is_some_and(|end| {
                        end > 0
                            && bytes[start + 1..=start + end]
                                .iter()
                                .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
                    })
            });
            if token.starts_with('/')
                || token.contains("./")
                || token.contains("//")
                || token.contains('~')
                || scheme
                || variable
                || (token.contains('$') && token.contains('/'))
                || (token.starts_with('-') && token.contains('/'))
            {
                return Some("machine or user path");
            }
            if !slash_paths {
                let mut parts = token.split('/').peekable();
                while let Some(part) = parts.next() {
                    if parts.peek().is_some() && (part.starts_with('.') || path_prefix(part)) {
                        return Some("repository or evidence path");
                    }
                }
            }
            let mut segments = token.split('/');
            let first = segments.next().unwrap_or_default();
            let second = segments.next();
            let prefix = path_prefix(first);
            if second.is_some()
                && (token.ends_with('/')
                    || prefix
                    || first.starts_with('.')
                    || (slash_paths
                        && (segments.next().is_some()
                            || first.contains('.')
                            || second.is_some_and(|part| part.contains('.')))))
            {
                return Some("repository or evidence path");
            }
        }
    }
    let bytes = text.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        if !bytes[start].is_ascii_alphanumeric() {
            start += 1;
            continue;
        }
        let mut end = start;
        while end < bytes.len() && bytes[end].is_ascii_alphanumeric() {
            end += 1;
        }
        let task_ref = start >= 2
            && &bytes[start - 2..start] == b"w-"
            && (start == 2 || !bytes[start - 3].is_ascii_alphanumeric());
        if !task_ref
            && (8..=64).contains(&(end - start))
            && bytes[start..end].iter().all(u8::is_ascii_hexdigit)
        {
            return Some("possible locator");
        }
        start = end;
    }
    None
}

fn path_prefix(segment: &str) -> bool {
    matches!(
        segment,
        "src"
            | "docs"
            | "scripts"
            | "target"
            | "tests"
            | ".worktrees"
            | ".engram"
            | ".git"
            | "users"
            | "home"
            | "evidence"
            | "scratch"
    )
}
