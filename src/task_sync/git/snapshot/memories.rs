//! Validate the complete memory component when retaining a data commit.
use super::super::delta::{Entry, Reader};
use anyhow::{Context, Result, ensure};
use std::collections::BTreeMap;

pub(super) fn validate(
    reader: &Reader<'_>,
    root: Entry,
    project: &crate::ProjectId,
) -> Result<usize> {
    ensure!(
        root.mode == "040000" && root.kind == "tree",
        "memory carrier must be an ordinary tree"
    );
    let keys = reader.entries(&root.oid, &[])?;
    ensure!(keys.len() <= 100_000, "memory key count limit exceeded");
    let mut total = 0usize;
    for key in keys {
        ensure!(
            key.mode == "040000" && key.kind == "tree",
            "memory key must be an ordinary tree"
        );
        let files = reader.entries(&key.oid, &[])?;
        ensure!(files.len() <= 100_000, "memory file count limit exceeded");
        let mut documents = BTreeMap::new();
        for file in files {
            ensure!(
                file.mode == "100644" && file.kind == "blob",
                "memory file must be an ordinary blob"
            );
            let bytes = reader.evidence_blob(&file.oid, 1024 * 1024)?;
            total = total
                .checked_add(bytes.len())
                .context("memory snapshot byte overflow")?;
            ensure!(
                total <= 128 * 1024 * 1024,
                "memory snapshot exceeds 128 MiB"
            );
            documents.insert(file.name, bytes);
        }
        let selected = crate::task_sync::memories::directory::decode_key(&key.name, documents)?;
        ensure!(
            &selected.project_id == project,
            "memory snapshot belongs to another project"
        );
    }
    Ok(total)
}
