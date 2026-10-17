use super::*;
use std::cell::Cell;

std::thread_local! {
    static DIRECT_LAUNCHES: Cell<usize> = const { Cell::new(0) };
}

pub(super) fn record_direct_launch() {
    DIRECT_LAUNCHES.with(|count| count.set(count.get() + 1));
}

fn launches() -> (usize, usize) {
    (
        DIRECT_LAUNCHES.with(Cell::get),
        super::super::super::test_support::LAUNCHES.with(Cell::get),
    )
}

#[derive(Clone, Copy)]
struct Group {
    name: &'static str,
    cases: usize,
    direct: usize,
    product: usize,
}

// Each owned child retains the ordinary watchdog. Bound sequential work instead
// of raising request deadlines when another case needs more Git launches.
const GROUPS: [Group; 2] = [
    Group {
        name: "capture-helper-drift",
        cases: 3,
        direct: 4,
        product: 21,
    },
    Group {
        name: "ssh-config-rewrite",
        cases: 4,
        direct: 9,
        product: 18,
    },
];

impl Group {
    fn assert_bounded(self) {
        assert!(
            self.direct + self.product <= 30,
            "group={} exceeds the 30-launch ceiling; split its cases",
            self.name,
        );
    }

    fn report(self, direct: usize, product: usize, cases: usize) -> String {
        format!(
            "configuration-workload group={} cases={}/{} direct={}/{} product={}/{} used={} cap=30",
            self.name,
            cases,
            self.cases,
            direct,
            self.direct,
            product,
            self.product,
            direct + product,
        )
    }
}

struct Budget {
    group: Group,
    before: (usize, usize),
    cases: usize,
    started: Instant,
}

impl Budget {
    fn new(group: Group) -> Self {
        group.assert_bounded();
        Self {
            group,
            before: launches(),
            cases: 0,
            started: Instant::now(),
        }
    }

    fn case_finished(&mut self) {
        self.cases += 1;
        let (direct, product) = self.counts();
        assert!(
            self.cases <= self.group.cases
                && direct <= self.group.direct
                && product <= self.group.product
                && direct + product <= 30,
            "{}; add a group instead of increasing deadlines",
            self.group.report(direct, product, self.cases),
        );
    }

    fn counts(&self) -> (usize, usize) {
        let (direct, product) = launches();
        (direct - self.before.0, product - self.before.1)
    }

    fn finish(self) -> String {
        let (direct, product) = self.counts();
        assert_eq!(
            (direct, product, self.cases),
            (self.group.direct, self.group.product, self.group.cases),
            "every declared configuration case and launch must execute",
        );
        format!(
            "{} elapsed_ms={}",
            self.group.report(direct, product, self.cases),
            self.started.elapsed().as_millis(),
        )
    }
}

#[test]
fn task_exchange_windows_config_groups_fit_supervisor_launch_budget() {
    for group in GROUPS {
        group.assert_bounded();
    }
}

pub(super) async fn run_groups() {
    for group in GROUPS {
        group.assert_bounded();
        let fixture = Fixture::new();
        fs::write(
            fixture.0.join("global.config"),
            b"[credential]\nhelper = manager\n",
        )
        .unwrap();
        // Configuration belongs only to this child, never the runner's globals.
        let mut command = fixture.isolated_config_command_for(
            "task_sync::exchange::tests::windows::configuration::configuration_fixture",
        );
        command.env("ENGRAM_EXCHANGE_CONFIGURATION_GROUP", group.name);
        let supervisor_before = launches().1;
        let started = Instant::now();
        let result =
            diagnostics::supervise(command, RunLimits::default(), &Cancellation::default()).await;
        let supervisor_ms = started.elapsed().as_millis();
        if let Err(report) = &result {
            panic!(
                "{} isolated configuration fixture failed at {}:\n{report}",
                group.name,
                fs::read_to_string(fixture.0.join("phase")).unwrap_or_default()
            );
        }
        assert_eq!(
            launches().1,
            supervisor_before + 1,
            "one supervisor per group"
        );
        let output = String::from_utf8(result.unwrap().stdout).unwrap();
        let reports: Vec<_> = output
            .lines()
            .filter(|line| line.starts_with("configuration-workload "))
            .collect();
        assert_eq!(reports.len(), 1, "{output}");
        let expected = format!(
            "{} elapsed_ms=",
            group.report(group.direct, group.product, group.cases)
        );
        let _: u128 = reports[0]
            .strip_prefix(&expected)
            .and_then(|value| value.parse().ok())
            .expect("one exact workload report with diagnostic elapsed milliseconds");
        eprintln!("{} supervisor_ms={supervisor_ms} supervisors=1", reports[0]);
    }
}

