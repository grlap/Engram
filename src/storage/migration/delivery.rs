//! Per-page audit of attribution derived during the explicit source migration.
//! Original staged bytes and tokens remain unchanged. New pages never inherit it.

use rusqlite::{Connection, OptionalExtension, params, types::ValueRef};
use serde::{Deserialize, Serialize};

use crate::{CanonicalObject, ObjectHash, ProjectId, SessionId, SqliteStore, StoreError};

use super::{ExportManifest, TableManifest, rows};

#[cfg(test)]
mod tests;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Attribution {
    source_id: String,
    source_row: i64,
    project: ProjectId,
    session: SessionId,
    confirmed: i64,
    through: i64,
    token: String,
    payload_hash: ObjectHash,
    /// Dense project-feed positions whose absent bit is derived as true.
    /// Absence alone does not establish the age or producer of a payload.
    derived_positions: Vec<i64>,
}

fn invalid(reason: &str) -> StoreError {
    StoreError::InvalidWorkProjection(format!("migration delivery attribution: {reason}"))
}

fn session_table(manifest: &ExportManifest) -> Result<&TableManifest, StoreError> {
    manifest
        .tables
        .iter()
        .find(|t| t.name == "work_session_state")
        .ok_or_else(|| invalid("source session table is missing"))
}

fn derive(
    connection: &Connection,
    table: &TableManifest,
    source: &str,
    row_number: i64,
    encoded: &[u8],
) -> Result<Option<Attribution>, StoreError> {
    let count = table
        .columns
        .iter()
        .filter(|column| column.hidden != 1)
        .count()
        + usize::from(table.rowid_alias.is_some());
    let cells = rows::decode(encoded, count).map_err(|e| invalid(&e.to_string()))?;
    let cell = |name: &str| -> Result<ValueRef<'_>, StoreError> {
        let index = table
            .columns
            .iter()
            .filter(|column| column.hidden != 1)
            .position(|c| c.name == name)
            .ok_or_else(|| invalid("source session column is missing"))?
            + usize::from(table.rowid_alias.is_some());
        Ok(cells[index].0)
    };
    if cell("tentative_project_cursor")? == ValueRef::Null {
        return Ok(None);
    }
    let text = |name| {
        cell(name)?
            .as_str()
            .map_err(|_| invalid("expected text cell"))
    };
    let integer = |name| {
        cell(name)?
            .as_i64()
            .map_err(|_| invalid("expected integer cell"))
    };
    let hash: ObjectHash = text("tentative_delivery_payload_hash")?
        .parse()
        .map_err(|_| invalid("invalid payload address"))?;
    let ValueRef::Blob(bytes) = cell("tentative_delivery_payload")? else {
        return Err(invalid("payload is not a BLOB"));
    };
    let payload = CanonicalObject::verify(&hash, bytes.to_vec())?;
    let raw: serde_json::Value = payload.decode()?;
    let page: crate::work_service::StagedWorkChangePage = payload.decode()?;
    let raw_changes = raw["changes"]
        .as_array()
        .ok_or_else(|| invalid("missing changes"))?;
    let session = SessionId(text("session_id")?.into());
    let mut derived_positions = Vec::new();
    for (change, raw) in page.changes.iter().zip(raw_changes) {
        let original: Vec<u8> = connection.query_row(
            "SELECT canonical_json FROM migration_original_objects WHERE object_hash=?1 AND object_kind=?2",
            params![change.entry.object_hash.as_str(), change.entry.object_kind], |r| r.get(0),
        ).optional()?.ok_or_else(|| invalid("original staged source is missing"))?;
        let object: serde_json::Value =
            CanonicalObject::verify(&change.entry.object_hash, original)?.decode()?;
        let expected = matches!(
            change.delivery,
            crate::work_service::WorkChangeProjection::Visible(_)
        ) && crate::work_service::source_is_from_session(
            &change.entry.object_kind,
            &object,
            &session,
        );
        if change.from_current_session != expected {
            if expected && raw.get("from_current_session").is_none() {
                derived_positions.push(change.entry.position.position);
            } else {
                return Err(invalid(
                    "explicit staged attribution contradicts canonical source",
                ));
            }
        }
    }
    if derived_positions.is_empty() {
        return Ok(None);
    }
    Ok(Some(Attribution {
        source_id: source.into(),
        source_row: row_number,
        project: ProjectId(text("project_id")?.into()),
        session,
        confirmed: integer("project_cursor")?,
        through: integer("tentative_project_cursor")?,
        token: text("tentative_delivery_token")?.into(),
        payload_hash: hash,
        derived_positions,
    }))
}

