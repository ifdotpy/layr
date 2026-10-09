//! File access with the caller's permissions while the service runs as root.
//!
//! `setfsuid`/`setfsgid` change only the identity used for file permission checks, for the
//! current thread. Commands that read or write paths named by the caller outside the store use
//! them, so the service cannot be used to reach files the caller cannot reach.

use crate::ctx::Ctx;
use anyhow::Result;

pub fn as_caller_fs<T>(ctx: &Ctx, f: impl FnOnce() -> Result<T>) -> Result<T> {
    if unsafe { libc::geteuid() } != 0 || ctx.caller.uid == 0 {
        return f();
    }
    unsafe {
        libc::setfsgid(ctx.caller.gid);
        libc::setfsuid(ctx.caller.uid);
    }
    let r = f();
    unsafe {
        libc::setfsuid(0);
        libc::setfsgid(0);
    }
    r
}

/// Check that the caller can write into `dir` (it exists and is writable for the caller).
pub fn check_caller_writable(ctx: &Ctx, dir: &std::path::Path) -> Result<()> {
    as_caller_fs(ctx, || {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(dir.as_os_str().as_bytes())?;
        // With fsuid set, access(2) with AT_EACCESS uses the effective ids; check by creating a probe.
        let probe = dir.join(format!(".layr-probe-{}", std::process::id()));
        let _ = c;
        std::fs::write(&probe, b"")?;
        std::fs::remove_file(&probe)?;
        Ok(())
    })
    .map_err(|e| anyhow::anyhow!("permission denied: cannot write {} ({e})", dir.display()))
}
