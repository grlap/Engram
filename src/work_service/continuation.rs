//! Shared stateless navigation encoding. Tokens are readable, not confidential
//! or authenticated; each owning reader validates its scope and read cut.

use super::{DateTime, StoreError, Utc};
use crate::domain::WorkCatalogReadCut;
use serde::{Serialize, de::DeserializeOwned};
use std::fmt::Write as _;

const MAX_CURSOR_BYTES: usize = 8192;

/// The boundary's index is also the number of older rows. Newer appends
/// leave it unchanged, while additions to or removals from the prefix move it.
pub(super) fn older_rows<T>(
    rows: &[T],
    older: usize,
    cut: &WorkCatalogReadCut,
    now: DateTime<Utc>,
    matches_boundary: impl FnOnce(&T) -> bool,
) -> Result<usize, StoreError> {
    if now < cut.observed_at {
        return Err(StoreError::WorkShowCursorInvalid {
            reason: "the window changed; start a fresh window".into(),
        });
    }
    if !rows.get(older).is_some_and(matches_boundary) {
        return Err(StoreError::WorkShowCursorInvalid {
            reason: "continuation boundary no longer matches this window".into(),
        });
    }
    Ok(older)
}

pub(super) fn encode<T: Serialize>(prefix: &str, value: &T) -> Option<String> {
    let bytes = serde_json::to_vec(value).ok()?;
    let mut token = prefix.to_owned();
    for byte in bytes {
        write!(token, "{byte:02x}").ok()?;
    }
    (token.len() <= MAX_CURSOR_BYTES).then_some(token)
}

pub(super) fn decode<T: DeserializeOwned>(prefix: &str, token: &str) -> Option<T> {
    if token.len() > MAX_CURSOR_BYTES {
        return None;
    }
    let encoded = token.strip_prefix(prefix)?;
    if encoded.len() % 2 != 0 || !encoded.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let bytes = encoded
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let digit = |byte: u8| {
                if byte.is_ascii_digit() {
                    byte - b'0'
                } else {
                    byte.to_ascii_lowercase() - b'a' + 10
                }
            };
            digit(pair[0]) * 16 + digit(pair[1])
        })
        .collect::<Vec<_>>();
    serde_json::from_slice(&bytes).ok()
}
