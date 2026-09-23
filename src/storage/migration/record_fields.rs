//! One explicit stored-reply conversion; never recurse into user payloads.

use super::{Connection, Json, MigrationError, quoted, refused};
use crate::{CanonicalObject, canonical::canonical_bytes};
use rusqlite::params;
use serde::Serialize;

/// Aggregated report of one named stored-field conversion during import.
/// Zero-count entries are omitted from `ImportReport::rewritten_fields`.
/// This is not an inventory of every changed value: delivery digests,
/// decision hashes, and checkpoint intent hashes are recomputed alongside
/// the reported conversions, without separate entries or counts.
#[derive(Debug, Serialize)]
pub struct RewrittenField {
    /// Destination table containing the converted values.
    pub table: String,
    /// Column containing the converted value or JSON document.
    pub column: String,
    /// Canonical object-kind filter, when the conversion targets one kind.
    pub object_kind: Option<String>,
    /// Named source field or typed path affected by the conversion.
    pub field: String,
    /// Free-text description of the replacement, removal, or recomputation;
    /// this is not necessarily a destination field name.
    pub replacement: String,
    /// Number of renamed or removed typed members, not rows. The sole
    /// comparison entry instead counts converted supersession
    /// `replacement_decision` bindings. Other recomputed fingerprints are
    /// not included in this count.
    pub values: u64,
}

fn report(
    reports: &mut Vec<RewrittenField>,
    table: &str,
    column: &str,
    kind: Option<&str>,
    field: &str,
    replacement: &str,
    values: u64,
) {
    if values == 0 {
        return;
    }
    if let Some(entry) = reports.iter_mut().find(|entry| {
        entry.table == table
            && entry.column == column
            && entry.object_kind.as_deref() == kind
            && entry.field == field
    }) {
        entry.values += values;
    } else {
        reports.push(RewrittenField {
            table: table.into(),
            column: column.into(),
            object_kind: kind.map(str::to_owned),
            field: field.into(),
            replacement: replacement.into(),
            values,
        });
    }
}

/// Paths are literals owned here; `*` visits array members, not arbitrary maps.
fn rename_at(value: &mut Json, path: &[&str], old: &str, new: &str) -> Result<u64, MigrationError> {
    if let Some((head, rest)) = path.split_first() {
        if *head == "*" {
            let mut count = 0;
            if let Some(items) = value.as_array_mut() {
                for item in items {
                    count += rename_at(item, rest, old, new)?;
                }
            }
            return Ok(count);
        }
        return match value.get_mut(*head) {
            Some(child) => rename_at(child, rest, old, new),
            None => Ok(0),
        };
    }
    let Some(object) = value.as_object_mut() else {
        return Ok(0);
    };
    if !object.contains_key(old) {
        return Ok(0);
    }
    if object.contains_key(new) {
        return Err(refused(format!(
            "stored reply contains both {old} and {new}"
        )));
    }
    if let Some(old_value) = object.remove(old) {
        object.insert(new.into(), old_value);
        Ok(1)
    } else {
        Ok(0)
    }
}

fn convert_column(
    connection: &Connection,
    table: &str,
    column: &str,
    kind: Option<&str>,
    field_change: (&str, &str),
    reports: &mut Vec<RewrittenField>,
    rewrite: impl Fn(&mut Json) -> Result<u64, MigrationError>,
) -> Result<(), MigrationError> {
    let filter = if kind.is_some() {
        " AND object_kind = ?1"
    } else {
        ""
    };
    let mut statement = connection.prepare(&format!(
        "SELECT rowid, {} FROM {} WHERE {} IS NOT NULL{filter}",
        quoted(column),
        quoted(table),
        quoted(column),
    ))?;
    let mut rows = statement.query(rusqlite::params_from_iter(kind))?;
    while let Some(row) = rows.next()? {
        let rowid: i64 = row.get(0)?;
        let bytes: Vec<u8> = row.get(1)?;
        let mut value: Json = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            // Leave malformed pending pages for the existing admission reader,
            // which refuses with the session and a private-safe shape diagnosis.
            Err(_) if table == "work_session_state" => continue,
            Err(error) => return Err(error.into()),
        };
        let count = rewrite(&mut value)?;
        if count == 0 {
            continue;
        }
        connection.execute(
            &format!(
                "UPDATE {} SET {} = ?1 WHERE rowid = ?2",
                quoted(table),
                quoted(column)
            ),
            params![canonical_bytes(&value)?, rowid],
        )?;
        report(
            reports,
            table,
            column,
            kind,
            field_change.0,
            field_change.1,
            count,
        );
    }
    Ok(())
}

