//! The existing baseline membership fingerprint, shared by typed delta bases.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{TaskDocument, invalid, pretty};
use crate::storage::StoreError;

/// Order-independent sum of exported member leaves modulo 2^256. This is a
/// content comparison, never a record identity or an integrity guarantee.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct InventoryRoot([u8; 32]);

impl InventoryRoot {
    pub const ZERO: Self = Self([0; 32]);

    /// Parse the baseline's existing lowercase hexadecimal representation.
    #[must_use]
    pub fn parse(hex: &str) -> Option<Self> {
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return None;
        }
        let mut bytes = [0; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(hex.get(index * 2..index * 2 + 2)?, 16).ok()?;
        }
        Some(Self(bytes))
    }

    #[must_use]
    pub fn hex(self) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut hex = String::with_capacity(64);
        for byte in self.0 {
            hex.push(char::from(DIGITS[usize::from(byte >> 4)]));
            hex.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
        }
        hex
    }

    /// Add this member's exported path and bytes to the comparison root.
    ///
    /// # Errors
    /// Returns a serialization error when the document cannot be encoded.
    pub fn including(self, document: &TaskDocument) -> Result<Self, StoreError> {
        Ok(self.add(&leaf(document)?))
    }

    /// Remove the exact old member. The caller establishes its membership.
    ///
    /// # Errors
    /// Returns a serialization error when the document cannot be encoded.
    pub fn excluding(self, document: &TaskDocument) -> Result<Self, StoreError> {
        Ok(self.sub(&leaf(document)?))
    }

    /// Big-endian addition with the final carry dropped.
    pub(crate) fn add(self, leaf: &[u8; 32]) -> Self {
        let mut sum = [0; 32];
        let mut carry = false;
        for index in (0..32).rev() {
            let (partial, first) = self.0[index].overflowing_add(leaf[index]);
            let (total, second) = partial.overflowing_add(u8::from(carry));
            sum[index] = total;
            carry = first || second;
        }
        Self(sum)
    }

    /// Big-endian subtraction with the final borrow dropped.
    pub(crate) fn sub(self, leaf: &[u8; 32]) -> Self {
        let mut difference = [0; 32];
        let mut borrow = false;
        for index in (0..32).rev() {
            let (partial, first) = self.0[index].overflowing_sub(leaf[index]);
            let (total, second) = partial.overflowing_sub(u8::from(borrow));
            difference[index] = total;
            borrow = first || second;
        }
        Self(difference)
    }
}

impl FromStr for InventoryRoot {
    type Err = StoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value).ok_or_else(|| invalid("invalid task inventory root"))
    }
}

impl TryFrom<String> for InventoryRoot {
    type Error = StoreError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<InventoryRoot> for String {
    fn from(value: InventoryRoot) -> Self {
        value.hex()
    }
}

impl fmt::Display for InventoryRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.hex())
    }
}

/// The baseline's original path-and-exported-bytes leaf algorithm.
pub(crate) fn leaf(document: &TaskDocument) -> Result<[u8; 32], StoreError> {
    let path = format!("tasks/{}.json", document.id.0);
    let bytes = pretty(document)?;
    let mut hash = Sha256::new();
    hash.update((path.len() as u64).to_le_bytes());
    hash.update(path.as_bytes());
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(&bytes);
    Ok(hash.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_root_preserves_modular_carry_borrow_and_strict_string_encoding() {
        let maximum = InventoryRoot([u8::MAX; 32]);
        let mut one = [0; 32];
        one[31] = 1;
        assert_eq!(maximum.add(&one), InventoryRoot::ZERO);
        assert_eq!(InventoryRoot::ZERO.sub(&one), maximum);
        assert_eq!(maximum.add(&one).sub(&one), maximum);
        let encoded = serde_json::to_value(maximum).unwrap();
        assert_eq!(encoded, maximum.hex());
        assert_eq!(
            serde_json::from_value::<InventoryRoot>(encoded).unwrap(),
            maximum
        );
        for invalid in [
            maximum.hex().to_uppercase(),
            maximum.hex()[1..].into(),
            "x".repeat(64),
        ] {
            assert!(serde_json::from_value::<InventoryRoot>(serde_json::json!(invalid)).is_err());
        }
    }
}
