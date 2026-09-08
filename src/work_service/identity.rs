//! Agent display attribution, not identity admission or authentication.

use sha2::{Digest, Sha256};
use std::fmt::Write as _;

use super::{LocalWorkService, ProjectId, SessionId};

/// Deterministic project-scoped pseudonyms. Guessable inputs remain guessable;
/// these labels are never resolved back to an actor or session by any operation.
#[derive(Clone, Copy)]
pub(crate) struct DisplayIdentity<'a> {
    pub(crate) project: &'a ProjectId,
    pub(crate) actor: &'a str,
    pub(crate) session: &'a SessionId,
}

impl DisplayIdentity<'_> {
    pub(crate) fn session(&self, session: &SessionId) -> String {
        if session == self.session {
            "you".into()
        } else {
            pseudonym(self.project, "session", &session.0)
        }
    }

    pub(crate) fn actor(&self, actor: &str) -> String {
        // An actor alone does not identify the reading session.
        pseudonym(self.project, "actor", actor)
    }

    pub(crate) fn author(&self, actor: &str, session: Option<&SessionId>) -> String {
        match session {
            Some(session) if session != self.session || actor == self.actor => {
                self.session(session)
            }
            _ => self.actor(actor),
        }
    }
}

fn pseudonym(project: &ProjectId, kind: &str, value: &str) -> String {
    let mut hash = Sha256::new();
    // Length-framed components keep project, identity kind and value distinct.
    for part in ["engram-display-peer-v1", project.0.as_str(), kind, value] {
        hash.update((part.len() as u128).to_le_bytes());
        hash.update(part.as_bytes());
    }
    let digest = hash.finalize();
    let mut suffix = String::with_capacity(24);
    for byte in &digest[..12] {
        // Formatting into a String has no fallible I/O sink.
        let _ = write!(suffix, "{byte:02x}");
    }
    let prefix = if kind == "actor" {
        "peer-actor"
    } else {
        "peer"
    };
    format!("{prefix}-{suffix}")
}

/// Recognize generated display labels to prevent accidental handoff targeting.
/// This usability guard neither resolves identities nor authenticates callers.
pub(crate) fn is_display_label(value: &str) -> bool {
    value
        .strip_prefix("peer-actor-")
        .or_else(|| value.strip_prefix("peer-"))
        .is_some_and(|suffix| {
            suffix.len() == 24 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

impl LocalWorkService {
    pub(crate) fn display_identity(&self) -> DisplayIdentity<'_> {
        DisplayIdentity {
            project: &self.project_id,
            actor: &self.actor_id,
            session: &self.session_id,
        }
    }
}
