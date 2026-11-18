//! Validate the complete memory component when retaining a data commit.
//!
//! The component is read through one listing of its keys, one recursive
//! listing of every key and file, and a size check and a blob batch per 8,192
//! files, never one listing per key and one read per file.
use super::super::delta::{Entry, Reader};
use anyhow::{Result, ensure};
use std::collections::BTreeMap;

const MAX_KEYS: usize = 100_000;
const MAX_FILES_PER_KEY: usize = 100_000;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 128 * 1024 * 1024;

pub(super) fn validate(
    reader: &Reader<'_>,
    root: &Entry,
    project: &crate::ProjectId,
) -> Result<usize> {
    Ok(read(reader, root, project)?.1)
}

pub(super) fn read(
    reader: &Reader<'_>,
    root: &Entry,
    project: &crate::ProjectId,
) -> Result<(
    BTreeMap<String, crate::task_sync::memories::KeyHistory>,
    usize,
)> {
    ensure!(
        root.mode == "040000" && root.kind == "tree",
        "memory carrier must be an ordinary tree"
    );
    // Each key with its files' names and object ids, in listing order. The
    // recursive listing names an entry by its path, so a malformed root entry
    // whose own name holds a slash would read as another key's file: the
    // root's own entries are listed alone first, and each must be a key tree.
    let mut keys: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for entry in reader.entries(&root.oid, &[])? {
        ensure!(
            entry.mode == "040000" && entry.kind == "tree" && !entry.name.contains('/'),
            "memory key must be an ordinary tree"
        );
        keys.insert(entry.name, Vec::new());
        ensure!(keys.len() <= MAX_KEYS, "memory key count limit exceeded");
    }
    for entry in reader.recursive_entries(&root.oid)? {
        match entry.name.split_once('/') {
            None => ensure!(
                entry.mode == "040000" && entry.kind == "tree" && keys.contains_key(&entry.name),
                "memory key must be an ordinary tree"
            ),
            Some((key, file)) => {
                ensure!(
                    !file.contains('/') && entry.mode == "100644" && entry.kind == "blob",
                    "memory file must be an ordinary blob"
                );
                let files = keys
                    .get_mut(key)
                    .ok_or_else(|| anyhow::anyhow!("memory key must be an ordinary tree"))?;
                files.push((file.to_owned(), entry.oid));
                ensure!(
                    files.len() <= MAX_FILES_PER_KEY,
                    "memory file count limit exceeded"
                );
            }
        }
    }
    let paths = keys
        .iter()
        .flat_map(|(key, files)| {
            files
                .iter()
                .map(move |(file, _)| format!(".engram/memories/{key}/{file}"))
        })
        .collect::<Vec<_>>();
    let wanted = keys
        .values()
        .flatten()
        .zip(&paths)
        .map(|((_, oid), path)| (oid.as_str(), path.as_str()))
        .collect::<Vec<_>>();
    let mut bodies = reader
        .memory_blobs(&wanted, MAX_FILE_BYTES, MAX_TOTAL_BYTES)?
        .into_iter();
    let mut bytes = 0usize;
    let mut histories = BTreeMap::new();
    for (key, files) in keys {
        let mut documents = BTreeMap::new();
        for (file, _) in files {
            let body = bodies
                .next()
                .ok_or_else(|| anyhow::anyhow!("memory blob batch ended early"))?;
            bytes += body.len();
            documents.insert(file, body);
        }
        let selected = crate::task_sync::memories::directory::decode_key(&key, documents)?;
        ensure!(
            &selected.project_id == project,
            "memory snapshot belongs to another project"
        );
        histories.insert(key, selected);
    }
    Ok((histories, bytes))
}
