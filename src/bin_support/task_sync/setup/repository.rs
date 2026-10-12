//! Admission uses local, hardened Git queries and ordinary bounded files.

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use engram::ProjectId;

use super::{Assertions, Basis, policy};

const MAX_BYTES: u64 = 1024 * 1024;
const GIT_BOUND: Duration = Duration::from_secs(30);

pub(super) fn inspect(
    marker: &Path,
    project: &ProjectId,
    root: &Path,
    branch: &str,
    assertions: Assertions,
) -> Result<Basis> {
    ordinary_ancestors(root)?;
    let root = fs::canonicalize(root)?;
    ensure!(
        fs::metadata(&root)?.is_dir(),
        "repository must be an ordinary directory"
    );
    ensure!(
        git(&root, &["rev-parse", "--is-bare-repository"])? == b"false\n",
        "bare repository cannot prepare task sync"
    );
    let top = git_path(&root, "--show-toplevel")?;
    ensure!(
        same_file::is_same_file(&root, &top)?,
        "selected directory is not the repository root"
    );
    let admin = git_path(&root, "--git-dir")?;
    let common = git_path(&root, "--git-common-dir")?;
    ensure!(
        same_file::is_same_file(&admin, &common)?,
        "linked worktree cannot prepare task sync; use the main checkout"
    );
    // Git's main inventory entry is the administration directory for
    // --separate-git-dir without core.worktree. The worktree top-level and
    // equality of git-dir/common-dir establish the selected main checkout;
    // comparing that inventory entry to the checkout would reject this layout.
    ordinary_ancestors(&admin)?;
    let marker_path = std::path::absolute(marker)?;
    ordinary_ancestors(&marker_path)?;
    ensure!(
        same_file::is_same_file(
            marker_path.parent().context("marker has no directory")?,
            &root
        )?,
        "selected project marker must belong to the repository root"
    );
    let marker_bytes = read_optional(&marker_path)?.context("project marker missing")?;
    ensure!(
        marker_bytes.len() <= 4096 && std::str::from_utf8(&marker_bytes)?.trim() == project.0,
        "selected project marker changed or differs from project"
    );
    ensure!(
        !branch.is_empty() && !branch.starts_with('-'),
        "select an ordinary branch name"
    );
    git(
        &root,
        &["check-ref-format", &format!("refs/heads/{branch}")],
    )?;
    let actual_branch = git(&root, &["symbolic-ref", "--quiet", "HEAD"])?;
    ensure!(
        actual_branch == format!("refs/heads/{branch}\n").as_bytes(),
        "attached branch differs from explicitly selected branch"
    );
    let sparse = git_allow_missing(&root, &["config", "--bool", "--get", "core.sparseCheckout"])?;
    ensure!(
        sparse.is_empty() || sparse == b"false\n",
        "sparse checkout cannot prepare task sync"
    );
    let marker_path = fs::canonicalize(marker_path)?;
    let marker_name = marker_path
        .file_name()
        .and_then(|name| name.to_str())
        .context("selected project marker must have a UTF-8 filename")?;
    // No reserved shared state can be adopted, including ignored equal bytes.
    // Do this before recursive status can apply a filename-specific filter.
    ordinary_ancestors(&root.join(".engram"))?;
    for relative in [
        ".engram/project.json",
        ".engram/tasks",
        ".engram/.gitignore",
    ] {
        ordinary_ancestors(&root.join(relative))?;
        ensure!(
            fs::symlink_metadata(root.join(relative))
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
            "reserved shared path is occupied: {relative}; relocate explicitly before preparation"
        );
    }
    let paths = [
        ".engram/project.json",
        ".engram/tasks",
        ".engram/.gitignore",
        ".gitignore",
        ".gitattributes",
        marker_name,
    ];
    // Git pathspecs are case-sensitive even on a case-folding checkout.
    // Inspect bounded names without pathspecs, then conservatively refuse
    // aliases of the admitted paths before status can apply their filters.
    for entry in git(&root, &["ls-files", "-v", "-z"])?
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        ensure!(
            entry.len() > 2 && entry[1] == b' ',
            "malformed Git index result"
        );
        let name = std::str::from_utf8(&entry[2..])?;
        let Some(selected) = selected_path(name, &paths) else {
            continue;
        };
        ensure!(
            !selected.starts_with(".engram/"),
            "reserved shared path is occupied in index: {name}"
        );
        ensure!(
            name == selected,
            "case alias of setup path in index: {name}"
        );
        ensure!(
            entry[0] != b'S' && !entry[0].is_ascii_lowercase(),
            "index inspection is suppressed for setup path: {name}; resolve its flags first"
        );
    }
    for entry in git(&root, &["ls-tree", "-r", "--name-only", "-z", "HEAD"])?
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let name = std::str::from_utf8(entry)?;
        if let Some(selected) = selected_path(name, &paths) {
            ensure!(
                !selected.starts_with(".engram/"),
                "reserved shared path is occupied in HEAD: {name}"
            );
            ensure!(name == selected, "case alias of setup path in HEAD: {name}");
        }
    }
    // Check actual attributes before status could compare filtered setup files.
    // Merely installing a filter is not a reason to refuse a repository.
    super::attributes::check(&root, &admin, marker_name)?;
    let head = String::from_utf8(git(&root, &["rev-parse", "--verify", "HEAD"])?)?
        .trim()
        .to_owned();
    let mut args = vec![
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--ignored=no",
        "--",
    ];
    args.extend(paths);
    ensure!(
        git(&root, &args)?.is_empty(),
        "reserved/setup paths are dirty, staged or conflicted; preserve and resolve them first"
    );
    let mut edits = Vec::new();
    for relative in [".gitignore", ".gitattributes"] {
        edits.push(policy::propose(
            relative,
            read_optional(&root.join(relative))?,
        )?);
    }
    Ok(Basis {
        repository: root,
        git_directory: admin,
        project_file: marker_path,
        project_id: project.clone(),
        marker: marker_bytes,
        branch: branch.into(),
        head,
        assertions,
        edits,
    })
}

