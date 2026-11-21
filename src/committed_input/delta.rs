//! Git-selected staged, unstaged and untracked inputs for the shared identity.

use super::{Query, Repository, before_deadline};
use crate::source::{Entry, GitDelta, Gitlink};
use anyhow::{Context, Result, bail, ensure};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read as _,
    path::{Component, Path},
    time::Instant,
};

#[path = "submodules.rs"]
mod submodules;

const STATUS: &[&str] = &[
    "status",
    "--porcelain=v2",
    "--branch",
    "--no-ahead-behind",
    "-z",
    "--untracked-files=all",
    "--renames",
    "--ignore-submodules=dirty",
];
const STAGED: &[&str] = &[
    "diff",
    "--cached",
    "--raw",
    "-z",
    "--no-abbrev",
    "--no-renames",
    "--ita-invisible-in-index",
    "--no-relative",
    "--ignore-submodules=dirty",
    "-O/dev/null",
];
const UNSTAGED: &[&str] = &[
    "diff",
    "--raw",
    "-z",
    "--no-abbrev",
    "--patch",
    "--binary",
    "--full-index",
    "--no-renames",
    "--no-ext-diff",
    "--no-textconv",
    "--no-color",
    "--no-relative",
    "--no-indent-heuristic",
    "--diff-algorithm=myers",
    "--unified=3",
    "--inter-hunk-context=0",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    "--output-indicator-new=+",
    "--output-indicator-old=-",
    "--output-indicator-context= ",
    "--ita-invisible-in-index",
    "--ignore-submodules=dirty",
    "--submodule=short",
    "-O/dev/null",
    "--",
];

pub(super) fn capture(
    query: &Query<'_>,
    root: &Repository<'_, '_>,
) -> Result<(String, GitDelta, Vec<Entry>)> {
    let mut hash = DeltaHash {
        hash: Sha256::new(),
        deadline: query.deadline,
    };
    hash.frame(b"engram source Git delta 1")?;
    let mut gitlinks = BTreeMap::new();
    let (commit, entries) = repository(query, root, "", &mut hash, &mut gitlinks)?;
    Ok((
        commit,
        GitDelta {
            sha256: format!("{:x}", hash.hash.finalize()),
            dirty: !entries.is_empty() || !gitlinks.is_empty(),
            gitlinks: gitlinks.into_values().collect(),
        },
        entries,
    ))
}

struct DeltaHash {
    hash: Sha256,
    deadline: Instant,
}

impl DeltaHash {
    fn frame(&mut self, bytes: &[u8]) -> Result<()> {
        before_deadline(self.deadline, Ok(()))?;
        self.hash.update((bytes.len() as u64).to_be_bytes());
        self.hash.update(bytes);
        Ok(())
    }

    fn file(&mut self, mut file: std::fs::File, expected: u64) -> Result<()> {
        self.hash.update(expected.to_be_bytes());
        let mut buffer = [0; 8192];
        let mut total = 0;
        loop {
            before_deadline(self.deadline, Ok(()))?;
            let count = file.read(&mut buffer)?;
            if count == 0 {
                ensure!(
                    total == expected,
                    "untracked file length changed during its read"
                );
                return Ok(());
            }
            total += count as u64;
            ensure!(total <= expected, "untracked file grew during its read");
            self.hash.update(&buffer[..count]);
        }
    }

    fn unstaged_patch(&mut self, config: &Repository<'_, '_>) -> Result<()> {
        let mut hashed = Self {
            hash: std::mem::take(&mut self.hash),
            deadline: self.deadline,
        };
        self.hash = config.stream(UNSTAGED, move |mut stdout| {
            // Fixed-length chunks make framing independent of how the pipe
            // splits reads. An empty frame terminates the patch.
            const PATCH_CHUNK: u16 = 16 * 1024;
            let mut bytes = Vec::with_capacity(usize::from(PATCH_CHUNK));
            loop {
                before_deadline(hashed.deadline, Ok(()))?;
                bytes.clear();
                (&mut stdout)
                    .take(u64::from(PATCH_CHUNK))
                    .read_to_end(&mut bytes)?;
                hashed.frame(&bytes)?;
                if bytes.is_empty() {
                    return Ok(hashed.hash);
                }
            }
        })?;
        Ok(())
    }
}