fn rewrite_grant(grant: &mut Json) -> Result<u64, MigrationError> {
    let count = rename_at(
        grant,
        &["delivery", "delta", "changes", "*"],
        "object_hash",
        "object_id",
    )?;
    if count != 0 {
        // The digest compares the serialized delivery, not a record identity.
        // Both copies must agree before updating them together.
        let paths = [
            "/delivery/page/content_digest",
            "/basis/inline_delivery/content_digest",
        ];
        for path in paths {
            if grant.pointer(path).and_then(Json::as_str).is_none() {
                return Err(refused(format!(
                    "control grant {path} must be an existing string"
                )));
            }
        }
        if grant.pointer(paths[0]) != grant.pointer(paths[1]) {
            return Err(refused("control grant delivery digest copies disagree"));
        }
        let delivery = &grant["delivery"];
        let digest = CanonicalObject::freeze(&serde_json::json!({
            "context": delivery["context"], "delta": delivery["delta"],
        }))?;
        for path in paths {
            let field = grant.pointer_mut(path).ok_or_else(|| {
                refused(format!("control grant {path} must be an existing string"))
            })?;
            *field = Json::from(digest.key().as_str());
        }
    }
    Ok(count)
}

fn remove_empty_leases(grant: &mut Json) -> Result<u64, MigrationError> {
    let Some(basis) = grant.get_mut("basis").and_then(Json::as_object_mut) else {
        return Ok(0);
    };
    let Some(leases) = basis.get("leases") else {
        return Ok(0);
    };
    if leases.as_array().is_none_or(|items| !items.is_empty()) {
        return Err(refused(
            "control grant basis.leases is not an empty array; cannot retire its authority",
        ));
    }
    basis.remove("leases");
    Ok(1)
}

