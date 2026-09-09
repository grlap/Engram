//! Lossless SQLite cell framing, independent of UTF-8 and domain JSON.

use rusqlite::{Row, types::ValueRef};

use super::{MigrationError, refused};

pub(super) struct Cell<'a>(pub(super) ValueRef<'a>);

impl rusqlite::ToSql for Cell<'_> {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::Borrowed(self.0))
    }
}

pub(super) fn decode(mut bytes: &[u8], count: usize) -> Result<Vec<Cell<'_>>, MigrationError> {
    validate(bytes, count)?;
    let mut result = Vec::with_capacity(count);
    for _ in 0..count {
        let (&tag, remaining) = bytes.split_first().ok_or_else(|| refused("missing cell"))?;
        bytes = remaining;
        let value = match tag {
            0 => ValueRef::Null,
            1 | 2 => {
                let bits: [u8; 8] = bytes
                    .get(..8)
                    .ok_or_else(|| refused("missing numeric cell"))?
                    .try_into()
                    .map_err(|_| refused("invalid numeric cell"))?;
                bytes = &bytes[8..];
                if tag == 1 {
                    ValueRef::Integer(i64::from_be_bytes(bits))
                } else {
                    ValueRef::Real(f64::from_bits(u64::from_be_bytes(bits)))
                }
            }
            3 | 4 => {
                let prefix: [u8; 8] = bytes
                    .get(..8)
                    .ok_or_else(|| refused("missing cell length"))?
                    .try_into()
                    .map_err(|_| refused("invalid cell length"))?;
                let length = usize::try_from(u64::from_be_bytes(prefix))
                    .map_err(|_| refused("cell too large"))?;
                bytes = &bytes[8..];
                let (body, tail) = bytes.split_at(length);
                bytes = tail;
                if tag == 3 {
                    ValueRef::Text(body)
                } else {
                    ValueRef::Blob(body)
                }
            }
            _ => return Err(refused("unknown cell tag")),
        };
        result.push(Cell(value));
    }
    Ok(result)
}

pub(super) fn encode(row: &Row<'_>, count: usize) -> Result<Vec<u8>, MigrationError> {
    let mut result = Vec::new();
    for index in 0..count {
        match row.get_ref(index)? {
            ValueRef::Null => result.push(0),
            ValueRef::Integer(value) => {
                result.push(1);
                result.extend_from_slice(&value.to_be_bytes());
            }
            ValueRef::Real(value) => {
                result.push(2);
                result.extend_from_slice(&value.to_bits().to_be_bytes());
            }
            ValueRef::Text(value) | ValueRef::Blob(value) => {
                result.push(if matches!(row.get_ref(index)?, ValueRef::Text(_)) {
                    3
                } else {
                    4
                });
                result.extend_from_slice(
                    &u64::try_from(value.len())
                        .map_err(|_| refused("cell length exceeds the archive format"))?
                        .to_be_bytes(),
                );
                result.extend_from_slice(value);
            }
        }
    }
    Ok(result)
}

pub(super) fn validate(mut bytes: &[u8], count: usize) -> Result<(), MigrationError> {
    for _ in 0..count {
        let Some((&tag, remaining)) = bytes.split_first() else {
            return Err(refused("truncated row cell"));
        };
        bytes = remaining;
        let length = match tag {
            0 => 0,
            1 | 2 => 8,
            3 | 4 => {
                let prefix = bytes
                    .get(..8)
                    .ok_or_else(|| refused("truncated cell length"))?;
                let length = u64::from_be_bytes(
                    prefix
                        .try_into()
                        .map_err(|_| refused("invalid cell length"))?,
                );
                bytes = &bytes[8..];
                usize::try_from(length).map_err(|_| refused("cell too large for this host"))?
            }
            _ => return Err(refused("unknown SQLite cell tag")),
        };
        bytes = bytes
            .get(length..)
            .ok_or_else(|| refused("truncated cell body"))?;
    }
    if !bytes.is_empty() {
        return Err(refused("extra bytes after row cells"));
    }
    Ok(())
}
