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

    /// The label of a record's author: "you" only for a record of this
    /// session made by this actor as the kind of actor the agent words record
    /// as; a record of this session by another actor, or by this actor as
    /// another kind (a host operator, say), is labelled by its actor alone.
    ///
    /// Comparing with `WORD_ACTOR_KIND` is comparing with the reader's own
    /// kind: the service that builds this identity records every action of
    /// its own with that kind, and so does the host-control channel. A writer
    /// that recorded this session's work as another kind would no longer be
    /// called "you".
    pub(crate) fn author(&self, actor: &str, kind: &str, session: Option<&SessionId>) -> String {
        match session {
            Some(session)
                if session != self.session || self.is_reader(actor, kind, Some(session)) =>
            {
                self.session(session)
            }
            _ => self.actor(actor),
        }
    }

    /// Whether a record is the reader's own: made in this session by this
    /// actor as the kind of actor the agent words record as, exactly when
    /// [`Self::author`] labels it "you".
    pub(crate) fn is_reader(&self, actor: &str, kind: &str, session: Option<&SessionId>) -> bool {
        session == Some(self.session) && actor == self.actor && kind == super::WORD_ACTOR_KIND
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

#[cfg(test)]
mod tests {
    use super::{DisplayIdentity, ProjectId, SessionId};
    use crate::work_service::WORD_ACTOR_KIND;

    /// "You" needs this session, this actor and the kind the words record
    /// as; a record of this session by this actor as another kind is labelled
    /// by its actor, as one by another actor is; another session keeps its
    /// peer label and an actor alone keeps its actor label.
    #[test]
    fn author_is_you_only_for_this_session_actor_and_kind() {
        let project = ProjectId("identity".into());
        let session = SessionId("reader-session".into());
        let identity = DisplayIdentity {
            project: &project,
            actor: "reader",
            session: &session,
        };
        let other = SessionId("other-session".into());
        assert_eq!(
            identity.author("reader", WORD_ACTOR_KIND, Some(&session)),
            "you"
        );
        for kind in ["host_operator", "system", ""] {
            assert_eq!(
                identity.author("reader", kind, Some(&session)),
                identity.actor("reader"),
                "{kind}"
            );
        }
        assert_eq!(
            identity.author("someone", WORD_ACTOR_KIND, Some(&session)),
            identity.actor("someone")
        );
        for kind in [WORD_ACTOR_KIND, "host_operator"] {
            assert_eq!(
                identity.author("reader", kind, Some(&other)),
                identity.session(&other),
                "{kind}"
            );
        }
        assert_eq!(
            identity.author("reader", WORD_ACTOR_KIND, None),
            identity.actor("reader")
        );
        // The reader's own records are exactly those labelled "you".
        for (actor, kind, session) in [
            ("reader", WORD_ACTOR_KIND, Some(&session)),
            ("reader", "host_operator", Some(&session)),
            ("someone", WORD_ACTOR_KIND, Some(&session)),
            ("reader", WORD_ACTOR_KIND, Some(&other)),
            ("reader", WORD_ACTOR_KIND, None),
        ] {
            assert_eq!(
                identity.is_reader(actor, kind, session),
                identity.author(actor, kind, session) == "you",
                "{actor} {kind} {session:?}"
            );
        }
        assert!(identity.is_reader("reader", WORD_ACTOR_KIND, Some(&session)));
    }
}
