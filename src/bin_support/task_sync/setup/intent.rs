//! One immutable private intent with immediate lock refusal and no-clobber publication.

use std::{
    io::{Read, Write},
    path::Path,
};

use anyhow::{Result, ensure};

use super::{Attribution, Basis, Intent, Receipt, files::Directory};

const FORMAT: &str = "engram-task-sync-setup-v1";
const LOCK: &[u8] = b"engram-task-sync-setup-lock-v1\n";

pub(super) fn record(basis: Basis, attribution: Attribution) -> Result<Receipt> {
    let directory = &basis.git_directory;
    let bound = Directory::open(directory)?;
    let lock = acquire_lock(&bound)?;
    let path = directory.join("engram-task-sync-setup.json");
    let current = super::repository::inspect(
        &basis.project_file,
        &basis.project_id,
        &basis.repository,
        &basis.branch,
        basis.assertions.clone(),
    )?;
    ensure!(
        basis == current,
        "repository basis changed during setup preparation"
    );
    if let Some(bytes) = bound.read(Path::new("engram-task-sync-setup.json"))? {
        let old: Intent = serde_json::from_slice(&bytes)?;
        ensure!(
            old.format == FORMAT && old.basis == basis,
            "existing setup intent has a different basis or assertion; preserve it and inspect before preparing again"
        );
        return Ok(receipt(path, old.operation, true));
    }
    let intent = Intent {
        format: FORMAT.into(),
        operation: uuid::Uuid::now_v7(),
        prepared_at: chrono::Utc::now(),
        basis,
        attribution,
    };
    // Compact arrays keep maximal admitted raw edits within the reader's bound.
    let mut bytes = serde_json::to_vec(&intent)?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() <= super::files::MAX_PRIVATE_BYTES,
        "prepared intent exceeded private reader bound"
    );
    let pending_name = format!("engram-task-sync-setup-{}.pending", intent.operation);
    let mut pending = bound.open_file(Path::new(&pending_name), true, true)?;
    pending.write_all(&bytes)?;
    pending.sync_all()?;
    drop(pending);
    // A hard link publishes the complete file without replacing any entry.
    // Unsupported filesystems fail closed; a crash can leave only our named pending file.
    let published =
        bound
            .dir()
            .hard_link(&pending_name, bound.dir(), "engram-task-sync-setup.json");
    let cleanup = bound.dir().remove_file(&pending_name);
    published?;
    cleanup?;
    drop(lock);
    Ok(receipt(path, intent.operation, false))
}

fn acquire_lock(bound: &Directory) -> Result<std::fs::File> {
    let lock_name = Path::new("engram-task-sync-setup.lock");
    match bound.dir().symlink_metadata(lock_name) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Publish only complete ownership bytes. A crash or competing first
            // caller cannot strand a zero-length file at the lock's final name.
            let pending_name = format!("engram-task-sync-lock-{}.pending", uuid::Uuid::now_v7());
            let mut pending = bound.open_file(Path::new(&pending_name), true, true)?;
            pending.write_all(LOCK)?;
            pending.sync_all()?;
            drop(pending);
            let result = bound.dir().hard_link(&pending_name, bound.dir(), lock_name);
            let cleanup = bound.dir().remove_file(&pending_name);
            if let Err(error) = result
                && error.kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(error.into());
            }
            cleanup?;
        }
        Err(error) => return Err(error.into()),
        Ok(_) => (),
    }
    // Windows byte-range locks can prevent a contended read. Acquire first and
    // validate ownership on that same handle without changing its bytes.
    let file = bound.open_file(lock_name, true, false)?;
    file.try_lock()
        .map_err(|error| anyhow::anyhow!("setup preparation busy: {error}"))?;
    let mut actual = Vec::new();
    file.try_clone()?
        .take(LOCK.len() as u64 + 1)
        .read_to_end(&mut actual)?;
    ensure!(
        actual == LOCK,
        "unowned setup lock path or changed lock; preserve it"
    );
    Ok(file)
}

fn receipt(path: std::path::PathBuf, operation: uuid::Uuid, repeated: bool) -> Receipt {
    Receipt {
        intent: path,
        operation,
        repeated,
        prepared: true,
        enabled: false,
        assurance: "operator assertions only; repository visibility/readership are not authenticated",
        redactor: "development no-op; preparation does not approve task-field disclosure",
    }
}
