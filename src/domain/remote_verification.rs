//! A test run on another host, as the host-private operation states it.
//!
//! The completion holds the launcher's own facts, as that host's native owner
//! retained them; the association names the local destination and how the
//! result is meant to be used. These are host assertions Engram checks against
//! what it owns, not independent attestation of the remote execution.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

use super::{
    ControlWorkBinding, GitTreeId, ProjectId, RegisteredVerificationRecord,
    RegisteredVerificationRoot, SessionId, SourceRootState,
};
use crate::{ActorContext, ObjectId};

/// The canonical kind of one project's immutable terminal facts of one run.
pub const REMOTE_TERMINAL_ORIGIN_KIND: &str = "remote_terminal_origin";
/// The canonical kind of one delivery of a result to one destination.
pub const REMOTE_VERIFICATION_ASSOCIATION_KIND: &str = "remote_verification_association";
/// The completion format this build reads.
pub const REMOTE_COMPLETION_SCHEMA_VERSION: u16 = 1;
/// The native launcher's reason for a run whose only change is new untracked
/// paths. It stays a failed run until the integration owner disposes of them.
pub const NEW_UNTRACKED_ONLY_REASON: &str = "new-untracked-only";
/// Where the Merger's test refs live: the only refs the landing-gate lane
/// admits.
pub const AUTHORIZED_TEST_REF_PREFIX: &str = "refs/heads/test/";

/// A supplied optional member must have its type: `null` is not "absent".
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteRunOrigin {
    pub host_id: String,
    pub runner_session_id: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteRunMode {
    Full,
    Focused,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RemoteRunVerdict {
    Pass,
    Fail,
    Interrupted,
    Running,
    Unknown,
}

impl RemoteRunVerdict {
    const fn word(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Interrupted => "INTERRUPTED",
            Self::Running => "RUNNING",
            Self::Unknown => "UNKNOWN",
        }
    }
}

/// The launcher's result, unchanged. `exit` must be stated, and may be null.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteRunResult {
    pub verdict: RemoteRunVerdict,
    pub terminal: bool,
    #[serde(deserialize_with = "Option::deserialize")]
    pub exit: Option<i64>,
    pub reason: String,
}

/// A complete shared endpoint pair or the unchanged historical launcher facts.
/// The alternatives are closed: shared input never falls back to a legacy hint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum RemoteRunInput {
    SourceId(Box<RemoteSourceIdInput>),
    Legacy(LegacyRemoteRunInput),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RemoteSourceIdKind {
    SourceId,
}

/// Both keys are required. A null endpoint reports an unavailable measurement;
/// even two null endpoints supply no source identity and cannot earn credit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteSourceIdInput {
    kind: RemoteSourceIdKind,
    #[serde(deserialize_with = "Option::deserialize")]
    pub source_id_before: Option<crate::source::SourceId>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub source_id_after: Option<crate::source::SourceId>,
}

impl RemoteSourceIdInput {
    #[must_use]
    pub fn new(
        source_id_before: Option<crate::source::SourceId>,
        source_id_after: Option<crate::source::SourceId>,
    ) -> Self {
        Self {
            kind: RemoteSourceIdKind::SourceId,
            source_id_before,
            source_id_after,
        }
    }

    #[must_use]
    pub fn matching_source_id(&self) -> Option<&crate::source::SourceId> {
        self.source_id_before
            .as_ref()
            .filter(|before| Some(*before) == self.source_id_after.as_ref())
    }
}

impl From<LegacyRemoteRunInput> for RemoteRunInput {
    fn from(input: LegacyRemoteRunInput) -> Self {
        Self::Legacy(input)
    }
}

/// What historical launchers stated. Missing facts stay missing; these fields
/// keep their original wire, stored bytes and qualification semantics.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyRemoteRunInput {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub commit: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub tree: Option<GitTreeId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub before_fingerprint: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub after_fingerprint: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteRunCompletion {
    pub schema_version: u16,
    pub origin: RemoteRunOrigin,
    pub run_id: String,
    pub mode: RemoteRunMode,
    pub result: RemoteRunResult,
    pub input: RemoteRunInput,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub finished_at: Option<DateTime<Utc>>,
}

impl RemoteRunCompletion {
    /// Whether these facts can fix the run's immutable terminal. A running,
    /// unknown or inferred snapshot never can, whatever the launcher's terminal
    /// flag says about that snapshot: the launcher really emits UNKNOWN with
    /// `terminal` true and INTERRUPTED with `terminal` false.
    #[must_use]
    pub const fn is_authoritative_terminal(&self) -> bool {
        self.result.terminal
            && matches!(
                self.result.verdict,
                RemoteRunVerdict::Pass | RemoteRunVerdict::Fail | RemoteRunVerdict::Interrupted
            )
    }

