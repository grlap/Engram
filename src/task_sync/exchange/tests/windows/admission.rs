//! Exercise the real job wrapper's unsupported-capability path. This is not a
//! `TermAl` safeguard probe or a simulated successful Windows job assignment.

use super::*;
use process_wrap::tokio::ChildWrapper;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

/// A terminal child deliberately exposes no native process handle. It retains
/// the spawned process so the test can observe actual termination afterwards.
#[derive(Debug)]
struct OpaqueChild {
    _child: Arc<Mutex<tokio::process::Child>>,
    kill_called: Arc<AtomicBool>,
}

impl ChildWrapper for OpaqueChild {
    fn inner(&self) -> &dyn ChildWrapper {
        self
    }
    fn inner_mut(&mut self) -> &mut dyn ChildWrapper {
        self
    }
    fn into_inner(self: Box<Self>) -> Box<dyn ChildWrapper> {
        self
    }
    fn start_kill(&mut self) -> std::io::Result<()> {
        self.kill_called.store(true, Ordering::Release);
        // Deliberately defer native termination. Immediate killing cannot
        // discriminate suspension from a process killed before Rust startup.
        Ok(())
    }
}

#[tokio::test]
async fn task_exchange_windows_unsupported_controller_kills_suspended_child_before_code_runs() {
    // Positive control: this exact single-process fixture, without suspension,
    // must execute its first observable action within the same observation bound.
    let positive = Fixture::new();
    let mut command = tokio::process::Command::from(positive.command("grandchild"));
    command
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut running = command.spawn().unwrap();
    let started = Instant::now();
    let became_ready = loop {
        if descendant_holds_lock(&positive.0, "grandchild") {
            break true;
        }
        if started.elapsed() >= Duration::from_secs(8) {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    running.kill().await.unwrap();
    assert!(became_ready, "unsuspended positive control never executed");

    let fixture = Fixture::new();
    let mut wrapped =
        crate::task_sync::exchange::process::confined_command(fixture.command("grandchild"));
    let mut observed = None;
    let kill_called = Arc::new(AtomicBool::new(false));
    let result = wrapped.spawn_with_child(|command| {
        let child = Arc::new(Mutex::new(command.spawn()?));
        observed = Some(child.clone());
        Ok(Box::new(OpaqueChild {
            _child: child,
            kill_called: kill_called.clone(),
        }))
    });
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
    assert!(kill_called.load(Ordering::Acquire));
    let child = observed.unwrap();
    let observation = Instant::now();
    while observation.elapsed() < Duration::from_secs(8) {
        assert!(child.lock().unwrap().try_wait().unwrap().is_none());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let code_ran = fixture.0.join("grandchild.lock").exists();
    child.lock().unwrap().start_kill().unwrap();
    let started = Instant::now();
    loop {
        if child.lock().unwrap().try_wait().unwrap().is_some() {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "unsupported suspended child survived refusal"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        fs::read_dir(&fixture.0).unwrap().count(),
        0,
        "child code ran before unsupported-controller refusal"
    );
    assert!(
        !code_ran,
        "suspension regressed during the deferred-kill observation"
    );
}
