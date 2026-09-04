//! Shell attribution: asserted actor and process-default session resolution
//! for the CLI work and graph words.

use std::{env, process, sync::OnceLock};

use engram::{
    WorkActorDefaultSource, WorkAttributionDefaults, new_process_default_work_session_id,
};

/// Shell attribution resolved before the CLI dispatches a work word.
///
/// Defaulted origins become durable actor provenance. The generated session
/// is stable for this process and reusable for seven days, so its notice
/// exposes the exact value a human may reuse for a session-bound follow-up.
pub(crate) struct ShellWorkAttribution {
    pub(crate) actor_id: String,
    pub(crate) session_id: String,
    pub(crate) defaults: WorkAttributionDefaults,
}

static DEFAULT_WORK_SESSION_ID: OnceLock<String> = OnceLock::new();

impl ShellWorkAttribution {
    pub(crate) fn print_notices(&self) {
        match self.defaults.actor {
            Some(WorkActorDefaultSource::OsUserEnvironment) => eprintln!(
                "NOTICE: ENGRAM_ACTOR_ID was absent; attribution uses the asserted OS-user environment and is marked defaulted."
            ),
            Some(WorkActorDefaultSource::ProcessFallback) => eprintln!(
                "NOTICE: ENGRAM_ACTOR_ID and conventional OS-user environment variables were absent; attribution uses a synthetic process actor and is marked defaulted."
            ),
            None => {}
        }
        if self.defaults.session {
            eprintln!(
                "NOTICE: ENGRAM_SESSION_ID was absent; this command uses {}. Reuse it within seven days with --session-id {} for a follow-up that must retain focus, claim authority, or exact retry identity.",
                self.session_id, self.session_id
            );
        }
    }
}

/// Resolves omitted local-shell attribution without rewriting injected bytes.
pub(crate) fn resolve_shell_work_attribution(
    actor_id: Option<String>,
    session_id: Option<String>,
) -> ShellWorkAttribution {
    let (actor_id, actor_default) =
        actor_id.map_or_else(default_shell_actor, |actor_id| (actor_id, None));
    let session_defaulted = session_id.is_none();
    ShellWorkAttribution {
        actor_id,
        session_id: session_id.unwrap_or_else(default_process_session_id),
        defaults: WorkAttributionDefaults {
            actor: actor_default,
            session: session_defaulted,
        },
    }
}

/// Derives an asserted local actor from conventional OS-user environment
/// variables, with a separately marked synthetic fallback.
fn default_shell_actor() -> (String, Option<WorkActorDefaultSource>) {
    default_shell_actor_from(|name| env::var(name).ok())
}

fn default_shell_actor_from(
    mut environment_value: impl FnMut(&str) -> Option<String>,
) -> (String, Option<WorkActorDefaultSource>) {
    let candidates: &[&str] = if cfg!(windows) {
        &["USERNAME", "USER", "LOGNAME"]
    } else {
        &["USER", "LOGNAME", "USERNAME"]
    };
    candidates
        .iter()
        .find_map(|name| {
            environment_value(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        })
        .map_or_else(
            || {
                (
                    format!("local-user-{}", process::id()),
                    Some(WorkActorDefaultSource::ProcessFallback),
                )
            },
            |actor_id| (actor_id, Some(WorkActorDefaultSource::OsUserEnvironment)),
        )
}

/// Returns one opaque session id that is stable for this CLI process and
/// reusable during the bounded process-default retention window.
fn default_process_session_id() -> String {
    DEFAULT_WORK_SESSION_ID
        .get_or_init(new_process_default_work_session_id)
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_work_attribution_defaults_without_rewriting_injected_values() {
        let (environment_actor, environment_source) =
            default_shell_actor_from(|name| (name == "USER").then(|| " env user ".into()));
        assert_eq!(environment_actor, "env user");
        assert_eq!(
            environment_source,
            Some(WorkActorDefaultSource::OsUserEnvironment)
        );
        let (fallback_actor, fallback_source) = default_shell_actor_from(|_| None);
        assert_eq!(fallback_actor, format!("local-user-{}", process::id()));
        assert_eq!(
            fallback_source,
            Some(WorkActorDefaultSource::ProcessFallback)
        );

        let first = resolve_shell_work_attribution(None, None);
        let second = resolve_shell_work_attribution(None, None);
        assert_eq!(first.session_id, second.session_id);
        assert!(first.defaults.actor.is_some());
        assert!(first.defaults.session);
        assert!(first.session_id.starts_with("local-process-v1-"));

        let default_actor = resolve_shell_work_attribution(None, Some("host session".into()));
        assert!(default_actor.defaults.actor.is_some());
        assert!(!default_actor.defaults.session);
        assert_eq!(default_actor.session_id, "host session");

        let default_session = resolve_shell_work_attribution(Some("host actor".into()), None);
        assert_eq!(default_session.actor_id, "host actor");
        assert!(default_session.defaults.actor.is_none());
        assert!(default_session.defaults.session);

        let injected = resolve_shell_work_attribution(
            Some(" host actor ".into()),
            Some(" host session ".into()),
        );
        assert_eq!(injected.actor_id, " host actor ");
        assert_eq!(injected.session_id, " host session ");
        assert_eq!(injected.defaults, WorkAttributionDefaults::default());
    }
}
