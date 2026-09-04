//! Preserve the complete input before canonical hashing or build discrimination.
//! `serde_json::Value` alone silently keeps the last duplicate object member.
//! This visitor is JSON-only: `from_slice` supplies number and nesting limits;
//! it is not an entry point for arbitrary Serde deserializers.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

use crate::storage::StoreError;

pub(super) fn parse(bytes: &[u8]) -> Result<Value, StoreError> {
    serde_json::from_slice::<UniqueValue>(bytes)
        .map(|value| value.0)
        .map_err(|error| StoreError::InvalidGraphSnapshot(error.to_string()))
}

struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(UniqueValueVisitor).map(Self)
    }
}

struct UniqueValueVisitor;

impl<'de> Visitor<'de> for UniqueValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON with unique object members")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<UniqueValue>()? {
            values.push(value.0);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut object: A) -> Result<Value, A::Error> {
        let mut members = Map::new();
        while let Some(key) = object.next_key::<String>()? {
            if members.contains_key(&key) {
                // Do not echo untrusted member names into the refusal.
                return Err(de::Error::custom("duplicate JSON member"));
            }
            members.insert(key, object.next_value::<UniqueValue>()?.0);
        }
        Ok(Value::Object(members))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_detection_recurses_and_compares_decoded_member_names() {
        for bytes in [
            br#"{"key":1,"key":2}"#.as_slice(),
            br#"[{"nested":{"key":null,"k\u0065y":false}}]"#.as_slice(),
        ] {
            assert!(
                matches!(parse(bytes), Err(StoreError::InvalidGraphSnapshot(message)) if message.contains("duplicate JSON member"))
            );
        }
    }

    #[test]
    fn unique_json_keeps_scalar_kinds_and_independent_object_members() {
        let bytes = br#"[{"key":null},{"key":true},-1,18446744073709551615,1.25,"text",{}]"#;
        assert_eq!(
            parse(bytes).expect("unique JSON"),
            serde_json::from_slice::<Value>(bytes).expect("ordinary JSON")
        );
        assert!(parse(b"{} {}").is_err(), "trailing input is not ignored");
    }
}
