//! Portable project-memory text and asserted source attribution.
//!
//! Shared identities name text revisions. They neither restore native objects
//! nor authenticate a source actor or grant local execution authority.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    ActorContext, ObjectId, ProjectId,
    domain::{ProjectMemoryRetiringTarget, ProvenanceLink, ProvenanceRelation},
    storage::StoreError,
};

pub const FORMAT: &str = "engram-project-memories-v1";
pub const IMPORT_MARKER: &str = "engram:project-memory-import:v1";
pub const RETIREMENT_MARKER: &str = "engram:project-memory-retirement:v1";
pub(crate) const TERMINAL_SOURCE: &str = "engram:project-memory-sync-terminal:v1";
const MAX_TAG_BYTES: usize = 4096;
const MAX_FILE_BYTES: usize = 1024 * 1024;

/// The original asserted author, distinct from the importing local recorder.
/// Session/run/tool/path custody deliberately does not enter shared text.
pub use crate::domain::ProjectMemoryReportedSource as Source;

impl Source {
    #[must_use]
    pub fn from_actor(actor: &ActorContext, remembered_at: DateTime<Utc>) -> Self {
        Self {
            actor_id: actor.actor_id.clone(),
            actor_kind: actor.actor_kind.clone(),
            actor_context: actor.attribution_context().map(str::to_owned),
            reason: actor.reason.clone(),
            remembered_at,
        }
    }

    fn validate(&self) -> Result<(), StoreError> {
        for value in [&self.actor_id, &self.actor_kind, &self.reason] {
            if value.trim().is_empty() || value.len() > 4096 {
                return Err(invalid(
                    "source attribution text outside 1..4096 UTF-8 bytes",
                ));
            }
        }
        if self.actor_context.as_ref().is_some_and(|v| {
            v.trim().is_empty() || v.len() > crate::domain::MAX_ACTOR_CONTEXT_BYTES
        }) {
            return Err(invalid("source actor context outside its admitted bound"));
        }
        Ok(())
    }
}

/// Flat metadata retained in one recognized local provenance link. It carries
/// enough source state to render a relay without wrapping the import recorder.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImportProvenance {
    pub format: String,
    pub project_id: ProjectId,
    pub key: String,
    pub revision_id: ObjectId,
    pub previous: Option<ObjectId>,
    pub body_sha256: String,
    pub retiring_target: Option<ProjectMemoryRetiringTarget>,
    pub source_clear: bool,
    pub source: Source,
}

impl ImportProvenance {
    /// Parses only the versioned marked string accepted by `remember`.
    ///
    /// # Errors
    /// Refuses an unknown marker, unknown field, unsupported identity or bound.
    pub fn parse(value: &str) -> Result<Self, StoreError> {
        if value.len() > MAX_TAG_BYTES {
            return Err(invalid("import provenance exceeds 4096 UTF-8 bytes"));
        }
        let json = value
            .strip_prefix(IMPORT_MARKER)
            .and_then(|v| v.strip_prefix(':'))
            .ok_or_else(|| {
                invalid(
                    "import provenance requires engram:project-memory-import:v1: followed by JSON",
                )
            })?;
        let tag: Self = serde_json::from_str(json)
            .map_err(|e| invalid(&format!("invalid import provenance: {e}")))?;
        tag.validate()?;
        Ok(tag)
    }

    /// Serializes a bounded explicit marker for a CLI seed or native import.
    ///
    /// # Errors
    /// Refuses metadata outside the transport or local attribution bounds.
    pub fn marked(&self) -> Result<String, StoreError> {
        self.validate()?;
        let marked = format!("{IMPORT_MARKER}:{}", serde_json::to_string(self)?);
        if marked.len() > MAX_TAG_BYTES {
            return Err(invalid("import provenance exceeds 4096 UTF-8 bytes"));
        }
        Ok(marked)
    }