#[tokio::test]
#[ignore = "isolated subprocess entry; exercised by configuration test"]
async fn configuration_fixture() {
    let name = std::env::var("ENGRAM_EXCHANGE_CONFIGURATION_GROUP").unwrap();
    let group = GROUPS
        .into_iter()
        .find(|group| group.name == name)
        .expect("unknown owned configuration group");
    let mut budget = Budget::new(group);
    let _trace = diagnostics::begin();
    let fixture = Fixture(PathBuf::from(
        std::env::var_os("ENGRAM_EXCHANGE_FIXTURE_ROOT").unwrap(),
    ));
    fixture.phase("initialize");
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
        "capture-helper-drift" => capture_helper_drift(&fixture, &selection, &mut budget).await,
        "ssh-config-rewrite" => ssh_config_rewrite(&fixture, &selection, &mut budget).await,
        _ => panic!("unknown owned configuration group"),
    }
    println!("\n{}", budget.finish());
}

async fn capture_helper_drift(fixture: &Fixture, selection: &TargetSelection, budget: &mut Budget) {
    let limits = RunLimits::default();
    let cancel = Cancellation::default();
    let before = fixture.snapshot();
    let target = ExchangeTarget::capture(selection, limits, &cancel)
        .await
        .unwrap();
    assert_eq!(target.url, "https://example.invalid/repo.git");
    assert_eq!(target.helpers, ["manager"]);
    assert_eq!(fixture.snapshot(), before);
    budget.case_finished();
    fixture.phase("unsupported helper");
    fixture.git(&[
        "config",
        "credential.helper",
        "!echo invoked > helper-invoked",
    ]);
    let before = fixture.snapshot();
    let error = super::super::super::run(&target, &ExchangeOperation::Observe, limits, &cancel)
        .await
        .unwrap_err();
    assert_eq!(error, RunError::UnsupportedHelper);
    assert!(!fixture.0.join("helper-invoked").exists());
    assert_eq!(fixture.snapshot(), before);
    budget.case_finished();
    fixture.phase("helper drift");
    fixture.git(&["config", "credential.helper", "wincred"]);
    let before = fixture.snapshot();
    assert_eq!(
        super::super::super::run(&target, &ExchangeOperation::Observe, limits, &cancel)
            .await
            .unwrap_err(),
        RunError::TargetChanged
    );
    assert_eq!(fixture.snapshot(), before);
    budget.case_finished();
}

async fn ssh_config_rewrite(fixture: &Fixture, selection: &TargetSelection, budget: &mut Budget) {
    let limits = RunLimits::default();
    let cancel = Cancellation::default();
    fixture.git(&[
        "remote",
        "set-url",
        "origin",
        "git@example.invalid:repo.git",
    ]);
    fixture.phase("unsupported SSH");
    assert_eq!(
        ExchangeTarget::capture(selection, limits, &cancel)
            .await
            .unwrap_err(),
        RunError::UnsupportedTarget
    );
    budget.case_finished();
    fixture.git(&[
        "remote",
        "set-url",
        "origin",
        "https://example.invalid/repo.git",
    ]);
    for key in ["fetch.bundleURI", "push.pushOption"] {
        fixture.phase(key);
        fixture.git(&["config", key, "not-admitted"]);
        let before = fixture.snapshot();
        assert_eq!(
            ExchangeTarget::capture(selection, limits, &cancel)
                .await
                .unwrap_err(),
            RunError::UnsupportedConfiguration
        );
        assert_eq!(fixture.snapshot(), before);
        budget.case_finished();
        fixture.git(&["config", "--unset", key]);
    }
    fixture.git(&[
        "config",
        "url.https://other.invalid/.insteadOf",
        "https://example.invalid/",
    ]);
    fixture.phase("URL rewrite");
    assert_eq!(
        ExchangeTarget::capture(selection, limits, &cancel)
            .await
            .unwrap_err(),
        RunError::UnsupportedRewrite
    );
    budget.case_finished();
}
