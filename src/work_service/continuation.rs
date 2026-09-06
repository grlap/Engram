//! Shared stateless navigation encoding. Tokens are readable, not confidential
//! or authenticated; each owning reader validates its scope and read cut.

use serde::{Serialize, de::DeserializeOwned};
use std::fmt::Write as _;

const MAX_CURSOR_BYTES: usize = 8192;

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
