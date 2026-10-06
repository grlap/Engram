//! The source observation that decided the move an evaluation reads stale
//! for, as every `show` surface names it: one projection and one line, so the
//! plain read, the complete read, the evaluations window and one record's
//! detail name it alike.

use super::identity::DisplayIdentity;
use super::{DateTime, Serialize, Utc};
use crate::storage::DecidingObservation;

/// The deciding observation with its reporting session as the display label
/// `show` uses for sessions. `None` fields were not recorded.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ShownDecidingObservation {
    pub observation: String,
    pub position: i64,
    pub source_changed: bool,
    pub admitted: bool,
    pub workspace: Option<String>,
    pub revision: Option<String>,
    pub root_generation: Option<i64>,
    pub reporting_session: String,
    pub observed_at: Option<DateTime<Utc>>,
    pub recorded_at: DateTime<Utc>,
    pub evaluated_revision: Option<String>,
    pub evaluated_revision_declared: bool,
    /// The two revisions compared aloud, as the refusal sentence says it.
    pub revisions_compared: &'static str,
}

/// Host-recorded text shown per field, so that the line and the field always
/// fit beside everything else a show surface must carry. `show
/// --observations` lists the fields whole.
const MAX_SHOWN_FIELD_BYTES: usize = 128;

pub(crate) fn bounded(value: &str) -> String {
    if value.len() <= MAX_SHOWN_FIELD_BYTES {
        return value.to_owned();
    }
    let mut end = MAX_SHOWN_FIELD_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes stored)", &value[..end], value.len())
}

impl ShownDecidingObservation {
    pub(crate) fn new(observation: &DecidingObservation, identity: &DisplayIdentity<'_>) -> Self {
        Self {
            observation: observation.observation.as_str().to_owned(),
            position: observation.position,
            source_changed: observation.source_changed,
            admitted: observation.admitted,
            workspace: observation.workspace.as_deref().map(bounded),
            revision: observation.revision.as_deref().map(bounded),
            root_generation: observation.root_generation,
            reporting_session: identity.session(&observation.reporting_session),
            observed_at: observation.observed_at,
            recorded_at: observation.recorded_at,
            evaluated_revision: observation.evaluated_revision.as_deref().map(bounded),
            evaluated_revision_declared: observation.evaluated_revision_declared,
            revisions_compared: observation.revisions_compared(),
        }
    }

    /// One line, stored text made terminal-safe by the caller's `safe`.
    pub(crate) fn line(&self, safe: impl Fn(&str) -> String) -> String {
        let field = |value: Option<&String>| {
            value.map_or_else(|| "not recorded".to_owned(), |value| safe(value))
        };
        if self.source_changed && !self.admitted {
            return format!(
                "unadmitted source change at run-feed position {}: workspace {}, revision {}, reported by {}, observed {} (recorded {}); the evaluation {} revision {}{}, {}; a barrier whatever revision it reports, so request a fresh evaluation",
                self.position,
                field(self.workspace.as_ref()),
                field(self.revision.as_ref()),
                safe(&self.reporting_session),
                self.observed_at
                    .map_or_else(|| "at a time not recorded".to_owned(), |at| at.to_rfc3339()),
                self.recorded_at.to_rfc3339(),
                if self.evaluated_revision_declared {
                    "declared"
                } else {
                    "judged"
                },
                self.evaluated_revision
                    .as_ref()
                    .map_or_else(|| "not known".to_owned(), |revision| safe(revision)),
                if self.evaluated_revision_declared {
                    ""
                } else {
                    " at its cut"
                },
                self.revisions_compared,
            );
        }
        format!(
            "source moved at run-feed position {}: {} observation, workspace {}, revision {}, reported by {}, observed {} (recorded {}); the evaluation {} revision {}{}",
            self.position,
            if self.source_changed {
                "a change"
            } else {
                "a sighting"
            },
            field(self.workspace.as_ref()),
            field(self.revision.as_ref()),
            safe(&self.reporting_session),
            self.observed_at
                .map_or_else(|| "at a time not recorded".to_owned(), |at| at.to_rfc3339()),
            self.recorded_at.to_rfc3339(),
            if self.evaluated_revision_declared {
                "declared"
            } else {
                "judged"
            },
            self.evaluated_revision
                .as_ref()
                .map_or_else(|| "not known".to_owned(), |revision| safe(revision)),
            if self.evaluated_revision_declared {
                ""
            } else {
                " at its cut"
            },
        )
    }
}
