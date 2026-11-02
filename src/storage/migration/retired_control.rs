//! The one explicit conversion from the former turn-admission schema.
//! Remove this conversion after every live store has moved to the current format.

use super::{Json, MigrationError, SqlValue, decode, refused};

#[derive(Clone, Copy)]
enum Cell {
    Text,
    Integer,
    Blob,
    OptionalText,
    OptionalInteger,
}

use Cell::{Blob, Integer, OptionalInteger, OptionalText, Text};

fn columns(table: &str) -> Option<&'static [(&'static str, Cell)]> {
    Some(match table {
        "control_sessions" => &[
            ("session_id", Text),
            ("project_id", Text),
            ("task_id", Text),
            ("root_execution_id", OptionalText),
            ("work_id", OptionalText),
            ("run_id", OptionalText),
            ("work_revision", OptionalInteger),
            ("claim_id", OptionalText),
            ("claim_fence", OptionalInteger),
            ("routing_token", Text),
            ("actor_json", Blob),
            ("bind_key", Text),
            ("bind_intent_hash", Text),
            ("bind_intent_json", Blob),
            ("phase", Text),
            ("assurance", Text),
            ("mediated_effects_json", Text),
            ("confirmed_cursor", Integer),
            ("tentative_cursor", OptionalInteger),
            ("project_policy_epoch", Integer),
            ("task_admission_epoch", Integer),
            ("blocking_watermark", Integer),
            ("capability_map_revision", Integer),
            ("revision", Integer),
            ("updated_at_ms", Integer),
        ],
        "control_turn_results" => &[
            ("sequence", Integer),
            ("session_id", Text),
            ("task_id", Text),
            ("idempotency_key", Text),
            ("intent_hash", Text),
            ("intent_json", Blob),
            ("decision_hash", Text),
            ("decision_json", Blob),
            ("created_at_ms", Integer),
        ],
        "control_turn_grants" => &[
            ("grant_id", Text),
            ("session_id", Text),
            ("task_id", Text),
            ("request_key", Text),
            ("grant_json", Blob),
            ("state", Text),
            ("issued_at_ms", Integer),
            ("expires_at_ms", Integer),
            ("begun_at_ms", OptionalInteger),
            ("completed_at_ms", OptionalInteger),
        ],
        "control_turn_grant_supersessions" => &[
            ("superseded_grant_id", Text),
            ("session_id", Text),
            ("task_id", Text),
            ("replacement_request_key", Text),
            ("replacement_decision_hash", Text),
            ("supersession_json", Blob),
            ("superseded_at_ms", Integer),
        ],
        _ => return None,
    })
}

pub(super) fn declaration(table: &str, declared: &[String]) -> Result<bool, MigrationError> {
    let Some(expected) = columns(table) else {
        return Ok(false);
    };
    for column in declared {
        if !expected.iter().any(|(name, _)| column == name) {
            return Err(refused(format!(
                "unknown column {column} of retired table {table}"
            )));
        }
    }
    for (column, _) in expected {
        if !declared.iter().any(|name| name == column) {
            return Err(refused(format!(
                "retired table {table} lacks column {column}"
            )));
        }
    }
    Ok(true)
}

fn cell(table: &str, column: &str, expected: Cell, value: Json) -> Result<(), MigrationError> {
    let value = decode(value)?;
    let valid = matches!(
        (expected, &value),
        (Text | OptionalText, SqlValue::Text(_))
            | (Integer | OptionalInteger, SqlValue::Integer(_))
            | (Blob, SqlValue::Blob(_))
            | (OptionalText | OptionalInteger, SqlValue::Null)
    );
    if !valid {
        return Err(refused(format!(
            "invalid storage class in {table}.{column}"
        )));
    }
    if column.ends_with("_json") {
        match &value {
            SqlValue::Blob(bytes) => {
                serde_json::from_slice::<Json>(bytes)
                    .map_err(|_| refused(format!("invalid JSON in {table}.{column}")))?;
            }
            SqlValue::Text(text) => {
                serde_json::from_str::<Json>(text)
                    .map_err(|_| refused(format!("invalid JSON in {table}.{column}")))?;
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) fn row(
    table: &str,
    mut values: serde_json::Map<String, Json>,
) -> Result<(), MigrationError> {
    let expected =
        columns(table).ok_or_else(|| refused(format!("unknown retired table {table}")))?;
    for (column, kind) in expected {
        let value = values.remove(*column).ok_or_else(|| {
            refused(format!(
                "a row of retired table {table} lacks column {column}"
            ))
        })?;
        cell(table, column, *kind, value)?;
    }
    if let Some(extra) = values.keys().next() {
        return Err(refused(format!(
            "a row of retired table {table} has undeclared column {extra}"
        )));
    }
    Ok(())
}

pub(super) const POLICY_COLUMNS: &[&str] = &[
    "required_assurance",
    "supported_effects_json",
    "grant_ttl_seconds",
];

pub(super) fn policy_row(values: &mut serde_json::Map<String, Json>) -> Result<(), MigrationError> {
    for column in POLICY_COLUMNS {
        let value = values.remove(*column).ok_or_else(|| {
            refused(format!(
                "control_policy_state lacks retired column {column}"
            ))
        })?;
        cell(
            "control_policy_state",
            column,
            if *column == "grant_ttl_seconds" {
                Integer
            } else {
                Text
            },
            value,
        )?;
    }
    Ok(())
}
