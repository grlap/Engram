//! Host-measured Git identity and the restriction on a native check's use.

use serde::{Deserialize, Deserializer, Serialize, de};

/// A full Git tree object name, independent of Engram's minted record ids.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct GitTreeId(String);

pub(super) fn full_git_object_name(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

impl GitTreeId {
    /// Validate the Git spelling without normalizing it.
    ///
    /// # Errors
    /// Refuses anything other than 40 or 64 lowercase ASCII hex characters.
    pub fn parse(value: String) -> Result<Self, &'static str> {
        if full_git_object_name(&value) {
            Ok(Self(value))
        } else {
            Err(
                "source_basis.git_tree / applicability.tree must be 40 or 64 lowercase ASCII hex characters",
            )
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for GitTreeId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer).map_err(|_| {
            de::Error::custom(
                "source_basis.git_tree / applicability.tree must be a Git tree string",
            )
        })?;
        Self::parse(value).map_err(de::Error::custom)
    }
}

/// A closed restriction; omission on a check means unrestricted.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerificationApplicability {
    GitTree { tree: GitTreeId },
}

impl<'de> Deserialize<'de> for VerificationApplicability {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            kind: String,
            tree: GitTreeId,
        }
        let wire = Wire::deserialize(deserializer).map_err(|error| {
            let message = error.to_string();
            if [
                "unknown field `",
                "missing field `",
                "duplicate field `",
                "source_basis.git_tree / applicability.tree",
            ]
            .iter()
            .any(|prefix| message.starts_with(prefix))
            {
                de::Error::custom(message)
            } else {
                de::Error::custom("applicability must be an object with string kind and Git tree")
            }
        })?;
        if wire.kind != "git_tree" {
            return Err(de::Error::custom("applicability.kind must be git_tree"));
        }
        Ok(Self::GitTree { tree: wire.tree })
    }
}

pub(super) fn present_git_tree<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<GitTreeId>, D::Error> {
    GitTreeId::deserialize(deserializer).map(Some)
}

pub(super) fn present_applicability<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<VerificationApplicability>, D::Error> {
    VerificationApplicability::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ExecutionSourceBasis, VerificationEvidenceMismatch};
    use serde_json::json;

    fn basis(tree: Option<&str>) -> ExecutionSourceBasis {
        let mut value = json!({"workspace_id":"workspace", "source_revision":"content"});
        if let Some(tree) = tree {
            value["git_tree"] = json!(tree);
        }
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn git_tree_wire_preserves_absence_and_refuses_malformed_present_values() {
        let old = json!({"workspace_id":"workspace", "source_revision":"content"});
        let decoded: ExecutionSourceBasis = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(
            crate::CanonicalObject::freeze(&old).unwrap().bytes(),
            crate::CanonicalObject::freeze(&decoded).unwrap().bytes()
        );
        for length in [40, 64] {
            assert_eq!(
                basis(Some(&"a".repeat(length))).git_tree.unwrap().as_str(),
                "a".repeat(length)
            );
        }
        for bad in [
            json!(null),
            json!(false),
            json!(12),
            json!("a".repeat(39)),
            json!("a".repeat(41)),
            json!("A".repeat(40)),
            json!("g".repeat(64)),
            json!("private-malformed-value"),
        ] {
            let mut value = old.clone();
            value["git_tree"] = bad;
            let error = serde_json::from_value::<ExecutionSourceBasis>(value)
                .unwrap_err()
                .to_string();
            assert!(error.contains("git_tree"), "{error}");
            assert!(!error.contains("private-malformed-value"), "{error}");
        }
        for invalid in [
            json!(null),
            json!({"kind":"private-invalid-kind","tree":"a".repeat(40)}),
            json!({"kind":"git_tree","tree":null}),
            json!({"kind":"git_tree","tree":"a".repeat(40),"undeclared":null}),
        ] {
            let error = serde_json::from_value::<VerificationApplicability>(invalid)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("applicability") || error.contains("undeclared"),
                "{error}"
            );
            assert!(!error.contains("private-invalid-kind"), "{error}");
        }
    }

    #[test]
    fn git_tree_optional_restriction_distinguishes_omission_from_present_null() {
        let old = json!({"producer_observation":{"kind":"observation_id","observation_id":"check"},"check_kind":"test"});
        let decoded: crate::domain::VerificationEvidenceInput =
            serde_json::from_value(old.clone()).unwrap();
        assert_eq!(
            crate::CanonicalObject::freeze(&old).unwrap().bytes(),
            crate::CanonicalObject::freeze(&decoded).unwrap().bytes()
        );
        let mut null = old.clone();
        null["applicability"] = serde_json::Value::Null;
        let error = serde_json::from_value::<crate::domain::VerificationEvidenceInput>(null)
            .unwrap_err()
            .to_string();
        assert!(error.contains("applicability"), "{error}");
        let duplicate = format!(
            "{{\"kind\":\"git_tree\",\"kind\":\"git_tree\",\"tree\":\"{}\"}}",
            "a".repeat(40)
        );
        assert!(
            serde_json::from_str::<VerificationApplicability>(&duplicate)
                .unwrap_err()
                .to_string()
                .contains("duplicate field")
        );
    }

    #[test]
    fn git_tree_applicability_checks_each_selected_record_without_a_history_veto() {
        use crate::control::check_verification_applicability as applies;
        let a = basis(Some(&"a".repeat(40)));
        let b = basis(Some(&"b".repeat(40)));
        let absent = basis(None);
        let restricted_a = VerificationApplicability::GitTree {
            tree: a.git_tree.clone().unwrap(),
        };
        let restricted_b = VerificationApplicability::GitTree {
            tree: b.git_tree.clone().unwrap(),
        };
        assert_eq!(
            applies(Some(&restricted_a), Some(&b)),
            Err(VerificationEvidenceMismatch::GitTreeMismatch)
        );
        assert_eq!(
            applies(Some(&restricted_a), Some(&absent)),
            Err(VerificationEvidenceMismatch::GitTreeMissing)
        );
        assert_eq!(
            applies(Some(&restricted_a), None),
            Err(VerificationEvidenceMismatch::GitTreeMissing)
        );
        assert_eq!(applies(None, Some(&b)), Ok(()));
        assert_eq!(applies(Some(&restricted_b), Some(&b)), Ok(()));
        assert_eq!(applies(Some(&restricted_a), Some(&a)), Ok(()));
        // Commit identity is intentionally absent from this predicate.
        assert_eq!(a.source_revision, b.source_revision);
    }
}
