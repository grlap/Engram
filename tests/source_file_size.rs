//! Keeps guarded Rust source files below the project's file-size limit.
//!
//! A guarded family is a module file and the directory of child modules split
//! out of it, so a module extracted from a guarded file is inventoried with it
//! and cannot escape the limit by moving. The children directory is
//! conventionally `MODULE/`, but a family may name an explicit directory
//! instead (for example the binary crate root `src/main`, whose children live
//! under `src/bin_support`). Add a family here when a file is brought under
//! the limit.

#[path = "../src/test_support.rs"]
mod test_support;

use std::{
    fs,
    path::{Path, PathBuf},
};

/// The most physical lines a guarded source file may have.
const LIMIT: usize = 2_499;

/// One guarded family: the module file `MODULE.rs` and every `.rs` file under
/// its children directory, conventionally `MODULE/`.
struct Family {
    /// Module path relative to the crate root, without the `.rs` extension.
    module: &'static str,
    /// The children directory, relative to the crate root, when it is not
    /// the conventional `MODULE/` directory. `None` means `MODULE/`.
    children: Option<&'static str>,
    /// The file was split into child modules, so its children directory must
    /// exist. A family that was not split still has any child directory
    /// inventoried.
    split: bool,
}

const FAMILIES: &[Family] = &[
    Family {
        module: "src/storage/migration/tests",
        children: None,
        split: true,
    },
    Family {
        module: "src/storage/work/completion",
        children: None,
        split: true,
    },
    Family {
        module: "src/storage/work/acceptance_evaluation/tests",
        children: None,
        split: true,
    },
    Family {
        module: "src/verbs/handlers",
        children: None,
        split: true,
    },
    Family {
        module: "src/main",
        children: Some("src/bin_support"),
        split: true,
    },
    Family {
        module: "src/storage/work/planning",
        children: None,
        split: true,
    },
    Family {
        module: "src/storage/work/execution",
        children: None,
        split: true,
    },
    Family {
        module: "src/storage/work/query",
        children: None,
        split: true,
    },
    Family {
        module: "src/storage/graph_snapshot/tests",
        children: None,
        split: true,
    },
];

/// Physical lines: every line feed ends a line, and a nonempty final line
/// without one still counts once. A CRLF ending counts as one line, the same
/// as LF, and blank and comment lines count like any other.
fn physical_lines(bytes: &[u8]) -> usize {
    // Splitting on line feeds yields one more piece than there are line feeds.
    let breaks = bytes.split(|&byte| byte == b'\n').count() - 1;
    breaks + usize::from(bytes.last().is_some_and(|&byte| byte != b'\n'))
}

/// Every file of one family with its physical line count, in path order.
///
/// Refuses a missing or unreadable module file, a missing child directory for
/// a split family, and any child directory, entry or file whose metadata or
/// contents cannot be read: nothing is skipped because it could not be seen.
fn inventory(root: &Path, family: &Family) -> Result<Vec<(String, usize)>, String> {
    let module_file = root.join(format!("{}.rs", family.module));
    let mut counted = vec![count(root, &module_file)?];
    let children = root.join(family.children.unwrap_or(family.module));
    let present = children
        .try_exists()
        .map_err(|error| format!("cannot inspect {}: {error}", children.display()))?;
    if present {
        let mut files = Vec::new();
        collect_rust_files(&children, &mut files)?;
        for file in files {
            counted.push(count(root, &file)?);
        }
    } else if family.split {
        return Err(format!(
            "split family {} has no child directory {}",
            family.module,
            children.display()
        ));
    }
    counted.sort();
    Ok(counted)
}

/// One file's path relative to `root`, with `/` separators, and its count.
fn count(root: &Path, file: &Path) -> Result<(String, usize), String> {
    let bytes =
        fs::read(file).map_err(|error| format!("cannot read {}: {error}", file.display()))?;
    let relative = file
        .strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/");
    Ok((relative, physical_lines(&bytes)))
}

fn collect_rust_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("cannot list {}: {error}", directory.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("cannot list {}: {error}", directory.display()))?;
        let path = entry.path();
        // Follows a link to what it names; an error is refused, never read as
        // "not a directory".
        let metadata = fs::metadata(&path)
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        if metadata.is_dir() {
            collect_rust_files(&path, files)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    Ok(())
}

