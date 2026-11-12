//! Stage exact retained packs in the data checkout, returning metadata only.

use super::{DataWorktree, PrivateDirectory};
use crate::{
    ObjectId, ProjectId,
    task_sync::{
        evidence::{EvidencePack, closure, limits},
        retention,
    },
};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::path::{Component, Path};

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct PackFile {
    pub pack: ObjectId,
    pub publishing_origin: ObjectId,
    pub path: String,
    pub encoded_bytes: usize,
    pub records: usize,
    pub content_fingerprint: ObjectId,
}

impl DataWorktree {
    /// Stage one bounded page of existing immutable packs, never a Git commit.
    /// Exact existing bytes replay; divergent or partial files remain untouched.
    /// An I/O interruption may leave a partial page: preserve its cursor and
    /// inspect it before retrying. No file is overwritten or removed.
    ///
    /// # Errors
    /// Refuses foreign packs, profile limits, unsafe paths or changed bytes.
    pub fn stage_evidence_packs(
        &self,
        project: &ProjectId,
        packs: &[EvidencePack],
    ) -> Result<Vec<PackFile>> {
        self.revalidate()?;
        let encoded = packs
            .iter()
            .map(EvidencePack::to_bytes)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        closure::validate_batch_payloads(encoded.iter().map(Vec::as_slice))?;
        let _reservation = retention::audit::AuditReservation::acquire(&self.git_directory)?;
        let root = PrivateDirectory::open(&self.checkout)?;
        let mut selected = Vec::with_capacity(packs.len());
        let mut seen = std::collections::BTreeSet::new();
        for (pack, bytes) in packs.iter().zip(&encoded) {
            ensure!(
                pack.project_id() == project,
                "evidence pack belongs to another project"
            );
            let path = pack.relative_path();
            ensure!(
                seen.insert(path.clone()),
                "duplicate selected evidence pack"
            );
            compare(&root, Path::new(&path), bytes)?;
            selected.push(PackFile {
                pack: pack.pack_id().clone(),
                publishing_origin: pack.publishing_origin().clone(),
                path,
                encoded_bytes: bytes.len(),
                records: pack.records().len(),
                content_fingerprint: ObjectId::from_stored(format!("{:x}", Sha256::digest(bytes)))
                    .context("evidence content fingerprint is invalid")?,
            });
        }
        // Refuse all conflicting destinations before creating any pack file.
        self.revalidate()?;
        for (file, bytes) in selected.iter().zip(&encoded) {
            let path = Path::new(&file.path);
            let parent = directory(
                &root,
                path.parent().context("pack path has no parent")?,
                true,
            )?
            .context("pack directory is missing")?;
            let name = Path::new(path.file_name().context("pack name is missing")?);
            if retention::audit::read_bytes(&parent, name, limits::MAX_PACK_BYTES)?.is_none() {
                retention::write_new(&parent, name, bytes)?;
            }
            compare(&root, path, bytes)?;
        }
        self.revalidate()?;
        Ok(selected)
    }
}

fn compare(root: &PrivateDirectory, path: &Path, bytes: &[u8]) -> Result<()> {
    let Some(parent) = directory(root, path.parent().context("pack parent missing")?, false)?
    else {
        return Ok(());
    };
    if let Some(existing) = retention::audit::read_bytes(
        &parent,
        Path::new(path.file_name().context("pack name missing")?),
        limits::MAX_PACK_BYTES,
    )? {
        ensure!(
            existing == bytes,
            "staged evidence differs or is partial; preserve original selection"
        );
    }
    Ok(())
}

fn directory(
    root: &PrivateDirectory,
    relative: &Path,
    create: bool,
) -> Result<Option<PrivateDirectory>> {
    let mut parts = relative.components();
    let Some(Component::Normal(first)) = parts.next() else {
        anyhow::bail!("invalid evidence directory");
    };
    let child = |parent: &PrivateDirectory, name: &Path| -> Result<Option<PrivateDirectory>> {
        match parent.child(name) {
            Ok(value) => Ok(Some(value)),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                if create {
                    Ok(Some(retention::container(parent, name)?))
                } else {
                    Ok(None)
                }
            }
            Err(error) => Err(error),
        }
    };
    let Some(mut current) = child(root, Path::new(first))? else {
        return Ok(None);
    };
    for part in parts {
        let Component::Normal(name) = part else {
            anyhow::bail!("invalid evidence directory");
        };
        let Some(next) = child(&current, Path::new(name))? else {
            return Ok(None);
        };
        current = next;
    }
    Ok(Some(current))
}