    /// Whether these facts settle the run's terminal when delivered at `now`.
    /// A finish time ahead of this host's clock is well formed but not yet
    /// settled: it stays provisional, and a later delivery can settle it.
    #[must_use]
    pub fn settles_terminal_at(&self, now: DateTime<Utc>) -> bool {
        self.is_authoritative_terminal() && self.finished_at.is_none_or(|at| at <= now)
    }

    /// The run and its verdict, short enough for a change line.
    #[must_use]
    pub fn headline(&self) -> String {
        format!(
            "remote {} run {} on {}: {}",
            match self.mode {
                RemoteRunMode::Full => "full",
                RemoteRunMode::Focused => "focused",
            },
            self.run_id,
            self.origin.host_id,
            self.result.verdict.word(),
        )
    }

    /// One line of the run's own facts, with its time apart from any local one.
    #[must_use]
    pub fn describe(&self) -> String {
        let fact = |value: Option<&str>| value.unwrap_or("missing").to_owned();
        let input = match &self.input {
            RemoteRunInput::Legacy(input) => format!(
                "commit={} tree={} before={} after={}",
                fact(input.commit.as_deref()),
                fact(input.tree.as_ref().map(GitTreeId::as_str)),
                fact(input.before_fingerprint.as_deref()),
                fact(input.after_fingerprint.as_deref()),
            ),
            RemoteRunInput::SourceId(input) => {
                let endpoint = |source: Option<&crate::source::SourceId>| {
                    source.map_or_else(
                        || "unavailable".to_owned(),
                        |source| {
                            format!(
                                "commit {} tree {} ({} changes, {} submodules)",
                                source.commit,
                                source.tree,
                                source.changes.len(),
                                source.submodules.len()
                            )
                        },
                    )
                };
                format!(
                    "source_id_before={} source_id_after={}",
                    endpoint(input.source_id_before.as_ref()),
                    endpoint(input.source_id_after.as_ref())
                )
            }
        };
        format!(
            "remote {} run {} on {} runner {}: {} terminal={} exit={} reason={}; {}; finished_at={}",
            match self.mode {
                RemoteRunMode::Full => "full",
                RemoteRunMode::Focused => "focused",
            },
            self.run_id,
            self.origin.host_id,
            self.origin.runner_session_id,
            self.result.verdict.word(),
            self.result.terminal,
            self.result
                .exit
                .map_or_else(|| "null".to_owned(), |exit| exit.to_string()),
            self.result.reason,
            input,
            self.finished_at
                .map_or_else(|| "missing".to_owned(), |at| at.to_rfc3339()),
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteVerificationPurpose {
    Verification,
    LandingGate,
}

/// The input lane and use the host intends. It is checked against the actual
/// target and grants no authority of its own.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteVerificationUse {
    CommittedInput {
        expected_commit: String,
        purpose: RemoteVerificationPurpose,
    },
    AuthorizedTestRef {
        ref_name: String,
        expected_tree: GitTreeId,
        purpose: RemoteVerificationPurpose,
    },
}

impl RemoteVerificationUse {
    #[must_use]
    pub const fn purpose(&self) -> RemoteVerificationPurpose {
        match self {
            Self::CommittedInput { purpose, .. } | Self::AuthorizedTestRef { purpose, .. } => {
                *purpose
            }
        }
    }

    /// The source revision credited evidence carries: the same spelling as
    /// the registered commit and staged-tree inputs.
    #[must_use]
    pub fn revision(&self) -> String {
        match self {
            Self::CommittedInput {
                expected_commit, ..
            } => format!("git-commit:{expected_commit}"),
            Self::AuthorizedTestRef { expected_tree, .. } => {
                format!("git-tree:{}", expected_tree.as_str())
            }
        }
    }

    /// The tree credited evidence names; a commit is identified by itself.
    #[must_use]
    pub fn git_tree(&self) -> Option<GitTreeId> {
        match self {
            Self::CommittedInput { .. } => None,
            Self::AuthorizedTestRef { expected_tree, .. } => Some(expected_tree.clone()),
        }
    }

    fn describe(&self) -> String {
        let purpose = match self.purpose() {
            RemoteVerificationPurpose::Verification => "verification",
            RemoteVerificationPurpose::LandingGate => "landing_gate",
        };
        match self {
            Self::CommittedInput {
                expected_commit, ..
            } => format!("committed_input {purpose} expected_commit={expected_commit}"),
            Self::AuthorizedTestRef {
                ref_name,
                expected_tree,
                ..
            } => format!(
                "authorized_test_ref {purpose} ref={ref_name} expected_tree={}",
                expected_tree.as_str()
            ),
        }
    }
}

/// The destination of one delivery. A new wrapper, not a registration: it has
/// no registered run behind it, and one origin may have many of these.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteVerificationAssociation {
    pub binding: ControlWorkBinding,
    pub root: RegisteredVerificationRoot,
    #[serde(rename = "use")]
    pub intended_use: RemoteVerificationUse,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteVerificationRequest {
    pub operation_key: String,
    pub completion: RemoteRunCompletion,
    pub association: RemoteVerificationAssociation,
}

/// Why a delivered result earns no credit. A malformed request is not among
/// these: `invalid_request` is the operation's own refusal and records nothing.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteVerificationReason {
    ProducerUnsupported,
    AssociationStale,
    ResultConflict,
    NotFullMode,
    UntrackedDebrisPendingDisposition,
    ResultNotPass,
    CommitMismatch,
    TreeMismatch,
    FingerprintMissingOrChanged,
    SourceIdMissingOrChanged,
}

impl RemoteVerificationReason {
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::ProducerUnsupported => "producer_unsupported",
            Self::AssociationStale => "association_stale",
            Self::ResultConflict => "result_conflict",
            Self::NotFullMode => "not_full_mode",
            Self::UntrackedDebrisPendingDisposition => "untracked_debris_pending_disposition",
            Self::ResultNotPass => "result_not_pass",
            Self::CommitMismatch => "commit_mismatch",
            Self::TreeMismatch => "tree_mismatch",
            Self::FingerprintMissingOrChanged => "fingerprint_missing_or_changed",
            Self::SourceIdMissingOrChanged => "source_id_missing_or_changed",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteVerificationQualification {
    pub credited: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub reason: Option<RemoteVerificationReason>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub detail: Option<String>,
}

impl RemoteVerificationQualification {
    #[must_use]
    pub const fn credited() -> Self {
        Self {
            credited: true,
            reason: None,
            detail: None,
        }
    }

