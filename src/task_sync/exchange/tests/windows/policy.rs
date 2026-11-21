use super::*;
use crate::task_sync::exchange::tests::policy_cases::{self, Host, MALFORMED_URLS};

pub(super) mod workload;

/// The shared cases count every direct launch and case against the group's
/// fixed launch budget.
impl Host for Fixture {
    fn root(&self) -> &Path {
        &self.0
    }

    fn git(&self, args: &[&str]) {
        Fixture::git(self, args);
    }

    fn output(&self, command: &mut Command) -> std::process::Output {
        workload::output(command)
    }

    fn case_finished(&self) {
        workload::case_finished();
    }

    fn launches(&self) -> (usize, usize) {
        workload::counts()
    }
}

#[tokio::test]
async fn task_exchange_windows_policy_groups_have_bounded_launch_workloads() {
    for group in workload::GROUPS {
        let fixture = Fixture::new();
        fs::write(
            fixture.0.join("global.config"),
            b"[credential]\nhelper = manager\n",
        )
        .unwrap();
        let mut command = fixture.isolated_config_command_for(
            "task_sync::exchange::tests::windows::policy::policy_configuration_fixture",
        );
        command.env("ENGRAM_EXCHANGE_POLICY_GROUP", group.name);
        // Every product request retains its ordinary deadline; deterministic
        // launch ceilings bound the workload, so the supervisor is a hang guard.
        let supervisor_before =
            super::super::super::test_support::LAUNCHES.with(std::cell::Cell::get);
        let result =
            diagnostics::supervise(command, supervisor_limits(), &Cancellation::default()).await;
        if let Err(report) = &result {
            panic!(
                "{} policy fixture failed at {}:\n{report}",
                group.name,
                fs::read_to_string(fixture.0.join("phase")).unwrap_or_default()
            );
        }
        assert_eq!(
            super::super::super::test_support::LAUNCHES.with(std::cell::Cell::get),
            supervisor_before + 1,
            "one owned supervisor per group"
        );
        let output = String::from_utf8(result.unwrap().stdout).unwrap();
        group.assert_trace(output.as_bytes());
        let reports: Vec<_> = output
            .lines()
            .filter(|line| line.starts_with("policy-workload "))
            .collect();
        assert_eq!(reports.len(), 1, "{output}");
        let expected = format!("{} elapsed_ms=", group.report());
        let _: u128 = reports[0]
            .strip_prefix(&expected)
            .and_then(|value| value.parse().ok())
            .expect("one exact workload report with measured elapsed milliseconds");
        eprintln!("{} supervisor=1", reports[0]);
    }
}

#[tokio::test]
#[ignore = "isolated policy subprocess entry; exercised by bounded parent test"]
async fn policy_configuration_fixture() {
    let name = std::env::var("ENGRAM_EXCHANGE_POLICY_GROUP").unwrap();
    let group = workload::GROUPS
        .into_iter()
        .find(|group| group.name == name)
        .expect("unknown owned fixture group");
    workload::begin(group);
    let _trace = diagnostics::begin();
    let fixture = Fixture(PathBuf::from(
        std::env::var_os("ENGRAM_EXCHANGE_FIXTURE_ROOT").unwrap(),
    ));
    fixture.phase(group.name);
    fixture.git(&["init", "-q"]);
    fixture.git(&[
        "remote",
        "add",
        "origin",
        "https://example.invalid/repo.git",
    ]);
    let selection = TargetSelection {
        repository: fixture.0.clone(),
        remote: "origin".into(),
        reference: "refs/heads/main".into(),
    };
    match group.name {
        "url-1-7" => {
            policy_cases::malformed_url_cases(&fixture, &selection, &MALFORMED_URLS[..7]).await;
        }
        "url-8-14" => {
            policy_cases::malformed_url_cases(&fixture, &selection, &MALFORMED_URLS[7..]).await;
        }
        "encoded-fabricated" => policy_cases::encoded_url_cases(&fixture, &selection).await,
        "helper-order-drift" => {
            policy_cases::helper_order_and_drift_cases(&fixture, &selection).await;
        }
        "helper-refusals" => policy_cases::helper_refusal_cases(&fixture, &selection).await,
        _ => panic!("unknown owned fixture group"),
    }
    println!("\n{}", workload::finish());
}
