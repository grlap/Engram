//! Keeps every Rust source file under `src` below the project's file-size
//! limit.
//!
//! The whole-tree check counts every `.rs` file under `src`, however it got
//! there, so a new or growing file cannot pass unlisted. A file already over
//! the limit passes only as a known exception, listed by path in
//! `source_file_size_exceptions.json` with its current count as its ceiling
//! and the item that splits it. The check fails if it grows past that ceiling
//! or shrinks below it without the ceiling coming down, and once it is back
//! within the limit its entry must go.
//!
//! Guarded families remain for per-file evidence. A guarded family is a module
//! file and the directory of child modules split out of it, so a module
//! extracted from a guarded file is inventoried with it. The children
//! directory is conventionally `MODULE/`, but a family may name an explicit
//! directory instead (for example the binary crate root `src/main`, whose
//! children live under `src/bin_support`). Each family has a test of its own
//! whose report is small enough for a host-observed run. Admission does not
//! require splitting: `split: false` also guards an unsplit file.

#[path = "../src/test_support.rs"]
mod test_support;

use serde::Deserialize;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

/// The most physical lines a source file may have, unless it is a known
/// exception.
const LIMIT: usize = 2_499;

/// The known exceptions, relative to the crate root.
const EXCEPTIONS_FILE: &str = "tests/source_file_size_exceptions.json";

/// A source file admitted over the limit when the whole-tree check began
/// covering it: it may not grow past its ceiling, the ceiling comes down as it
/// shrinks, and it names the item that splits it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Exception {
    /// Repository-relative path with `/` separators, such as `src/a/b.rs`.
    path: String,
    /// Its ceiling: its current physical line count, over the limit.
    max_lines: usize,
    /// Where the work that brings it under the limit is tracked.
    split_item: String,
}

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

/// Declares every guarded family once: the `FAMILIES` list the all-families
/// check uses, and one test per family, `family::NAME`, that checks and
/// prints only that family. A host that needs one family's evidence runs
/// that test alone with `--exact`, so its report stays small.
macro_rules! guarded_families {
    ($($name:ident => $family:expr;)+) => {
        const FAMILIES: &[Family] = &[$($family),+];

        mod family {
            use super::*;

            $(
                #[test]
                fn $name() {
                    check_family(&$family);
                }
            )+
        }
    };
}

guarded_families! {
    storage_migration_tests => Family {
        module: "src/storage/migration/tests",
        children: None,
        split: true,
    };
    storage_work_completion => Family {
        module: "src/storage/work/completion",
        children: None,
        split: true,
    };
    storage_work_acceptance_evaluation => Family {
        module: "src/storage/work/acceptance_evaluation",
        children: None,
        split: true,
    };
    verbs_handlers => Family {
        module: "src/verbs/handlers",
        children: None,
        split: true,
    };
    main => Family {
        module: "src/main",
        children: Some("src/bin_support"),
        split: true,
    };
    storage_work_planning => Family {
        module: "src/storage/work/planning",
        children: None,
        split: true,
    };
    storage_work_execution => Family {
        module: "src/storage/work/execution",
        children: None,
        split: true,
    };
    storage_work_query => Family {
        module: "src/storage/work/query",
        children: None,
        split: true,
    };
    storage_graph_snapshot_tests => Family {
        module: "src/storage/graph_snapshot/tests",
        children: None,
        split: true,
    };
}

/// Most bytes one family's report may take, so a host-observed run of that
/// family's test fits a 4096-byte verification summary together with the
/// command, its exit status and the harness's result line.
const FAMILY_REPORT_BUDGET: usize = 3_072;

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

/// The metadata of `path` itself, refusing a link rather than following it:
/// one pointing back up the tree would recurse without end, and one pointing
/// elsewhere would count foreign files under this tree's names. An error is
/// refused, never read as "not a directory".
fn inspect(path: &Path) -> Result<fs::Metadata, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "{} is a link; the size check counts files, never links",
            path.display()
        ));
    }
    Ok(metadata)
}

/// One file's path relative to `root`, with `/` separators, and its count.
/// A link is refused, so a module file cannot stand in for another file.
fn count(root: &Path, file: &Path) -> Result<(String, usize), String> {
    inspect(file)?;
    let bytes =
        fs::read(file).map_err(|error| format!("cannot read {}: {error}", file.display()))?;
    let relative = file
        .strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/");
    Ok((relative, physical_lines(&bytes)))
}

