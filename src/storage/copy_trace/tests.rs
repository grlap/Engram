use super::*;

fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

#[test]
fn busy_observations_are_bounded_and_distinguish_sleep_from_outside_time() {
    start();
    let began = RECORD.with(|record| record.borrow().as_ref().unwrap().began);
    mark(|| "first read begins".into());
    busy_enter(began + ms(1));
    busy_sleep(ms(1200));
    busy_return(began + ms(1201), None);
    busy_enter(began + ms(1205));
    busy_sleep(ms(2));
    busy_return(began + ms(1207), Some("copy deadline"));
    RECORD.with(|record| {
        let record = record.borrow();
        let record = record.as_ref().unwrap();
        assert_eq!(record.steps.len(), 1, "no per-callback step allocation");
        assert_eq!(record.busy.calls, 2);
        assert_eq!(record.busy.sleeps, 2);
        assert_eq!(record.busy.first_entry, Some(ms(1)));
        assert_eq!(record.busy.last_entry, Some(ms(1205)));
        assert_eq!(record.busy.last_return, Some(ms(1207)));
        assert_eq!(record.busy.max_outside, ms(4));
        assert_eq!(record.busy.max_sleep, ms(1200));
        assert_eq!(record.busy.final_refusal, Some((ms(1207), "copy deadline")));
    });
    let trace = take();
    assert_eq!(trace.len(), 2);
    assert!(trace[1].starts_with("busy callbacks:"));
    assert!(take().is_empty(), "taking disables the recorder");
}

#[test]
fn disabled_and_other_thread_callbacks_record_nothing() {
    take();
    busy_enter(Instant::now());
    busy_sleep(ms(5));
    busy_return(Instant::now(), Some("no copy scope"));
    assert_eq!(take(), Vec::<String>::new());
    start();
    std::thread::spawn(|| {
        busy_enter(Instant::now());
        busy_return(Instant::now(), Some("no copy scope"));
        assert_eq!(take(), Vec::<String>::new());
    })
    .join()
    .unwrap();
    assert_eq!(take(), Vec::<String>::new());
}

#[test]
fn disposal_is_bracketed_and_sqlite_codes_survive_before_classification() {
    struct DuringDrop;
    impl Drop for DuringDrop {
        fn drop(&mut self) {
            mark(|| "resource drop".into());
        }
    }
    start();
    dispose(DuringDrop, "admission failure");
    let trace = take();
    assert_eq!(trace.len(), 3);
    assert!(trace[0].ends_with("admission failure: close begins"));
    assert!(trace[1].ends_with("resource drop"));
    assert!(trace[2].ends_with("admission failure: closed"));

    let code = rusqlite::ffi::SQLITE_BUSY_RECOVERY;
    let error = StoreError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(code),
        None,
    ));
    let recorded = store_error(&error);
    assert!(recorded.contains("DatabaseBusy"), "{recorded}");
    assert!(
        recorded.ends_with(&format!("extended {code}")),
        "{recorded}"
    );
    let wrapped = StoreError::SqliteFile {
        path: std::path::PathBuf::from("fixture.db"),
        utf16_length: 10,
        source: Box::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            None,
        )),
    };
    let recorded = store_error(&wrapped);
    assert!(recorded.contains("sqlite code DatabaseBusy"), "{recorded}");
    assert!(
        recorded.ends_with(&format!("extended {code}")),
        "{recorded}"
    );
}
