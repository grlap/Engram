//! Canonical JSON and content-addressing.
//!
//! RFC 8785 bytes are the cross-substrate identity contract. SQLite uses the
//! digest as a row key today; a future Git backend can use the same digest as
//! an object filename without rewriting history.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

use crate::storage::StoreError;

/// A lowercase SHA-256 digest of RFC 8785 canonical JSON bytes.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ObjectHash(String);

impl ObjectHash {
    /// Computes a digest from bytes that are already canonical.
    #[must_use]
    pub fn from_canonical_bytes(bytes: &[u8]) -> Self {
        let digest = Sha256::digest(bytes);
        Self(format!("{digest:x}"))
    }

    /// Returns the digest string used as the persistent object key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn from_stored(value: String) -> Option<Self> {
        if value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Some(Self(value))
        } else {
            None
        }
    }
}

impl fmt::Display for ObjectHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for ObjectHash {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_stored(value.to_owned()).ok_or("expected a lowercase 64-character SHA-256 hash")
    }
}

/// A typed value frozen into canonical bytes and addressed by their digest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalObject {
    hash: ObjectHash,
    bytes: Vec<u8>,
}

impl CanonicalObject {
    /// Canonicalizes and hashes a serializable value.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when JSON serialization or canonicalization
    /// fails.
    pub fn freeze<T: Serialize>(value: &T) -> Result<Self, StoreError> {
        let bytes = serde_json_canonicalizer::to_vec(value)?;
        let hash = ObjectHash::from_canonical_bytes(&bytes);
        Ok(Self { hash, bytes })
    }

    /// Reconstructs a canonical object only when the supplied bytes are RFC
    /// 8785 canonical and match the expected digest.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the bytes are invalid JSON, are not
    /// canonical, or do not match `expected`.
    pub fn verify(expected: &ObjectHash, bytes: Vec<u8>) -> Result<Self, StoreError> {
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let canonical = serde_json_canonicalizer::to_vec(&value)?;
        if canonical != bytes {
            return Err(StoreError::NonCanonicalObject(expected.clone()));
        }

        let actual = ObjectHash::from_canonical_bytes(&bytes);
        if &actual != expected {
            return Err(StoreError::HashMismatch {
                expected: expected.clone(),
                actual,
            });
        }

        Ok(Self {
            hash: expected.clone(),
            bytes,
        })
    }

    /// Returns the content address.
    #[must_use]
    pub fn hash(&self) -> &ObjectHash {
        &self.hash
    }

    /// Returns the immutable canonical representation.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Deserializes the frozen value after integrity verification has occurred.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the canonical bytes cannot be deserialized
    /// into `T`.
    pub fn decode<T: DeserializeOwned>(&self) -> Result<T, StoreError> {
        Ok(serde_json::from_slice(&self.bytes)?)
    }
}

#[cfg(test)]
mod tests {
    use serde::Serialize;

    use super::*;

    #[derive(Serialize)]
    struct OutOfOrder<'a> {
        z: u8,
        a: &'a str,
    }

    #[test]
    fn canonical_identity_is_independent_of_struct_field_order() {
        let object = CanonicalObject::freeze(&OutOfOrder { z: 2, a: "first" }).unwrap();

        assert_eq!(object.bytes(), br#"{"a":"first","z":2}"#);
        assert_eq!(object.hash().as_str().len(), 64);
    }

    #[test]
    fn verification_rejects_noncanonical_json() {
        let bytes = br#"{"z":2,"a":"first"}"#.to_vec();
        let hash = ObjectHash::from_canonical_bytes(&bytes);

        assert!(matches!(
            CanonicalObject::verify(&hash, bytes),
            Err(StoreError::NonCanonicalObject(_))
        ));
    }
}
