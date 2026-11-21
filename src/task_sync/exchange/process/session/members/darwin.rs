//! macOS `arm64/x86_64` user `kinfo_proc` ABI, not a Rust memory cast.
//!
//! Layout source: SDK `sys/sysctl.h::kinfo_proc` and `sys/proc.h::extern_proc`:
//! `kp_proc.p_un.__p_starttime` at 0, `p_stat` at 36, `p_pid` at 40;
//! `kp_eproc.e_pgid` at 296 + 268. The 64-bit record occupies 648 bytes.
//! <https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/sysctl.h>
//! <https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/proc.h>
//!
//! Check width, scalar ranges, group and the controller's known leader on
//! every snapshot. A different ABI fails closed instead of looking empty.

use super::Member;
use std::io;

const WIDTH: usize = 648;
const STATE: usize = 36;
const PID: usize = 40;
const GROUP: usize = 564;

pub(super) fn decode(bytes: &[u8], group: u32) -> io::Result<Vec<Member>> {
    let invalid = || io::Error::other("process snapshot layout mismatch");
    if !cfg!(any(target_arch = "aarch64", target_arch = "x86_64"))
        || bytes.len() > WIDTH * 1_048_576
        || !bytes.len().is_multiple_of(WIDTH)
    {
        return Err(invalid());
    }
    let mut members = Vec::new();
    for record in bytes.as_chunks::<WIDTH>().0 {
        let number = |offset| -> io::Result<u32> {
            Ok(u32::from_ne_bytes(
                record
                    .get(offset..offset + 4)
                    .ok_or_else(invalid)?
                    .try_into()
                    .map_err(|_| invalid())?,
            ))
        };
        let pid = number(PID)?;
        let status = *record.get(STATE).ok_or_else(invalid)?;
        if pid == 0
            || pid > i32::MAX.cast_unsigned()
            || number(GROUP)? != group
            || !(1..=5).contains(&status)
        {
            return Err(invalid());
        }
        let seconds = u64::from_ne_bytes(
            record
                .get(..8)
                .ok_or_else(invalid)?
                .try_into()
                .map_err(|_| invalid())?,
        );
        let micros = number(8)?;
        if micros >= 1_000_000 {
            return Err(invalid());
        }
        let identity = seconds
            .checked_mul(1_000_000)
            .and_then(|t| t.checked_add(u64::from(micros)))
            .ok_or_else(invalid)?;
        members.push(Member {
            pid,
            identity,
            terminal: status == 5,
        });
    }
    members.sort_unstable();
    if members.windows(2).any(|w| w[0].pid == w[1].pid) {
        return Err(invalid());
    }
    Ok(members)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_decodes_identity_and_rejects_partial_or_foreign_records() {
        let mut bytes = [0; WIDTH];
        bytes[..8].copy_from_slice(&123_u64.to_ne_bytes());
        bytes[8..12].copy_from_slice(&456_u32.to_ne_bytes());
        bytes[STATE] = 5;
        bytes[PID..PID + 4].copy_from_slice(&73_u32.to_ne_bytes());
        bytes[GROUP..GROUP + 4].copy_from_slice(&73_u32.to_ne_bytes());
        assert_eq!(
            decode(&bytes, 73).unwrap(),
            vec![Member {
                pid: 73,
                identity: 123_000_456,
                terminal: true
            }]
        );
        bytes[STATE] = 2;
        assert!(!decode(&bytes, 73).unwrap()[0].terminal);
        assert!(decode(&bytes[..WIDTH - 1], 73).is_err());
        assert!(decode(&bytes, 74).is_err());
        bytes[STATE] = 0;
        assert!(decode(&bytes, 73).is_err());
    }
}
