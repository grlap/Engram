//! `work ls --json --page-bytes` lets a program fit a larger page than the
//! agent budget, with the same rows, order and continuation.

#[path = "../src/test_support.rs"]
mod test_support;

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

use serde_json::Value;

/// One more full page than `--limit 200` holds, so the opt-in shows a literal
/// 200-row page followed by a continuation.
const ITEMS: usize = 205;
const PAGE: usize = 200;
/// Enough rows for the agent budget to cut a page short.
const SMALL: usize = 45;
const LARGEST: &str = "1048576";

struct Fixture {
    _home: test_support::TempHome,
    home: PathBuf,
    marker: PathBuf,
}

impl Fixture {
    fn new(items: usize) -> Self {
        let home = test_support::temp_home().unwrap();
        let root = home.path().to_path_buf();
        let marker = root.join(".engram-project");
        fs::write(&marker, "work-ls-page-bytes\n").unwrap();
        let fixture = Self {
            home: root.join("store"),
            _home: home,
            marker,
        };
        let output = fixture.run(&["init"]);
        assert!(output.status.success(), "{}", stderr(&output));
        for index in 0..items {
            let title = format!(
                "page budget item {index:03} {}",
                "with a long title ".repeat(8)
            );
            let output = fixture.work(&["add", &title, "--accept", "the row is listed once"]);
            assert!(output.status.success(), "{}", stderr(&output));
        }
        fixture
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_engram"))
            .arg("--home")
            .arg(&self.home)
            .arg("--project-file")
            .arg(&self.marker)
            .args(args)
            .output()
            .unwrap()
    }

    fn work(&self, args: &[&str]) -> Output {
        let mut full = vec![
            "work",
            "--actor-id",
            "page-reader",
            "--session-id",
            "page-reader-session",
        ];
        full.extend_from_slice(args);
        self.run(&full)
    }

    fn page(&self, args: &[&str]) -> Value {
        let output = self.work(args);
        assert!(output.status.success(), "{}", stderr(&output));
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Row refs: verbose rows carry `work.short_ref`, compact rows `ref`.
fn refs(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            row["work"]["short_ref"]
                .as_str()
                .or_else(|| row["ref"].as_str())
                .unwrap_or_else(|| panic!("a row without a ref: {row}"))
                .to_owned()
        })
        .collect()
}

fn with<'a>(base: &[&'a str], extra: &[&'a str]) -> Vec<&'a str> {
    [base, extra].concat()
}

#[test]
fn work_ls_page_bytes_fits_a_full_json_page_and_keeps_continuation() {
    let fixture = Fixture::new(ITEMS);
    let listing = ["ls", "--all", "--verbose", "--json", "--limit", "200"];

    // The agent budget cuts the page well short of the limit.
    let default = fixture.page(&listing);
    let first = refs(&default);
    assert!(first.len() < SMALL, "{} rows fit 12 KiB", first.len());
    assert_eq!(default["more"], true);
    assert!(default["byte_budget"].as_u64().unwrap() <= 12_288);
    let after = default["after"].as_str().unwrap().to_owned();

    // The larger budget fills the whole --limit 200 page, in the same order,
    // within the requested bytes including the final newline.
    let large = with(&listing, &["--page-bytes", LARGEST]);
    let output = fixture.work(&large);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(output.stdout.len() <= 1_048_576);
    let full: Value = serde_json::from_slice(&output.stdout).unwrap();
    let head = refs(&full);
    assert_eq!(head.len(), PAGE);
    assert_eq!(full["total"], ITEMS);
    assert_eq!(full["shown_before"], 0);
    assert_eq!(full["omitted"], ITEMS - PAGE);
    assert_eq!(full["more"], true);
    assert!(full["byte_budget"].as_u64().unwrap() > 12_288);
    assert_eq!(head[..first.len()], first[..]);

    // Its continuation, anchored on row 200, returns the last five rows.
    let tail = fixture.page(&with(&large, &["--after", full["after"].as_str().unwrap()]));
    assert_eq!(tail["shown_before"], PAGE);
    assert_eq!(tail["more"], false);
    assert!(tail.get("after").is_none());
    let all = [head.clone(), refs(&tail)].concat();
    assert_eq!(all.len(), ITEMS);
    let mut sorted = all.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), ITEMS, "every row is listed exactly once");

    // A cursor minted under the agent budget continues under the larger one.
    let rest = fixture.page(&with(&large, &["--after", &after]));
    assert_eq!(rest["shown_before"], first.len());
    assert_eq!(rest["more"], false);
    assert_eq!(
        [first.clone(), refs(&rest)].concat(),
        all,
        "the continuation resumes after the last emitted row"
    );

    // A cursor minted under a larger budget continues under the agent budget.
    let part = fixture.page(&with(&listing, &["--page-bytes", "24576"]));
    let shown = refs(&part);
    assert!(shown.len() > first.len() && shown.len() < PAGE);
    let next = fixture.page(&with(
        &listing,
        &["--after", part["after"].as_str().unwrap()],
    ));
    assert_eq!(next["shown_before"], shown.len());
    let resumed = refs(&next);
    assert_eq!(resumed[..], all[shown.len()..shown.len() + resumed.len()]);

    // The global --json may also precede the word.
    let mut leading = vec!["--json"];
    leading.extend(large.iter().copied().filter(|arg| *arg != "--json"));
    assert_eq!(refs(&fixture.page(&leading)), head);

    // Without the option the default page is unchanged.
    let again = fixture.page(&listing);
    assert_eq!(refs(&again), first);
    assert_same_page(&again, &default);
}

/// Two reads of the same page: the `after` token also carries each read's
/// observation time, so pages compare by rows and counts.
fn assert_same_page(left: &Value, right: &Value) {
    assert_eq!(refs(left), refs(right));
    for field in ["byte_budget", "total", "omitted", "more", "shown_before"] {
        assert_eq!(left[field], right[field], "{field}");
    }
    assert_eq!(left.get("after").is_some(), right.get("after").is_some());
}

#[test]
fn work_ls_page_bytes_is_json_only_and_bounded() {
    let fixture = Fixture::new(SMALL);

    let text = fixture.work(&["ls", "--page-bytes", LARGEST]);
    assert!(!text.status.success());
    assert!(
        stderr(&text).contains("--page-bytes applies only to JSON output"),
        "{}",
        stderr(&text)
    );

    for out_of_range in ["12287", "1048577", "0"] {
        let output = fixture.work(&["ls", "--json", "--page-bytes", out_of_range]);
        assert!(!output.status.success(), "{out_of_range} was accepted");
        assert!(
            stderr(&output).contains("12288..=1048576"),
            "{out_of_range}: {}",
            stderr(&output)
        );
    }

    // The smallest budget is the agent budget.
    let smallest = fixture.page(&["ls", "--json", "--page-bytes", "12288"]);
    let default = fixture.page(&["ls", "--json"]);
    assert_same_page(&smallest, &default);
}