fn selected_path<'a>(name: &str, paths: &[&'a str]) -> Option<&'a str> {
    paths.iter().copied().find(|path| {
        name.eq_ignore_ascii_case(path)
            || (path.starts_with(".engram/")
                && name
                    .get(..path.len())
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(path))
                && name.as_bytes().get(path.len()) == Some(&b'/'))
    })
}

fn git_path(root: &Path, flag: &str) -> Result<PathBuf> {
    let bytes = git(root, &["rev-parse", "--path-format=absolute", flag])?;
    let text = std::str::from_utf8(&bytes)?
        .strip_suffix('\n')
        .context("unterminated Git path")?;
    ensure!(!text.contains(['\r', '\n', '\0']), "ambiguous Git path");
    ordinary_ancestors(Path::new(text))?;
    Ok(fs::canonicalize(text)?)
}

pub(super) fn ordinary_ancestors(path: &Path) -> Result<()> {
    let absolute = std::path::absolute(path)?;
    for part in absolute.ancestors() {
        match fs::symlink_metadata(part) {
            Ok(meta) => {
                ensure!(
                    !meta.file_type().is_symlink(),
                    "linked path refused: {}",
                    part.display()
                );
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    ensure!(
                        meta.file_attributes() & 0x400 == 0,
                        "reparse path refused: {}",
                        part.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(super) fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    ordinary_ancestors(path)?;
    let parent = super::files::Directory::open(path.parent().context("setup file has no parent")?)?;
    let bytes = parent.read(Path::new(
        path.file_name().context("setup file has no name")?,
    ))?;
    ensure!(
        bytes.as_ref().is_none_or(|bytes| bytes.len() <= 64 * 1024),
        "setup file exceeds 64 KiB bound"
    );
    Ok(bytes)
}

pub(super) fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    git_run(root, args, false)
}

fn git_allow_missing(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    git_run(root, args, true)
}

fn git_run(root: &Path, args: &[&str], missing: bool) -> Result<Vec<u8>> {
    let mut command = crate::bin_support::doctor::landings::git_command(root, args);
    command.env_remove("GIT_ATTR_SOURCE");
    command.env("GIT_LITERAL_PATHSPECS", "1");
    command.stdout(Stdio::piped());
    let mut child = command.spawn().context("start local Git admission query")?;
    let stdout = child.stdout.take().context("missing Git stdout")?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(result);
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < GIT_BOUND => {
                std::thread::sleep(Duration::from_millis(10));
            }
            outcome => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("local Git query failed or exceeded its bound: {outcome:?}");
            }
        }
    };
    let bytes = receiver.recv_timeout(
        GIT_BOUND
            .saturating_sub(start.elapsed())
            .max(Duration::from_secs(1)),
    )??;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "Git admission output exceeded byte bound"
    );
    ensure!(
        status.success() || (missing && status.code() == Some(1) && bytes.is_empty()),
        "local Git admission query refused: {}",
        args[0]
    );
    Ok(bytes)
}
