//! Canonical JSON, record identity, and content fingerprints.
//!
//! A stored record is identified by an id minted once, when the record is
//! created, and never derived from its bytes: changing how a record is written
//! never changes its identity or any link to it, so a format change is a plain
//! reshaping of JSON. Hashing survives only as a fingerprint, for the places
//! that compare content (an idempotent retry's intent, a frozen report, a
//! format identity); a fingerprint is never an identity and never a link.

use std::fmt;
use std::str::FromStr;

#[cfg(test)]
use std::cell::Cell;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

use crate::storage::StoreError;

/// Serializes a value to RFC 8785 canonical JSON bytes without deriving an id
/// or content fingerprint.
///
/// # Errors
///
/// Returns [`StoreError`] when JSON serialization or canonicalization fails.
pub(crate) fn canonical_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, StoreError> {
    Ok(serde_json_canonicalizer::to_vec(value)?)
}

#[cfg(test)]
thread_local! {
    static CANONICAL_DECODE_COUNT: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_canonical_decode_count() {
    CANONICAL_DECODE_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn canonical_decode_count() -> usize {
    CANONICAL_DECODE_COUNT.with(Cell::get)
}

/// A record id or a content fingerprint, as lowercase hex.
///
/// A record id is 32 hex digits of a random UUID minted at creation. Records
/// written before ids were minted keep the 64-digit value they were stored
/// under; it is an opaque id like any other and is never recomputed or
/// compared with their bytes. A content fingerprint is the 64-digit SHA-256 of
/// canonical bytes and is used only to compare content.
/// This shared scalar representation does not make those two roles equivalent;
/// the owning field or constructor determines the role. Renaming the record-id
/// API does not change existing serialized ids. Fingerprints of definitions
/// that include Rust type names, such as the graph snapshot schema, can change.
#[derive(Clone, Debug, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ObjectId(String);

impl ObjectId {
    /// Fingerprints bytes that are already canonical. Not an identity.
    #[must_use]
    pub fn from_canonical_bytes(bytes: &[u8]) -> Self {
        let digest = Sha256::digest(bytes);
        Self(format!("{digest:x}"))
    }

    /// Mints the identity of a new record: a random UUID, independent of the
    /// record's bytes. Random rather than time-ordered, so a short prefix of
    /// it is a usable locator.
    #[must_use]
    pub fn mint() -> Self {
        Self(uuid::Uuid::new_v4().simple().to_string())
    }

    /// Returns the persistent key string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn from_stored(value: String) -> Option<Self> {
        if matches!(value.len(), 32 | 64)
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

impl fmt::Display for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for ObjectId {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_stored(value.to_owned()).ok_or("expected a lowercase hex record id")
    }
}

impl<'de> Deserialize<'de> for ObjectId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_stored(value)
            .ok_or_else(|| serde::de::Error::custom("expected a lowercase hex record id"))
    }
}

/// A typed value in canonical JSON bytes, carrying either the id of a stored
/// record or the fingerprint of compared content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalObject {
    key: ObjectId,
    bytes: Vec<u8>,
}

impl CanonicalObject {
    /// Serializes a new record and mints its identity. The id does not depend
    /// on the bytes: the same value minted twice is two records.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when JSON serialization or canonicalization
    /// fails.
    pub fn mint<T: Serialize>(value: &T) -> Result<Self, StoreError> {
        Ok(Self {
            key: ObjectId::mint(),
            bytes: canonical_bytes(value)?,
        })
    }

    /// Canonicalizes a value under an id the caller already holds, such as a
    /// record read back from a file that names it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when JSON serialization or canonicalization
    /// fails.
    pub fn identified<T: Serialize>(id: &ObjectId, value: &T) -> Result<Self, StoreError> {
        Ok(Self {
            key: id.clone(),
            bytes: canonical_bytes(value)?,
        })
    }

