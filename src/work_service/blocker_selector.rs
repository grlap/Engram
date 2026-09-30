//! The agent-facing selector of one blocker: `b1-` and the stored blocker id
//! in unpadded base64url, with exactly one spelling per id. It is navigation
//! only: reading one grants nothing, it is no fingerprint and no new id, and
//! every clear it names is still admitted by the same checks as any other.

/// The prefix that names this selector's encoding.
pub(crate) const BLOCKER_SELECTOR_PREFIX: &str = "b1-";

/// A stored blocker id is a UUID; anything far longer is no selector.
const MAX_BLOCKER_SELECTOR_BYTES: usize = 256;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// The one selector of the stored blocker id `blocker_id`.
pub(crate) fn encode(blocker_id: &str) -> String {
    let bytes = blocker_id.as_bytes();
    let mut selector =
        String::with_capacity(BLOCKER_SELECTOR_PREFIX.len() + bytes.len().div_ceil(3) * 4);
    selector.push_str(BLOCKER_SELECTOR_PREFIX);
    for chunk in bytes.chunks(3) {
        let group = chunk
            .iter()
            .enumerate()
            .fold(0_u32, |group, (index, byte)| {
                group | (u32::from(*byte) << (16 - 8 * index))
            });
        for index in 0..=chunk.len() {
            let symbol = (group >> (18 - 6 * index)) & 0x3f;
            selector.push(char::from(ALPHABET[symbol as usize]));
        }
    }
    selector
}

/// The stored blocker id `selector` names, or `None` when it is not exactly
/// the spelling [`encode`] gives some non-empty id: a wrong prefix, a
/// character outside the alphabet, a length no encoding has, or trailing
/// bits a second spelling of the same id would differ in.
pub(crate) fn decode(selector: &str) -> Option<String> {
    if selector.len() > MAX_BLOCKER_SELECTOR_BYTES {
        return None;
    }
    let encoded = selector.strip_prefix(BLOCKER_SELECTOR_PREFIX)?;
    if encoded.is_empty() || encoded.len() % 4 == 1 {
        return None;
    }
    let mut bytes = Vec::with_capacity(encoded.len() / 4 * 3 + 2);
    for chunk in encoded.as_bytes().chunks(4) {
        let mut group = 0_u32;
        for (index, symbol) in chunk.iter().enumerate() {
            let value = ALPHABET.iter().position(|candidate| candidate == symbol)?;
            group |= u32::try_from(value).ok()? << (18 - 6 * index);
        }
        let [_, first, second, third] = group.to_be_bytes();
        bytes.extend_from_slice(&[first, second, third][..chunk.len() - 1]);
    }
    let blocker_id = String::from_utf8(bytes).ok()?;
    (encode(&blocker_id) == selector).then_some(blocker_id)
}

#[cfg(test)]
mod tests {
    use super::{BLOCKER_SELECTOR_PREFIX, decode, encode};

    #[test]
    fn every_id_has_one_selector_that_names_it_back() {
        let id = uuid::Uuid::now_v7().to_string();
        let selector = encode(&id);
        assert!(selector.starts_with(BLOCKER_SELECTOR_PREFIX));
        assert!(
            selector[BLOCKER_SELECTOR_PREFIX.len()..]
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        );
        assert_eq!(decode(&selector).as_deref(), Some(id.as_str()));
        for length in 1..=7 {
            let text = "blocker"[..length].to_owned();
            assert_eq!(decode(&encode(&text)), Some(text));
        }
    }

    #[test]
    fn only_the_one_spelling_decodes() {
        let selector = encode(&uuid::Uuid::now_v7().to_string());
        let body = &selector[BLOCKER_SELECTOR_PREFIX.len()..];
        for refused in [
            String::new(),
            BLOCKER_SELECTOR_PREFIX.to_owned(),
            body.to_owned(),
            format!("b2-{body}"),
            format!("{selector}="),
            format!("{selector}A"),
            selector.replace('-', "+"),
            format!("{BLOCKER_SELECTOR_PREFIX}{}", "A".repeat(300)),
            format!(" {selector}"),
        ] {
            assert_eq!(decode(&refused), None, "{refused:?}");
        }
        // "YQ" and "YR" carry the same byte; only the canonical one decodes.
        assert_eq!(decode("b1-YQ").as_deref(), Some("a"));
        assert_eq!(decode("b1-YR"), None);
    }
}
