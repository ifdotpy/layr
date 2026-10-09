//! Acting as a user while the service runs as root.
//!
//! - File access: `setfsuid`/`setfsgid` change only the identity used for permission checks, for
//!   the current thread. Commands that read or write paths named by the caller outside the store
//!   use them, so the service cannot reach files the caller cannot reach.
//! - Child processes: a clean environment (never the service's own), the user's groups, then the
//!   user's gid and uid, all set in the child before `exec`.

use crate::ctx::Ctx;
use anyhow::{anyhow, bail, Result};
use std::ffi::CString;
use std::process::Command;

pub fn as_caller_fs<T>(ctx: &Ctx, f: impl FnOnce() -> Result<T>) -> Result<T> {
    as_user_fs(ctx.caller.uid, ctx.caller.gid, f)
}

pub fn as_user_fs<T>(uid: u32, gid: u32, f: impl FnOnce() -> Result<T>) -> Result<T> {
    if unsafe { libc::geteuid() } != 0 || uid == 0 {
        return f();
    }
    let mut target_groups = user(uid)?.groups;
    if !target_groups.contains(&gid) {
        target_groups.push(gid);
    }
    let old_count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if old_count < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut old_groups = vec![0; old_count as usize];
    if old_count > 0 && unsafe { libc::getgroups(old_count, old_groups.as_mut_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    unsafe {
        if libc::setgroups(target_groups.len(), target_groups.as_ptr()) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    // setfsgid/setfsuid return the previous id; a second call with the same id confirms it.
    let old_gid = unsafe { libc::setfsgid(gid) } as u32;
    if unsafe { libc::setfsgid(gid) } as u32 != gid {
        unsafe {
            libc::setgroups(old_groups.len(), old_groups.as_ptr());
        }
        bail!("cannot set filesystem gid to {gid}");
    }
    let old_uid = unsafe { libc::setfsuid(uid) } as u32;
    if unsafe { libc::setfsuid(uid) } as u32 != uid {
        unsafe {
            libc::setfsgid(old_gid);
            libc::setgroups(old_groups.len(), old_groups.as_ptr());
        }
        bail!("cannot set filesystem uid to {uid}");
    }
    let r = f();
    let uid_ok = unsafe {
        libc::setfsuid(old_uid);
        libc::setfsuid(old_uid) as u32 == old_uid
    };
    let gid_ok = unsafe {
        libc::setfsgid(old_gid);
        libc::setfsgid(old_gid) as u32 == old_gid
    };
    let groups_ok = unsafe { libc::setgroups(old_groups.len(), old_groups.as_ptr()) == 0 };
    if !uid_ok || !gid_ok || !groups_ok {
        return Err(anyhow!("cannot restore filesystem credentials"));
    }
    r
}

/// Check that the caller can write into `dir` (it exists and is writable for the caller).
pub fn check_caller_writable(ctx: &Ctx, dir: &std::path::Path) -> Result<()> {
    as_caller_fs(ctx, || {
        let probe = dir.join(format!(".layr-probe-{}-{}", std::process::id(), rand::random::<u32>()));
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().write(true).create_new(true).custom_flags(libc::O_NOFOLLOW).open(&probe)?;
        std::fs::remove_file(&probe)?;
        Ok(())
    })
    .map_err(|e| anyhow::anyhow!("permission denied: cannot write {} ({e})", dir.display()))
}

pub struct User {
    pub uid: u32,
    pub gid: u32,
    pub name: String,
    pub home: String,
    pub groups: Vec<libc::gid_t>,
}

/// A user and the groups it belongs to.
pub fn user(uid: u32) -> Result<User> {
    let c = crate::ctx::Caller::from_uid(uid);
    let mut groups: Vec<libc::gid_t> = vec![0; 256];
    let mut n: libc::c_int = groups.len() as libc::c_int;
    let name = CString::new(c.user.clone())?;
    let rc = unsafe { libc::getgrouplist(name.as_ptr(), c.gid, groups.as_mut_ptr(), &mut n) };
    if rc < 0 {
        groups = vec![c.gid];
    } else {
        groups.truncate(n as usize);
    }
    Ok(User { uid, gid: c.gid, name: c.user, home: c.home.to_string_lossy().into_owned(), groups })
}

/// Make `cmd` run as `uid` with a clean environment: HOME, USER, LOGNAME, PATH, and `env`.
/// When the service is root and `uid` is not, the child sets the user's groups, gid and uid
/// before `exec`; nothing of the service's environment or groups reaches it.
pub fn command_as(cmd: &mut Command, uid: u32, env: &[(String, String)]) -> Result<()> {
    let u = user(uid)?;
    cmd.env_clear();
    cmd.env("HOME", &u.home).env("USER", &u.name).env("LOGNAME", &u.name).env("PATH", "/usr/local/bin:/usr/bin:/bin");
    for (k, v) in env {
        cmd.env(k, v);
    }
    if unsafe { libc::geteuid() } == 0 && uid != 0 {
        use std::os::unix::process::CommandExt;
        let groups = u.groups.clone();
        let (gid, uid) = (u.gid, u.uid);
        unsafe {
            cmd.pre_exec(move || {
                if libc::setgroups(groups.len(), groups.as_ptr()) != 0
                    || libc::setgid(gid) != 0
                    || libc::setuid(uid) != 0
                    || libc::geteuid() != uid
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    } else if unsafe { libc::geteuid() } != uid && unsafe { libc::geteuid() } != 0 {
        bail!("cannot run a command as uid {uid}");
    }
    Ok(())
}

/// Selected variables of the caller's environment.
pub fn pick_env(ctx: &Ctx, keys: &[&str]) -> Vec<(String, String)> {
    keys.iter().filter_map(|k| ctx.env(k).map(|v| (k.to_string(), v.to_string()))).collect()
}
