use super::*;

pub(super) mod workload;

const MALFORMED_URLS: [&str; 14] = [
    "https://user=token@host/repo",
    "https://ho=st/repo",
    "https://host:4=43/repo",
    "https://host/re=po",
    "https://host/repo?q=x",
    "https://host/repo#x=y",
    "https://host/.helper=!echo;",
    "https://host/repo!",
    "https://host/repo;",
    "https://host/re po",
    "https://host/re\tpo",
    "https://host/re\rpo",
    "https://host/re\npo",
    "https://host/répo",
];

async fn malformed_url_cases(fixture: &Fixture, selection: &TargetSelection, urls: &[&str]) {
    let limits = RunLimits::default();
    let cancel = Cancellation::default();
    for url in urls {
        fixture.git(&["remote", "set-url", "origin", url]);
        let before = fixture.snapshot();
        let error = ExchangeTarget::capture(selection, limits, &cancel)
            .await
            .unwrap_err();
        assert_eq!(error, RunError::UnsupportedTarget);
        assert!(error.to_string().contains("ASCII unreserved path"));
        assert_eq!(fixture.snapshot(), before);
        workload::case_finished();
    }
}

async fn encoded_url_cases(fixture: &Fixture, selection: &TargetSelection) {
    let limits = RunLimits::default();
    let cancel = Cancellation::default();
    fixture.git(&[
        "remote",
        "set-url",
        "origin",
        "https://example.invalid/repo%3Dname.git",
    ]);
    let encoded = ExchangeTarget::capture(selection, limits, &cancel)
        .await
        .unwrap();
    assert_eq!(encoded.url, "https://example.invalid/repo%3Dname.git");
    assert_config(&encoded, "credential.helper", "manager");
    assert_config(&encoded, "http.sslVerify", "true");
    assert_config(&encoded, "http.followRedirects", "false");
    workload::case_finished();

    // Even a fabricated internal target cannot split a key into a helper value.
    // Public capture above refuses this spelling before constructing a target.
    let mut fabricated = encoded.clone();
    fabricated.url = "https://example.invalid/.helper=!sentinel;".into();
    let before_build = workload::counts();
    let built =
        super::super::super::command::network(&fabricated, &ExchangeOperation::Observe).unwrap();
    assert!(
        built
            .get_args()
            .take_while(|arg| *arg != "ls-remote")
            .all(|arg| !arg.to_string_lossy().contains("sentinel"))
    );
    assert!(
        built
            .get_envs()
            .filter(|(key, _)| key.to_string_lossy().starts_with("GIT_CONFIG_VALUE_"))
            .all(|(_, value)| !value.unwrap().to_string_lossy().contains("sentinel"))
    );
    assert_eq!(
        workload::counts(),
        before_build,
        "building launches nothing"
    );
    assert_config(&fabricated, "credential.helper", "manager");
    assert_config(&fabricated, "http.sslVerify", "true");
    assert_config(&fabricated, "http.followRedirects", "false");
    workload::case_finished();
}

async fn helper_order_and_drift_cases(fixture: &Fixture, selection: &TargetSelection) {
    let limits = RunLimits::default();
    let cancel = Cancellation::default();
    fixture.git(&["config", "--add", "credential.helper", "manager-core"]);
    fixture.git(&["config", "--add", "credential.helper", "manager"]);
    let pinned = ExchangeTarget::capture(selection, limits, &cancel)
        .await
        .unwrap();
    assert_eq!(pinned.helpers, ["manager", "manager-core", "manager"]);
    workload::case_finished();
    fixture.git(&["config", "--unset-all", "credential.helper"]);
    fixture.git(&["config", "--add", "credential.helper", "wincred"]);
    fixture.git(&["config", "--add", "credential.helper", "manager"]);
    assert_eq!(
        super::super::super::run(&pinned, &ExchangeOperation::Observe, limits, &cancel)
            .await
            .unwrap_err(),
        RunError::TargetChanged
    );
    workload::case_finished();
}

