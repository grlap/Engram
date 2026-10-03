//! Decoding a named-root state by its variant: each state takes exactly its
//! own fields, a field it does not name is refused rather than dropped, and
//! a repeated key is refused before either value is used. The derived
//! internally tagged decoder could not do this, because serde does not
//! enforce `deny_unknown_fields` on a unit variant. Serialization stays
//! derived, so the wire shape is unchanged.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::de::{self, Deserialize, Deserializer, MapAccess, Visitor};

use super::NamedRootState;

const VARIANTS: &[&str] = &["none", "bound", "unbound_by_release"];
const NONE_FIELDS: &[&str] = &["state"];
const BOUND_FIELDS: &[&str] = &["state", "workspace_id", "generation", "named_at"];
const UNBOUND_FIELDS: &[&str] = &["state", "last_generation", "released_at_position"];
const FIELDS: &[&str] = &[
    "state",
    "workspace_id",
    "generation",
    "named_at",
    "last_generation",
    "released_at_position",
];

impl<'de> Deserialize<'de> for NamedRootState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(StateVisitor)
    }
}

/// A key some state names; any other key is refused as it is read.
#[derive(Clone, Copy)]
enum Field {
    State,
    WorkspaceId,
    Generation,
    NamedAt,
    LastGeneration,
    ReleasedAtPosition,
}

impl Field {
    const fn name(self) -> &'static str {
        match self {
            Self::State => "state",
            Self::WorkspaceId => "workspace_id",
            Self::Generation => "generation",
            Self::NamedAt => "named_at",
            Self::LastGeneration => "last_generation",
            Self::ReleasedAtPosition => "released_at_position",
        }
    }
}

impl<'de> Deserialize<'de> for Field {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_identifier(FieldVisitor)
    }
}

struct FieldVisitor;

impl Visitor<'_> for FieldVisitor {
    type Value = Field;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a named-root state field")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Field, E> {
        Ok(match value {
            "state" => Field::State,
            "workspace_id" => Field::WorkspaceId,
            "generation" => Field::Generation,
            "named_at" => Field::NamedAt,
            "last_generation" => Field::LastGeneration,
            "released_at_position" => Field::ReleasedAtPosition,
            other => return Err(E::unknown_field(other, FIELDS)),
        })
    }

    fn visit_bytes<E: de::Error>(self, value: &[u8]) -> Result<Field, E> {
        match std::str::from_utf8(value) {
            Ok(value) => self.visit_str(value),
            Err(_) => Err(E::invalid_value(de::Unexpected::Bytes(value), &self)),
        }
    }
}

struct StateVisitor;

/// A field the state requires, refused by name when absent.
fn required<T, E: de::Error>(value: Option<T>, field: Field) -> Result<T, E> {
    value.ok_or_else(|| E::missing_field(field.name()))
}

/// Reads the next value into an empty slot; a key already read is refused.
fn take<'de, A: MapAccess<'de>, T: Deserialize<'de>>(
    map: &mut A,
    slot: &mut Option<T>,
    field: Field,
) -> Result<(), A::Error> {
    if slot.is_some() {
        return Err(de::Error::duplicate_field(field.name()));
    }
    *slot = Some(map.next_value()?);
    Ok(())
}

impl<'de> Visitor<'de> for StateVisitor {
    type Value = NamedRootState;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a named-root state object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<NamedRootState, A::Error> {
        let mut state: Option<String> = None;
        let mut workspace_id: Option<String> = None;
        let mut generation: Option<i64> = None;
        let mut named_at: Option<DateTime<Utc>> = None;
        let mut last_generation: Option<i64> = None;
        let mut released_at_position: Option<i64> = None;
        while let Some(field) = map.next_key::<Field>()? {
            match field {
                Field::State => take(&mut map, &mut state, field)?,
                Field::WorkspaceId => take(&mut map, &mut workspace_id, field)?,
                Field::Generation => take(&mut map, &mut generation, field)?,
                Field::NamedAt => take(&mut map, &mut named_at, field)?,
                Field::LastGeneration => take(&mut map, &mut last_generation, field)?,
                Field::ReleasedAtPosition => take(&mut map, &mut released_at_position, field)?,
            }
        }
        let state = state.ok_or_else(|| de::Error::missing_field("state"))?;
        let present = [
            (Field::WorkspaceId, workspace_id.is_some()),
            (Field::Generation, generation.is_some()),
            (Field::NamedAt, named_at.is_some()),
            (Field::LastGeneration, last_generation.is_some()),
            (Field::ReleasedAtPosition, released_at_position.is_some()),
        ];
        let only = |allowed: &'static [&'static str]| -> Result<(), A::Error> {
            match present
                .iter()
                .find(|(field, present)| *present && !allowed.contains(&field.name()))
            {
                Some((field, _)) => Err(de::Error::unknown_field(field.name(), allowed)),
                None => Ok(()),
            }
        };
        match state.as_str() {
            "none" => {
                only(NONE_FIELDS)?;
                Ok(NamedRootState::NoRoot)
            }
            "bound" => {
                only(BOUND_FIELDS)?;
                Ok(NamedRootState::Bound {
                    workspace_id: required(workspace_id, Field::WorkspaceId)?,
                    generation: required(generation, Field::Generation)?,
                    named_at: required(named_at, Field::NamedAt)?,
                })
            }
            "unbound_by_release" => {
                only(UNBOUND_FIELDS)?;
                Ok(NamedRootState::UnboundByRelease {
                    last_generation: required(last_generation, Field::LastGeneration)?,
                    released_at_position: required(
                        released_at_position,
                        Field::ReleasedAtPosition,
                    )?,
                })
            }
            other => Err(de::Error::unknown_variant(other, VARIANTS)),
        }
    }
}

#[cfg(test)]
mod tests;