#[derive(Default)]
struct Selection {
    entries: Vec<Entry>,
    staged: BTreeSet<String>,
    unstaged: BTreeSet<String>,
    untracked: BTreeSet<String>,
    gitlinks: BTreeMap<String, GitlinkPins>,
}

struct GitlinkPins {
    head: Option<String>,
    index: Option<String>,
}

impl GitlinkPins {
    fn from_fields(
        head_mode: &[u8],
        head_oid: &[u8],
        index_mode: &[u8],
        index_oid: &[u8],
    ) -> Result<Option<Self>> {
        let pin = |mode: &[u8], oid: &[u8]| -> Result<Option<String>> {
            if mode != b"160000" {
                return Ok(None);
            }
            let oid = std::str::from_utf8(oid)?;
            ensure!(
                super::valid_oid(oid) && oid.bytes().any(|byte| byte != b'0'),
                "invalid Gitlink pin"
            );
            Ok(Some(oid.to_owned()))
        };
        let head = pin(head_mode, head_oid)?;
        let index = pin(index_mode, index_oid)?;
        Ok((head.is_some() || index.is_some()).then_some(Self { head, index }))
    }
}

fn repository(
    query: &Query<'_>,
    config: &Repository<'_, '_>,
    prefix: &str,
    hash: &mut DeltaHash,
    gitlinks: &mut BTreeMap<String, Gitlink>,
) -> Result<(String, Vec<Entry>)> {
    let root = config.root;
    let (commit, mut status) = selection(&config.run(STATUS)?)?;

    // Gitlinks may be staged out of the index; the raw stage still names them.
    let mut staged_args = STAGED.to_vec();
    staged_args.extend([commit.as_str(), "--"]);
    let staged = if status.staged.is_empty() {
        Vec::new()
    } else {
        config.run(&staged_args)?
    };
    let staged_paths = staged_paths(&staged)?;
    let declarations = submodules::declared_paths(
        config,
        &commit,
        staged_paths.modules,
        status.unstaged.contains(".gitmodules") || status.untracked.contains(".gitmodules"),
    )?;
    let mut pins = std::mem::take(&mut status.gitlinks);
    pins.extend(staged_paths.gitlinks);
    // Git's status and diffs carry pointer changes for undeclared gitlinks.
    // Only declared children contribute a separate checkout observation.
    pins.retain(|path, _| declarations.contains(path));
    submodules::append_unchanged_pins(config, &commit, &declarations, &mut pins)?;

    let mut former_checkouts = BTreeMap::new();
    for (path, pins) in &pins {
        if pins.index.is_none()
            && status.untracked.contains(path)
            && let Some(child) = initialized_child(root, path)?
        {
            // Observe a staged-out checkout only if parent Git reports it as untracked.
            former_checkouts.insert(path.clone(), child);
            status.untracked.remove(path);
            status.entries.retain(|entry| {
                entry.status != "??" || entry.path.trim_end_matches('/') != path.as_str()
            });
        }
    }
    let untracked = &status.untracked;
    let unstaged = !status.unstaged.is_empty();
    if prefix.is_empty() || !staged.is_empty() || unstaged || !untracked.is_empty() {
        hash.frame(b"repository")?;
        hash.frame(prefix.as_bytes())?;
        hash.frame(b"staged")?;
        hash.frame(&staged)?;
        hash.frame(b"unstaged")?;
        if unstaged {
            // Git selects the changed endpoints, including ordinary files
            // that replace gitlinks. Path count never affects argv length.
            hash.unstaged_patch(config)?;
        } else {
            hash.frame(b"")?;
        }
        hash.frame(b"untracked")?;
        if !untracked.is_empty() {
            let directory = Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
            for path in untracked {
                add_untracked(&directory, path, hash)?;
            }
        }
        hash.frame(b"")?;
    }

    for (
        path,
        GitlinkPins {
            head: head_pin,
            index: index_pin,
        },
    ) in pins
    {
        let name = if prefix.is_empty() {
            path.clone()
        } else {
            format!("{prefix}/{path}")
        };
        let child = if index_pin.is_some() {
            initialized_child(root, &path)?
        } else {
            former_checkouts.remove(&path)
        };
        let (checkout_head, child_dirty) = if let Some(child) = child {
            let child_config = query.for_repository(&child);
            super::check_toplevel(&child_config, &child)?;
            let child_filters = child_config.filter_overrides()?;
            let child_config = Repository {
                filter_overrides: &child_filters,
                ..child_config
            };
            let (head, entries) = repository(query, &child_config, &name, hash, gitlinks)
                .with_context(|| format!("submodule '{name}'"))?;
            (Some(head), !entries.is_empty())
        } else {
            ensure!(index_pin.is_none(), "submodule '{name}': uninitialized");
            (None, false)
        };
        let differs = head_pin != index_pin || index_pin != checkout_head || child_dirty;
        if differs {
            if !status.entries.iter().any(|entry| entry.path == path) {
                status.entries.push(Entry {
                    path: path.clone(),
                    status: " M".to_owned(),
                    origin: None,
                });
            }
            gitlinks.insert(
                name.clone(),
                Gitlink {
                    path: name,
                    head_pin,
                    index_pin,
                    checkout_head,
                },
            );
        } else {
            status.entries.retain(|entry| entry.path != path);
        }
    }

    Ok((commit, status.entries))
}

