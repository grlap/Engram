//! A peek's direction to list project memories while the host's context
//! generation has no recorded listing. It states the host's assertion and
//! what is recorded about listings; it never says that a compaction happened
//! or that notes were read.

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
