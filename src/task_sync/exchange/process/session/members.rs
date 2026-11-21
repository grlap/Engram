//! Complete member observations. A terminal Linux leader is insufficient: its
//! other threads may still run. macOS sysctl supplies the group including its
//! zombies; raw bytes are checked before reading any scalar field.

use std::io;

#[cfg(any(target_os = "macos", test))]
mod darwin;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(super) struct Member {
    pub(super) pid: u32,
    pub(super) identity: u64,
    pub(super) terminal: bool,
}

#[cfg(target_os = "linux")]
pub(super) fn snapshot(group: u32) -> io::Result<Vec<Member>> {
    use std::{fs, io::Read};
    const MAX_PROCESSES: usize = 1_048_576;
    let mut members = Vec::new();
    for (count, entry) in fs::read_dir("/proc")?.enumerate() {
        if count >= MAX_PROCESSES {
            return Err(io::Error::other("process enumeration limit"));
        }
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let read = |path: &std::path::Path, pid| -> io::Result<Option<Stat>> {
            let mut bytes = Vec::new();
            let result =
                fs::File::open(path).and_then(|file| file.take(8193).read_to_end(&mut bytes));
            match result {
                // A process can disappear after open, while procfs fills stat.
                Err(error) if vanished(&error) => return Ok(None),
                Err(error) => return Err(error),
                Ok(_) => {}
            }
            if bytes.len() > 8192 {
                return Err(io::Error::other("process stat limit"));
            }
            parse_member_stat(&bytes, pid, group, || current_group(pid))
        };
        let Some(stat) = read(&entry.path().join("stat"), pid)? else {
            continue;
        };
        let mut terminal = stat.terminal;
        if terminal {
            // A main thread can become a zombie while sibling threads live.
            let tasks = match fs::read_dir(entry.path().join("task")) {
                Ok(tasks) => tasks,
                Err(error) if vanished(&error) => continue,
                Err(error) => return Err(error),
            };
            for (count, task) in tasks.enumerate() {
                if count >= MAX_PROCESSES {
                    return Err(io::Error::other("thread enumeration limit"));
                }
                let task = task?;
                let thread_id = task
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse::<u32>().ok())
                    .ok_or_else(|| io::Error::other("invalid process thread identity"))?;
                if let Some(thread) = read(&task.path().join("stat"), thread_id)? {
                    terminal &= thread.terminal;
                }
            }
        }
        members.push(Member {
            pid,
            identity: stat.start,
            terminal,
        });
    }
    members.sort_unstable();
    Ok(members)
}

#[cfg(target_os = "linux")]
fn current_group(pid: u32) -> io::Result<Option<u32>> {
    use rustix::{
        io::Errno,
        process::{Pid, getpgid},
    };
    let pid = i32::try_from(pid).map_err(io::Error::other)?;
    let pid = Pid::from_raw(pid)
        .ok_or_else(|| io::Error::other("invalid process id for group lookup"))?;
    match getpgid(Some(pid)) {
        Ok(group) => u32::try_from(group.as_raw_pid())
            .map(Some)
            .map_err(io::Error::other),
        Err(Errno::SRCH) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(any(target_os = "linux", test))]
fn parse_member_stat(
    bytes: &[u8],
    pid: u32,
    group: u32,
    lookup_group: impl FnOnce() -> io::Result<Option<u32>>,
) -> io::Result<Option<Stat>> {
    let stat = match parse_stat(bytes) {
        Ok(stat) => stat,
        Err(error) => {
            // A process may exit or change state while procfs fills stat.
            // Ignore malformed records only after the kernel independently
            // excludes the PID. The reserved leader must always be readable.
            if pid != group && lookup_group()? != Some(group) {
                return Ok(None);
            }
            return Err(io::Error::new(
                error.kind(),
                format!("{error} for pid={pid}"),
            ));
        }
    };
    if stat.group != group {
        return Ok(None);
    }
    if stat.pid != pid {
        return Err(io::Error::other("process identity mismatch"));
    }
    Ok(Some(stat))
}

#[cfg(target_os = "linux")]
fn vanished(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
        || error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error())
}

#[cfg(any(target_os = "linux", test))]
struct Stat {
    pid: u32,
    group: u32,
    start: u64,
    terminal: bool,
}

#[cfg(any(target_os = "linux", test))]
fn parse_stat(bytes: &[u8]) -> io::Result<Stat> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid process stat");
    // comm is allowed to contain spaces, newlines and parentheses.
    let open = bytes.iter().position(|b| *b == b'(').ok_or_else(invalid)?;
    let close = bytes.iter().rposition(|b| *b == b')').ok_or_else(invalid)?;
    let pid = std::str::from_utf8(bytes.get(..open).ok_or_else(invalid)?)
        .map_err(|_| invalid())?
        .trim()
        .parse()
        .map_err(|_| invalid())?;
    let rest =
        std::str::from_utf8(bytes.get(close + 1..).ok_or_else(invalid)?).map_err(|_| invalid())?;
    let fields: Vec<_> = rest.split_ascii_whitespace().collect();
    let state = *fields.first().ok_or_else(invalid)?;
    if !matches!(
        state,
        "R" | "S" | "D" | "Z" | "X" | "x" | "T" | "t" | "W" | "I" | "P" | "K"
    ) {
        return Err(invalid());
    }
    Ok(Stat {
        pid,
        group: fields
            .get(2)
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())?,
        start: fields
            .get(19)
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())?,
        terminal: matches!(state, "Z" | "X" | "x"),
    })
}

#[cfg(target_os = "macos")]
pub(super) fn snapshot(group: u32) -> io::Result<Vec<Member>> {
    let bytes = macos_process_snapshot::process_group(group)?;
    darwin::decode(&bytes, group)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn snapshot(_: u32) -> io::Result<Vec<Member>> {
    Err(io::Error::other(
        "process membership unsupported on this Unix",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stat_comm_does_not_shift_identity_or_terminal_fields() {
        let mut fields = vec!["0"; 20];
        fields[0] = "Z";
        fields[1] = "1";
        fields[2] = "73";
        fields[19] = "987";
        let tail = fields.join(" ");
        let stat = parse_stat(format!("42 (a ) spaced\n(name) {tail}").as_bytes()).unwrap();
        assert_eq!(
            (stat.pid, stat.group, stat.start, stat.terminal),
            (42, 73, 987, true)
        );
        assert!(parse_stat(b"42 (bad) Z 1").is_err());
    }

    #[test]
    fn malformed_stat_requires_independent_exclusion_from_the_owned_group() {
        let malformed = b"42 (exiting) Z 1";
        assert!(
            parse_member_stat(malformed, 42, 73, || Ok(None))
                .unwrap()
                .is_none()
        );
        assert!(
            parse_member_stat(malformed, 42, 73, || Ok(Some(99)))
                .unwrap()
                .is_none()
        );
        assert!(parse_member_stat(malformed, 42, 73, || Ok(Some(73))).is_err());
        assert!(
            parse_member_stat(malformed, 42, 73, || Err(
                io::ErrorKind::PermissionDenied.into()
            ))
            .is_err()
        );
        assert!(
            parse_member_stat(malformed, 73, 73, || panic!("leader must stay reserved")).is_err()
        );
    }
}
