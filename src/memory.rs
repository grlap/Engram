//! Low-friction capture policy shared by every interface.

use crate::domain::{Authority, Delivery, MemoryKind, MemoryStatus, NoteVisibility, Scope};

/// Pre-write inspection port. Production deployments replace the development
/// implementation with their own DLP or secret-scanning backend.
pub trait Redactor {
    /// Inspects prose before it crosses the persistence boundary.
    ///
    /// # Errors
    ///
    /// Returns a human-readable refusal when capture must fail closed.
    fn inspect(&self, prose: &str) -> Result<(), String>;

    /// Stable description surfaced by diagnostics and receipts.
    fn description(&self) -> &'static str;
}

/// Visibly non-protective development adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct DevelopmentNoopRedactor;

impl Redactor for DevelopmentNoopRedactor {
    fn inspect(&self, _prose: &str) -> Result<(), String> {
        Ok(())
    }

    fn description(&self) -> &'static str {
        "development no-op; no secret or PII protection"
    }
}

/// Result of deterministic prose classification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NoteClassification {
    pub kind: MemoryKind,
    pub authority: Authority,
    pub delivery: Delivery,
    pub title: String,
    pub body: String,
    pub classification_reason: String,
    pub delivery_override_reason: Option<String>,
}

pub(crate) fn classify_note(
    prose: &str,
    requested_title: Option<&str>,
    requested_kind: Option<MemoryKind>,
    requested_authority: Option<Authority>,
    visibility: NoteVisibility,
) -> NoteClassification {
    let normalized = prose.trim();
    let (inferred_kind, body, inference_reason) = infer_kind(normalized);
    let kind = requested_kind
        .or(inferred_kind)
        .unwrap_or(MemoryKind::Episode);
    let authority = requested_authority.unwrap_or_else(|| default_authority(kind));
    let mut delivery = default_delivery(kind, authority);
    let mut delivery_override_reason = None;
    if visibility == NoteVisibility::Shared && delivery == Delivery::OnDemand {
        delivery = Delivery::Index;
        delivery_override_reason =
            Some("task-shared operational notes enter the peer-visible context index".into());
    }

    let title = requested_title
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map_or_else(|| summarize_title(body), ToOwned::to_owned);
    let classification_reason = if requested_kind.is_some() {
        "kind explicitly supplied by caller"
    } else {
        inference_reason.unwrap_or("unclassified prose retained as an attributed episode")
    };

    NoteClassification {
        kind,
        authority,
        delivery,
        title,
        body: body.to_owned(),
        classification_reason: classification_reason.into(),
        delivery_override_reason,
    }
}

pub(crate) fn activation_policy(scope: &Scope, kind: MemoryKind) -> (MemoryStatus, String) {
    match scope {
        Scope::Task { .. } => (
            MemoryStatus::Active,
            "task execution memory activates for participants; publication remains gated".into(),
        ),
        Scope::Work { .. } => (
            MemoryStatus::Active,
            "local-work execution memory activates for participants; external publication remains optional and gated".into(),
        ),
        Scope::Agent { .. } => (
            MemoryStatus::Active,
            "private scratch activates only for its owning agent".into(),
        ),
        Scope::Project { .. } if kind == MemoryKind::Episode => (
            MemoryStatus::Active,
            "attributed episodes activate on-demand under project policy".into(),
        ),
        Scope::Project { .. } => (
            MemoryStatus::Proposed,
            "agent-authored project knowledge requires review".into(),
        ),
    }
}

fn infer_kind(prose: &str) -> (Option<MemoryKind>, &str, Option<&'static str>) {
    const PREFIXES: [(&str, MemoryKind); 9] = [
        ("constraint:", MemoryKind::Constraint),
        ("must:", MemoryKind::Constraint),
        ("decision:", MemoryKind::Decision),
        ("decided:", MemoryKind::Decision),
        ("convention:", MemoryKind::Convention),
        ("fact:", MemoryKind::Fact),
        ("evidence:", MemoryKind::Fact),
        ("preference:", MemoryKind::Preference),
        ("episode:", MemoryKind::Episode),
    ];
    const CONSTRAINT_CUES: [&str; 7] = [
        "always ",
        "do not ",
        "don't ",
        "must ",
        "must not ",
        "never ",
        "only ",
    ];

    for (prefix, kind) in PREFIXES {
        if prose
            .get(..prefix.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
        {
            return (
                Some(kind),
                prose[prefix.len()..].trim(),
                Some("kind inferred from prose prefix"),
            );
        }
    }

    if CONSTRAINT_CUES.iter().any(|cue| {
        prose
            .get(..cue.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(cue))
    }) {
        return (
            Some(MemoryKind::Constraint),
            prose,
            Some("kind inferred from natural-language rule cue"),
        );
    }

    (None, prose, None)
}

fn default_authority(kind: MemoryKind) -> Authority {
    match kind {
        MemoryKind::Constraint | MemoryKind::Decision => Authority::Firm,
        MemoryKind::Convention
        | MemoryKind::Fact
        | MemoryKind::Preference
        | MemoryKind::Episode => Authority::Soft,
    }
}

fn default_delivery(kind: MemoryKind, authority: Authority) -> Delivery {
    match (kind, authority) {
        (MemoryKind::Constraint, Authority::Hard | Authority::Firm)
        | (MemoryKind::Decision, Authority::Hard) => Delivery::Pinned,
        (MemoryKind::Episode, _) => Delivery::OnDemand,
        _ => Delivery::Index,
    }
}

fn summarize_title(body: &str) -> String {
    let first_line = body
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(body);
    let mut title: String = first_line.trim().chars().take(96).collect();
    if first_line.trim().chars().count() > 96 {
        title.push('…');
    }
    if title.is_empty() {
        "Untitled note".into()
    } else {
        title
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_inference_is_explainable_and_shared_episodes_are_indexed() {
        let decision = classify_note(
            "Decision: use one canonical write path",
            None,
            None,
            None,
            NoteVisibility::Shared,
        );
        assert_eq!(decision.kind, MemoryKind::Decision);
        assert_eq!(decision.authority, Authority::Firm);
        assert_eq!(decision.delivery, Delivery::Index);
        assert_eq!(decision.body, "use one canonical write path");

        let episode = classify_note(
            "The failing test only reproduces in a worktree.",
            None,
            None,
            None,
            NoteVisibility::Shared,
        );
        assert_eq!(episode.kind, MemoryKind::Episode);
        assert_eq!(episode.delivery, Delivery::Index);
        assert!(episode.delivery_override_reason.is_some());
    }

    #[test]
    fn natural_rule_cues_become_pinned_constraints_without_flags() {
        for prose in [
            "Never put task IDs in source comments.",
            "Always preserve private scope.",
            "Do not truncate pinned rules.",
            "Must retain the original receipt.",
            "Only publish a frozen report.",
        ] {
            let classified = classify_note(prose, None, None, None, NoteVisibility::Shared);
            assert_eq!(classified.kind, MemoryKind::Constraint);
            assert_eq!(classified.authority, Authority::Firm);
            assert_eq!(classified.delivery, Delivery::Pinned);
            assert_eq!(classified.body, prose);
            assert_eq!(
                classified.classification_reason,
                "kind inferred from natural-language rule cue"
            );
        }
    }
}
