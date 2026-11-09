//! A peek's direction to list project memories while the host's context
//! generation has no recorded listing. It states the host's assertion and
//! what is recorded about listings; it never says that a compaction happened
//! or that notes were read.

use std::fmt::Write as _;

use crate::work_service::WorkNextPeek;

const DIRECTION: &str = "the host reports a new context for this session: before acting, list project memories through the continuation and read the relevant current entries in full";

/// The reminder a peek puts first, present until a memories listing of the
/// session carries the host's context generation.
pub(super) fn reminder(peek: Option<&WorkNextPeek>) -> Option<String> {
    peek.is_some_and(|peek| peek.memory_listing_due)
        .then(|| DIRECTION.to_owned())
}

/// The listing command a peek offers first. While the listing is due the
/// command carries the host's generation, because the agent does not
/// otherwise know it and only a listing that carries it is recorded. An
/// admitted generation is a plain token, so it is printed as it is.
pub(super) fn listing_command(
    peek: Option<&WorkNextPeek>,
    context_generation: Option<&str>,
) -> String {
    match context_generation {
        Some(generation) if peek.is_some_and(|peek| peek.memory_listing_due) => {
            format!("engram work memories --context-generation {generation}")
        }
        _ => "engram work memories".into(),
    }
}

/// The `memories:` line of next: the count and, while something changed for
/// the session, which keys changed since its recorded position, newest first,
/// with the command that lists them all when more changed than are named.
pub(super) fn signal_line(
    signal: &crate::work_service::ProjectMemorySignal,
    context_generation: Option<&str>,
) -> String {
    let count = signal.count;
    if !signal.changed {
        return format!("memories: {count} retained");
    }
    let Some(changes) = &signal.changes else {
        return format!(
            "memories: {count} retained (changed; no memories listing is recorded for this session)"
        );
    };
    if changes.count == 0 {
        return format!(
            "memories: {count} retained (changed: a new context; no memory changed since this session last listed or was shown them)"
        );
    }
    let mut line = format!(
        "memories: {count} retained ({} changed since this session last listed or was shown them: {}",
        changes.count,
        changes_summary(changes)
    );
    if changes.count > changes.keys.len() {
        let command = match context_generation {
            Some(generation) => format!("engram work memories --context-generation {generation}"),
            None => "engram work memories".into(),
        };
        let _ = write!(
            line,
            "; every key is listed by {}",
            super::terminal_command(&command)
        );
    }
    line.push(')');
    line
}

/// The changed keys named, newest first and forgotten ones marked, then how
/// many more changed and how many of those were forgotten.
pub(super) fn changes_summary(changes: &crate::domain::ProjectMemoryKeyChanges) -> String {
    let mut summary = changes
        .keys
        .iter()
        .map(|change| {
            if change.forgotten {
                format!("{} (forgotten)", change.key)
            } else {
                change.key.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let unnamed = changes.count.saturating_sub(changes.keys.len());
    if unnamed > 0 {
        let named_forgotten = changes
            .keys
            .iter()
            .filter(|change| change.forgotten)
            .count();
        let unnamed_forgotten = changes.forgotten.saturating_sub(named_forgotten);
        let _ = write!(summary, " and {unnamed} more");
        if unnamed_forgotten > 0 {
            let _ = write!(summary, ", {unnamed_forgotten} of them forgotten");
        }
    }
    summary
}

/// The lines a peek's text opens with: the direction, then its command.
pub(super) fn opening_lines(
    peek: Option<&WorkNextPeek>,
    context_generation: Option<&str>,
) -> Vec<String> {
    let Some(reminder) = reminder(peek) else {
        return Vec::new();
    };
    vec![
        reminder,
        format!(
            "  {}",
            super::terminal_command(&listing_command(peek, context_generation))
        ),
    ]
}
