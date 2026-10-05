//! Provenance links, source snapshots, and the asserted actor attribution
//! retained on every durable object.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ObjectId;

use super::{AssuranceLevel, SessionId, is_unsafe_rendered_text_char};

/// How an assertion reached the actor recording it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceRelation {
    AssertedBy,
    RelayedBy,
    DerivedFrom,
}

/// One retained hop in an assertion's provenance chain.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProvenanceLink {
    pub relation: ProvenanceRelation,
    pub source: String,
    pub reference: Option<String>,
}

/// Fingerprint of mutable source material as it was observed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceSnapshot {
    pub source_ref: String,
    pub fingerprint: String,
    pub observed_at: DateTime<Utc>,
}

/// Selected external fields retained when organizational work is imported.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WorkSourceProjection {
    pub title: Option<String>,
    pub body: Option<String>,
    pub status: Option<String>,
    pub owner: Option<String>,
}

/// Immutable, backend-neutral provenance for one explicit work import.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WorkSourceSnapshot {
    pub schema_version: u16,
    pub adapter_kind: String,
    pub canonical_ref: String,
    pub projected: WorkSourceProjection,
    pub captured_at: DateTime<Utc>,
    pub source_revision: Option<String>,
    pub fingerprint: String,
    pub canonical_url: Option<String>,
    pub payload_hash: ObjectId,
    #[serde(default)]
    pub raw: BTreeMap<String, Value>,
}

/// Attribution retained on every durable object.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ActorContext {
    pub actor_id: String,
    pub actor_kind: String,
    pub assurance: AssuranceLevel,
    pub run_id: Option<String>,
    pub session_id: Option<SessionId>,
    pub source_tool: Option<String>,
    pub source_skill: Option<String>,
    #[serde(default)]
    pub provenance_chain: Vec<ProvenanceLink>,
    pub reason: String,
}

/// Provenance reference carried by evidence appended after its work completed.
pub(crate) const POST_COMPLETION_EVIDENCE_PROVENANCE_REFERENCE: &str = "post_completion";
/// Stable provenance source paired with the post-completion evidence marker.
pub(crate) const POST_COMPLETION_EVIDENCE_PROVENANCE_SOURCE: &str = "work_evidence:post_completion";
/// Marker used inside [`ActorContext::provenance_chain`] for optional,
/// host-asserted execution context that describes an actor without changing
/// the actor principal used by assignment or authority checks.
pub(crate) const ACTOR_CONTEXT_PROVENANCE_REFERENCE: &str = "actor_context";
/// Provenance reference recording that supplied actor context was normalized
/// before it was retained.
pub(crate) const ACTOR_CONTEXT_NORMALIZED_REFERENCE: &str = "actor_context_normalized";
/// Maximum retained UTF-8 bytes for optional host-asserted actor context.
pub const MAX_ACTOR_CONTEXT_BYTES: usize = 256;
/// Provenance source marking an actor id taken from an OS-user variable.
pub(crate) const DEFAULTED_OS_USER_ACTOR_SOURCE: &str = "defaulted:os_user_environment";
/// Provenance source marking a synthetic actor id this process supplied.
pub(crate) const DEFAULTED_PROCESS_ACTOR_SOURCE: &str = "defaulted:process_actor";
/// Provenance reference naming the field a shell default supplied.
pub(crate) const DEFAULTED_ACTOR_REFERENCE: &str = "actor_id";
/// Provenance source marking a session id this process supplied.
pub(crate) const DEFAULTED_PROCESS_SESSION_SOURCE: &str = "defaulted:process_session";
/// Provenance reference naming a defaulted session id.
pub(crate) const DEFAULTED_SESSION_REFERENCE: &str = "session_id";

impl ProvenanceLink {
    /// Whether this link marks the actor id as a shell default.
    fn defaults_actor(&self) -> bool {
        self.relation == ProvenanceRelation::DerivedFrom
            && matches!(
                self.source.as_str(),
                DEFAULTED_OS_USER_ACTOR_SOURCE | DEFAULTED_PROCESS_ACTOR_SOURCE
            )
            && self.reference.as_deref() == Some(DEFAULTED_ACTOR_REFERENCE)
    }

    /// Whether this link marks the session id as a process default.
    fn defaults_session(&self) -> bool {
        self.relation == ProvenanceRelation::DerivedFrom
            && self.source == DEFAULTED_PROCESS_SESSION_SOURCE
            && self.reference.as_deref() == Some(DEFAULTED_SESSION_REFERENCE)
    }
}

impl ActorContext {
    /// Whether this actor id was supplied by a shell default (the OS user or
    /// a synthetic process actor) rather than asserted by the caller or host.
    #[must_use]
    pub(crate) fn actor_defaulted(&self) -> bool {
        self.provenance_chain
            .iter()
            .any(ProvenanceLink::defaults_actor)
    }

    /// Retry comparison excludes only the known shell-default audit markers.
    /// Principals, assurance, host context and all other provenance still bind
    /// the intent. Persist the original actor, never this comparison projection.
    pub(crate) fn retry_stable(&self) -> Self {
        let mut actor = self.clone();
        actor
            .provenance_chain
            .retain(|link| !(link.defaults_actor() || link.defaults_session()));
        actor
    }

    /// Validates the optional actor-context provenance contract.
    ///
    /// # Errors
    ///
    /// Returns a stable reason when context or its normalization marker is
    /// duplicated, malformed, unsafe for rendering, or outside its byte bound.
    pub fn validate_attribution_context(&self) -> Result<(), &'static str> {
        let context_links = self
            .provenance_chain
            .iter()
            .filter(|link| link.reference.as_deref() == Some(ACTOR_CONTEXT_PROVENANCE_REFERENCE))
            .collect::<Vec<_>>();
        if context_links.len() > 1 {
            return Err("actor context provenance must contain at most one value");
        }
        if let Some(link) = context_links.first()
            && (link.relation != ProvenanceRelation::DerivedFrom
                || link.source.trim() != link.source
                || link.source.is_empty()
                || link.source.len() > MAX_ACTOR_CONTEXT_BYTES
                || link.source.chars().any(is_unsafe_rendered_text_char))
        {
            return Err("actor context provenance is not normalized and bounded");
        }

        let normalized_links = self
            .provenance_chain
            .iter()
            .filter(|link| link.reference.as_deref() == Some(ACTOR_CONTEXT_NORMALIZED_REFERENCE))
            .collect::<Vec<_>>();
        if normalized_links.len() > 1 {
            return Err("actor context normalization provenance must be unique");
        }
        if let Some(link) = normalized_links.first()
            && (link.relation != ProvenanceRelation::DerivedFrom
                || link.source != "actor_context:normalized")
        {
            return Err("actor context normalization provenance is invalid");
        }
        Ok(())
    }

    /// Returns optional host-asserted execution context retained as
    /// attribution provenance.
    #[must_use]
    pub fn attribution_context(&self) -> Option<&str> {
        self.validate_attribution_context().ok()?;
        self.provenance_chain.iter().find_map(|link| {
            (link.relation == ProvenanceRelation::DerivedFrom
                && link.reference.as_deref() == Some(ACTOR_CONTEXT_PROVENANCE_REFERENCE))
            .then_some(link.source.as_str())
        })
    }
}
