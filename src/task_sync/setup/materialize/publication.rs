//! Stage directories, rename once, and prove public files against retained links.

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
    checkpoint: &mut impl FnMut(&str) -> Result<()>,
) -> Result<bool> {
    let pins = private.child(Path::new("pins"))?;
    let present = root.dir().symlink_metadata(".engram");
    if record.existing_engram.is_none() {
        match present {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ensure!(!completed, "completed shared directory disappeared");
                let stage = private.child(Path::new("stage"))?;
                ensure!(
                    stage.identity()? == record.stage
                        && stage.child(Path::new("tasks"))?.identity()? == record.stage_tasks,
                    "staged directory identity changed"
                );
                check_tree(&stage, &pins, documents)?;
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
        ensure!(
            public.identity()? == record.stage,
            "shared directory is unowned; preserve it"
        );
        ensure!(
            public.child(Path::new("tasks"))?.identity()? == record.stage_tasks,
            "shared task directory was independently replaced"
        );
        // Only the staged source has an exact two-entry inventory. Once public,
        // unrelated private siblings may coexist with the owned shared paths.
        check_reserved_names(&public)?;
        check_owned_tree(&public, &pins, documents)?;
        return Ok(false);
    }
    let public = root.child(Path::new(".engram"))?;
    ensure!(
        Some(public.identity()?) == record.existing_engram,
        "existing private .engram directory changed; preserve it"
    );
    check_reserved_names(&public)?;
    let stage = private.child(Path::new("stage"))?;
    ensure!(
        stage.identity()? == record.stage,
        "staged directory identity changed"
    );
    let expected_empty = !documents.keys().any(|path| path.starts_with("tasks/"));
    let equal_empty = match public.dir().symlink_metadata("tasks") {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(!completed, "completed shared task directory disappeared");
            let tasks = stage.child(Path::new("tasks"))?;
            ensure!(
                tasks.identity()? == record.stage_tasks,
                "staged tasks identity changed"
            );
            check_tasks(&tasks, &pins.child(Path::new("tasks"))?, documents)?;
            drop(tasks);
            checkpoint("before-tasks-rename")?;
            stage.dir().rename("tasks", public.dir(), "tasks")?;
            checkpoint("tasks-renamed")?;
            false
        }
        Err(error) => return Err(error.into()),
        Ok(_) => {
            let tasks = public.child(Path::new("tasks"))?;
            if tasks.identity()? == record.stage_tasks {
                check_tasks(&tasks, &pins.child(Path::new("tasks"))?, documents)?;
                false
            } else {
                ensure!(
                    expected_empty && tasks.names(1)?.is_empty(),
                    "unowned shared task directory is divergent; preserve it"
                );
                // Equal unowned empty content is reported, never owned or cleaned.
                true
            }
        }
    };
    let tasks = public.child(Path::new("tasks"))?;
    if !equal_empty {
        ensure!(
            tasks.identity()? == record.stage_tasks,
            "published tasks identity changed"
        );
        check_tasks(&tasks, &pins.child(Path::new("tasks"))?, documents)?;
    }
    let project = documents
        .get("project.json")
        .expect("validated candidate has project document");
    if !replacement::owned(&public, Path::new("project.json"), &pins, project)? {
        ensure!(!completed, "completed shared project file disappeared");
        checkpoint("before-project-link")?;
        pins.dir()
            .hard_link("project.json", public.dir(), "project.json")?;
        checkpoint("project-linked")?;
        ensure!(
            replacement::owned(&public, Path::new("project.json"), &pins, project)?,
            "shared project missing after publication"
        );
    }
    Ok(equal_empty)
}

pub(super) fn check_reserved_names(public: &Directory) -> Result<()> {
    for name in public.names_with_room(100_000, &["project.json", "tasks"])? {
        if let Some(name) = name.to_str()
            && let Some(expected) = ["project.json", "tasks"]
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

fn check_tree(public: &Directory, pins: &Directory, documents: &Documents) -> Result<()> {
    ensure!(
        candidate::read(public)? == *documents,
        "published task tree bytes or inventory changed"
    );
    check_owned_tree(public, pins, documents)
}

fn check_owned_tree(public: &Directory, pins: &Directory, documents: &Documents) -> Result<()> {
    let project = documents
        .get("project.json")
        .expect("validated project document");
    ensure!(
        replacement::owned(public, Path::new("project.json"), pins, project)?,
        "shared project missing"
    );
    check_tasks(
        &public.child(Path::new("tasks"))?,
        &pins.child(Path::new("tasks"))?,
        documents,
    )
}

fn check_tasks(public: &Directory, pins: &Directory, documents: &Documents) -> Result<()> {
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
                replacement::owned(public, Path::new(name), pins, bytes)?,
                "shared task file missing"
            );
        }
    }
    Ok(())
}