/// Every `.rs` file under `directory`, which may not itself be a link, nor
/// hold one at any depth.
fn collect_rust_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    inspect(directory)?;
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("cannot list {}: {error}", directory.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("cannot list {}: {error}", directory.display()))?;
        let path = entry.path();
        let metadata = inspect(&path)?;
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

/// Every `.rs` file under `root/src`, at any depth, with its count, in path
/// order. Nothing under `src` is skipped because it could not be read.
fn tree_inventory(root: &Path) -> Result<Vec<(String, usize)>, String> {
    let mut files = Vec::new();
    collect_rust_files(&root.join("src"), &mut files)?;
    let mut counted = files
        .iter()
        .map(|file| count(root, file))
        .collect::<Result<Vec<_>, _>>()?;
    counted.sort();
    Ok(counted)
}

/// Reads and checks the known exceptions under `root`.
fn load_exceptions(root: &Path) -> Result<Vec<Exception>, String> {
    let file = root.join(EXCEPTIONS_FILE);
    let text = fs::read_to_string(&file)
        .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
    parse_exceptions(&text)
}

/// Parses the known exceptions, refusing anything but a JSON array of
/// `{path, max_lines, split_item}` objects that each name a `.rs` file under
/// `src` by a plain `/`-separated path, admit it over the limit, name a split
/// item, and appear once.
fn parse_exceptions(text: &str) -> Result<Vec<Exception>, String> {
    let exceptions: Vec<Exception> = serde_json::from_str(text)
        .map_err(|error| format!("{EXCEPTIONS_FILE} is malformed: {error}"))?;
    let mut seen = BTreeSet::new();
    for exception in &exceptions {
        let path = &exception.path;
        let plain = path.strip_prefix("src/").is_some_and(|rest| {
            rest.split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..")
        });
        // The same extension test that collects files, so an entry can only
        // name a file the inventory counts.
        let rust = Path::new(path)
            .extension()
            .is_some_and(|extension| extension == "rs");
        if !plain || !rust || path.contains('\\') {
            return Err(format!(
                "{EXCEPTIONS_FILE}: {path:?} is not a plain path to a .rs file under src"
            ));
        }
        if exception.max_lines <= LIMIT {
            return Err(format!(
                "{EXCEPTIONS_FILE}: {path} admits {} lines, within the limit {LIMIT}; \
                 a file within the limit needs no exception",
                exception.max_lines
            ));
        }
        if exception.split_item.trim().is_empty() {
            return Err(format!("{EXCEPTIONS_FILE}: {path} names no split item"));
        }
        if !seen.insert(path.as_str()) {
            return Err(format!("{EXCEPTIONS_FILE}: {path} is listed twice"));
        }
    }
    Ok(exceptions)
}

/// The known exception for `path`, if any.
fn exception_for<'a>(exceptions: &'a [Exception], path: &str) -> Option<&'a Exception> {
    exceptions.iter().find(|exception| exception.path == path)
}

/// The report line for one counted file: its path, count and the limit, or
/// the ceiling of its known exception. It never names the split item.
fn report_line(path: &str, lines: usize, exceptions: &[Exception]) -> String {
    match exception_for(exceptions, path) {
        Some(exception) => format!(
            "{path}: {lines} physical lines (known exception, ceiling {})",
            exception.max_lines
        ),
        None => format!("{path}: {lines} physical lines (limit {LIMIT})"),
    }
}

/// The report lines for counted files, one per file in their order.
fn report_lines(counted: &[(String, usize)], exceptions: &[Exception]) -> Vec<String> {
    counted
        .iter()
        .map(|(path, lines)| report_line(path, *lines, exceptions))
        .collect()
}

/// One family's report lines, one per file in path order, or the files of
/// that family over the limit or past their known exception's ceiling.
/// Exceptions for files outside the family are neither applied nor checked.
fn family_report(
    root: &Path,
    family: &Family,
    exceptions: &[Exception],
) -> Result<Vec<String>, String> {
    let counted = inventory(root, family)?;
    let offenders = over_limit(&counted, exceptions);
    if !offenders.is_empty() {
        return Err(format!(
            "guarded files over the limit:\n{}",
            offenders.join("\n")
        ));
    }
    Ok(report_lines(&counted, exceptions))
}