fn visit(
    connection: &Connection,
    manifest: &ExportManifest,
    source: &str,
    mut visitor: impl FnMut(Attribution) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    let table = session_table(manifest)?;
    let mut query = connection.prepare("SELECT row_number,cells FROM migration_original_rows WHERE source_id=?1 AND table_name='work_session_state' ORDER BY row_number")?;
    let mut rows = query.query([source])?;
    while let Some(row) = rows.next()? {
        if let Some(audit) = derive(
            connection,
            table,
            source,
            row.get(0)?,
            &row.get::<_, Vec<u8>>(1)?,
        )? {
            visitor(audit)?;
        }
    }
    Ok(())
}

pub(super) fn record_attribution(
    connection: &Connection,
    manifest: &ExportManifest,
    source: &str,
) -> Result<(), StoreError> {
    visit(connection, manifest, source, |audit| {
        let object = CanonicalObject::freeze(&audit)?;
        connection.execute(
            "INSERT INTO migration_delivery_attribution VALUES (?1,?2,?3,?4,?5)",
            params![
                audit.project.0,
                audit.session.0,
                audit.payload_hash.as_str(),
                object.bytes(),
                object.hash().as_str()
            ],
        )?;
        Ok(())
    })
}

pub(super) fn verify_attribution(
    connection: &Connection,
    manifest: &ExportManifest,
    source: &str,
) -> Result<(), StoreError> {
    let mut count = 0_i64;
    visit(connection, manifest, source, |audit| {
        let stored = lookup(
            connection,
            &audit.project,
            &audit.session,
            &audit.payload_hash,
        )?;
        if stored.as_ref() != Some(&audit) {
            return Err(invalid(
                "audit differs from original page and canonical source",
            ));
        }
        count += 1;
        Ok(())
    })?;
    let actual: i64 = connection.query_row(
        "SELECT COUNT(*) FROM migration_delivery_attribution",
        [],
        |r| r.get(0),
    )?;
    if actual != count {
        return Err(invalid("unexpected page audit"));
    }
    Ok(())
}

fn lookup(
    connection: &Connection,
    project: &ProjectId,
    session: &SessionId,
    hash: &ObjectHash,
) -> Result<Option<Attribution>, StoreError> {
    let row: Option<(Vec<u8>,String)> = connection.query_row(
        "SELECT audit_json,audit_hash FROM migration_delivery_attribution WHERE project_id=?1 AND session_id=?2 AND payload_hash=?3",
        params![project.0,session.0,hash.as_str()], |r| Ok((r.get(0)?,r.get(1)?)),
    ).optional()?;
    row.map(|(bytes, hash)| {
        let hash = hash.parse().map_err(|_| invalid("invalid audit hash"))?;
        CanonicalObject::verify(&hash, bytes)?.decode()
    })
    .transpose()
}

impl SqliteStore {
    pub(crate) fn migrated_delivery_attribution(
        &self,
        project: &ProjectId,
        session: &SessionId,
        confirmed: i64,
        through: i64,
        payload: &CanonicalObject,
    ) -> Result<Vec<i64>, StoreError> {
        let Some(audit) = lookup(&self.connection, project, session, payload.hash())? else {
            return Ok(Vec::new());
        };
        let token: String = self.connection.query_row(
            "SELECT tentative_delivery_token FROM work_session_state WHERE project_id=?1 AND session_id=?2 AND project_cursor=?3 AND tentative_project_cursor=?4 AND tentative_delivery_payload_hash=?5",
            params![project.0,session.0,confirmed,through,payload.hash().as_str()], |r| r.get(0),
        ).optional()?.ok_or_else(|| invalid("page no longer matches pending state"))?;
        if audit.project != *project
            || audit.session != *session
            || audit.confirmed != confirmed
            || audit.through != through
            || audit.token != token
            || audit.payload_hash != *payload.hash()
        {
            return Err(invalid("audit does not bind this exact pending page"));
        }
        let (profile,document,hash): (String,Vec<u8>,String) = self.connection.query_row(
            "SELECT profile,document,document_hash FROM migration_source_manifest WHERE source_id=?1",[&audit.source_id],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
        ).optional()?.ok_or_else(|| invalid("source manifest is missing"))?;
        if profile != "aggregate-root-v1" || hash != audit.source_id {
            return Err(invalid("source profile mismatch"));
        }
        let hash = hash.parse().map_err(|_| invalid("invalid manifest hash"))?;
        let manifest: ExportManifest = CanonicalObject::verify(&hash, document)?.decode()?;
        let bytes: Vec<u8> = self.connection.query_row(
            "SELECT cells FROM migration_original_rows WHERE source_id=?1 AND table_name='work_session_state' AND row_number=?2",
            params![audit.source_id,audit.source_row], |r| r.get(0),
        ).optional()?.ok_or_else(|| invalid("original session row is missing"))?;
        if derive(
            &self.connection,
            session_table(&manifest)?,
            &audit.source_id,
            audit.source_row,
            &bytes,
        )?
        .as_ref()
            != Some(&audit)
        {
            return Err(invalid(
                "audit differs from original page and canonical source",
            ));
        }
        Ok(audit.derived_positions)
    }
}
