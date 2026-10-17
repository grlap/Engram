//! Fixed launch budgets for owned policy fixtures, separate from request timeouts.

use std::{
    cell::Cell,
    process::{Command, Output},
    time::Instant,
};

#[derive(Clone, Copy)]
pub(super) struct Group {
    pub name: &'static str,
    cases: usize,
    direct: usize,
    product: usize,
}

// Counts include repository initialization (two direct Git launches). Each
// malformed capture launches three queries; a complete capture launches seven.
pub(super) const GROUPS: [Group; 5] = [
    Group {
        name: "url-1-7",
        cases: 7,
        direct: 9,
        product: 21,
    },
    Group {
        name: "url-8-14",
        cases: 7,
        direct: 9,
        product: 21,
    },
    Group {
        name: "encoded-fabricated",
        cases: 2,
        direct: 9,
        product: 7,
    },
    Group {
        name: "helper-order-drift",
        cases: 2,
        direct: 7,
        product: 14,
    },
    Group {
        name: "helper-refusals",
        cases: 2,
        direct: 8,
        product: 14,
    },
];

impl Group {
    pub(super) fn assert_trace(self, stdout: &[u8]) {
        super::super::diagnostics::assert_complete_trace(stdout, self.direct, self.product);
    }

    pub(super) fn assert_bounded(self) {
        assert!(
            self.direct + self.product <= 30,
            "group={} has {} launches; split groups above the 30-launch ceiling",
            self.name,
            self.direct + self.product,
        );
    }

    pub fn report(self) -> String {
        format!(
            "policy-workload group={} cases={} direct={} product={} bound={} used={} fabricated-build=0",
            self.name,
            self.cases,
            self.direct,
            self.product,
            self.direct + self.product,
            self.direct + self.product,
        )
    }
}

#[derive(Clone, Copy)]
struct State {
    group: Group,
    started: Instant,
    product_start: usize,
    direct: usize,
    cases: usize,
}

std::thread_local! {
    static STATE: Cell<Option<State>> = const { Cell::new(None) };
}

fn product_launches() -> usize {
    super::super::super::super::test_support::LAUNCHES.with(Cell::get)
}

pub(super) fn begin(group: Group) {
    group.assert_bounded();
    STATE.with(|state| {
        assert!(state.get().is_none(), "one group per owned child");
        state.set(Some(State {
            group,
            started: Instant::now(),
            product_start: product_launches(),
            direct: 0,
            cases: 0,
        }));
    });
}

#[test]
fn task_exchange_policy_groups_fit_supervisor_launch_budget() {
    for group in GROUPS {
        group.assert_bounded();
    }
}

pub(super) fn counts() -> (usize, usize) {
    let state = STATE.with(Cell::get).expect("owned policy group is active");
    (state.direct, product_launches() - state.product_start)
}

fn check(state: State) {
    let product = product_launches() - state.product_start;
    assert!(
        state.direct <= state.group.direct && product <= state.group.product,
        "group={} direct={}/{} product={}/{}; add a group instead of increasing deadlines",
        state.group.name,
        state.direct,
        state.group.direct,
        product,
        state.group.product,
    );
}

// Fixture::git and assert_config bypass the product runner, so count them here.
// Other Windows fixture tests use this wrapper without an active policy group.
pub(in super::super) fn output(command: &mut Command) -> Output {
    STATE.with(|slot| {
        if let Some(mut state) = slot.get() {
            state.direct += 1;
            check(state);
            slot.set(Some(state));
        }
    });
    let trace = crate::task_sync::exchange::fixture_diagnostics::direct(command);
    let output = command.output();
    let error = match &output {
        Ok(output) if output.status.success() => None,
        Ok(_) => Some(crate::task_sync::exchange::RunError::Git),
        Err(_) => Some(crate::task_sync::exchange::RunError::Spawn),
    };
    trace.finish(error.as_ref());
    output.unwrap()
}

pub(super) fn case_finished() {
    STATE.with(|slot| {
        let mut state = slot.get().expect("owned policy group is active");
        check(state);
        state.cases += 1;
        assert!(
            state.cases <= state.group.cases,
            "unexpected extra policy case"
        );
        slot.set(Some(state));
    });
}

pub(super) fn finish() -> String {
    let state = STATE
        .with(|slot| slot.replace(None))
        .expect("owned policy group is active");
    let product = product_launches() - state.product_start;
    assert_eq!(
        (state.direct, product, state.cases),
        (state.group.direct, state.group.product, state.group.cases),
        "group={} must exercise every declared case and launch",
        state.group.name,
    );
    format!(
        "{} elapsed_ms={}",
        state.group.report(),
        state.started.elapsed().as_millis(),
    )
}