/// Every guarded file across `families` with its count, in path order, so
/// the report is the same on every run and platform. A file guarded by two
/// families is refused rather than reported twice.
fn guarded_inventory(root: &Path, families: &[Family]) -> Result<Vec<(String, usize)>, String> {
    let mut counted = Vec::new();
    for family in families {
        counted.extend(inventory(root, family)?);
    }
    counted.sort();
    if let Some(pair) = counted.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        return Err(format!("{} is guarded by more than one family", pair[0].0));
    }
    Ok(counted)
}

/// The report line for one guarded file: its path, count and the limit.
fn report_line(path: &str, lines: usize) -> String {
    format!("{path}: {lines} physical lines (limit {LIMIT})")
}

/// The files over the limit, named with their counts.
fn over_limit(counted: &[(String, usize)]) -> Vec<String> {
    counted
        .iter()
        .filter(|(_, lines)| *lines > LIMIT)
        .map(|(path, lines)| format!("{path}: {lines} lines (limit {LIMIT})"))
        .collect()
}

#[test]
fn guarded_source_families_stay_within_the_limit() {
    assert!(!FAMILIES.is_empty(), "no guarded family is listed");
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let counted = guarded_inventory(root, FAMILIES).unwrap_or_else(|error| panic!("{error}"));
    // One line per guarded file, shown by `--nocapture`, so a host-observed
    // run carries every count the limit was checked against.
    for (path, lines) in &counted {
        println!("{}", report_line(path, *lines));
    }
    let offenders = over_limit(&counted);
    assert!(
        offenders.is_empty(),
        "guarded files over the limit:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_limit_admits_2499_lines_and_refuses_2500() {
    let at_limit = physical_lines("line\n".repeat(2_499).as_bytes());
    let over = physical_lines("line\n".repeat(2_500).as_bytes());
    assert_eq!((at_limit, over), (2_499, 2_500));
    assert!(over_limit(&[("at.rs".into(), at_limit)]).is_empty());
    assert_eq!(
        over_limit(&[("over.rs".into(), over)]),
        ["over.rs: 2500 lines (limit 2499)"]
    );
}

#[test]
fn the_inventory_spans_families_in_path_order_and_names_every_oversized_file() {
    let directory = test_support::temp_home().expect("directory");
    let root = directory.path();
    fs::write(root.join("b.rs"), "fn b() {}\n").expect("small module");
    fs::write(root.join("a.rs"), "line\n".repeat(2_500)).expect("oversized module");
    fs::create_dir_all(root.join("a")).expect("children");
    fs::write(root.join("a/child.rs"), "line\n".repeat(2_600)).expect("oversized child");
    // Listed out of path order, and one unsplit: the report is still sorted.
    let families = [
        Family {
            module: "b",
            children: None,
            split: false,
        },
        Family {
            module: "a",
            children: None,
            split: true,
        },
    ];
    let counted = guarded_inventory(root, &families).expect("inventory");
    assert_eq!(
        counted,
        [
            ("a.rs".to_owned(), 2_500),
            ("a/child.rs".to_owned(), 2_600),
            ("b.rs".to_owned(), 1),
        ]
    );
    assert_eq!(
        counted
            .iter()
            .map(|(path, lines)| report_line(path, *lines))
            .collect::<Vec<_>>(),
        [
            "a.rs: 2500 physical lines (limit 2499)",
            "a/child.rs: 2600 physical lines (limit 2499)",
            "b.rs: 1 physical lines (limit 2499)",
        ]
    );
    assert_eq!(
        over_limit(&counted),
        [
            "a.rs: 2500 lines (limit 2499)",
            "a/child.rs: 2600 lines (limit 2499)",
        ]
    );

    // A child module listed as a family of its own overlaps its parent's
    // family: the shared file is refused, not reported twice.
    let overlapping = [
        Family {
            module: "a",
            children: None,
            split: true,
        },
        Family {
            module: "a/child",
            children: None,
            split: false,
        },
    ];
    let refused = guarded_inventory(root, &overlapping).expect_err("overlapping families");
    assert!(
        refused.contains("a/child.rs is guarded by more than one family"),
        "{refused}"
    );
}

#[test]
fn line_endings_and_a_final_unterminated_line_count_consistently() {
    assert_eq!(physical_lines(b"a\nb\n"), 2);
    assert_eq!(physical_lines(b"a\r\nb\r\n"), 2);
    assert_eq!(physical_lines(b"a\nb"), 2);
    assert_eq!(physical_lines(b"a\r\nb"), 2);
    assert_eq!(physical_lines(b"\n\n// comment\n"), 3);
    assert_eq!(physical_lines(b""), 0);
}

#[test]
fn a_missing_module_file_or_split_directory_is_refused_and_children_are_inventoried() {
    let directory = test_support::temp_home().expect("directory");
    let root = directory.path();
    let split = |module| Family {
        module,
        children: None,
        split: true,
    };
    let missing = inventory(root, &split("absent")).expect_err("a missing module file");
    assert!(missing.contains("cannot read"), "{missing}");

    // Children alone do not make an inventory: the module file itself is required.
    fs::create_dir_all(root.join("family")).expect("children");
    fs::write(root.join("family/child.rs"), "fn child() {}\n").expect("child");
    let orphaned = inventory(root, &split("family")).expect_err("children without their file");
    assert!(orphaned.contains("family.rs"), "{orphaned}");

    // A split family whose child directory is gone is refused, not reduced to
    // its module file; an unsplit one is just its module file.
    fs::write(root.join("lonely.rs"), "fn lonely() {}\n").expect("module file");
    let unsplit = inventory(root, &split("lonely")).expect_err("a split family without children");
    assert!(unsplit.contains("has no child directory"), "{unsplit}");
    let standalone = Family {
        module: "lonely",
        children: None,
        split: false,
    };
    assert_eq!(
        inventory(root, &standalone).expect("an unsplit family"),
        [("lonely.rs".to_owned(), 1)]
    );
    // A module later moved out of an unsplit family is still counted with it.
    fs::create_dir_all(root.join("lonely")).expect("children of an unsplit family");
    fs::write(root.join("lonely/extra.rs"), "a\nb\nc\n").expect("moved module");
    assert_eq!(
        inventory(root, &standalone).expect("an unsplit family with children"),
        [
            ("lonely.rs".to_owned(), 1),
            ("lonely/extra.rs".to_owned(), 3)
        ]
    );

    fs::write(root.join("family.rs"), "mod child;\n").expect("module file");
    fs::create_dir_all(root.join("family/nested")).expect("nested");
    fs::write(root.join("family/nested/deeper.rs"), "a\r\nb").expect("nested child");
    fs::write(root.join("family/notes.txt"), "not rust\n").expect("other file");
    assert_eq!(
        inventory(root, &split("family")).expect("inventory"),
        [
            ("family.rs".to_owned(), 1),
            ("family/child.rs".to_owned(), 1),
            ("family/nested/deeper.rs".to_owned(), 2),
        ]
    );
}

#[test]
fn an_explicit_children_directory_is_inventoried_and_a_missing_one_is_refused() {
    let directory = test_support::temp_home().expect("directory");
    let root = directory.path();

    // An explicit children directory is consulted instead of the
    // conventional MODULE/ directory, which is left uninventoried.
    fs::write(root.join("app.rs"), "fn app() {}\n").expect("module file");
    fs::create_dir_all(root.join("app")).expect("conventional directory");
    fs::write(root.join("app/ignored.rs"), "a\nb\nc\nd\n").expect("uninventoried sibling");
    fs::create_dir_all(root.join("support")).expect("explicit children directory");
    fs::write(root.join("support/helper.rs"), "a\nb\n").expect("explicit child");
    let explicit = Family {
        module: "app",
        children: Some("support"),
        split: true,
    };
    assert_eq!(
        inventory(root, &explicit).expect("an explicit children directory"),
        [
            ("app.rs".to_owned(), 1),
            ("support/helper.rs".to_owned(), 2),
        ]
    );

    // A split family naming a missing explicit children directory is
    // refused, the same as a missing conventional one.
    let missing_explicit = Family {
        module: "app",
        children: Some("absent-support"),
        split: true,
    };
    let refused =
        inventory(root, &missing_explicit).expect_err("a missing explicit children directory");
    assert!(refused.contains("has no child directory"), "{refused}");
    assert!(refused.contains("absent-support"), "{refused}");
}
