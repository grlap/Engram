//! Actual Git transport against a private bare remote. Only the process/HTTPS
//! adapter is replaced; canonical progress, retention and driver decisions run
//! through their ordinary implementation. No installed store or remote is used.

use super::*;
use std::{path::PathBuf, process::Output};

const BRANCH: &str = "refs/heads/engram-data";

struct GitFixture {
    left: PathBuf,
    right: PathBuf,
    config: PathBuf,
    race: bool,
    unknown: bool,
    pushes: usize,
}

fn git(root: &Path, config: &Path, args: &[&str]) -> Output {
    super::super::super::command::local(root, &[])
        .env("GIT_CONFIG_GLOBAL", config)
        .env("GIT_CONFIG_SYSTEM", config)
        .env("GIT_ALLOW_PROTOCOL", "file")
        .env("LC_ALL", "C")
        .args([
            "-c",
            "protocol.file.allow=always",
            "-c",
            "user.name=Publication fixture",
            "-c",
            "user.email=publication@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap()
}

fn ok(root: &Path, config: &Path, args: &[&str]) -> String {
    let output = git(root, config, args);
    assert!(
        output.status.success(),
        "fixture Git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

impl GitFixture {
    fn advance_right(&self) {
        ok(&self.right, &self.config, &["fetch", "origin", BRANCH]);
        ok(
            &self.right,
            &self.config,
            &["reset", "--hard", "FETCH_HEAD"],
        );
        ok(
            &self.right,
            &self.config,
            &[
                "commit",
                "--allow-empty",
                "-m",
                "Advance concurrent data history",
            ],
        );
        ok(
            &self.right,
            &self.config,
            &["push", "--porcelain", "origin", BRANCH],
        );
    }

    fn retain(&self, input: &mut InspectedExchange, commit: &str) {
        let output = git(&self.left, &self.config, &["cat-file", "commit", commit]);
        assert!(output.status.success());
        let observed = crate::task_sync::git::observe_commit(commit, &output.stdout).unwrap();
        input.intent.commit.oid = observed.oid;
        input.intent.commit.tree = observed.tree;
        input.intent.commit.parents = observed.parents;
        input.intent.commit.raw_bytes_fingerprint = observed.raw_bytes_fingerprint;
        save_commit(input);
        ok(
            &self.left,
            &self.config,
            &["update-ref", &input.intent.commit.retention_ref, commit],
        );
    }
}

impl AttemptIo for GitFixture {
    fn revalidate(&mut self, input: &InspectedExchange) -> impl Future<Output = Result<()>> {
        ready_on_poll(move || {
            input.recheck_files()?;
            ensure!(
                ok(&self.left, &self.config, &["symbolic-ref", "HEAD"]) == BRANCH,
                "fixture data branch changed"
            );
            ensure!(
                ok(&self.left, &self.config, &["rev-parse", "HEAD"]) == input.intent.commit.oid,
                "fixture selected head changed"
            );
            ensure!(
                ok(
                    &self.left,
                    &self.config,
                    &["rev-parse", &input.intent.commit.retention_ref]
                ) == input.intent.commit.oid,
                "fixture retained pin changed"
            );
            Ok(())
        })
    }

    fn tip(&mut self) -> impl Future<Output = Result<Option<String>>> {
        ready_on_poll(move || {
            let output = git(
                &self.left,
                &self.config,
                &["ls-remote", "--refs", "origin", BRANCH],
            );
            ensure!(output.status.success(), "fixture remote observation failed");
            selected_tip(&output.stdout, BRANCH)
        })
    }

    fn fetch(&mut self, tip: &str) -> impl Future<Output = Result<String>> {
        ready_on_poll(move || {
            let reference = format!("refs/engram-exchange/fixture/publication/{tip}");
            ok(
                &self.left,
                &self.config,
                &[
                    "fetch",
                    "--no-tags",
                    "--no-write-fetch-head",
                    "origin",
                    &format!("{tip}:{reference}"),
                ],
            );
            ensure!(
                ok(&self.left, &self.config, &["rev-parse", &reference]) == tip,
                "fixture fetched pin differs"
            );
            Ok(reference)
        })
    }

    fn ancestor(&mut self, tip: &str, commit: &str) -> impl Future<Output = Result<bool>> {
        ready_on_poll(move || {
            Ok(ok(
                &self.left,
                &self.config,
                &["rev-list", "--max-count=1", tip, "--not", commit, "--"],
            )
            .is_empty())
        })
    }

    fn push(&mut self, commit: &str) -> impl Future<Output = std::result::Result<(), RunError>> {
        ready_on_poll(move || {
            self.pushes += 1;
            if std::mem::take(&mut self.unknown) {
                return Err(RunError::Process);
            }
            if std::mem::take(&mut self.race) {
                self.advance_right();
            }
            let output = git(
                &self.left,
                &self.config,
                &[
                    "push",
                    "--porcelain",
                    "origin",
                    &format!("{commit}:{BRANCH}"),
                ],
            );
            if output.status.success() {
                return Ok(());
            }
            Err(super::super::super::push::classify_failure(
                RunError::Git,
                output.status.code(),
                &output.stdout,
                commit,
                BRANCH,
            ))
        })
    }
}

#[tokio::test]
async fn two_clones_reconcile_both_refusals_once_and_preserve_exhausted_or_unknown_outcomes() {
    for push_race in [false, true] {
        for finish in ["confirmed", "ancestry_stop", "push_stop", "unknown"] {
            let home = crate::test_support::temp_home().unwrap();
            let mut binding = binding(home.path());
            let config = home.path().join("empty-config");
            fs::write(&config, b"").unwrap();
            let bare = home.path().join("data.git");
            let right = home.path().join("right");
            fs::create_dir_all(&binding.repository.main_checkout).unwrap();
            ok(
                home.path(),
                &config,
                &["init", "--bare", bare.to_str().unwrap()],
            );
            ok(
                &binding.repository.main_checkout,
                &config,
                &["init", "--initial-branch=engram-data"],
            );
            binding.repository.git_directory = PathBuf::from(ok(
                &binding.repository.main_checkout,
                &config,
                &["rev-parse", "--absolute-git-dir"],
            ));
            let mut store = SqliteStore::open(&binding.store.database).unwrap();
            let id = store.activate_task_sync_binding(&binding).unwrap();
            let mut original = input(&store, &id, &binding);
            // A valid shared TaskTree is carried unchanged while concurrent
            // empty data commits exercise ancestry and publication races.
            let folder = binding.repository.main_checkout.join(".engram");
            fs::create_dir(&folder).unwrap();
            fs::write(
                folder.join("project.json"),
                original.tree.documents().unwrap()["project.json"].clone(),
            )
            .unwrap();
            ok(
                &binding.repository.main_checkout,
                &config,
                &["add", ".engram/project.json"],
            );
            ok(
                &binding.repository.main_checkout,
                &config,
                &["commit", "-m", "Seed shared data"],
            );
            let seed = ok(
                &binding.repository.main_checkout,
                &config,
                &["rev-parse", "HEAD"],
            );
            ok(
                &binding.repository.main_checkout,
                &config,
                &["remote", "add", "origin", bare.to_str().unwrap()],
            );
            ok(
                &binding.repository.main_checkout,
                &config,
                &["push", "origin", BRANCH],
            );
            ok(
                home.path(),
                &config,
                &[
                    "clone",
                    "--branch",
                    "engram-data",
                    bare.to_str().unwrap(),
                    right.to_str().unwrap(),
                ],
            );
            let mut io = GitFixture {
                left: binding.repository.main_checkout.clone(),
                right,
                config,
                race: push_race,
                unknown: false,
                pushes: 0,
            };
            ok(
                &io.left,
                &io.config,
                &["commit", "--allow-empty", "-m", "Retain local data history"],
            );
            let local = ok(&io.left, &io.config, &["rev-parse", "HEAD"]);
            original.intent.expected_head = Some(seed);
            io.retain(&mut original, &local);
            let prepared = prepare(&mut store, &original);
            let request = CompletedExchangeRequest {
                binding: id.clone(),
                intent: original.intent.clone(),
            };
            if !push_race {
                io.advance_right();
            }
            let PublicationAttempt::ReconciliationRequired { failure, .. } =
                publish_with(&mut store, &original, &request, &mut io)
                    .await
                    .unwrap()
            else {
                panic!("actual data divergence must request reconciliation");
            };
            assert_eq!(
                matches!(failure, OutgoingFailure::NonFastForward { .. }),
                push_race
            );
            assert_eq!(io.pushes, usize::from(push_race));
            let tip = failure.fetched_tip().to_owned();
            let mut resolved = input(&store, &id, &binding);
            resolved.intent.expected_head = Some(local.clone());
            let selected = ok(
                &io.left,
                &io.config,
                &[
                    "commit-tree",
                    &original.intent.commit.tree,
                    "-p",
                    &local,
                    "-p",
                    &tip,
                    "-m",
                    "Retain agent-resolved data parents",
                ],
            );
            io.retain(&mut resolved, &selected);
            ok(
                &io.left,
                &io.config,
                &["update-ref", BRANCH, &selected, &local],
            );
            store
                .record_task_sync_exchange_progress(
                    &resolved,
                    OutgoingProgress::Reconciled {
                        prepared,
                        reconciliation: Box::new(OutgoingReconciliation::Resolved {
                            failure,
                            resolved: Box::new(resolved.intent.clone()),
                        }),
                    },
                    chrono::Utc::now(),
                )
                .unwrap();
            match finish {
                "ancestry_stop" => io.advance_right(),
                "push_stop" => io.race = true,
                "unknown" => io.unknown = true,
                _ => {}
            }
            let result = publish_with(&mut store, &resolved, &request, &mut io)
                .await
                .unwrap();
            assert_eq!(
                matches!(result, PublicationAttempt::Confirmed { .. }),
                finish == "confirmed"
            );
            assert_eq!(
                matches!(result, PublicationAttempt::Stopped { .. }),
                finish.ends_with("stop")
            );
            assert_eq!(
                matches!(result, PublicationAttempt::Unknown { .. }),
                finish == "unknown"
            );
            assert_eq!(
                io.pushes,
                usize::from(push_race) + usize::from(finish != "ancestry_stop")
            );
            let pushes = io.pushes;
            drop(store);
            let mut store = SqliteStore::open(&binding.store.database).unwrap();
            assert_eq!(
                publish_with(&mut store, &resolved, &request, &mut io)
                    .await
                    .unwrap(),
                result
            );
            assert_eq!(io.pushes, pushes);
            assert_eq!(
                store
                    .completed_task_sync_exchange(&request)
                    .unwrap()
                    .is_some(),
                finish == "confirmed"
            );
            if finish == "confirmed" {
                assert_eq!(io.tip().await.unwrap().as_deref(), Some(selected.as_str()));
            }
            assert_eq!(
                ok(
                    &io.left,
                    &io.config,
                    &["rev-list", "--parents", "-n", "1", &selected]
                ),
                format!("{selected} {local} {tip}")
            );
            assert!(store.verify_all().unwrap().is_healthy());
        }
    }
}
