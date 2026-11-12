//! Validate carrier names and modes without opening evidence payloads.
use super::super::delta::{Entry, Reader};
use crate::task_sync::{
    TaskSyncRefusal,
    evidence::path::{self, Kind},
};
use anyhow::Result;

pub(super) fn validate(reader: &Reader<'_>, entry: Entry) -> Result<()> {
    let mut pending = vec![(entry, ".engram/evidence".to_owned())];
    while let Some((entry, name)) = pending.pop() {
        match path::kind(&name) {
            Some(Kind::Directory) => {
                if entry.mode != "040000" || entry.kind != "tree" {
                    return Err(TaskSyncRefusal::DestinationNotDirectory
                        .invalid("evidence carrier directory must be an ordinary tree")
                        .into());
                }
                for child in reader.entries(&entry.oid, &[])? {
                    let name = format!("{name}/{}", child.name);
                    pending.push((child, name));
                }
            }
            Some(Kind::Pack) => {
                if entry.mode != "100644" || entry.kind != "blob" {
                    return Err(TaskSyncRefusal::DestinationNotFile
                        .invalid("evidence carrier pack must be an ordinary blob")
                        .into());
                }
            }
            None => {
                return Err(TaskSyncRefusal::DestinationUnknownEntry
                    .invalid("unknown evidence carrier path")
                    .into());
            }
        }
    }
    Ok(())
}