    pub(crate) fn validate(&self) -> Result<(), StoreError> {
        if self.format != FORMAT || self.project_id.0.trim().is_empty() {
            return Err(invalid("unsupported memory format or empty project"));
        }
        validate_key(&self.key)?;
        validate_id(&self.revision_id)?;
        if let Some(previous) = &self.previous {
            validate_id(previous)?;
            if previous == &self.revision_id {
                return Err(invalid("revision cannot precede itself"));
            }
        }
        if self.body_sha256.len() != 64
            || !self
                .body_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(invalid("body_sha256 must be 64 lowercase hex digits"));
        }
        if self.source_clear && self.retiring_target.is_some() {
            return Err(invalid("source clear cannot carry a target"));
        }
        self.source.validate()
    }

    /// Binds asserted source metadata to the actual local capture. This checks
    /// content; it does not attest that a caller owns the shared identity.
    ///
    /// # Errors
    /// Refuses a different project/key/body before a store is opened for write.
    pub fn validate_capture(
        &self,
        project: &ProjectId,
        key: Option<&str>,
        body: &str,
    ) -> Result<(), StoreError> {
        self.validate()?;
        if &self.project_id != project || key != Some(self.key.as_str()) {
            return Err(invalid("import provenance project/key mismatch"));
        }
        if body_sha256(body) != self.body_sha256 {
            return Err(invalid("import provenance body SHA-256 mismatch"));
        }
        Ok(())
    }

    /// Adds asserted source metadata without changing the local recorder.
    /// Ordinary memory admission still enforces the full actor's bounds.
    ///
    /// # Errors
    /// Refuses duplicate source tags or metadata outside its byte bound.
    pub fn attach(&self, actor: &mut ActorContext) -> Result<(), StoreError> {
        if actor
            .provenance_chain
            .iter()
            .any(|link| link.source == IMPORT_MARKER)
        {
            return Err(invalid("duplicate memory import provenance"));
        }
        actor.provenance_chain.push(ProvenanceLink {
            relation: ProvenanceRelation::DerivedFrom,
            source: IMPORT_MARKER.into(),
            reference: Some(self.marked()?),
        });
        Ok(())
    }

    /// Recognizes only a single correctly marked asserted import link.
    ///
    /// # Errors
    /// Refuses malformed or repeated recognized links instead of treating them
    /// as newly authored revisions on a relay.
    pub fn from_actor(actor: &ActorContext) -> Result<Option<Self>, StoreError> {
        let mut found = None;
        for link in &actor.provenance_chain {
            if link.source != IMPORT_MARKER {
                continue;
            }
            if found.is_some() || link.relation != ProvenanceRelation::DerivedFrom {
                return Err(invalid("duplicate or invalid memory import link"));
            }
            found =
                Some(Self::parse(link.reference.as_deref().ok_or_else(
                    || invalid("memory import link has no reference"),
                )?)?);
        }
        Ok(found)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Revision {
    pub provenance: ImportProvenance,
    pub body: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Head {
    Active { chain: Vec<ObjectId> },
    Tombstone { source: Source },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KeyHistory {
    pub format: String,
    pub project_id: ProjectId,
    pub key: String,
    pub head: Head,
    pub revisions: BTreeMap<ObjectId, Revision>,
}

/// Compared inputs retained by the caller for an explicit agent merge.
/// All three histories are text selections, never host execution authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MergeInputs {
    pub common: Option<KeyHistory>,
    pub local: KeyHistory,
    pub incoming: KeyHistory,
}

impl MergeInputs {
    pub(crate) fn validate_selection(&self, selected: &KeyHistory) -> Result<usize, StoreError> {
        selected.validate()?;
        self.local.validate()?;
        self.incoming.validate()?;
        for input in [&self.local, &self.incoming]
            .into_iter()
            .chain(self.common.iter())
        {
            input.validate()?;
            if input.project_id != selected.project_id || input.key != selected.key {
                return Err(invalid("memory merge input project/key mismatch"));
            }
        }
        if matches!(selected.head, Head::Tombstone { .. }) {
            return Ok(0);
        }
        let Head::Active { chain: local } = &self.local.head else {
            return Err(invalid("retirement cannot merge into active memory"));
        };
        let Head::Active { chain: incoming } = &self.incoming.head else {
            return Err(invalid("retirement cannot merge into active memory"));
        };
        let common = local
            .iter()
            .zip(incoming)
            .take_while(|(a, b)| a == b)
            .count();
        match &self.common {
            Some(basis) => {
                let Head::Active { chain } = &basis.head else {
                    return Err(invalid("active merge has retired common basis"));
                };
                if chain.as_slice() != &local[..common] {
                    return Err(invalid(
                        "memory merge common basis differs from compared prefix",
                    ));
                }
                for id in chain {
                    if basis.revisions.get(id) != self.local.revisions.get(id)
                        || basis.revisions.get(id) != self.incoming.revisions.get(id)
                    {
                        return Err(invalid("memory merge rewrites common revision bytes"));
                    }
                }
            }
            None if common != 0 => {
                return Err(invalid("memory merge requires its nonempty common basis"));
            }
            None => {}
        }
        let Head::Active { chain } = &selected.head else {
            unreachable!()
        };
        if chain.len() != common + 1 || chain[..common] != local[..common] {
            return Err(invalid(
                "agent merge must select common prefix plus one new result",
            ));
        }
        for id in &chain[..common] {
            if selected.revisions.get(id) != self.local.revisions.get(id) {
                return Err(invalid("agent merge rewrites selected prefix"));
            }
        }
        if self.local.revisions.contains_key(&chain[common])
            || self.incoming.revisions.contains_key(&chain[common])
        {
            return Err(invalid(
                "agent merge result must have a fresh authored identity",
            ));
        }
        Ok(common)
    }
}

/// One per-key import fence and explicitly selected reconciliation evidence.
pub struct ImportRequest<'a> {
    pub selected: &'a KeyHistory,
    pub expected_local_head: Option<&'a ObjectId>,
    pub actor: &'a ActorContext,
    pub session_id: &'a crate::SessionId,
    pub now: DateTime<Utc>,
    pub dry_run: bool,
    pub merge: Option<&'a MergeInputs>,
    pub seed: Option<&'a SeedManifestEntry>,
    /// Operator-only fallback for an old seed without a stored source tag.
    pub mark_untagged_seed: bool,
}

/// Explicit per-key agent selections; their presence grants no authority.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImportOptions {
    pub merge: Option<MergeInputs>,
    pub seed: Option<SeedManifestEntry>,
    #[serde(default)]
    pub mark_untagged_seed: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ImportOutcome {
    pub key: String,
    pub appended_revisions: usize,
    pub retired: bool,
    pub duplicate: bool,
    pub dry_run: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum KeyImportResult {
    Applied { key: String, outcome: ImportOutcome },
    Refused { key: String, reason: String },
}

#[derive(Clone, Debug, Serialize)]
pub struct FolderImportOutcome {
    pub tasks: crate::storage::TaskSyncImportResult,
    pub memories: Vec<KeyImportResult>,
    pub memory_partial: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct WithheldKey {
    pub key: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ExportOutcome {
    pub keys: BTreeMap<String, KeyHistory>,
    pub withheld: Vec<WithheldKey>,
}

impl KeyHistory {
    /// Finds the selected shared anchor of one tagged local revision.
    /// The tag is asserted provenance; identity and full portable equality
    /// must both agree before a relay or seeded rebaseline uses it.
    ///
    /// # Errors
    /// Refuses a retired key, absent identity, changed body or source metadata.
    pub fn imported_anchor(
        &self,
        tag: &ImportProvenance,
        local_body: &str,
    ) -> Result<usize, StoreError> {
        self.validate()?;
        tag.validate_capture(&self.project_id, Some(&self.key), local_body)?;
        let Head::Active { chain } = &self.head else {
            return Err(invalid("seed anchor belongs to a retired shared key"));
        };
        let index = chain
            .iter()
            .position(|id| id == &tag.revision_id)
            .ok_or_else(|| invalid("seed anchor id absent from selected head chain"))?;
        let shared = self
            .revisions
            .get(&tag.revision_id)
            .ok_or_else(|| invalid("seed anchor revision file missing"))?;
        if &shared.provenance != tag || shared.body != local_body {
            return Err(invalid("seed anchor shared content or provenance mismatch"));
        }
        Ok(index)
    }

    /// Admits a whole key, including every selected predecessor and body.
    ///
    /// # Errors
    /// Refuses structural unions, unreferenced files, missing predecessors,
    /// non-UUID revision identities, body mismatches and per-file overflows.
    pub fn validate(&self) -> Result<(), StoreError> {
        validate_key(&self.key)?;
        if self.format != FORMAT || self.project_id.0.trim().is_empty() {
            return Err(invalid("unsupported memory format or empty project"));
        }
        match &self.head {
            Head::Tombstone { source } => {
                source.validate()?;
                if !self.revisions.is_empty() {
                    return Err(invalid("tombstone must not contain revision bodies"));
                }
            }
            Head::Active { chain } => {
                if chain.is_empty() || chain.len() != self.revisions.len() {
                    return Err(invalid(
                        "active head must select exactly every revision file",
                    ));
                }
                let mut seen = BTreeSet::new();
                let mut previous = None;
                for id in chain {
                    if !seen.insert(id) {
                        return Err(invalid("duplicate revision in head"));
                    }
                    let revision = self
                        .revisions
                        .get(id)
                        .ok_or_else(|| invalid("head names missing revision file"))?;
                    let tag = &revision.provenance;
                    tag.validate_capture(&self.project_id, Some(&self.key), &revision.body)?;
                    if &tag.revision_id != id || tag.previous.as_ref() != previous {
                        return Err(invalid("head is not one ordered predecessor chain"));
                    }
                    if revision.body.trim().is_empty()
                        || revision.body.len() > crate::domain::MAX_PROJECT_MEMORY_BODY_BYTES
                    {
                        return Err(invalid("memory body outside its admitted bound"));
                    }
                    tag.marked()?;
                    if serde_json::to_vec(revision)?.len() > MAX_FILE_BYTES {
                        return Err(invalid("memory revision exceeds 1 MiB file bound"));
                    }
                    previous = Some(id);
                }
            }
        }
        if serde_json::to_vec(&self.head)?.len() > MAX_FILE_BYTES {
            return Err(invalid("memory head exceeds 1 MiB file bound"));
        }
        Ok(())
    }
}

/// Caller-selected first-seed evidence. It is not a host-private checkpoint
/// and grants no mapping without the tag written into the immutable seed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SeedManifestEntry {
    pub project_id: ProjectId,
    pub key: String,
    pub local_version_id: ObjectId,
    pub shared_revision_id: ObjectId,
    pub body_sha256: String,
}

impl SeedManifestEntry {
    /// Validates an explicit anchor-only rebaseline without changing history.
    /// Returns the index after which ordinary suffix import starts.
    ///
    /// # Errors
    /// Refuses a different local head, manifest binding/hash, missing seed tag,
    /// missing shared identity or changed selected source metadata.
    pub fn validate_anchor(
        &self,
        selected: &KeyHistory,
        local_version_id: &ObjectId,
        local_body: &str,
        tag: Option<&ImportProvenance>,
    ) -> Result<usize, StoreError> {
        if self.project_id != selected.project_id || self.key != selected.key {
            return Err(invalid("seed manifest project/key mismatch"));
        }
        if &self.local_version_id != local_version_id {
            return Err(invalid("seed manifest local head changed"));
        }
        if self.body_sha256 != body_sha256(local_body) {
            return Err(invalid("seed manifest body SHA-256 mismatch"));
        }
        let tag = tag.ok_or_else(|| invalid("seed has no import provenance tag"))?;
        if self.shared_revision_id != tag.revision_id || self.body_sha256 != tag.body_sha256 {
            return Err(invalid("seed manifest does not match stored source tag"));
        }
        selected.imported_anchor(tag, local_body)
    }
}

#[must_use]
pub fn body_sha256(body: &str) -> String {
    format!("{:x}", Sha256::digest(body.as_bytes()))
}

pub(crate) fn validate_id(id: &ObjectId) -> Result<(), StoreError> {
    if ObjectId::from_stored(id.to_string()).is_none() {
        return Err(invalid("shared memory revision must be a stored ObjectId"));
    }
    Ok(())
}

pub(crate) fn validate_key(key: &str) -> Result<(), StoreError> {
    let bytes = key.as_bytes();
    if bytes.is_empty()
        || bytes.len() > crate::domain::MAX_PROJECT_MEMORY_KEY_BYTES
        || !(bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        || !bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(b))
    {
        return Err(invalid("unsafe project memory key"));
    }
    let stem = key.split('.').next().unwrap_or_default();
    if key.ends_with('.')
        || key.ends_with(' ')
        || matches!(stem, "con" | "nul" | "aux" | "prn")
        || ["com", "lpt"].iter().any(|prefix| {
            stem.strip_prefix(*prefix)
                .is_some_and(|n| n.len() == 1 && matches!(n.as_bytes()[0], b'1'..=b'9'))
        })
    {
        return Err(invalid("project memory key is unsafe on Windows"));
    }
    Ok(())
}

fn invalid(message: &str) -> StoreError {
    StoreError::InvalidProjectMemory(message.into())
}

pub mod directory;
#[cfg(test)]
mod tests;
