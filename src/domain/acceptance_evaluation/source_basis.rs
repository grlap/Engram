//! Adapt the historical wire name to the existing source identity owner.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{domain::VerificationSourceIdentity, source::SourceId};

/// Host-measured source identity at evaluation time. Asserted context unless
/// a host channel mediates it; equality with a later measurement is asserted
/// freshness, not independent verification.
#[derive(Clone, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(from = "Wire", into = "Wire")]
pub struct AcceptanceSourceBasis {
    pub workspace_id: Option<String>,
    pub identity: VerificationSourceIdentity,
}

impl std::fmt::Debug for AcceptanceSourceBasis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcceptanceSourceBasis")
            .field("workspace_id", &self.workspace_id)
            .field("source", &self.identity.description())
            .finish()
    }
}

// Omission is handled by the enclosing request's default; present null refuses.
pub(super) fn present<'de, D>(deserializer: D) -> Result<Option<AcceptanceSourceBasis>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    AcceptanceSourceBasis::deserialize(deserializer).map(Some)
}

#[derive(Clone, Deserialize, JsonSchema, Serialize)]
#[serde(untagged)]
enum Wire {
    Legacy(Legacy),
    Shared(Shared),
}

#[derive(Clone, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct Legacy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace_id: Option<String>,
    fingerprint: String,
}

#[derive(Clone, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct Shared {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace_id: Option<String>,
    source_id: SourceId,
}

impl From<Wire> for AcceptanceSourceBasis {
    fn from(wire: Wire) -> Self {
        match wire {
            Wire::Legacy(value) => Self::legacy(value.workspace_id, value.fingerprint),
            Wire::Shared(value) => Self {
                workspace_id: value.workspace_id,
                identity: VerificationSourceIdentity::SourceId {
                    source_id: value.source_id,
                },
            },
        }
    }
}

impl From<AcceptanceSourceBasis> for Wire {
    fn from(value: AcceptanceSourceBasis) -> Self {
        match value.identity {
            VerificationSourceIdentity::Legacy {
                source_revision, ..
            } => Self::Legacy(Legacy {
                workspace_id: value.workspace_id,
                fingerprint: source_revision,
            }),
            VerificationSourceIdentity::SourceId { source_id } => Self::Shared(Shared {
                workspace_id: value.workspace_id,
                source_id,
            }),
        }
    }
}

impl AcceptanceSourceBasis {
    #[must_use]
    pub fn legacy(workspace_id: Option<String>, fingerprint: String) -> Self {
        Self {
            workspace_id,
            identity: VerificationSourceIdentity::Legacy {
                source_revision: fingerprint,
                git_tree: None,
            },
        }
    }

    #[must_use]
    pub fn legacy_fingerprint(&self) -> Option<&str> {
        self.identity.legacy_revision()
    }

    #[must_use]
    pub fn source_id(&self) -> Option<&SourceId> {
        match &self.identity {
            VerificationSourceIdentity::SourceId { source_id } => Some(source_id),
            VerificationSourceIdentity::Legacy { .. } => None,
        }
    }

    pub(crate) fn validate_declaration(
        fingerprint: Option<&str>,
        source_id: Option<&SourceId>,
    ) -> Result<(), crate::StoreError> {
        if fingerprint.is_some() && source_id.is_some() {
            return Err(crate::StoreError::InvalidWork(
                "source_fingerprint and source_id are exclusive".into(),
            ));
        }
        if let Some(source) = source_id {
            source.validate().map_err(|_| {
                crate::StoreError::InvalidWork("source_id has invalid values or ordering".into())
            })?;
            if crate::canonical::canonical_bytes(source)?.len() > crate::source::MAX_JSON_BYTES {
                return Err(crate::StoreError::InvalidWork(
                    "source_id exceeds its owner byte limit; recapture the complete identity"
                        .into(),
                ));
            }
        }
        Ok(())
    }

    /// Assemble a declaration from the two exclusive front-door alternatives.
    ///
    /// # Errors
    /// Refuses a request supplying both a fingerprint and a complete identity.
    pub fn from_declaration(
        fingerprint: Option<String>,
        source_id: Option<SourceId>,
    ) -> Result<Option<Self>, &'static str> {
        Ok(match (fingerprint, source_id) {
            (None, None) => None,
            (Some(value), None) => Some(Self::legacy(None, value)),
            (None, Some(source_id)) => Some(Self {
                workspace_id: None,
                identity: VerificationSourceIdentity::SourceId { source_id },
            }),
            (Some(_), Some(_)) => return Err("source_fingerprint and source_id are exclusive"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> SourceId {
        SourceId {
            commit: "a".repeat(40),
            tree: "b".repeat(40),
            changes: vec![],
            submodules: vec![],
        }
    }

    #[test]
    fn legacy_bytes_are_unchanged_and_shared_is_a_closed_alternative() {
        let legacy = serde_json::json!({"workspace_id":"workspace", "fingerprint":"revision"});
        let value: AcceptanceSourceBasis = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(serde_json::to_value(&value).unwrap(), legacy);
        let shared = serde_json::json!({"source_id":source()});
        let value: AcceptanceSourceBasis = serde_json::from_value(shared.clone()).unwrap();
        assert_eq!(serde_json::to_value(&value).unwrap(), shared);
        for malformed in [
            serde_json::json!({}),
            serde_json::json!({"fingerprint":null}),
            serde_json::json!({"source_id":null}),
            serde_json::json!({"source_id":source(),"fingerprint":"revision"}),
            serde_json::json!({"source_id":source(),"extra":true}),
        ] {
            assert!(serde_json::from_value::<AcceptanceSourceBasis>(malformed).is_err());
        }
    }

    #[test]
    fn dirty_content_and_submodules_use_the_existing_complete_identity_equality() {
        let mut left = source();
        left.changes.push(crate::source::Change {
            path: "file".into(),
            status: " M".into(),
            origin: None,
            blob: Some("c".repeat(40)),
            mode: Some(crate::source::Mode::Regular),
        });
        let mut right = left.clone();
        right.changes[0].blob = Some("d".repeat(40));
        let identity = |source_id| VerificationSourceIdentity::SourceId { source_id };
        assert!(identity(left.clone()).same_source(&identity(left.clone())));
        assert!(!identity(left.clone()).same_source(&identity(right)));
        assert!(
            !identity(left)
                .same_source(&AcceptanceSourceBasis::legacy(None, "git-commit:a".into()).identity)
        );
        let mut child = source();
        child.submodules.push(crate::source::Submodule {
            path: "child".into(),
            commit: "c".repeat(40),
            changes: vec![],
        });
        assert!(!identity(source()).same_source(&identity(child)));
    }
}