fn initialized_child(root: &Path, path: &str) -> Result<Option<std::path::PathBuf>> {
    let child = root.join(path);
    match child.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_dir() && child.join(".git").try_exists()? => {
            ensure!(
                child.symlink_metadata()?.file_type().is_dir()
                    && child.canonicalize()?.starts_with(root),
                "unavailable gitlink worktree association"
            );
            Ok(Some(child))
        }
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn records(bytes: &[u8]) -> Result<impl Iterator<Item = &[u8]>> {
    ensure!(
        bytes.is_empty() || bytes.last() == Some(&0),
        "truncated Git records"
    );
    Ok(bytes
        .split_inclusive(|byte| *byte == 0)
        .map(|row| &row[..row.len() - 1]))
}

fn selection(bytes: &[u8]) -> Result<(String, Selection)> {
    let mut selected = Selection::default();
    let mut commit = None;
    let mut rows = records(bytes)?;
    while let Some(row) = rows.next() {
        if let Some(oid) = row.strip_prefix(b"# branch.oid ") {
            let oid = std::str::from_utf8(oid)?;
            ensure!(
                super::valid_oid(oid),
                "Git status did not report a HEAD commit"
            );
            ensure!(
                commit.replace(oid.to_owned()).is_none(),
                "duplicate Git status HEAD commit"
            );
            continue;
        }
        if row.starts_with(b"# ") {
            continue;
        }
        if let Some(path) = row.strip_prefix(b"? ") {
            let path = validate_path(path)?;
            let canonical = path.trim_end_matches('/');
            selected.untracked.insert(canonical.to_owned());
            selected.entries.push(Entry {
                path: path.to_owned(),
                status: "??".to_owned(),
                origin: None,
            });
            continue;
        }
        let (row, renamed) = match (row.strip_prefix(b"1 "), row.strip_prefix(b"2 ")) {
            (Some(row), _) => (row, false),
            (_, Some(row)) => (row, true),
            _ => bail!("unsupported or unmerged porcelain record"),
        };
        let mut fields = row.splitn(if renamed { 9 } else { 8 }, |byte| *byte == b' ');
        let xy = fields.next().context("missing porcelain status")?;
        let sub = fields.next().context("missing porcelain submodule state")?;
        ensure!(
            xy.len() == 2
                && xy.iter().all(|c| b".MADTRC".contains(c))
                && xy != b".."
                && renamed == xy.iter().any(|c| b"RC".contains(c)),
            "invalid porcelain status"
        );
        ensure!(
            sub == b"N..."
                || (sub.len() == 4
                    && sub[0] == b'S'
                    && b".C".contains(&sub[1])
                    && b".M".contains(&sub[2])
                    && b".U".contains(&sub[3])),
            "invalid porcelain submodule state"
        );
        let head_mode = fields.next().context("missing porcelain HEAD mode")?;
        let index_mode = fields.next().context("missing porcelain index mode")?;
        fields.next().context("missing porcelain worktree mode")?;
        let head_oid = fields.next().context("missing porcelain HEAD object")?;
        let index_oid = fields.next().context("missing porcelain index object")?;
        let origin = if renamed {
            fields
                .next()
                .context("missing porcelain rename similarity")?;
            Some(validate_path(rows.next().context("missing porcelain rename origin")?)?.to_owned())
        } else {
            None
        };
        let path = validate_path(fields.next().context("missing porcelain path")?)?.to_owned();
        // The staged raw leg supplies both path endpoints of a staged
        // rename. An unstaged-only row contains this path's pins directly.
        if xy[0] == b'.'
            && let Some(pins) =
                GitlinkPins::from_fields(head_mode, head_oid, index_mode, index_oid)?
        {
            selected.gitlinks.insert(path.clone(), pins);
        }
        if xy[0] != b'.' {
            selected.staged.insert(path.clone());
        }
        // The patch carries checkout pointer changes even for Gitlinks
        // whose child worktrees are outside the declaration walk.
        if xy[1] != b'.' {
            selected.unstaged.insert(path.clone());
            selected.unstaged.extend(origin.iter().cloned());
        }
        selected.entries.push(Entry {
            path,
            status: xy
                .iter()
                .map(|&b| char::from(if b == b'.' { b' ' } else { b }))
                .collect(),
            origin,
        });
    }
    Ok((commit.context("missing Git status HEAD commit")?, selected))
}

#[derive(Default)]
struct StagedPaths<'a> {
    gitlinks: BTreeMap<String, GitlinkPins>,
    modules: Option<submodules::ModuleBlobs<'a>>,
}

