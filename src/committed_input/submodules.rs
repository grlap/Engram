//! Select declared Gitlinks without asking a parent Git query to read child dirt.

use super::{GitlinkPins, Repository, before_deadline, open_regular, records, validate_path};
use crate::committed_input::valid_oid;
use anyhow::{Context, Result, bail, ensure};
use cap_std::fs::Dir;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read as _,
};

const MODULES: &str = ".gitmodules";
const MAX_MODULE_BYTES: u64 = 1024 * 1024;
// Leave room for the executable, options and Windows command-line quoting.
const TREE_PATH_ARGUMENT_BYTES: usize = 12_000;

pub(super) struct ModuleBlobs<'a> {
    pub(super) head: Option<&'a str>,
    pub(super) index: Option<&'a str>,
}

fn worktree_modules(repository: &Repository<'_, '_>) -> Result<Option<Vec<u8>>> {
    let directory = Dir::open_ambient_dir(repository.root, cap_std::ambient_authority())?;
    match directory.symlink_metadata(MODULES) {
        Ok(metadata) => ensure!(
            metadata.is_file(),
            "unsupported worktree .gitmodules file type"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let (file, metadata) = open_regular(&directory, MODULES)?;
    if metadata.len() > MAX_MODULE_BYTES {
        return Err(crate::task_sync::git::QueryFailure::OutputLimit.into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_MODULE_BYTES + 1).read_to_end(&mut bytes)?;
    before_deadline(repository.deadline, Ok(()))?;
    if bytes.len() as u64 > MAX_MODULE_BYTES {
        return Err(crate::task_sync::git::QueryFailure::OutputLimit.into());
    }
    Ok(Some(bytes))
}

pub(super) fn config_blob<'a>(mode: &[u8], oid: &'a [u8]) -> Result<Option<&'a str>> {
    match mode {
        b"000000" => Ok(None),
        b"100644" | b"100755" => {
            let id = std::str::from_utf8(oid)?;
            ensure!(valid_oid(id), "invalid .gitmodules blob identity");
            Ok(Some(id))
        }
        _ => bail!("unsupported .gitmodules file type"),
    }
}

pub(super) fn declared_paths(
    repository: &Repository<'_, '_>,
    commit: &str,
    staged: Option<ModuleBlobs<'_>>,
    changed_worktree: bool,
) -> Result<BTreeSet<String>> {
    let original;
    let blobs = if let Some(staged) = staged {
        staged
    } else {
        original = tree_rows(repository, commit, &[MODULES])?;
        let row = original.first();
        ensure!(
            original.len() <= 1 && row.is_none_or(|row| row.path == MODULES),
            "unexpected .gitmodules tree output"
        );
        let head = row
            .map(|row| config_blob(row.mode.as_bytes(), row.oid.as_bytes()))
            .transpose()?
            .flatten();
        ModuleBlobs { head, index: head }
    };

    let mut paths = BTreeSet::new();
    if let Some(head) = blobs.head {
        paths.extend(read_declarations(repository, ["--blob", head], None)?);
    }
    if let Some(index) = blobs.index.filter(|index| Some(*index) != blobs.head) {
        paths.extend(read_declarations(repository, ["--blob", index], None)?);
    }
    if changed_worktree && let Some(bytes) = worktree_modules(repository)? {
        paths.extend(read_declarations(
            repository,
            ["--file", "-"],
            Some(&bytes),
        )?);
    }
    Ok(paths)
}

fn read_declarations(
    repository: &Repository<'_, '_>,
    location: [&str; 2],
    input: Option<&[u8]>,
) -> Result<Vec<String>> {
    let bytes = repository.run_with_input(
        &[
            "config",
            "--null",
            "--no-includes",
            location[0],
            location[1],
            "--get-regexp",
            "^submodule\\..+\\.path$",
        ],
        true,
        input,
    )?;
    records(&bytes)?
        .map(|row| {
            let (_, path) = std::str::from_utf8(row)?
                .split_once('\n')
                .context("malformed .gitmodules declaration")?;
            Ok(validate_path(path.as_bytes())?.to_owned())
        })
        .collect()
}

pub(super) fn append_unchanged_pins(
    repository: &Repository<'_, '_>,
    commit: &str,
    declarations: &BTreeSet<String>,
    pins: &mut BTreeMap<String, GitlinkPins>,
) -> Result<()> {
    // The staged diff already contains both endpoints of every changed
    // gitlink. The remaining declared paths need only their committed pins.
    let unchanged: Vec<_> = declarations
        .iter()
        .filter(|path| !pins.contains_key(*path))
        .map(String::as_str)
        .collect();
    if unchanged.is_empty() {
        return Ok(());
    }
    for row in tree_rows(repository, commit, &unchanged)? {
        if row.mode == "160000" && declarations.contains(&row.path) {
            pins.insert(
                row.path,
                GitlinkPins {
                    head: Some(row.oid.clone()),
                    index: Some(row.oid),
                },
            );
        }
    }
    Ok(())
}

struct TreeRow {
    mode: String,
    oid: String,
    path: String,
}

fn tree_rows(
    repository: &Repository<'_, '_>,
    commit: &str,
    paths: &[&str],
) -> Result<Vec<TreeRow>> {
    let mut args = vec!["ls-tree", "-z", "--full-tree", commit, "--"];
    let fixed = args.len();
    let mut size = 0;
    let mut rows = Vec::new();
    for path in paths {
        let argument = path.len().saturating_mul(2).saturating_add(3);
        ensure!(
            argument <= TREE_PATH_ARGUMENT_BYTES,
            "declared Gitlink path exceeds the query argument bound"
        );
        if size + argument > TREE_PATH_ARGUMENT_BYTES {
            rows.extend(read_tree_rows(repository, &args)?);
            args.truncate(fixed);
            size = 0;
        }
        args.push(path);
        size += argument;
    }
    if args.len() > fixed {
        rows.extend(read_tree_rows(repository, &args)?);
    }
    Ok(rows)
}

fn read_tree_rows(repository: &Repository<'_, '_>, args: &[&str]) -> Result<Vec<TreeRow>> {
    let bytes = repository.run(args)?;
    records(&bytes)?
        .map(|row| {
            let (header, path) = std::str::from_utf8(row)?
                .split_once('\t')
                .context("malformed Git tree row")?;
            let mut fields = header.split(' ');
            let mode = fields.next().context("missing Git tree mode")?;
            let kind = fields.next().context("missing Git tree object type")?;
            let oid = fields.next().context("missing Git tree object id")?;
            ensure!(
                fields.next().is_none()
                    && valid_oid(oid)
                    && matches!(
                        (mode, kind),
                        ("100644" | "100755" | "120000", "blob")
                            | ("160000", "commit")
                            | ("040000", "tree")
                    ),
                "invalid Git tree row"
            );
            Ok(TreeRow {
                mode: mode.to_owned(),
                oid: oid.to_owned(),
                path: validate_path(path.as_bytes())?.to_owned(),
            })
        })
        .collect()
}
