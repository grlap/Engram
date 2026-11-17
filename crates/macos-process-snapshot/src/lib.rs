//! The sole native boundary for reading macOS process-group snapshot bytes.
//! This crate does not interpret records, signal processes, or own children.
//! Callers must validate the record layout and their known group leader.

#![cfg(target_os = "macos")]

use std::{io, ptr};

// Match the caller's maximum of 1,048,576 64-bit kinfo_proc records. Reject
// excessive size before allocating; successful reads may shrink this buffer.
const MAX_BYTES: usize = 648 * 1_048_576;

/// Read the kernel's KERN_PROC_PGRP bytes, including zombie members.
///
/// # Errors
/// Returns the kernel errno, or refuses an invalid group, excessive size,
/// allocation failure or a returned length outside the supplied buffer.
pub fn process_group(group: u32) -> io::Result<Vec<u8>> {
    let group = i32::try_from(group)
        .ok()
        .filter(|group| *group > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid process group"))?;
    // CTL_KERN, KERN_PROC, KERN_PROC_PGRP. These legacy numeric nodes need no
    // oidfmt or sysctlbyname metadata lookup, which macOS does not provide.
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PGRP, group];
    let requested = read(&mut mib, None)?;
    if requested > MAX_BYTES {
        return Err(io::Error::other("process snapshot allocation limit"));
    }
    if requested == 0 {
        return Ok(Vec::new());
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(requested)
        .map_err(|_| io::Error::other("process snapshot allocation failed"))?;
    bytes.resize(requested, 0);
    let copied = read(&mut mib, Some(&mut bytes))?;
    bytes.truncate(copied);
    Ok(bytes)
}

fn read(mib: &mut [libc::c_int; 4], output: Option<&mut [u8]>) -> io::Result<usize> {
    let (buffer, capacity) = match output {
        Some(bytes) => (bytes.as_mut_ptr().cast::<libc::c_void>(), bytes.len()),
        None => (ptr::null_mut(), 0),
    };
    let mut length = capacity;
    // SAFETY: mib points to exactly four initialized c_ints and remains live
    // for the call. length is a writable size_t. A non-null buffer points to
    // capacity initialized writable bytes, exclusively borrowed for this call;
    // a null buffer requests only the required size. No new value is supplied,
    // so this is read-only. The kernel cannot retain these pointers. Its errno
    // is captured immediately on failure, before any other system call, and a
    // successful fill's returned length is checked before truncation or decode.
    let result =
        unsafe { libc::sysctl(mib.as_mut_ptr(), 4, buffer, &mut length, ptr::null_mut(), 0) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if !buffer.is_null() && length > capacity {
        return Err(io::Error::other("process snapshot exceeds supplied buffer"));
    }
    Ok(length)
}