fn staged_paths(bytes: &[u8]) -> Result<StagedPaths<'_>> {
    let mut selected = StagedPaths::default();
    let mut rows = records(bytes)?;
    while let Some(raw) = rows.next() {
        let mut fields = raw.split(|byte| *byte == b' ');
        let old = fields
            .next()
            .and_then(|mode| mode.strip_prefix(b":"))
            .context("missing staged source mode")?;
        let new = fields.next().context("missing staged destination mode")?;
        let old_oid = fields.next().context("missing staged source object")?;
        let new_oid = fields.next().context("missing staged destination object")?;
        ensure!(
            fields.next().is_some() && fields.next().is_none(),
            "malformed staged raw record"
        );
        let path = validate_path(rows.next().context("missing staged path")?)?;
        if path == ".gitmodules" {
            selected.modules = Some(submodules::ModuleBlobs {
                head: submodules::config_blob(old, old_oid)?,
                index: submodules::config_blob(new, new_oid)?,
            });
        }
        if let Some(pins) = GitlinkPins::from_fields(old, old_oid, new, new_oid)? {
            ensure!(
                selected.gitlinks.insert(path.to_owned(), pins).is_none(),
                "duplicate staged Gitlink"
            );
        }
    }
    Ok(selected)
}

fn validate_path(path: &[u8]) -> Result<&str> {
    let text = std::str::from_utf8(path).context("Git path is not UTF-8")?;
    ensure!(
        !text.is_empty()
            && !text.contains(['\0', '\r', '\n'])
            && !text
                .trim_end_matches('/')
                .split('/')
                .any(|part| matches!(part, "" | "." | ".."))
            && Path::new(text)
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "unsafe or unsupported Git path"
    );
    Ok(text)
}

fn add_untracked(root: &Dir, path: &str, hash: &mut DeltaHash) -> Result<()> {
    let metadata = root.symlink_metadata(path)?;
    hash.frame(path.as_bytes())?;
    if metadata.is_symlink() {
        let link = root.read_link_contents(path)?;
        hash.frame(b"120000")?;
        #[cfg(unix)]
        return {
            use std::os::unix::ffi::OsStrExt as _;
            hash.frame(link.as_os_str().as_bytes())
        };
        #[cfg(not(unix))]
        return hash.frame(link.to_str().context("non-UTF-8 link target")?.as_bytes());
    }
    ensure!(metadata.is_file(), "unsupported untracked filesystem entry");
    let (file, metadata) = open_regular(root, path)?;
    #[cfg(unix)]
    let mode: &[u8] =
        if std::os::unix::fs::PermissionsExt::mode(&metadata.permissions()) & 0o100 != 0 {
            b"100755"
        } else {
            b"100644"
        };
    #[cfg(not(unix))]
    let mode = b"100644";
    hash.frame(mode)?;
    hash.file(file, metadata.len())
}

fn open_regular(root: &Dir, path: &str) -> Result<(std::fs::File, std::fs::Metadata)> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_fs_ext::OpenOptionsSyncExt;
        options.nonblock(true);
    }
    let file = root.open_with(path, &options)?.into_std();
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "unsupported opened filesystem entry");
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "unsupported filesystem reparse point"
        );
    }
    Ok((file, metadata))
}
