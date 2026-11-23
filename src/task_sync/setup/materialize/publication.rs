//! Stage directories, rename once, and check public files against their bytes.

use super::super::files::Directory;
use super::{
    candidate::{self, Documents},
    journal::Record,
    replacement,
};
use anyhow::{Result, ensure};
use std::path::Path;

pub(super) fn apply(
    root: &Directory,
    private: &Directory,
    record: &Record,
    documents: &Documents,
    completed: bool,
    capacity: usize,
    checkpoint: &mut impl FnMut(&str) -> Result<()>,
) -> Result<bool> {
    let pins = private.child(Path::new("pins"))?;
    let present = root.dir().symlink_metadata(".engram");
    if !record.existing_engram {
        match present {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ensure!(!completed, "completed shared directory disappeared");
                let stage = private.child(Path::new("stage"))?;
                check_tree(&stage, documents)?;
                drop(stage);
                checkpoint("before-engram-rename")?;
                // The accepted std rename residual can replace an empty target.
                // A nonempty target fails; it is never deleted first.
                private.dir().rename("stage", root.dir(), ".engram")?;
                checkpoint("engram-renamed")?;
            }
            Err(error) => return Err(error.into()),
            Ok(_) => (),
        }
        let public = root.child(Path::new(".engram"))?;
        // Only the staged source has an exact four-entry inventory. Once public,
        // unrelated private siblings may coexist with the shared paths.
        check_reserved_names(&public, capacity)?;
        check_shared_tree(&public, documents)?;
        return Ok(false);
    }
    let public = root.child(Path::new(".engram"))?;
    check_reserved_names(&public, capacity)?;
    let stage = private.child(Path::new("stage"))?;
    let expected_empty = !documents.keys().any(|path| path.starts_with("tasks/"));
    let equal_empty = match public.dir().symlink_metadata("tasks") {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(!completed, "completed shared task directory disappeared");
            let tasks = stage.child(Path::new("tasks"))?;
            check_tasks(&tasks, documents)?;
            drop(tasks);
            checkpoint("before-tasks-rename")?;
            stage.dir().rename("tasks", public.dir(), "tasks")?;
            checkpoint("tasks-renamed")?;
            false
        }
        Err(error) => return Err(error.into()),
        Ok(_) => {
            // The staged task directory still in the stage was never moved,
            // so the public one predates this materialization.
            let staged = stage
                .dir()
                .symlink_metadata("tasks")
                .map(|_| true)
                .or_else(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        Ok(false)
                    } else {
                        Err(error)
                    }
                })?;
            if staged {
                ensure!(
                    expected_empty && public.child(Path::new("tasks"))?.names(1)?.is_empty(),
                    "existing shared task directory is divergent; preserve it"
                );
                // Equal existing empty content is reported, never adopted or cleaned.
                true
            } else {
                false
            }
        }
    };
    if !equal_empty {
        check_tasks(&public.child(Path::new("tasks"))?, documents)?;
    }
    let project = documents
        .get("project.json")
        .expect("validated candidate has project document");
    if !replacement::owned(&public, Path::new("project.json"), project)? {
        ensure!(!completed, "completed shared project file disappeared");
        checkpoint("before-project-link")?;
        pins.dir()
            .hard_link("project.json", public.dir(), "project.json")?;
        checkpoint("project-linked")?;
        ensure!(
            replacement::owned(&public, Path::new("project.json"), project)?,
            "shared project missing after publication"
        );
    }
    Ok(equal_empty)
}

pub(super) fn check_reserved_names(public: &Directory, capacity: usize) -> Result<()> {
    let selected = ["project.json", "tasks", ".gitignore", ".gitattributes"];
    for name in public.names_with_room(capacity, &selected)? {
        if let Some(name) = name.to_str()
            && let Some(expected) = selected
                .iter()
                .find(|expected| name.eq_ignore_ascii_case(expected))
        {
            ensure!(
                name == *expected,
                "case alias of reserved shared path; preserve it"
            );
        }
    }
    Ok(())
}

fn check_tree(public: &Directory, documents: &Documents) -> Result<()> {
    ensure!(
        candidate::read_shared(public)? == *documents,
        "published task tree bytes or inventory changed"
    );
    Ok(())
}

fn check_shared_tree(public: &Directory, documents: &Documents) -> Result<()> {
    let project = documents
        .get("project.json")
        .expect("validated project document");
    ensure!(
        replacement::owned(public, Path::new("project.json"), project)?,
        "shared project missing"
    );
    check_tasks(&public.child(Path::new("tasks"))?, documents)
}

fn check_tasks(public: &Directory, documents: &Documents) -> Result<()> {
    let expected: Vec<_> = documents
        .keys()
        .filter_map(|path| path.strip_prefix("tasks/"))
        .map(std::ffi::OsString::from)
        .collect();
    ensure!(
        public.names(100_000)? == expected,
        "shared task inventory changed"
    );
    for (path, bytes) in documents {
        if let Some(name) = path.strip_prefix("tasks/") {
            ensure!(
                replacement::owned(public, Path::new(name), bytes)?,
                "shared task file missing"
            );
        }
    }
    Ok(())
}
