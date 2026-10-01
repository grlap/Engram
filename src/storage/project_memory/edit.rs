//! Partial revisions of a project memory: the full body a revise stores,
//! built from the revision it names, and what that revision changed.

use super::StoreError;
use crate::domain::{ProjectMemoryChange, ProjectMemoryEdit};

/// Bytes of each changed-span excerpt a receipt shows; the rest is counted.
const CHANGE_EXCERPT_BYTES: usize = 512;

/// One `<!-- engram-section NAME -->` or `<!-- /engram-section NAME -->`
/// line, as a whole line without its terminator.
fn marker(line: &str) -> Option<(bool, &str)> {
    let inner = line.strip_prefix("<!-- ")?.strip_suffix(" -->")?;
    let (closing, name) = match inner.strip_prefix("/engram-section ") {
        Some(name) => (true, name),
        None => (false, inner.strip_prefix("engram-section ")?),
    };
    valid_section_name(name).then_some((closing, name))
}

/// Longest section name, in bytes; a refusal never echoes a longer one.
pub(crate) const MAX_SECTION_NAME_BYTES: usize = 64;

fn valid_section_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SECTION_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// A body's lines with their byte ranges, each without its `\n` or `\r\n`.
fn lines(body: &str) -> Vec<(usize, usize, &str)> {
    let mut out = Vec::new();
    let mut start = 0;
    for (index, _) in body.match_indices('\n') {
        let line = &body[start..index];
        out.push((start, index + 1, line.strip_suffix('\r').unwrap_or(line)));
        start = index + 1;
    }
    if start < body.len() {
        let line = &body[start..];
        out.push((start, body.len(), line));
    }
    out
}

/// A marked section: where its interior starts and ends in the body.
struct Section<'a> {
    name: &'a str,
    interior: (usize, usize),
}

/// Every section of `body`, in order. Markers must pair up by name, each
/// name once, with no section inside another.
fn sections(body: &str) -> Result<Vec<Section<'_>>, StoreError> {
    let mut found: Vec<Section<'_>> = Vec::new();
    let mut open: Option<(&str, usize)> = None;
    for (start, end, line) in lines(body) {
        let Some((closing, name)) = marker(line) else {
            continue;
        };
        match (closing, open) {
            (false, None) => {
                if found.iter().any(|section| section.name == name) {
                    return Err(invalid(format!("section `{name}` is marked twice")));
                }
                open = Some((name, end));
            }
            (false, Some((outer, _))) => {
                return Err(invalid(format!(
                    "section `{name}` opens inside section `{outer}`"
                )));
            }
            (true, Some((opened, interior_start))) if opened == name => {
                found.push(Section {
                    name,
                    interior: (interior_start, start),
                });
                open = None;
            }
            (true, _) => {
                return Err(invalid(format!(
                    "section `{name}` closes without being open"
                )));
            }
        }
    }
    if let Some((name, _)) = open {
        return Err(invalid(format!("section `{name}` is never closed")));
    }
    Ok(found)
}

fn invalid(reason: String) -> StoreError {
    StoreError::InvalidProjectMemory(reason)
}

/// The full body a partial revise stores: `text` appended to `basis` as a
/// paragraph, or replacing the interior of one marked section, every other
/// byte of `basis` kept. A whole edit stores `text` itself.
///
/// # Errors
///
/// Refuses a section name outside `[a-z0-9-]`, replacement text that carries
/// section markers, a body whose markers do not pair, and a section `basis`
/// does not have, the last naming the sections it has.
pub(super) fn assemble(
    key: &str,
    revision: u64,
    basis: &str,
    edit: &ProjectMemoryEdit,
    text: &str,
) -> Result<String, StoreError> {
    let newline = if basis.contains("\r\n") { "\r\n" } else { "\n" };
    let body = match edit {
        ProjectMemoryEdit::Whole => return Ok(text.to_owned()),
        ProjectMemoryEdit::Append => {
            // A blank line separates the new paragraph from the old body.
            let blank = format!("{newline}{newline}");
            let separator = if basis.ends_with(&blank) {
                String::new()
            } else if basis.ends_with(newline) {
                newline.to_owned()
            } else {
                blank
            };
            format!("{basis}{separator}{text}")
        }
        ProjectMemoryEdit::Section { name } => {
            if !valid_section_name(name) {
                // The rejected name is not echoed: it may be any size.
                return Err(invalid(format!(
                    "a section name is 1 to {MAX_SECTION_NAME_BYTES} bytes of a-z, 0-9 and -"
                )));
            }
            if lines(text)
                .iter()
                .any(|(_, _, line)| marker(line).is_some())
            {
                return Err(invalid(
                    "replacement text for a section must not carry section markers".into(),
                ));
            }
            let found = sections(basis)?;
            let Some(section) = found.iter().find(|section| section.name == name) else {
                return Err(StoreError::ProjectMemorySectionNotFound(Box::new(
                    super::super::MissingMemorySection {
                        key: key.to_owned(),
                        revision,
                        section: name.clone(),
                        sections: found
                            .iter()
                            .map(|section| section.name.to_owned())
                            .collect(),
                    },
                )));
            };
            let (start, end) = section.interior;
            // Empty text clears the section, leaving its markers.
            let terminator = if text.is_empty() || text.ends_with('\n') {
                ""
            } else {
                newline
            };
            // The text carries no markers, so the body's sections stay the
            // basis's, which were just read whole.
            return Ok(format!(
                "{}{text}{terminator}{}",
                &basis[..start],
                &basis[end..]
            ));
        }
    };
    // An append must leave well-marked sections, so a later section edit can
    // find them. A basis that was not well marked, for example one quoting a
    // marker in prose, is not held against the append: then only the
    // appended text must pair its own markers.
    if sections(basis).is_ok() {
        sections(&body)?;
    } else {
        sections(text)?;
    }
    Ok(body)
}

/// What `after` changed relative to `before`: the one differing span, as
/// bounded excerpts with exact counts.
pub(super) fn change(edit: &ProjectMemoryEdit, before: &str, after: &str) -> ProjectMemoryChange {
    let prefix = before
        .char_indices()
        .zip(after.chars())
        .find(|((_, left), right)| left != right)
        .map_or_else(|| before.len().min(after.len()), |((index, _), _)| index);
    let suffix = before[prefix..]
        .chars()
        .rev()
        .zip(after[prefix..].chars().rev())
        .take_while(|(left, right)| left == right)
        .map(|(left, _)| left.len_utf8())
        .sum::<usize>();
    let removed = &before[prefix..before.len() - suffix];
    let added = &after[prefix..after.len() - suffix];
    let excerpt = |span: &str| {
        let mut end = span.len().min(CHANGE_EXCERPT_BYTES);
        while !span.is_char_boundary(end) {
            end -= 1;
        }
        (span[..end].to_owned(), span.len() - end)
    };
    let (removed_excerpt, removed_omitted_bytes) = excerpt(removed);
    let (added_excerpt, added_omitted_bytes) = excerpt(added);
    ProjectMemoryChange {
        edit: edit.word().to_owned(),
        section: match edit {
            ProjectMemoryEdit::Section { name } => Some(name.clone()),
            _ => None,
        },
        before_bytes: before.len(),
        after_bytes: after.len(),
        span_start: prefix,
        removed: removed_excerpt,
        removed_bytes: removed.len(),
        removed_omitted_bytes,
        added: added_excerpt,
        added_bytes: added.len(),
        added_omitted_bytes,
    }
}
