//! Current-path preflight. Materialization must recheck every actual task path.

use super::repository::{git, ordinary_ancestors, read_optional};
use anyhow::{Context, Result, ensure};
use std::{fs, path::Path};

pub(super) fn check(root: &Path, admin: &Path, marker_name: &str) -> Result<()> {
    // A deeper file would override the proposed managed root block. Refuse
    // occupancy, including ignored files, before Git reads it or runs status.
    let nested = root.join(".engram/.gitattributes");
    ordinary_ancestors(&nested)?;
    ensure!(
        fs::symlink_metadata(&nested).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "nested attribute file is occupied: .engram/.gitattributes"
    );
    // Inspect ordinary bounded files before Git's attribute reader accesses them.
    // In particular, status must not recurse into a directory named .gitignore
    // and apply an unchecked filter to one of its children.
    read_optional(&root.join(".gitignore"))?;
    read_optional(&root.join(".gitattributes"))?;
    let info = admin.join("info/attributes");
    ordinary_ancestors(&info)?;
    match fs::symlink_metadata(&info) {
        Ok(_) => {
            read_optional(&info)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    let paths = [
        ".engram/project.json",
        ".engram/tasks/00000000-0000-7000-8000-000000000001.json",
        ".gitignore",
        ".gitattributes",
        marker_name,
    ];
    let mut args = vec![
        "check-attr",
        "-z",
        "filter",
        "diff",
        "merge",
        "working-tree-encoding",
        "eol",
        "ident",
        "--",
    ];
    args.extend(paths);
    let output = git(root, &args)?;
    let fields: Vec<_> = output.split(|byte| *byte == 0).collect();
    ensure!(
        fields.last() == Some(&&b""[..]) && (fields.len() - 1) % 3 == 0,
        "malformed Git attribute result"
    );
    ensure!(
        fields.len() - 1 == paths.len() * 6 * 3,
        "incomplete Git attribute result"
    );
    for record in fields[..fields.len() - 1].as_chunks::<3>().0 {
        let path = std::str::from_utf8(record[0])?;
        let attribute = std::str::from_utf8(record[1])?;
        let value = std::str::from_utf8(record[2])?;
        let safe = matches!(value, "unset" | "unspecified")
            || matches!(
                (attribute, value),
                ("eol", "lf") | ("diff" | "merge", "set")
            );
        ensure!(
            safe,
            "Git attribute {attribute}={value} applies to {path}; inspect and resolve before setup preparation"
        );
    }
    Ok(())
}

pub(super) fn materialized(
    basis: &super::Basis,
    documents: &std::collections::BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    let root = &basis.repository;
    for relative in [
        ".engram/.gitattributes",
        ".engram/.gitignore",
        ".engram/tasks/.gitattributes",
        ".engram/tasks/.gitignore",
    ] {
        let path = root.join(relative);
        ordinary_ancestors(&path)?;
        ensure!(
            fs::symlink_metadata(path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
            "nested shared policy is occupied; preserve it and resolve before publication"
        );
    }
    // Safely inspect the complete ordinary files before Git reads them. Report
    // effective policy refusals before checking exact recorded after bytes, so
    // an appended ignore rule retains its actionable policy/remedy diagnostic.
    let directory = super::files::Directory::open(root)?;
    for edit in &basis.edits {
        directory.read(Path::new(&edit.path))
            .with_context(|| format!("published setup {} cannot be fully inspected within the safety bound; preserve state and restore recorded after bytes", edit.path))?
            .with_context(|| format!("published setup {} is missing; preserve state and restore recorded after bytes", edit.path))?;
    }
    let info = basis.git_directory.join("info/attributes");
    ordinary_ancestors(&info)?;
    if info.exists() {
        read_optional(&info)?;
    }
    let paths: Vec<_> = documents
        .keys()
        .map(|path| format!(".engram/{path}"))
        .collect();
    for paths in paths.chunks(128) {
        let mut args = vec![
            "check-attr",
            "-z",
            "text",
            "eol",
            "filter",
            "diff",
            "merge",
            "working-tree-encoding",
            "ident",
            "--",
        ];
        args.extend(paths.iter().map(String::as_str));
        let output = git(root, &args)?;
        let fields: Vec<_> = output.split(|byte| *byte == 0).collect();
        ensure!(
            fields.last() == Some(&&b""[..]) && fields.len() - 1 == paths.len() * 7 * 3,
            "incomplete materialized Git attribute result"
        );
        for (index, record) in fields[..fields.len() - 1]
            .as_chunks::<3>()
            .0
            .iter()
            .enumerate()
        {
            let path = std::str::from_utf8(record[0])?;
            let attribute = std::str::from_utf8(record[1])?;
            let value = std::str::from_utf8(record[2])?;
            let expected = [
                "text",
                "eol",
                "filter",
                "diff",
                "merge",
                "working-tree-encoding",
                "ident",
            ][index % 7];
            ensure!(
                path == paths[index / 7] && attribute == expected,
                "misrouted materialized Git attribute result"
            );
            let safe = matches!(
                (attribute, value),
                ("text" | "diff" | "merge", "set")
                    | ("eol", "lf")
                    | ("filter" | "working-tree-encoding" | "ident", "unset")
            );
            ensure!(
                safe,
                "effective task attribute policy refused for {path}: {attribute}; no task publication approved"
            );
        }
        let input: Vec<_> = paths
            .iter()
            .flat_map(|path| path.as_bytes().iter().copied().chain(std::iter::once(0)))
            .collect();
        ensure!(
            super::repository::git_input_allow_missing(
                root,
                &["check-ignore", "--no-index", "-z", "--stdin"],
                &input
            )?
            .is_empty(),
            "shared task path is ignored; resolve policy before publication"
        );
    }
    // Managed blocks can exceed the before-file cap. A policy-safe edit must
    // still match the complete recorded output before any shared publication.
    for edit in &basis.edits {
        ensure!(
            directory.read_bounded(Path::new(&edit.path), edit.after.len())
                .with_context(|| format!("published setup {} differs from recorded after bytes; preserve state and restore them before retrying", edit.path))?
                .as_ref() == Some(&edit.after),
            "published setup {} differs from recorded after bytes; preserve state and restore them before retrying", edit.path
        );
    }
    Ok(())
}