    /// Canonicalizes a value and fingerprints it, for comparing content. The
    /// fingerprint is not a record identity and must not be stored as a link.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when JSON serialization or canonicalization
    /// fails.
    pub fn freeze<T: Serialize>(value: &T) -> Result<Self, StoreError> {
        let bytes = canonical_bytes(value)?;
        let key = ObjectId::from_canonical_bytes(&bytes);
        Ok(Self { key, bytes })
    }

    /// The bytes stored under `id`, as they are. They are checked to be JSON
    /// and nothing more: nothing is derived from the id and nothing is compared
    /// with it, because a record's identity does not depend on how it is
    /// written, and the database owns corruption detection.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the bytes are not valid JSON.
    pub fn stored(id: &ObjectId, bytes: Vec<u8>) -> Result<Self, StoreError> {
        serde_json::from_slice::<serde::de::IgnoredAny>(&bytes)?;
        Ok(Self {
            key: id.clone(),
            bytes,
        })
    }

    /// Returns the record id, or the fingerprint of compared content.
    #[must_use]
    pub fn key(&self) -> &ObjectId {
        &self.key
    }

    /// Returns the immutable canonical representation.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Deserializes the canonical bytes. Nothing about the id is checked here or
    /// anywhere: the bytes are read as they are stored.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the canonical bytes cannot be deserialized
    /// into `T`.
    pub fn decode<T: DeserializeOwned>(&self) -> Result<T, StoreError> {
        Self::decode_bytes(&self.bytes)
    }

    /// Decodes stored JSON without inventing a record id or a checksum for it.
    pub(crate) fn decode_bytes<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, StoreError> {
        #[cfg(test)]
        CANONICAL_DECODE_COUNT.with(|count| count.set(count.get().saturating_add(1)));
        Ok(serde_json::from_slice(bytes)?)
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
    fn canonical_content_is_independent_of_struct_field_order() {
        let object = CanonicalObject::freeze(&OutOfOrder { z: 2, a: "first" }).unwrap();

        assert_eq!(object.bytes(), br#"{"a":"first","z":2}"#);
        assert_eq!(object.key().as_str().len(), 64);
    }

    // Identity is minted, not derived: the same value is two records with two
    // ids, and a record loads under its id however its bytes are written.
    #[test]
    fn a_record_identity_is_minted_and_never_derived_from_its_bytes() {
        let value = OutOfOrder { z: 2, a: "first" };
        let first = CanonicalObject::mint(&value).unwrap();
        let second = CanonicalObject::mint(&value).unwrap();
        assert_eq!(first.bytes(), second.bytes());
        assert_ne!(first.key(), second.key());
        assert_eq!(first.key().as_str().len(), 32);

        let reshaped = br#"{"z":2,"a":"first","added":true}"#.to_vec();
        let loaded = CanonicalObject::stored(first.key(), reshaped.clone()).unwrap();
        assert_eq!(loaded.key(), first.key());
        assert_eq!(loaded.bytes(), reshaped.as_slice());
        assert!(CanonicalObject::stored(first.key(), b"not json".to_vec()).is_err());
    }

    // A fingerprint compares content and is stable; it is not an identity.
    #[test]
    fn a_fingerprint_is_stable_for_equal_content() {
        let left = CanonicalObject::freeze(&OutOfOrder { z: 2, a: "first" }).unwrap();
        let right = CanonicalObject::freeze(&OutOfOrder { z: 2, a: "first" }).unwrap();
        assert_eq!(left.key(), right.key());
        assert_eq!(left.key().as_str().len(), 64);
    }

    #[test]
    fn id_deserialization_accepts_minted_and_earlier_ids_and_rejects_other_text() {
        assert!(serde_json::from_str::<ObjectId>(r#""bogus""#).is_err());
        assert!(serde_json::from_str::<ObjectId>(&format!(r#""{}""#, "A".repeat(64))).is_err());
        assert!(serde_json::from_str::<ObjectId>(&format!(r#""{}""#, "a".repeat(64))).is_ok());
        let minted = ObjectId::mint();
        assert_eq!(
            serde_json::from_str::<ObjectId>(&format!(r#""{minted}""#)).unwrap(),
            minted
        );
    }
}