async fn helper_refusal_cases(fixture: &Fixture, selection: &TargetSelection) {
    let limits = RunLimits::default();
    let cancel = Cancellation::default();
    fixture.git(&[
        "config",
        "--add",
        "credential.helper",
        "!echo invoked > helper-invoked",
    ]);
    fixture.git(&["config", "--add", "credential.helper", "manager"]);
    assert_eq!(
        ExchangeTarget::capture(selection, limits, &cancel)
            .await
            .unwrap_err(),
        RunError::UnsupportedHelper
    );
    assert!(!fixture.0.join("helper-invoked").exists());
    workload::case_finished();
    fixture.git(&["config", "--unset-all", "credential.helper"]);
    let scoped = "credential.https://user:fake-token@example.invalid/.helper";
    fixture.git(&["config", scoped, "manager"]);
    let error = ExchangeTarget::capture(selection, limits, &cancel)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("credential.<redacted URL scope>.helper"));
    assert!(error.contains("file:.git/config"));
    assert!(error.contains("unscoped credential.helper"));
    assert!(!error.contains("fake-token"));
    workload::case_finished();
    fixture.git(&["config", "--unset", scoped]);
    fixture.git(&[
        "remote",
        "set-url",
        "origin",
        "https://example.invalid/repo.git",
    ]);
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
        // The watchdog and every product request retain their ordinary deadline;
        // deterministic launch ceilings are a separate test-workload assertion.
        let supervisor_before =
            super::super::super::test_support::LAUNCHES.with(std::cell::Cell::get);
        let result =
            super::super::super::execute(command, RunLimits::default(), &Cancellation::default())
                .await;
        assert!(
            result.is_ok(),
            "{} policy fixture failed at {}: {result:?}",
            group.name,
            fs::read_to_string(fixture.0.join("phase")).unwrap_or_default()
        );
        assert_eq!(
            super::super::super::test_support::LAUNCHES.with(std::cell::Cell::get),
            supervisor_before + 1,
            "one owned supervisor per group"
        );
        let output = String::from_utf8(result.unwrap().stdout).unwrap();
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
        "url-1-7" => malformed_url_cases(&fixture, &selection, &MALFORMED_URLS[..7]).await,
        "url-8-14" => malformed_url_cases(&fixture, &selection, &MALFORMED_URLS[7..]).await,
        "encoded-fabricated" => encoded_url_cases(&fixture, &selection).await,
        "helper-order-drift" => helper_order_and_drift_cases(&fixture, &selection).await,
        "helper-refusals" => helper_refusal_cases(&fixture, &selection).await,
        _ => panic!("unknown owned fixture group"),
    }
    println!("\n{}", workload::finish());
}

fn assert_config(target: &ExchangeTarget, key: &str, expected: &str) {
    let built = super::super::super::command::network(target, &ExchangeOperation::Observe).unwrap();
    let mut query = Command::new(built.get_program());
    query.current_dir(built.get_current_dir().unwrap());
    query.args(built.get_args().take_while(|arg| *arg != "ls-remote"));
    for (name, value) in built.get_envs() {
        if let Some(value) = value {
            query.env(name, value);
        } else {
            query.env_remove(name);
        }
    }
    query.args(["config", "--get-urlmatch", key, &target.url]);
    let output = workload::output(&mut query);
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), expected);
}

#[tokio::test]
async fn task_exchange_service_charges_admission_before_initial_capture() {
    let fixture = Fixture::new();
    let project = crate::ProjectId("exchange/budget".into());
    let database = crate::project_database_path(&fixture.0.join("home"), &project);
    let service = crate::LocalWorkService::new(
        database.clone(),
        project,
        "fixture".into(),
        crate::SessionId("fixture".into()),
        None,
    );
    service
        .task_sync_settings(
            &crate::task_sync::settings::Request {
                action: crate::task_sync::settings::Action::Enable,
                user_instruction: Some("Enable task sync for this owned test".into()),
            },
            chrono::Utc::now(),
        )
        .unwrap();
    let selection = TargetSelection {
        repository: fixture.0.clone(),
        remote: "origin".into(),
        reference: "refs/heads/main".into(),
    };
    let before = fixture.snapshot();
    let support = &super::super::super::test_support::LAUNCHES;
    let count = support.with(std::cell::Cell::get);
    super::super::super::test_support::SERVICE_STARTED.with(|value| {
        value.set(Some(
            Instant::now().checked_sub(Duration::from_secs(3)).unwrap(),
        ));
    });
    let error = service
        .run_task_sync_exchange(
            &selection,
            &ExchangeOperation::Observe,
            RunLimits {
                deadline: Duration::from_secs(2),
                ..RunLimits::default()
            },
            &Cancellation::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.downcast_ref::<RunError>(), Some(&RunError::Deadline));
    assert_eq!(support.with(std::cell::Cell::get), count);
    assert_eq!(fixture.snapshot(), before);
    assert!(!database.exists());
}
