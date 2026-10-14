use super::*;

async fn url_policy_cases(fixture: &Fixture, selection: &TargetSelection) {
    let limits = RunLimits::default();
    let cancel = Cancellation::default();
    fixture.phase("URL policy");
    for url in [
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
    ] {
        fixture.git(&["remote", "set-url", "origin", url]);
        let before = fixture.snapshot();
        let error = ExchangeTarget::capture(selection, limits, &cancel)
            .await
            .unwrap_err();
        assert_eq!(error, RunError::UnsupportedTarget);
        assert!(error.to_string().contains("ASCII unreserved path"));
        assert_eq!(fixture.snapshot(), before);
    }
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

    // Even a fabricated internal target cannot split a key into a helper value.
    // Public capture above refuses this spelling before constructing a target.
    let mut fabricated = encoded.clone();
    fabricated.url = "https://example.invalid/.helper=!sentinel;".into();
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
    assert_config(&fabricated, "credential.helper", "manager");
    assert_config(&fabricated, "http.sslVerify", "true");
    assert_config(&fabricated, "http.followRedirects", "false");
}

async fn helper_policy_cases(fixture: &Fixture, selection: &TargetSelection) {
    let limits = RunLimits::default();
    let cancel = Cancellation::default();
    fixture.phase("helper policy");
    fixture.git(&["config", "--add", "credential.helper", "manager-core"]);
    fixture.git(&["config", "--add", "credential.helper", "manager"]);
    let pinned = ExchangeTarget::capture(selection, limits, &cancel)
        .await
        .unwrap();
    assert_eq!(pinned.helpers, ["manager", "manager-core", "manager"]);
    fixture.git(&["config", "--unset-all", "credential.helper"]);
    fixture.git(&["config", "--add", "credential.helper", "wincred"]);
    fixture.git(&["config", "--add", "credential.helper", "manager"]);
    assert_eq!(
        super::super::super::run(&pinned, &ExchangeOperation::Observe, limits, &cancel)
            .await
            .unwrap_err(),
        RunError::TargetChanged
    );
    fixture.git(&["config", "--unset-all", "credential.helper"]);
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
    fixture.git(&["config", "--unset", scoped]);
    fixture.git(&[
        "remote",
        "set-url",
        "origin",
        "https://example.invalid/repo.git",
    ]);
}

#[tokio::test]
async fn task_exchange_windows_url_and_helper_policies_have_separate_process_budgets() {
    for group in ["url", "helper"] {
        let fixture = Fixture::new();
        fs::write(
            fixture.0.join("global.config"),
            b"[credential]\nhelper = manager\n",
        )
        .unwrap();
        let mut command = fixture.isolated_config_command_for(
            "task_sync::exchange::tests::windows::policy::policy_configuration_fixture",
        );
        command.env("ENGRAM_EXCHANGE_POLICY_GROUP", group);
        let result =
            super::super::super::execute(command, RunLimits::default(), &Cancellation::default())
                .await;
        assert!(
            result.is_ok(),
            "{group} policy fixture failed at {}: {result:?}",
            fs::read_to_string(fixture.0.join("phase")).unwrap_or_default()
        );
    }
}

#[tokio::test]
#[ignore = "isolated policy subprocess entry; exercised by bounded parent test"]
async fn policy_configuration_fixture() {
    let fixture = Fixture(PathBuf::from(
        std::env::var_os("ENGRAM_EXCHANGE_FIXTURE_ROOT").unwrap(),
    ));
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
    match std::env::var("ENGRAM_EXCHANGE_POLICY_GROUP")
        .unwrap()
        .as_str()
    {
        "url" => url_policy_cases(&fixture, &selection).await,
        "helper" => helper_policy_cases(&fixture, &selection).await,
        _ => panic!("unknown owned fixture group"),
    }
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
    let output = query.output().unwrap();
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
