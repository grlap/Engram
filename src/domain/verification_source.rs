//! A verification retains either its historical revision or the complete shared identity.

use serde::{Deserialize, Serialize};

use super::{ExecutionSourceBasis, GitTreeId, SourceRootState};
use crate::source::SourceId;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum VerificationSourceIdentity {
    Legacy {
        source_revision: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        git_tree: Option<GitTreeId>,
    },
    SourceId {
        source_id: SourceId,
    },
}

/// The legacy alternative serializes exactly as it did before shared identities.
/// A shared identity has no revision string, tree surrogate or cleanliness bit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Wire")]
pub struct VerificationSourceBasis {
    pub workspace_id: String,
    #[serde(flatten)]
    pub identity: VerificationSourceIdentity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_root_generation: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_root_state: Option<SourceRootState>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    workspace_id: String,
    #[serde(default, deserialize_with = "present_revision")]
    source_revision: Option<String>,
    #[serde(default, deserialize_with = "present_source_id")]
    source_id: Option<SourceId>,
    #[serde(
        default,
        deserialize_with = "super::verification_applicability::present_git_tree"
    )]
    git_tree: Option<GitTreeId>,
    #[serde(default)]
    source_root_generation: Option<i64>,
    #[serde(default)]
    source_root_state: Option<SourceRootState>,
}

fn present_revision<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    String::deserialize(d).map(Some)
}

fn present_source_id<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<SourceId>, D::Error> {
    SourceId::deserialize(d).map(Some)
}

impl TryFrom<Wire> for VerificationSourceBasis {
    type Error = &'static str;

    fn try_from(wire: Wire) -> Result<Self, Self::Error> {
        let identity = match (wire.source_revision, wire.source_id, wire.git_tree) {
            (Some(source_revision), None, git_tree) => VerificationSourceIdentity::Legacy {
                source_revision,
                git_tree,
            },
            (None, Some(source_id), None) => VerificationSourceIdentity::SourceId { source_id },
            _ => return Err("source basis requires exactly one revision or complete source_id"),
        };
        Ok(Self {
            workspace_id: wire.workspace_id,
            identity,
            source_root_generation: wire.source_root_generation,
            source_root_state: wire.source_root_state,
        })
    }
}

impl From<ExecutionSourceBasis> for VerificationSourceBasis {
    fn from(basis: ExecutionSourceBasis) -> Self {
        Self {
            workspace_id: basis.workspace_id,
            identity: VerificationSourceIdentity::Legacy {
                source_revision: basis.source_revision,
                git_tree: basis.git_tree,
            },
            source_root_generation: basis.source_root_generation,
            source_root_state: basis.source_root_state,
        }
    }
}

impl VerificationSourceBasis {
    #[must_use]
    pub fn legacy_revision(&self) -> Option<&str> {
        match &self.identity {
            VerificationSourceIdentity::Legacy {
                source_revision, ..
            } => Some(source_revision),
            VerificationSourceIdentity::SourceId { .. } => None,
        }
    }

    #[must_use]
    pub fn source_id(&self) -> Option<&SourceId> {
        match &self.identity {
            VerificationSourceIdentity::SourceId { source_id } => Some(source_id),
            VerificationSourceIdentity::Legacy { .. } => None,
        }
    }

    #[must_use]
    pub fn legacy_basis(&self) -> Option<ExecutionSourceBasis> {
        let VerificationSourceIdentity::Legacy {
            source_revision,
            git_tree,
        } = &self.identity
        else {
            return None;
        };
        Some(ExecutionSourceBasis {
            workspace_id: self.workspace_id.clone(),
            source_revision: source_revision.clone(),
            git_tree: git_tree.clone(),
            source_root_generation: self.source_root_generation,
            source_root_state: self.source_root_state,
        })
    }
}