pub(super) fn convert(connection: &Connection) -> Result<Vec<RewrittenField>, MigrationError> {
    let mut reports = Vec::new();
    let history = |value: &mut Json| {
        rename_at(
            value,
            &["focus", "history", "items", "*", "entry"],
            "object_hash",
            "object_id",
        )
    };
    convert_column(
        connection,
        "objects",
        "canonical_json",
        Some("work_protocol_result"),
        ("focus.history.items[].entry.object_hash", "object_id"),
        &mut reports,
        history,
    )?;
    convert_column(
        connection,
        "work_protocol_attempts",
        "result_json",
        None,
        ("focus.history.items[].entry.object_hash", "object_id"),
        &mut reports,
        history,
    )?;
    convert_column(
        connection,
        "work_session_state",
        "tentative_delivery_payload",
        None,
        ("changes[].entry.object_hash", "object_id"),
        &mut reports,
        |value| {
            rename_at(
                value,
                &["changes", "*", "entry"],
                "object_hash",
                "object_id",
            )
        },
    )?;
    convert_column(
        connection,
        "control_turn_grants",
        "grant_json",
        None,
        ("delivery.delta.changes[].object_hash", "object_id"),
        &mut reports,
        rewrite_grant,
    )?;
    convert_column(
        connection,
        "control_turn_grants",
        "grant_json",
        None,
        ("basis.leases", "removed empty field"),
        &mut reports,
        remove_empty_leases,
    )?;

    let mut statement = connection.prepare(
        "SELECT sequence, session_id, idempotency_key, decision_hash, decision_json FROM control_turn_results",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let sequence: i64 = row.get(0)?;
        let session: String = row.get(1)?;
        let request: String = row.get(2)?;
        let old_hash: String = row.get(3)?;
        let bytes: Vec<u8> = row.get(4)?;
        let mut decision: Json = serde_json::from_slice(&bytes)?;
        let count = match decision.get_mut("grant") {
            Some(grant) => rewrite_grant(grant)?,
            None => 0,
        };
        let retired_leases = match decision.get_mut("grant") {
            Some(grant) => remove_empty_leases(grant)?,
            None => 0,
        };
        if count == 0 && retired_leases == 0 {
            continue;
        }
        let converted = CanonicalObject::freeze(&decision)?;
        connection.execute("UPDATE control_turn_results SET decision_hash = ?1, decision_json = ?2 WHERE sequence = ?3",
            params![converted.key().as_str(), converted.bytes(), sequence])?;
        // Supersession binds the compared decision content. Do not translate
        // record ids, or silently repair an unrelated/mismatched binding.
        let mut supersessions = connection.prepare(
            "SELECT rowid, replacement_decision_hash, supersession_json FROM control_turn_grant_supersessions WHERE session_id = ?1 AND replacement_request_key = ?2",
        )?;
        let mut bindings = supersessions.query(params![session, request])?;
        while let Some(binding) = bindings.next()? {
            let rowid: i64 = binding.get(0)?;
            let compared: String = binding.get(1)?;
            let bytes: Vec<u8> = binding.get(2)?;
            let mut value: Json = serde_json::from_slice(&bytes)?;
            if compared != old_hash || value["replacement_decision"].as_str() != Some(&old_hash) {
                return Err(refused(
                    "control supersession replacement decision binding disagrees",
                ));
            }
            value["replacement_decision"] = Json::from(converted.key().as_str());
            connection.execute("UPDATE control_turn_grant_supersessions SET replacement_decision_hash = ?1, supersession_json = ?2 WHERE rowid = ?3",
                params![converted.key().as_str(), canonical_bytes(&value)?, rowid])?;
            report(
                &mut reports,
                "control_turn_grant_supersessions",
                "supersession_json",
                None,
                "replacement_decision",
                "converted decision fingerprint",
                1,
            );
        }
        report(
            &mut reports,
            "control_turn_results",
            "decision_json",
            None,
            "grant.delivery.delta.changes[].object_hash",
            "object_id",
            count,
        );
        report(
            &mut reports,
            "control_turn_results",
            "decision_json",
            None,
            "grant.basis.leases",
            "removed empty field",
            retired_leases,
        );
    }
    // Checkpoint retry intents contain typed references, not user-authored bodies.
    let mut statement = connection.prepare("SELECT sequence, intent_json FROM control_operation_results WHERE operation = 'turn_checkpoint'")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let sequence: i64 = row.get(0)?;
        let bytes: Vec<u8> = row.get(1)?;
        let mut intent: Json = serde_json::from_slice(&bytes)?;
        let mut count = 0;
        if let Some(evidence) = intent
            .get_mut("verification_evidence")
            .and_then(Json::as_array_mut)
        {
            for entry in evidence {
                for field in ["producer_observation", "environment"] {
                    if let Some(reference) = entry.get_mut(field)
                        && reference["kind"] == "object_hash"
                    {
                        let renamed = rename_at(reference, &[], "object_hash", "object_id")?;
                        if renamed == 0 {
                            return Err(refused(
                                "checkpoint object_hash reference lacks its object_hash field",
                            ));
                        }
                        count += renamed;
                        reference["kind"] = Json::from("object_id");
                    }
                }
            }
        }
        if count != 0 {
            let converted = CanonicalObject::freeze(&intent)?;
            connection.execute("UPDATE control_operation_results SET intent_hash = ?1, intent_json = ?2 WHERE sequence = ?3",
                params![converted.key().as_str(), converted.bytes(), sequence])?;
            report(
                &mut reports,
                "control_operation_results",
                "intent_json",
                None,
                "verification_evidence[].{producer_observation,environment}.object_hash",
                "object_id (including kind)",
                count,
            );
        }
    }
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_basis_retirement_is_empty_only_and_does_not_touch_payloads() {
        let body = serde_json::json!({"leases": ["user-owned"]});
        let mut grant = serde_json::json!({"basis": {"leases": [], "claim": "kept"}, "body": body});
        assert_eq!(remove_empty_leases(&mut grant).unwrap(), 1);
        assert_eq!(
            grant,
            serde_json::json!({"basis": {"claim": "kept"}, "body": body})
        );
        assert_eq!(remove_empty_leases(&mut grant).unwrap(), 0);
        for leases in [
            serde_json::json!(["authority"]),
            Json::Null,
            serde_json::json!({}),
        ] {
            grant["basis"]["leases"] = leases;
            let before = grant.clone();
            assert!(
                remove_empty_leases(&mut grant)
                    .unwrap_err()
                    .to_string()
                    .contains("not an empty array")
            );
            assert_eq!(grant, before);
        }
    }

    #[test]
    fn grant_field_conversion_requires_each_existing_digest_copy() {
        for path in ["/delivery/page", "/basis/inline_delivery", "/basis"] {
            for malformed in [Json::Null, serde_json::json!([]), serde_json::json!({})] {
                let mut grant = serde_json::json!({
                    "basis": {"inline_delivery": {"content_digest": "old-comparison"}},
                    "delivery": {
                        "context": null,
                        "page": {"content_digest": "old-comparison"},
                        "delta": {"changes": [{"object_hash": crate::ObjectId::mint()}]}
                    }
                });
                *grant.pointer_mut(path).unwrap() = malformed;
                let error = rewrite_grant(&mut grant).unwrap_err();
                assert!(
                    matches!(&error, MigrationError::Refused(reason)
                    if reason.contains("must be an existing string")),
                    "{path}: {error}"
                );
            }
        }
    }

    #[test]
    fn grant_field_conversion_preserves_payload_and_updates_both_content_comparisons() {
        let id = crate::ObjectId::mint();
        let user_body = serde_json::json!({"object_hash": "user-owned-key"});
        let mut grant = serde_json::json!({
            "basis": {"inline_delivery": {"content_digest": "old-comparison"}},
            "delivery": {
                "context": null,
                "page": {"content_digest": "old-comparison"},
                "delta": {"changes": [{"object_hash": id, "object": user_body}]}
            }
        });
        assert_eq!(rewrite_grant(&mut grant).unwrap(), 1);
        assert_eq!(
            grant["delivery"]["delta"]["changes"][0]["object_id"],
            serde_json::json!(id)
        );
        assert_eq!(
            grant["delivery"]["delta"]["changes"][0]["object"],
            user_body
        );
        let expected = CanonicalObject::freeze(&serde_json::json!({
            "context": null, "delta": grant["delivery"]["delta"],
        }))
        .unwrap();
        assert_eq!(
            grant["delivery"]["page"]["content_digest"],
            expected.key().as_str()
        );
        assert_eq!(
            grant["basis"]["inline_delivery"]["content_digest"],
            expected.key().as_str()
        );
        let converted = grant.clone();
        assert_eq!(rewrite_grant(&mut grant).unwrap(), 0);
        assert_eq!(grant, converted);
        grant["delivery"]["delta"]["changes"][0]["object_hash"] = serde_json::json!(id);
        assert!(
            rewrite_grant(&mut grant)
                .unwrap_err()
                .to_string()
                .contains("both object_hash and object_id")
        );
    }
}
