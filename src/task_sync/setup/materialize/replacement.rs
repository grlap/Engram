//! Move, verify and compensate before publishing a declared root setup edit.

use super::super::{
    files::{Directory, PhysicalId, file_identity},
    policy::Edit,
};
use super::journal::Record;
use anyhow::{Context, Result, ensure};
use std::{io::Write, path::Path};

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PlanBinding {
    directory: PhysicalId,
    plan: PhysicalId,
}

pub(super) fn prepare(
    private: &Directory,
    index: usize,
    before: Option<PhysicalId>,
    checkpoint: &mut impl FnMut(&str) -> Result<()>,
) -> Result<PlanBinding> {
    let attempt = private.create_child(Path::new(&format!("replace-{index}")))?;
    checkpoint(&format!("replace-created-{index}"))?;
    let bytes = plan(index, before)?;
    let mut pending = attempt.open_file(Path::new(".plan.pending"), true, true)?;
    checkpoint(&format!("replace-plan-created-{index}"))?;
    let split = bytes.len() / 2;
    pending.write_all(&bytes[..split])?;
    checkpoint(&format!("replace-plan-prefix-{index}"))?;
    pending.write_all(&bytes[split..])?;
    pending.sync_all()?;
    let identity = file_identity(&pending)?;
    drop(pending);
    checkpoint(&format!("replace-plan-synced-{index}"))?;
    attempt
        .dir()
        .hard_link(".plan.pending", attempt.dir(), "plan")?;
    checkpoint(&format!("replace-plan-published-{index}"))?;
    Ok(PlanBinding {
        directory: attempt.identity()?,
        plan: identity,
    })
}

pub(super) fn owned(
    target: &Directory,
    name: &Path,
    source: &Directory,
    bytes: &[u8],
) -> Result<bool> {
    if target.read(name)?.is_none() {
        return Ok(false);
    }
    let target_file = target.open_file(name, false, false)?;
    let source_file = source.open_file(name, false, false)?;
    ensure!(
        file_identity(&target_file)? == file_identity(&source_file)?,
        "public file is unowned, even if its bytes are equal; preserve it"
    );
    ensure!(
        target.read(name)?.as_deref() == Some(bytes),
        "owned public file was edited; preserve public and retained state"
    );
    Ok(true)
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
    let attempt = check_attempt(private, record, index)?;
    checkpoint(&format!("replacement-ready-{index}"))?;
    // Inspect first: an ordinary before file is allowed only for its original
    // physical identity and bytes. Equal after bytes without our link refuse.
    let present = root.read(name)?;
    if present.as_ref() == Some(&edit.after) {
        if let Some(before_bytes) = &edit.before {
            ensure!(
                attempt.read(Path::new("displaced"))?.as_ref() == Some(before_bytes)
                    && Some(file_identity(&attempt.open_file(
                        Path::new("displaced"),
                        false,
                        false
                    )?)?)
                        == record.before[index],
                "published setup edit lost or changed its retained before file; preserve state"
            );
        }
        ensure!(
            owned(root, name, &after, &edit.after)?,
            "published setup file missing"
        );
        return Ok(());
    }
    ensure!(
        !completed,
        "completed materialization setup file changed or disappeared"
    );
    if let Some(before_bytes) = &edit.before {
        let expected = record.before[index].context("prepared before identity missing")?;
        if attempt.read(Path::new("displaced"))?.is_none() {
            ensure!(
                present.as_ref() == Some(before_bytes)
                    && file_identity(&root.open_file(name, false, false)?)? == expected,
                "setup before file changed before replacement"
            );
            checkpoint(&format!("before-move-{index}"))?;
            root.dir().rename(name, attempt.dir(), "displaced")?;
            checkpoint(&format!("moved-{index}"))?;
        }
        let arrived = attempt.read(Path::new("displaced"));
        let matching = arrived
            .as_ref()
            .is_ok_and(|bytes| bytes.as_ref() == Some(before_bytes))
            && attempt
                .open_file(Path::new("displaced"), false, false)
                .and_then(|file| file_identity(&file))
                .is_ok_and(|id| id == expected);
        if !matching {
            checkpoint(&format!("before-restore-{index}"))?;
            // Restore only an ordinary displaced file by no-replace linking.
            // A link/special-file race stays named privately for explicit recovery.
            let restored = if let Ok(Some(bytes)) = arrived {
                restored_public(root, name, &attempt, &bytes).is_ok_and(|restored| restored)
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
        owned(root, name, &after, &edit.after)?,
        "setup file missing after publication"
    );
    checkpoint(&format!("setup-linked-{index}"))?;
    Ok(())
}

fn restored_public(
    root: &Directory,
    name: &Path,
    attempt: &Directory,
    bytes: &[u8],
) -> Result<bool> {
    if root.read(name)?.as_deref() != Some(bytes) {
        return Ok(false);
    }
    Ok(file_identity(&root.open_file(name, false, false)?)?
        == file_identity(&attempt.open_file(Path::new("displaced"), false, false)?)?)
}

fn plan(index: usize, before: Option<PhysicalId>) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&(index, before))?)
}

pub(super) fn check_attempt(
    private: &Directory,
    record: &Record,
    index: usize,
) -> Result<Directory> {
    let attempt = private.child(Path::new(&format!("replace-{index}")))?;
    let binding = record
        .replacements
        .get(index)
        .context("replacement binding missing")?;
    ensure!(
        attempt.identity()? == binding.directory,
        "setup replacement directory identity changed; preserve it"
    );
    let names = attempt.names(3)?;
    ensure!(
        names
            .iter()
            .all(|name| [".plan.pending", "plan", "displaced"]
                .iter()
                .any(|allowed| name == allowed)),
        "unknown setup replacement entry; preserve it"
    );
    ensure!(
        attempt.read(Path::new("plan"))?.as_ref() == Some(&plan(index, record.before[index])?),
        "incomplete or differently bound setup replacement attempt; preserve it"
    );
    let pending = attempt.open_file(Path::new(".plan.pending"), false, false)?;
    let published = attempt.open_file(Path::new("plan"), false, false)?;
    ensure!(
        file_identity(&pending)? == binding.plan && file_identity(&published)? == binding.plan,
        "setup replacement marker identity changed"
    );
    Ok(attempt)
}
