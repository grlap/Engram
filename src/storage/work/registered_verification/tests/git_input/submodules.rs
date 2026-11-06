//! Real Git submodule eligibility at the existing evaluate/done boundary.

use super::live_currency::{commit, git, live_fixture};
use super::*;
use std::path::{Path, PathBuf};

fn add_child(f: &mut Fixture, root: &Path) -> PathBuf {
    let source = f.source.as_ref().unwrap().path().join("child-source");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-q"]);
    std::fs::write(source.join("tracked"), "child checked\n").unwrap();
    git(&source, &["add", "tracked"]);
    commit(&source);
    git(
        root,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--",
            source.to_str().unwrap(),
            "child",
        ],
    );
    let revision = commit(root);
    f.request.input = RegisteredVerificationInput::Commit { commit: revision };
    root.join("child")
}

#[test]
fn submodule_git_currency_clean_pinned_child_evaluates_and_completes() {
    for evaluated in [false, true] {
        let (root, mut f) = live_fixture(true, evaluated);
        add_child(&mut f, &root);
        // Ordinary ignored generated output stays outside the dirt verdict.
        std::fs::write(root.join("child/.gitignore"), "generated/\n").unwrap();
        git(&root.join("child"), &["add", ".gitignore"]);
        commit(&root.join("child"));
        git(&root, &["add", "child"]);
        f.request.input = RegisteredVerificationInput::Commit {
            commit: commit(&root),
        };
        std::fs::create_dir(root.join("child/generated")).unwrap();
        std::fs::write(root.join("child/generated/output"), "ignored\n").unwrap();
        let check = f.record().unwrap().verifications[0].record_id.clone();
        let revision = f.request.input.revision();
        if evaluated {
            let input = evaluation(&f, &check, &revision);
            record_evaluation(&mut f, &input).unwrap();
        }
        let seal = complete(&mut f, &check, &revision, "submodule-clean-done").unwrap();
        assert!(seal.acceptance[0].satisfied);
    }
}

#[test]
fn submodule_git_currency_refusals_name_child_at_evaluate_and_done() {
    for (scenario, reason) in [
        ("dirty", "working tree is dirty"),
        ("hidden-untracked", "working tree is dirty"),
        ("off-pin", "HEAD differs from its pin"),
        ("uninitialized", "uninitialized"),
        ("filter", "configured Git filters"),
        ("missing-mapping", "no submodule mapping found"),
    ] {
        let (root, mut f) = live_fixture(true, true);
        let child = add_child(&mut f, &root);
        let check = f.record().unwrap().verifications[0].record_id.clone();
        let revision = f.request.input.revision();
        let mut input = evaluation(&f, &check, &revision);
        record_evaluation(&mut f, &input).unwrap();
        match scenario {
            "dirty" => std::fs::write(child.join("tracked"), "dirty\n").unwrap(),
            "hidden-untracked" => {
                git(&child, &["config", "status.showUntrackedFiles", "no"]);
                std::fs::write(child.join("untracked"), "visible despite local setting\n").unwrap();
            }
            "off-pin" => {
                commit(&child);
            }
            "uninitialized" => {
                git(&root, &["submodule", "deinit", "-f", "--", "child"]);
            }
            "filter" => {
                git(&child, &["config", "filter.never.clean", "must-not-run"]);
            }
            "missing-mapping" => {
                git(&root, &["rm", ".gitmodules"]);
                commit(&root);
            }
            _ => unreachable!(),
        }
        input.attempt_key = Some(format!("submodule-{scenario}-evaluation"));
        let error = record_evaluation(&mut f, &input).unwrap_err();
        assert!(error.to_string().contains("child"), "{scenario}: {error}");
        assert!(error.to_string().contains(reason), "{scenario}: {error}");
        let error = complete(
            &mut f,
            &check,
            &revision,
            &format!("submodule-{scenario}-done"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("child"), "{scenario}: {error}");
        assert!(error.to_string().contains(reason), "{scenario}: {error}");
    }
}

#[test]
fn submodule_git_currency_nested_clean_evaluates_and_completes() {
    for evaluated in [false, true] {
        let (root, mut f) = live_fixture(true, evaluated);
        let child = add_child(&mut f, &root);
        let source = f.source.as_ref().unwrap().path().join("child-source");
        git(
            &child,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "--",
                source.to_str().unwrap(),
                "nested",
            ],
        );
        commit(&child);
        git(&root, &["add", "child"]);
        f.request.input = RegisteredVerificationInput::Commit {
            commit: commit(&root),
        };
        let check = f.record().unwrap().verifications[0].record_id.clone();
        let revision = f.request.input.revision();
        if evaluated {
            let input = evaluation(&f, &check, &revision);
            record_evaluation(&mut f, &input).unwrap();
        }
        let seal = complete(&mut f, &check, &revision, "nested-clean-done").unwrap();
        assert!(seal.acceptance[0].satisfied);
    }
}