/// The whole tree's report lines, one per `.rs` file under `src` in path
/// order. Refuses any file over the limit or past its known exception's
/// ceiling, and any exception that names no source file, a file now within
/// the limit, or a file below its ceiling. So the list never outlives what it
/// excuses, and a ceiling only ever comes down.
fn whole_tree_report(root: &Path, exceptions: &[Exception]) -> Result<Vec<String>, String> {
    let counted = tree_inventory(root)?;
    let mut problems = Vec::new();
    let offenders = over_limit(&counted, exceptions);
    if !offenders.is_empty() {
        problems.push(format!(
            "source files over the limit:\n{}",
            offenders.join("\n")
        ));
    }
    let stale = stale_exceptions(&counted, exceptions);
    if !stale.is_empty() {
        problems.push(format!(
            "known exceptions to remove or lower in {EXCEPTIONS_FILE}:\n{}",
            stale.join("\n")
        ));
    }
    if !problems.is_empty() {
        return Err(problems.join("\n"));
    }
    Ok(report_lines(&counted, exceptions))
}

/// Exceptions that name no counted file, a file now within the limit, or a
/// file that shrank below its ceiling, whose ceiling must come down to its
/// count so the file can never grow back. A file over its ceiling is an
/// offender, reported by [`over_limit`].
fn stale_exceptions(counted: &[(String, usize)], exceptions: &[Exception]) -> Vec<String> {
    exceptions
        .iter()
        .filter_map(|exception| {
            let path = &exception.path;
            match counted
                .iter()
                .find(|(counted_path, _)| counted_path == path)
            {
                None => Some(format!("{path}: names no source file")),
                Some((_, lines)) if *lines <= LIMIT => Some(format!(
                    "{path}: {lines} lines is within the limit {LIMIT}; remove its entry"
                )),
                Some((_, lines)) if *lines < exception.max_lines => Some(format!(
                    "{path}: {lines} lines is under its ceiling {}; lower its max_lines to {lines}",
                    exception.max_lines
                )),
                Some(_) => None,
            }
        })
        .collect()
}

/// Bytes a report takes as printed lines.
fn report_bytes(report: &[String]) -> usize {
    report.iter().map(|line| line.len() + 1).sum()
}

/// Refuses a family report larger than `budget` bytes as printed: one
/// host-observed run of that family would no longer fit a verification
/// summary.
fn within_report_budget(module: &str, report: &[String], budget: usize) -> Result<(), String> {
    let bytes = report_bytes(report);
    if bytes > budget {
        return Err(format!(
            "{module} reports {bytes} bytes, over the {budget}-byte family report budget, \
             so one host-observed run of it no longer fits a verification summary"
        ));
    }
    Ok(())
}

/// Checks one family of this crate and prints its report, which must fit
/// the family report budget.
fn check_family(family: &Family) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let exceptions = load_exceptions(root).unwrap_or_else(|error| panic!("{error}"));
    let report = family_report(root, family, &exceptions).unwrap_or_else(|error| panic!("{error}"));
    for line in &report {
        println!("{line}");
    }
    within_report_budget(family.module, &report, FAMILY_REPORT_BUDGET)
        .unwrap_or_else(|error| panic!("{error}"));
}

/// The files over the limit, or past their known exception's ceiling, named
/// with their counts.
fn over_limit(counted: &[(String, usize)], exceptions: &[Exception]) -> Vec<String> {
    counted
        .iter()
        .filter_map(|(path, lines)| match exception_for(exceptions, path) {
            Some(exception) if *lines > exception.max_lines => Some(format!(
                "{path}: {lines} lines (known exception, ceiling {})",
                exception.max_lines
            )),
            None if *lines > LIMIT => Some(format!("{path}: {lines} lines (limit {LIMIT})")),
            Some(_) | None => None,
        })
        .collect()
}

#[test]
fn every_source_file_stays_within_the_limit() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let exceptions = load_exceptions(root).unwrap_or_else(|error| panic!("{error}"));
    let report = whole_tree_report(root, &exceptions).unwrap_or_else(|error| panic!("{error}"));
    assert!(!report.is_empty(), "no source file was counted under src");
    // One line per source file, shown by `--nocapture`.
    for line in &report {
        println!("{line}");
    }
}

