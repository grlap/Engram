//! Separate-process backup writer exclusion; this is not a consumer fence.

use std::{
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use engram::backup::{
    CopyKind,
    target::{PushLock, RecordPaths, TargetError},
};

use super::{Homes, PROJECT, ProjectId};

#[test]
fn holder_process() {
    let Some(home) = std::env::var_os("ENGRAM_RESTORE_LOCK_HOME") else {
        return;
    };
    let mut barrier =
        TcpStream::connect(std::env::var("ENGRAM_RESTORE_LOCK_BARRIER").unwrap()).unwrap();
    barrier
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let paths = RecordPaths::new(
        std::path::Path::new(&home),
        &ProjectId(PROJECT.into()),
        CopyKind::Store,
    );
    let lock = PushLock::try_acquire(&paths).unwrap();
    barrier.write_all(b"L").unwrap();
    let mut release = [0];
    barrier.read_exact(&mut release).unwrap();
    assert_eq!(release, *b"R");
    drop(lock);
    barrier.write_all(b"D").unwrap();
}

struct Holder(Child);

impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn another_process_excludes_restore_until_release_or_process_death() {
    let homes = Homes::new();
    homes.set_target("clean");
    let home = homes.path("clean");
    let paths = RecordPaths::new(&home, &ProjectId(PROJECT.into()), CopyKind::Store);
    for crash in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut holder = Holder(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "windows_lock::holder_process", "--nocapture"])
                .env("ENGRAM_RESTORE_LOCK_HOME", &home)
                .env(
                    "ENGRAM_RESTORE_LOCK_BARRIER",
                    listener.local_addr().unwrap().to_string(),
                )
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut barrier = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        holder.0.try_wait().unwrap().is_none(),
                        "holder exited before connecting"
                    );
                    assert!(Instant::now() < deadline, "holder never connected");
                    std::thread::yield_now();
                }
                Err(error) => panic!("barrier accept: {error}"),
            }
        };
        // Windows inherits the listener's nonblocking mode on accept.
        barrier.set_nonblocking(false).unwrap();
        barrier
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut ready = [0];
        barrier.read_exact(&mut ready).unwrap();
        assert_eq!(ready, *b"L");
        let refused = homes.engram(
            "clean",
            &[
                "backup",
                "restore",
                "not-a-copy",
                "--origin-retired-by",
                "greg",
                "--json",
            ],
        );
        assert!(!refused.status.success());
        let value: serde_json::Value = serde_json::from_slice(&refused.stdout).unwrap();
        assert_eq!(value["code"], "backup_push_running", "{value}");
        assert!(!homes.database("clean").exists());
        if crash {
            holder.0.kill().unwrap();
            holder.0.wait().unwrap();
            // Windows may release a terminated process's lock after wait returns.
            // Bound that wait and permit only the still-held lock to refuse.
            let release_deadline = Instant::now() + Duration::from_secs(30);
            loop {
                match PushLock::try_acquire(&paths) {
                    Ok(lock) => {
                        drop(lock);
                        break;
                    }
                    Err(TargetError::PushRunning { .. }) => {
                        assert!(
                            Instant::now() < release_deadline,
                            "the killed holder's lock was never released"
                        );
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(error) => panic!("writer lock after holder termination: {error}"),
                }
            }
        } else {
            barrier.write_all(b"R").unwrap();
            barrier.read_exact(&mut ready).unwrap();
            assert_eq!(ready, *b"D");
            assert!(holder.0.wait().unwrap().success());
            // The barrier follows an explicit, synchronous unlock.
            drop(PushLock::try_acquire(&paths).expect("released writer lock must be available"));
        }
    }
}
