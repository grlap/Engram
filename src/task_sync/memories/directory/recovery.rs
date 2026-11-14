//! Stateless, explicitly selected recovery; no accepted-input checkpoint exists.
use super::*;

// A stopped publication can still hold both admitted trees and its staged
// revision/head. Recovery streams this bounded union; final admission is 128 MiB.
const RECOVERY_LIMIT: usize = TREE_LIMIT * 2 + FILE_LIMIT * 2;

fn head_bytes(history: &KeyHistory) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec_pretty(&HeadDocument {
        format: FORMAT.into(),
        project_id: history.project_id.clone(),
        key: history.key.clone(),
        head: history.head.clone(),
    })?)
}

pub(super) fn prepare(
    root: &PrivateDirectory,
    basis: &BTreeMap<String, KeyHistory>,
    selected: &BTreeMap<String, KeyHistory>,
) -> Result<()> {
    let memories = match root.child(Path::new("memories")) {
        Ok(directory) => directory,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                && basis.is_empty() =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let names = memories.names(ENTRY_LIMIT)?;
    let mut cleanup = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut total = 0usize;
    for key in names {
        let key = key.to_str().context("non-UTF8 memory key")?;
        validate_key(key)?;
        let new = selected
            .get(key)
            .context("recovery contains an unrelated memory key")?;
        new.validate()?;
        let old = basis.get(key);
        if let Some(old) = old {
            ensure!(
                old.project_id == new.project_id,
                "recovery project mismatch"
            );
        }
        let desired_head = head_bytes(new)?;
        let before_head = old.map(head_bytes).transpose()?;
        let directory = memories.child(Path::new(key))?;
        let mut has_head = false;
        for name in directory.names(ENTRY_LIMIT)? {
            let name = name.to_str().context("non-UTF8 recovery filename")?;
            let bytes = directory
                .read_bounded(Path::new(name), FILE_LIMIT)?
                .context("recovery file disappeared")?;
            total = total
                .checked_add(bytes.len())
                .context("recovery tree byte overflow")?;
            ensure!(
                total <= RECOVERY_LIMIT,
                "recovery input exceeds retained/selected staging bound"
            );
            if name == "head.json" {
                has_head = true;
                let value: serde_json::Value = serde_json::from_slice(&bytes)?;
                ensure!(
                    value == serde_json::from_slice::<serde_json::Value>(&desired_head)?
                        || before_head
                            .as_ref()
                            .is_some_and(|old| serde_json::from_slice::<serde_json::Value>(old)
                                .ok()
                                .as_ref()
                                == Some(&value)),
                    "memory recovery selected head changed"
                );
                continue;
            }
            if let Some(id) = name
                .strip_prefix("head-")
                .and_then(|v| v.strip_suffix(".pending"))
            {
                uuid::Uuid::parse_str(id)?;
                ensure!(
                    desired_head.starts_with(&bytes),
                    "pending memory head differs from recovery selection"
                );
                cleanup.push((key.to_owned(), name.to_owned()));
                continue;
            }
            let pending = name
                .strip_prefix("revision-")
                .and_then(|v| v.strip_suffix(".pending"));
            let raw_id = pending
                .or_else(|| name.strip_suffix(".json"))
                .context("unknown memory recovery entry")?;
            let id: ObjectId = raw_id.parse().map_err(anyhow::Error::msg)?;
            ensure!(
                id.as_str().len() == 32,
                "recovery revision must be a minted UUID"
            );
            let old_revision = old.and_then(|history| history.revisions.get(&id));
            let new_revision = new.revisions.get(&id);
            let old_bytes = old_revision.map(serde_json::to_vec_pretty).transpose()?;
            let new_bytes = new_revision.map(serde_json::to_vec_pretty).transpose()?;
            if pending.is_some() {
                ensure!(
                    new_bytes
                        .as_ref()
                        .is_some_and(|expected| expected.starts_with(&bytes)),
                    "pending revision differs from canonical recovery input"
                );
                cleanup.push((key.to_owned(), name.to_owned()));
            } else if let Some(old) = &old_bytes {
                ensure!(old == &bytes, "immutable selected revision bytes changed");
                if let Some(new) = new_bytes {
                    ensure!(old == &new, "recovery rewrites immutable UUID");
                }
            } else {
                let expected = new_bytes
                    .context("unreferenced recovery revision is not in selected output")?;
                ensure!(
                    expected.starts_with(&bytes),
                    "uncommitted revision differs from canonical recovery input"
                );
                if expected != bytes {
                    cleanup.push((key.to_owned(), name.to_owned()));
                }
            }
        }
        ensure!(
            has_head || old.is_none(),
            "retained memory head missing during recovery"
        );
        seen.insert(key.to_owned());
    }
    ensure!(
        basis.keys().all(|key| seen.contains(key)),
        "retained memory key missing during recovery"
    );
    // Validate the whole interrupted selection before removing any owned,
    // content-confirmed transient or torn newly created file.
    for (key, name) in cleanup {
        memories.child(Path::new(&key))?.dir().remove_file(name)?;
    }
    Ok(())
}