#[test]
fn guarded_source_families_stay_within_the_limit() {
    assert!(!FAMILIES.is_empty(), "no guarded family is listed");
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let exceptions = load_exceptions(root).unwrap_or_else(|error| panic!("{error}"));
    let counted = guarded_inventory(root, FAMILIES).unwrap_or_else(|error| panic!("{error}"));
    // One line per guarded file, shown by `--nocapture`, so a host-observed
    // run carries every count the limit was checked against.
    for line in report_lines(&counted, &exceptions) {
        println!("{line}");
    }
    let offenders = over_limit(&counted, &exceptions);
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
    assert!(over_limit(&[("at.rs".into(), at_limit)], &[]).is_empty());
    assert_eq!(
        over_limit(&[("over.rs".into(), over)], &[]),
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
        report_lines(&counted, &[]),
        [
            "a.rs: 2500 physical lines (limit 2499)",
            "a/child.rs: 2600 physical lines (limit 2499)",
            "b.rs: 1 physical lines (limit 2499)",
        ]
    );
    assert_eq!(
        over_limit(&counted, &[]),
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
fn one_family_is_checked_and_reported_alone() {
    let directory = test_support::temp_home().expect("directory");
    let root = directory.path();
    fs::write(root.join("a.rs"), "mod child;\n").expect("module");
    fs::create_dir_all(root.join("a")).expect("children");
    fs::write(root.join("a/child.rs"), "a\nb\n").expect("child");
    fs::write(root.join("b.rs"), "line\n".repeat(2_500)).expect("oversized module");
    let family = |module| Family {
        module,
        children: None,
        split: false,
    };

    // A family's report names only its own files, and an oversized file
    // elsewhere does not fail it.
    let report = family_report(root, &family("a"), &[]).expect("family a");
    assert_eq!(
        report,
        [
            "a.rs: 1 physical lines (limit 2499)",
            "a/child.rs: 2 physical lines (limit 2499)",
        ]
    );
    // Printed, the two lines take 35 and 41 bytes plus a line feed each. The
    // budget admits a report of exactly its size and refuses one byte less.
    assert_eq!(report_bytes(&report), 78);
    within_report_budget("a", &report, 78).expect("a report at the budget fits");
    let over = within_report_budget("a", &report, 77).expect_err("one byte over the budget");
    assert!(
        over.contains("a reports 78 bytes, over the 77-byte family report budget"),
        "{over}"
    );
    let refused = family_report(root, &family("b"), &[]).expect_err("family b is over the limit");
    assert!(
        refused.contains("b.rs: 2500 lines (limit 2499)"),
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
    // Checked for a link before it is read, so the missing file fails there.
    assert!(missing.contains("cannot inspect"), "{missing}");
    assert!(missing.contains("absent.rs"), "{missing}");

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

/// One parsed exception, for fixtures.
fn exception(path: &str, max_lines: usize) -> Exception {
    Exception {
        path: path.to_owned(),
        max_lines,
        split_item: "split-it".to_owned(),
    }
}

#[test]
fn every_file_under_src_is_counted_and_an_unlisted_one_over_the_limit_fails() {
    let directory = test_support::temp_home().expect("directory");
    let root = directory.path();
    fs::create_dir_all(root.join("src/deep/er")).expect("nested source directories");
    fs::create_dir_all(root.join("other")).expect("directory outside src");
    fs::write(root.join("src/lib.rs"), "mod deep;\n").expect("crate root");
    fs::write(root.join("src/deep/er/leaf.rs"), "line\n".repeat(2_499)).expect("leaf");
    fs::write(root.join("src/deep/notes.txt"), "line\n".repeat(3_000)).expect("not rust");
    fs::write(root.join("other/big.rs"), "line\n".repeat(3_000)).expect("outside src");

    // A file at the limit passes, at any depth; files outside src and files
    // that are not Rust are not counted.
    assert_eq!(
        whole_tree_report(root, &[]).expect("within the limit"),
        [
            "src/deep/er/leaf.rs: 2499 physical lines (limit 2499)",
            "src/lib.rs: 1 physical lines (limit 2499)",
        ]
    );

    // One line more, in a file no family lists and no exception names, fails.
    fs::write(root.join("src/deep/er/leaf.rs"), "line\n".repeat(2_500)).expect("grown leaf");
    let refused = whole_tree_report(root, &[]).expect_err("an unlisted file over the limit");
    assert!(
        refused.contains("src/deep/er/leaf.rs: 2500 lines (limit 2499)"),
        "{refused}"
    );
    assert!(!refused.contains("other/big.rs"), "{refused}");
}

#[test]
fn a_known_exception_passes_at_its_ceiling_and_fails_one_line_over() {
    let directory = test_support::temp_home().expect("directory");
    let root = directory.path();
    fs::create_dir_all(root.join("src/big")).expect("source directory");
    fs::write(root.join("src/big.rs"), "line\n".repeat(2_600)).expect("oversized module");
    let exceptions = [exception("src/big.rs", 2_600)];

    let report = whole_tree_report(root, &exceptions).expect("an exception at its ceiling");
    assert_eq!(
        report,
        ["src/big.rs: 2600 physical lines (known exception, ceiling 2600)"]
    );
    // The printed report never names the split item.
    assert!(report.iter().all(|line| !line.contains("split-it")));
    // A family run applies the same exception.
    let family = Family {
        module: "src/big",
        children: None,
        split: false,
    };
    assert_eq!(
        family_report(root, &family, &exceptions).expect("the family at its ceiling"),
        report
    );

    // Growing by one line fails both runs, naming the ceiling.
    fs::write(root.join("src/big.rs"), "line\n".repeat(2_601)).expect("grown module");
    let grown = "src/big.rs: 2601 lines (known exception, ceiling 2600)";
    let refused = whole_tree_report(root, &exceptions).expect_err("growth past the ceiling");
    assert!(refused.contains(grown), "{refused}");
    let refused = family_report(root, &family, &exceptions).expect_err("growth in the family");
    assert!(refused.contains(grown), "{refused}");

    // Shrinking below the ceiling fails the whole tree until the ceiling
    // comes down to the new count, so the file can never grow back; a loose
    // ceiling is refused the same way. A family run only guards growth.
    fs::write(root.join("src/big.rs"), "line\n".repeat(2_599)).expect("shrunk module");
    let refused = whole_tree_report(root, &exceptions).expect_err("a shrunk exception");
    assert!(
        refused.contains(
            "src/big.rs: 2599 lines is under its ceiling 2600; lower its max_lines to 2599"
        ),
        "{refused}"
    );
    family_report(root, &family, &exceptions).expect("the family under its ceiling");
    let lowered = [exception("src/big.rs", 2_599)];
    assert_eq!(
        whole_tree_report(root, &lowered).expect("the lowered ceiling"),
        ["src/big.rs: 2599 physical lines (known exception, ceiling 2599)"]
    );
}

#[test]
fn a_link_in_the_tree_or_a_family_is_refused_rather_than_followed() {
    // Each case gets a fresh tree, under a folder name a shell would split at
    // `&` or expand at `%PATH%`, so the links are made without a shell.
    let tree = || {
        let directory = test_support::temp_home().expect("directory");
        let root = directory.path().join("a&b %PATH% ^c");
        fs::create_dir_all(root.join("src/inner")).expect("source directory");
        fs::write(root.join("src/lib.rs"), "mod inner;\n").expect("crate root");
        fs::create_dir_all(root.join("elsewhere")).expect("directory outside src");
        fs::write(root.join("elsewhere/foreign.rs"), "fn foreign() {}\n").expect("foreign");
        (directory, root)
    };
    let refuses_link = |refused: &str, name: &str| {
        assert!(refused.contains("is a link"), "{refused}");
        assert!(refused.contains(name), "{refused}");
        assert!(!refused.contains("foreign.rs"), "{refused}");
    };

    // A link back up the tree would recurse without end if followed.
    let (_directory, root) = tree();
    test_support::make_dir_link(&root.join("src"), &root.join("src/inner/loop"));
    let refused = whole_tree_report(&root, &[]).expect_err("a link back up the tree");
    refuses_link(&refused, "loop");

    // A link out of src would count foreign files under src's names.
    let (_directory, root) = tree();
    test_support::make_dir_link(&root.join("elsewhere"), &root.join("src/inner/out"));
    let refused = whole_tree_report(&root, &[]).expect_err("a link out of src");
    refuses_link(&refused, "out");

    // A family's child directory that is a link is refused by the family run
    // itself, not only by the whole tree.
    let (_directory, root) = tree();
    fs::write(root.join("src/fam.rs"), "mod child;\n").expect("family module");
    test_support::make_dir_link(&root.join("elsewhere"), &root.join("src/fam"));
    let family = Family {
        module: "src/fam",
        children: None,
        split: true,
    };
    let refused = family_report(&root, &family, &[]).expect_err("a linked child directory");
    refuses_link(&refused, "fam");
    let refused = whole_tree_report(&root, &[]).expect_err("the same link in the tree");
    refuses_link(&refused, "fam");

    // So is `src` itself.
    let (_directory, root) = tree();
    fs::rename(root.join("src"), root.join("real")).expect("move the sources aside");
    test_support::make_dir_link(&root.join("real"), &root.join("src"));
    let refused = whole_tree_report(&root, &[]).expect_err("a linked src");
    refuses_link(&refused, "src is a link");
}

#[test]
fn a_stale_exception_fails_the_whole_tree_but_not_another_family() {
    let directory = test_support::temp_home().expect("directory");
    let root = directory.path();
    fs::create_dir_all(root.join("src")).expect("source directory");
    fs::write(root.join("src/small.rs"), "line\n".repeat(10)).expect("small module");
    fs::write(root.join("src/other.rs"), "fn other() {}\n").expect("other module");
    let exceptions = [
        exception("src/gone.rs", 2_600),
        exception("src/small.rs", 2_600),
    ];

    let refused = whole_tree_report(root, &exceptions).expect_err("stale exceptions");
    assert!(
        refused.contains("src/gone.rs: names no source file"),
        "{refused}"
    );
    assert!(
        refused.contains("src/small.rs: 10 lines is within the limit 2499; remove its entry"),
        "{refused}"
    );

    // A family run checks only its own files against the limit, so another
    // family's exceptions never fail it.
    let family = Family {
        module: "src/other",
        children: None,
        split: false,
    };
    assert_eq!(
        family_report(root, &family, &exceptions).expect("an unrelated family"),
        ["src/other.rs: 1 physical lines (limit 2499)"]
    );
}

#[test]
fn malformed_or_duplicate_exceptions_are_refused() {
    assert!(parse_exceptions("[]").expect("an empty list").is_empty());
    let parsed = parse_exceptions(
        r#"[{"path": "src/a/b.rs", "max_lines": 2500, "split_item": "split-it"}]"#,
    )
    .expect("one exception");
    assert_eq!(
        (parsed[0].path.as_str(), parsed[0].max_lines),
        ("src/a/b.rs", 2_500)
    );

    let entry = |path: &str, max_lines: usize, split_item: &str| {
        format!(r#"{{"path": {path:?}, "max_lines": {max_lines}, "split_item": {split_item:?}}}"#)
    };
    let cases = [
        ("{}".to_owned(), "malformed"),
        (
            r#"[{"path": "src/a.rs", "max_lines": 2500}]"#.to_owned(),
            "malformed",
        ),
        (
            r#"[{"path": "src/a.rs", "max_lines": 2500, "split_item": "x", "note": "y"}]"#
                .to_owned(),
            "malformed",
        ),
        (
            r#"[{"path": "src/a.rs", "max_lines": -1, "split_item": "x"}]"#.to_owned(),
            "malformed",
        ),
        (
            format!("[{}]", entry("tests/a.rs", 2_500, "x")),
            "not a plain path",
        ),
        (
            format!("[{}]", entry("src/a.txt", 2_500, "x")),
            "not a plain path",
        ),
        (
            format!("[{}]", entry("src/a.RS", 2_500, "x")),
            "not a plain path",
        ),
        (
            format!("[{}]", entry("src/.rs", 2_500, "x")),
            "not a plain path",
        ),
        (
            format!("[{}]", entry("src\\a.rs", 2_500, "x")),
            "not a plain path",
        ),
        (
            format!("[{}]", entry("src/../a.rs", 2_500, "x")),
            "not a plain path",
        ),
        (
            format!("[{}]", entry("src//a.rs", 2_500, "x")),
            "not a plain path",
        ),
        (
            format!("[{}]", entry("src/./a.rs", 2_500, "x")),
            "not a plain path",
        ),
        (
            format!("[{}]", entry("src/a.rs", 2_499, "x")),
            "within the limit",
        ),
        (
            format!("[{}]", entry("src/a.rs", 2_500, " ")),
            "names no split item",
        ),
        (
            format!(
                "[{}, {}]",
                entry("src/a.rs", 2_500, "x"),
                entry("src/a.rs", 2_600, "y")
            ),
            "listed twice",
        ),
    ];
    for (text, expected) in cases {
        let refused = parse_exceptions(&text).expect_err(&text);
        assert!(refused.contains(expected), "{text}: {refused}");
    }
}