    #[must_use]
    pub fn refused(reason: RemoteVerificationReason, detail: Option<String>) -> Self {
        Self {
            credited: false,
            reason: Some(reason),
            detail,
        }
    }

    fn describe(&self) -> String {
        match (self.reason, &self.detail) {
            (None, _) => "credited".to_owned(),
            (Some(reason), None) => format!("not credited: {}", reason.word()),
            (Some(reason), Some(detail)) => {
                format!("not credited: {} ({detail})", reason.word())
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteVerificationReceipt {
    pub operation: ObjectId,
    #[serde(deserialize_with = "Option::deserialize")]
    pub origin: Option<ObjectId>,
    pub verifications: Vec<RegisteredVerificationRecord>,
    pub qualification: RemoteVerificationQualification,
}

/// One project's immutable terminal facts of one host's run, shared by every
/// destination the result is delivered to. It belongs to no work item.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteTerminalOrigin {
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub session_id: SessionId,
    pub completion: RemoteRunCompletion,
    pub actor: ActorContext,
    pub recorded_at: DateTime<Utc>,
}

/// One delivery to one destination: everything the host stated, the origin it
/// settled to, and the decision taken. Credit depends on this record's own
/// qualification; `associated_at` is the local time of the delivery and never
/// the run's execution time, which is the completion's.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteVerificationAssociationRecord {
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub session_id: SessionId,
    pub request: RemoteVerificationRequest,
    #[serde(deserialize_with = "Option::deserialize")]
    pub origin: Option<ObjectId>,
    pub qualification: RemoteVerificationQualification,
    pub actor: ActorContext,
    pub associated_at: DateTime<Utc>,
}

impl RemoteVerificationAssociationRecord {
    /// Whether the record stands on its destination's feeds. A stale or
    /// unsupported delivery was never admitted to that item's scope.
    #[must_use]
    pub fn is_in_item_scope(&self) -> bool {
        !matches!(
            self.qualification.reason,
            Some(
                RemoteVerificationReason::AssociationStale
                    | RemoteVerificationReason::ProducerUnsupported
            )
        )
    }

    /// The run and the decision, short enough for a change line.
    #[must_use]
    pub fn headline(&self) -> String {
        format!(
            "{}; {}",
            self.request.completion.headline(),
            self.qualification.describe()
        )
    }

    /// The remote facts, the local association time and the decision, kept
    /// apart. Read back through the item's records; it is not stored text.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "{}; associated_at={}; use={}; {}",
            self.request.completion.describe(),
            self.associated_at.to_rfc3339(),
            self.request.association.intended_use.describe(),
            self.qualification.describe(),
        )
    }
}

fn bounded_label(text: &str, limit: usize) -> bool {
    !text.trim().is_empty()
        && text.trim() == text
        && text.len() <= limit
        && !super::holds_refused_source_text(text)
}

fn full_commit(text: &str) -> bool {
    text.len() == 40
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// The largest exit magnitude canonical JSON keeps exactly.
const MAX_EXACT_INTEGER: i64 = (1 << 53) - 1;

impl RemoteRunCompletion {
    /// The shape every stored or delivered completion must have.
    ///
    /// # Errors
    /// Names the first field whose supplied value is malformed.
    pub fn validate(&self) -> Result<(), (&'static str, &'static str)> {
        if !bounded_label(&self.origin.host_id, 512)
            || !bounded_label(&self.origin.runner_session_id, 512)
        {
            return Err(("origin", "the origin must name a host and native runner"));
        }
        if !bounded_label(&self.run_id, 128) {
            return Err(("run_id", "the run id must be bounded safe text"));
        }
        if !bounded_label(&self.result.reason, 256) {
            return Err(("result.reason", "the reason must be bounded safe text"));
        }
        if self
            .result
            .exit
            .is_some_and(|exit| !(-MAX_EXACT_INTEGER..=MAX_EXACT_INTEGER).contains(&exit))
        {
            return Err(("result.exit", "the exit code is out of range"));
        }
        if let RemoteRunInput::Legacy(input) = &self.input {
            if input
                .commit
                .as_deref()
                .is_some_and(|commit| !full_commit(commit))
            {
                return Err(("input.commit", "a full lowercase commit id is required"));
            }
            if [&input.before_fingerprint, &input.after_fingerprint]
                .into_iter()
                .flatten()
                .any(|fingerprint| !bounded_label(fingerprint, 128))
            {
                return Err((
                    "input.fingerprint",
                    "a supplied fingerprint must be bounded safe text",
                ));
            }
        }
        Ok(())
    }
}

impl RemoteVerificationRequest {
    /// Validate new shared endpoints without changing what stored legacy facts
    /// or decisions mean. Null endpoints remain valid, non-qualifying reports.
    ///
    /// # Errors
    /// Names a malformed new endpoint or a malformed common request field.
    pub fn validate_new(&self) -> Result<(), (&'static str, &'static str)> {
        self.validate()?;
        // The landing-gate lane carries only the Merger's test refs. An
        // author's own test ref is evidence like a bundle run and reaches
        // Engram as a committed input. Stored requests keep being read as
        // stored, so only a new request is held to this.
        if let RemoteVerificationUse::AuthorizedTestRef { ref_name, .. } =
            &self.association.intended_use
            && !ref_name.starts_with(AUTHORIZED_TEST_REF_PREFIX)
        {
            return Err((
                "association.use.ref_name",
                "an authorized test ref is a refs/heads/test/ ref; report an author test ref as a committed input",
            ));
        }
        if let RemoteRunInput::SourceId(input) = &self.completion.input {
            for (field, source) in [
                ("input.source_id_before", &input.source_id_before),
                ("input.source_id_after", &input.source_id_after),
            ] {
                if let Some(source) = source {
                    source
                        .validate()
                        .map_err(|_| (field, "a valid complete source_id is required"))?;
                }
            }
        }
        Ok(())
    }

    /// The shape every stored or delivered request must have, independent of
    /// any clock or store.
    ///
    /// # Errors
    /// Names the first field whose supplied value is malformed.
    pub fn validate(&self) -> Result<(), (&'static str, &'static str)> {
        if !bounded_label(&self.operation_key, 512) {
            return Err((
                "operation_key",
                "the operation key must be bounded safe text",
            ));
        }
        self.completion.validate()?;
        let root = &self.association.root;
        if root.state != SourceRootState::Named
            || root.generation <= 0
            || !bounded_label(&root.workspace_id, 512)
        {
            return Err((
                "association.root",
                "a named workspace and positive generation are required",
            ));
        }
        match &self.association.intended_use {
            RemoteVerificationUse::CommittedInput {
                expected_commit, ..
            } if !full_commit(expected_commit) => Err((
                "association.use.expected_commit",
                "a full lowercase commit id is required",
            )),
            RemoteVerificationUse::AuthorizedTestRef {
                ref_name, purpose, ..
            } => {
                if !bounded_label(ref_name, 512) {
                    Err((
                        "association.use.ref_name",
                        "the ref name must be bounded safe text",
                    ))
                } else if *purpose == RemoteVerificationPurpose::LandingGate {
                    Ok(())
                } else {
                    Err((
                        "association.use.purpose",
                        "an authorized test ref serves only a landing gate",
                    ))
                }
            }
            RemoteVerificationUse::CommittedInput { .. } => Ok(()),
        }
    }
}
