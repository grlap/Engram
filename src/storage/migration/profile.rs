//! Named source profiles and an exhaustive table disposition for each import.

use super::{ExportManifest, MigrationError, TableManifest, refused, restore, schema};

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub enum MigrationProfile {
    #[serde(rename = "current")]
    Current,
    #[serde(rename = "aggregate-root-v1")]
    AggregateRootV1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub enum TableDisposition {
    #[serde(rename = "unchanged")]
    Unchanged,
    #[serde(rename = "transform")]
    Transform,
    #[serde(rename = "rebuild")]
    Rebuild,
}

pub(super) fn detect_profile(
    manifest: &ExportManifest,
) -> Result<MigrationProfile, MigrationError> {
    restore::source_blueprint(manifest)?;
    if is_aggregate_root(manifest) {
        Ok(MigrationProfile::AggregateRootV1)
    } else if schema::TABLES
        .iter()
        .all(|name| manifest.tables.iter().any(|table| table.name == *name))
    {
        Ok(MigrationProfile::Current)
    } else {
        Err(refused(
            "current import requires all six migration provenance tables; unpack still restores this layout",
        ))
    }
}

pub(super) fn is_aggregate_root(manifest: &ExportManifest) -> bool {
    manifest.tables.iter().any(|table| {
        table.name == "work_root_executions"
            && table
                .columns
                .iter()
                .any(|column| column.name == "execution_json")
    })
}

/// Every source table must have a disposition. Unknown kinds refuse here.
/// Unknown names are admitted or refused by `source_blueprint` schema
/// comparison, not by inventing a second name list.
pub(super) fn table_dispositions(
    profile: MigrationProfile,
    manifest: &ExportManifest,
) -> Result<Vec<(String, TableDisposition)>, MigrationError> {
    manifest
        .tables
        .iter()
        .map(|table| Ok((table.name.clone(), disposition(profile, table)?)))
        .collect()
}

fn disposition(
    profile: MigrationProfile,
    table: &TableManifest,
) -> Result<TableDisposition, MigrationError> {
    match table.kind.as_str() {
        "virtual" | "shadow" => Ok(TableDisposition::Rebuild),
        "table" => match profile {
            MigrationProfile::Current => Ok(TableDisposition::Unchanged),
            MigrationProfile::AggregateRootV1 => aggregate_table(table),
        },
        other => Err(refused(format!(
            "unknown table kind {other} for {}",
            table.name
        ))),
    }
}

fn aggregate_table(table: &TableManifest) -> Result<TableDisposition, MigrationError> {
    let provenance = schema::TABLES.contains(&table.name.as_str());
    let transform = table.name == "objects"
        || table.name == "work_root_executions"
        || table.name == "work_runs"
        || table.name == "work_operation_results"
        || table.name == "work_completion_seals"
        || table
            .foreign_keys
            .iter()
            .any(|foreign| foreign.target_table == "objects");
    if provenance {
        if table.rows != 0 {
            return Err(refused(
                "this source profile does not accept an already migrated store",
            ));
        }
        return Ok(TableDisposition::Unchanged);
    }
    if transform {
        return Ok(TableDisposition::Transform);
    }
    Ok(TableDisposition::Unchanged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::migration::TableManifest;
    use crate::test_support::temp_home;

    #[test]
    fn current_profile_assigns_a_disposition_to_every_exported_table() {
        let directory = temp_home().expect("directory");
        let source = directory.path().join("source.db");
        drop(crate::SqliteStore::open_unresolved(&source).expect("current"));
        let archive = directory.path().join("archive.db");
        let manifest = crate::storage::migration::export_store(&source, &archive).expect("export");
        assert_eq!(
            detect_profile(&manifest).expect("profile"),
            MigrationProfile::Current
        );
        let dispositions = table_dispositions(MigrationProfile::Current, &manifest).expect("all");
        assert_eq!(dispositions.len(), manifest.tables.len());
        assert!(dispositions.iter().any(|(name, disposition)| {
            name == "object_fts" && *disposition == TableDisposition::Rebuild
        }));
        assert!(dispositions.iter().any(|(name, disposition)| {
            name == "objects" && *disposition == TableDisposition::Unchanged
        }));
        assert!(
            dispositions
                .iter()
                .any(|(name, _)| schema::TABLES.contains(&name.as_str()))
        );
    }

    fn fixture_table(name: &str, kind: &str) -> TableManifest {
        TableManifest {
            name: name.into(),
            kind: kind.into(),
            without_rowid: false,
            strict: false,
            columns: Vec::new(),
            foreign_keys: Vec::new(),
            rowid_alias: None,
            rows: 1,
            encoded_bytes: 0,
            rows_sha256: String::new(),
        }
    }

    #[test]
    fn aggregate_profile_transforms_roots() {
        let directory = temp_home().expect("directory");
        let source = directory.path().join("source.db");
        super::super::tests::aggregate_profile(&source);
        let archive = directory.path().join("archive.db");
        let manifest = crate::storage::migration::export_store(&source, &archive).expect("export");
        assert_eq!(
            detect_profile(&manifest).expect("profile"),
            MigrationProfile::AggregateRootV1
        );
        let dispositions =
            table_dispositions(MigrationProfile::AggregateRootV1, &manifest).expect("all");
        assert_eq!(
            dispositions
                .iter()
                .find(|(name, _)| name == "work_root_executions")
                .map(|(_, disposition)| *disposition),
            Some(TableDisposition::Transform)
        );
    }

    #[test]
    fn unknown_table_kind_is_refused_by_disposition() {
        let directory = temp_home().expect("directory");
        let source = directory.path().join("source.db");
        drop(crate::SqliteStore::open_unresolved(&source).expect("current"));
        let archive = directory.path().join("archive.db");
        let mut unknown =
            crate::storage::migration::export_store(&source, &archive).expect("export");
        unknown
            .tables
            .push(fixture_table("unrecognized_future", "mystery"));
        let error = table_dispositions(MigrationProfile::Current, &unknown)
            .expect_err("unknown kind is not a table disposition");
        assert!(
            error.to_string().contains("unknown table kind mystery"),
            "{error}"
        );
    }

    #[test]
    fn unknown_table_name_is_classified_unchanged_by_disposition() {
        let directory = temp_home().expect("directory");
        let source = directory.path().join("source.db");
        drop(crate::SqliteStore::open_unresolved(&source).expect("current"));
        let archive = directory.path().join("archive.db");
        let mut extra = crate::storage::migration::export_store(&source, &archive).expect("export");
        extra
            .tables
            .push(fixture_table("unrecognized_future", "table"));
        let dispositions = table_dispositions(MigrationProfile::Current, &extra)
            .expect("names are not a second admit list");
        assert_eq!(
            dispositions
                .iter()
                .find(|(name, _)| name == "unrecognized_future")
                .map(|(_, disposition)| *disposition),
            Some(TableDisposition::Unchanged)
        );
    }

    #[test]
    fn unknown_schema_entry_is_refused_by_source_blueprint() {
        let directory = temp_home().expect("directory");
        let source = directory.path().join("source.db");
        drop(crate::SqliteStore::open_unresolved(&source).expect("current"));
        let archive = directory.path().join("archive.db");
        let mut extra = crate::storage::migration::export_store(&source, &archive).expect("export");
        extra.schema.push(crate::storage::migration::SchemaEntry {
            kind: "table".into(),
            name: "unrecognized_future".into(),
            table: "unrecognized_future".into(),
            root_page: 0,
            sql: Some("CREATE TABLE unrecognized_future(x)".into()),
        });
        let error = restore::source_blueprint(&extra)
            .expect_err("compiled schema comparison admits supported profiles");
        assert!(
            error
                .to_string()
                .contains("not an explicitly supported migration profile"),
            "{error}"
        );
    }

    #[test]
    fn aggregate_provenance_stays_unchanged_even_with_object_foreign_keys() {
        let directory = temp_home().expect("directory");
        let source = directory.path().join("source.db");
        super::super::tests::aggregate_profile(&source);
        let archive = directory.path().join("archive.db");
        let manifest = crate::storage::migration::export_store(&source, &archive).expect("export");
        let dispositions =
            table_dispositions(MigrationProfile::AggregateRootV1, &manifest).expect("all");
        assert_eq!(
            dispositions
                .iter()
                .find(|(name, _)| name == "migration_object_map")
                .map(|(_, disposition)| *disposition),
            Some(TableDisposition::Unchanged),
            "provenance is one classification, not a second transform because of object FKs"
        );
    }
}
