//! Move, verify and compensate before publishing a declared root setup edit.

use super::super::{files::Directory, policy::Edit};
use super::journal::Record;
use anyhow::{Result, ensure};
use std::{io::Write, path::Path};

pub(super) fn prepare(
    private: &Directory,
    index: usize,
    checkpoint: &mut impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    let attempt = private.create_child(Path::new(&format!("replace-{index}")))?;
    checkpoint(&format!("replace-created-{index}"))?;
    let bytes = plan(index)?;
    let mut pending = attempt.open_file(Path::new(".plan.pending"), true, true)?;
    checkpoint(&format!("replace-plan-created-{index}"))?;
    let split = bytes.len() / 2;
    pending.write_all(&bytes[..split])?;
    checkpoint(&format!("replace-plan-prefix-{index}"))?;
    pending.write_all(&bytes[split..])?;
    pending.sync_all()?;
    drop(pending);
    checkpoint(&format!("replace-plan-synced-{index}"))?;
    attempt
        .dir()
        .hard_link(".plan.pending", attempt.dir(), "plan")?;
    checkpoint(&format!("replace-plan-published-{index}"))?;
    Ok(())
}

/// Whether the public file holds exactly the bytes this materialization
/// publishes there: `false` when it is absent, a refusal when it differs.
pub(super) fn owned(target: &Directory, name: &Path, bytes: &[u8]) -> Result<bool> {
    match target.read(name)? {
        None => Ok(false),
        Some(present) => {
            ensure!(
                present == bytes,
                "public file holds other bytes; preserve public and retained state"
            );
            Ok(true)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn apply(
    root: &Directory,
    private: &Directory,
    record: &Record,
    index: usize,
    edit: &Edit,
    completed: bool,
    checkpoint: &mut impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    let name = Path::new(&edit.path);
    let after = private.child(Path::new("after"))?;
    let attempt_name = format!("replace-{index}");
    let attempt = check_attempt(private, index)?;
    checkpoint(&format!("replacement-ready-{index}"))?;
    // Inspect first: the public file is either already the after bytes, or
    // still the before bytes (or absent) this edit was admitted with.
    let present = root.read(name)?;
    if present.as_ref() == Some(&edit.after) {
        if let Some(before_bytes) = &edit.before {
            ensure!(
                attempt.read(Path::new("displaced"))?.as_ref() == Some(before_bytes),
                "published setup edit lost or changed its retained before file; preserve state"
            );
        }
        return Ok(());
    }
    ensure!(
        !completed,
        "completed materialization setup file changed or disappeared"
    );
    if let Some(before_bytes) = &edit.before {
        if attempt.read(Path::new("displaced"))?.is_none() {
            ensure!(
                present.as_ref() == Some(before_bytes),
                "setup before file changed before replacement"
            );
            checkpoint(&format!("before-move-{index}"))?;
            root.dir().rename(name, attempt.dir(), "displaced")?;
            checkpoint(&format!("moved-{index}"))?;
        }
        let arrived = attempt.read(Path::new("displaced"));
        let matching = arrived
            .as_ref()
            .is_ok_and(|bytes| bytes.as_ref() == Some(before_bytes));
        if !matching {
            checkpoint(&format!("before-restore-{index}"))?;
            // Restore only an ordinary displaced file by no-replace linking.
            // A link/special-file race stays named privately for explicit recovery.
            let restored = if let Ok(Some(bytes)) = arrived {
                restored_public(root, name, &bytes).is_ok_and(|restored| restored)
                    || attempt
                        .dir()
                        .hard_link("displaced", root.dir(), name)
                        .is_ok()
            } else {
                false
            };
            checkpoint(&format!("restore-checked-{index}"))?;
            anyhow::bail!(
                "setup move verification refused; restore_incomplete={}; displaced bytes retained at {}/engram-task-sync-materialize-{}-{}/{}/displaced; public entry preserved",
                !restored,
                record.basis.git_directory.display(),
                record.selection.setup_operation,
                record.selection.prepared_operation,
                attempt_name
            );
        }
    } else {
        ensure!(
            present.is_none(),
            "absent setup path is now occupied; preserve it"
        );
        ensure!(
            attempt.read(Path::new("displaced"))?.is_none(),
            "unexpected displaced file for absent setup path"
        );
    }
    checkpoint(&format!("before-setup-link-{index}"))?;
    after.dir().hard_link(name, root.dir(), name).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists && edit.before.is_some() {
            anyhow::Error::new(error).context(format!(
                "setup publication refused: public path {} is occupied; preserve it and retained displaced path {}/engram-task-sync-materialize-{}-{}/{}/displaced",
                record.basis.repository.join(name).display(),
                record.basis.git_directory.display(),
                record.selection.setup_operation,
                record.selection.prepared_operation,
                attempt_name
            ))
        } else {
            error.into()
        }
    })?;
    ensure!(
        owned(root, name, &edit.after)?,
        "setup file missing after publication"
    );
    checkpoint(&format!("setup-linked-{index}"))?;
    Ok(())
}

fn restored_public(root: &Directory, name: &Path, bytes: &[u8]) -> Result<bool> {
    Ok(root.read(name)?.as_deref() == Some(bytes))
}

fn plan(index: usize) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&[index])?)
}

pub(super) fn check_attempt(private: &Directory, index: usize) -> Result<Directory> {
    let attempt = private.child(Path::new(&format!("replace-{index}")))?;
    let names = attempt.names(3)?;
    ensure!(
        names
            .iter()
            .all(|name| [".plan.pending", "plan", "displaced"]
                .iter()
                .any(|allowed| name == allowed)),
        "unknown setup replacement entry; preserve it"
    );
    // The plan names its edit's index first; an older journal's plan also
    // carried a file identity after it, which is not compared.
    let planned = attempt
        .read(Path::new("plan"))?
        .and_then(|bytes| serde_json::from_slice::<Vec<serde_json::Value>>(&bytes).ok())
        .and_then(|plan| plan.first().and_then(serde_json::Value::as_u64));
    ensure!(
        planned == Some(index as u64),
        "incomplete or differently bound setup replacement attempt; preserve it"
    );
    Ok(attempt)
}
