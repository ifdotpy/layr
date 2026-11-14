//! Acting as a user while the service runs as root.
//!
//! - File access: `setfsuid`/`setfsgid` and the user's groups change only the identity used
//!   for permission checks. Commands that read or write paths named by the caller outside the
//!   store use them, so the service cannot reach files the caller cannot reach.
//! - Child processes: a clean environment (never the service's own), the user's groups, then the
//!   user's gid and uid, all set in the child before `exec`.

use crate::ctx::Ctx;
use crate::sys;
use anyhow::{anyhow, bail, Result};
use std::process::Command;

fn root() -> bool {
    rustix::process::geteuid().is_root()
}

/// A resolved filesystem identity. The primary group used for access is included in `groups`.
#[derive(Debug)]
pub struct FsIdentity {
    user: User,
}

impl FsIdentity {
    pub fn resolve(uid: u32, gid: u32) -> Result<Self> {
        let mut user = user(uid)?;
        user.gid = gid;
        if !user.groups.contains(&gid) {
            user.groups.push(gid);
        }
        Ok(Self { user })
    }

    pub fn uid(&self) -> u32 {
        self.user.uid
    }

    pub fn gid(&self) -> u32 {
        self.user.gid
    }

    pub fn groups(&self) -> &[u32] {
        &self.user.groups
    }

    pub fn command(&self, cmd: &mut Command, env: &[(String, String)]) -> Result<()> {
        command_as_user(cmd, &self.user, env)
    }
}

pub fn as_caller_fs<T>(ctx: &Ctx, f: impl FnOnce() -> Result<T>) -> Result<T> {
    if !root() || ctx.caller.is_root() {
        return f();
    }
    as_user_fs(ctx.caller.fs_identity()?.as_ref(), f)
}

/// Run `f` with an already resolved identity, then restore the previous credentials.
pub fn as_user_fs<T>(identity: &FsIdentity, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let (uid, gid) = (identity.uid(), identity.gid());
    if !root() || uid == 0 {
        return f();
    }
    let old_groups = sys::groups()?;
    sys::set_groups(&identity.user.groups)?;
    let (old_uid, old_gid) = match sys::set_fs_ids(uid, gid) {
        Ok(x) => x,
        Err(e) => {
            let _ = sys::set_groups(&old_groups);
            return Err(e);
        }
    };
    let r = f();
    let ids_ok = sys::set_fs_ids(old_uid, old_gid).is_ok();
    let groups_ok = sys::set_groups(&old_groups).is_ok();
    if !ids_ok || !groups_ok {
        return Err(anyhow!("cannot restore file system credentials"));
    }
    r
}

/// The file identity of a caller for many short steps (an objects checkout): the user's groups are
/// resolved once per request. On Linux the ids and groups belong to the thread; on macOS the
/// effective ids belong to the process, so the steps must run on one thread (`layr_objects::checkout_as` does).
pub struct CallerFs(std::sync::Arc<FsIdentity>);

impl CallerFs {
    /// `None` when this process is not root or the caller is root: steps then run as they are.
    pub fn of(ctx: &Ctx) -> Result<Option<CallerFs>> {
        if !root() || ctx.caller.is_root() {
            return Ok(None);
        }
        Ok(Some(Self(ctx.caller.fs_identity()?)))
    }

    /// Run one step with the caller's file identity, then take root's back.
    pub fn run(&self, step: &mut dyn FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
        as_user_fs(&self.0, || Ok(step()?)).map_err(|error| match error.downcast::<std::io::Error>() {
            Ok(error) => error,
            Err(error) => std::io::Error::other(error),
        })
    }
}

#[derive(Debug)]
pub struct User {
    pub uid: u32,
    pub gid: u32,
    pub name: String,
    pub home: String,
    pub groups: Vec<u32>,
}

/// A user and the groups it belongs to.
pub fn user(uid: u32) -> Result<User> {
    #[cfg(test)]
    IDENTITY_LOOKUPS.with(|lookups| lookups.set(lookups.get() + 1));
    match sys::try_user_by_uid(uid)? {
        Some(u) => {
            let groups = sys::user_groups(&u.name, u.gid)?;
            Ok(User { uid, gid: u.gid, name: u.name, home: u.home, groups })
        }
        None => Ok(User { uid, gid: uid, name: format!("uid{uid}"), home: "/".into(), groups: vec![uid] }),
    }
}

#[cfg(test)]
thread_local! {
    pub(crate) static IDENTITY_LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Use the request's identity for children as well as filesystem operations.
pub fn command_as_caller(cmd: &mut Command, ctx: &Ctx, env: &[(String, String)]) -> Result<()> {
    ctx.caller.fs_identity()?.command(cmd, env)
}

/// The PATH of a command that runs as a user for a service: the system folders, Homebrew's first on macOS (an old
/// `/usr/local/bin/git` of an Intel Mac does not run on Apple silicon without Rosetta).
const SERVICE_PATH: &str =
    if cfg!(target_os = "macos") { "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin" } else { "/usr/local/bin:/usr/bin:/bin" };

/// The PATH of a command that runs as `uid` from a process of `euid`: the caller's own PATH when the process is the
/// caller (it chose its tools, as in its shell), else `SERVICE_PATH`.
fn child_path(euid: u32, uid: u32) -> std::ffi::OsString {
    match std::env::var_os("PATH") {
        Some(path) if euid == uid && !path.is_empty() => path,
        _ => SERVICE_PATH.into(),
    }
}

/// Make `cmd` run as `uid` with a clean environment: HOME, USER, LOGNAME, PATH, and `env`.
/// When the service is root and `uid` is not, the child sets the user's groups, gid and uid
/// before `exec`; nothing of the service's environment or groups reaches it.
pub fn command_as(cmd: &mut Command, uid: u32, env: &[(String, String)]) -> Result<()> {
    command_as_user(cmd, &user(uid)?, env)
}

fn command_as_user(cmd: &mut Command, u: &User, env: &[(String, String)]) -> Result<()> {
    let uid = u.uid;
    cmd.env_clear();
    let path = child_path(rustix::process::geteuid().as_raw(), uid);
    cmd.env("HOME", &u.home).env("USER", &u.name).env("LOGNAME", &u.name).env("PATH", path);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let euid = rustix::process::geteuid().as_raw();
    if euid == 0 && uid != 0 {
        sys::switch_user_before_exec(cmd, u.uid, u.gid, u.groups.clone());
    } else if euid != uid && euid != 0 {
        bail!("cannot run a command as uid {uid}");
    }
    Ok(())
}

/// Selected variables of the caller's environment.
pub fn pick_env(ctx: &Ctx, keys: &[&str]) -> Vec<(String, String)> {
    keys.iter().filter_map(|k| ctx.env(k).map(|v| (k.to_string(), v.to_string()))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_identity_lookup_per_request_including_nested_contexts_and_backends() -> Result<()> {
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = Scratch(std::env::temp_dir().join(format!("layr-identity-{}", crate::ids::new_id())));
        let caller = crate::ctx::Caller::from_uid(rustix::process::geteuid().as_raw());
        let ctx = Ctx {
            store: crate::store::Store::open(&root.0.join("store"))?,
            caller,
            cwd: root.0.clone(),
            env: Default::default(),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
            author: Default::default(),
        };
        IDENTITY_LOOKUPS.with(|lookups| lookups.set(0));
        let nested = ctx.caller.clone();
        let identity = ctx.caller.fs_identity()?;
        let mut backend = crate::store_remote::backend::Backend::file(root.0.join("remote"));
        backend.as_caller(&ctx)?;
        backend.put_new("file", b"content".to_vec())?;
        for _ in 0..256 {
            assert!(std::sync::Arc::ptr_eq(&identity, &nested.fs_identity()?));
            let checkout = CallerFs(nested.fs_identity()?);
            assert!(std::sync::Arc::ptr_eq(&identity, &checkout.0));
            backend.as_caller(&ctx)?;
            let backend = backend.clone();
            assert_eq!(backend.get("file")?, Some(b"content".to_vec()));
            as_user_fs(&identity, || Ok(()))?;
            as_caller_fs(&ctx, || Ok(()))?;
            command_as_caller(&mut Command::new("git"), &ctx, &[])?;
            assert_eq!(crate::access::Who::caller(&nested).groups, identity.groups());
            ctx.caller.fs_identity()?.command(&mut Command::new("sh"), &[])?;
        }
        assert_eq!(IDENTITY_LOOKUPS.with(|lookups| lookups.get()), 1);
        let next_request = crate::ctx::Caller::from_uid(ctx.caller.uid);
        assert!(!std::sync::Arc::ptr_eq(&identity, &next_request.fs_identity()?));
        assert_eq!(IDENTITY_LOOKUPS.with(|lookups| lookups.get()), 2, "a new request refreshes memberships");
        Ok(())
    }

    #[test]
    fn caller_fs_keeps_the_step_io_error_kind() -> Result<()> {
        let caller = crate::ctx::Caller::from_uid(rustix::process::geteuid().as_raw());
        let fs = CallerFs(caller.fs_identity()?);
        for kind in [std::io::ErrorKind::NotFound, std::io::ErrorKind::PermissionDenied, std::io::ErrorKind::Interrupted] {
            let error = fs.run(&mut || Err(std::io::Error::new(kind, "step failed")))
                .err().ok_or_else(|| anyhow::anyhow!("filesystem step unexpectedly succeeded"))?;
            assert_eq!(error.kind(), kind);
            assert_eq!(error.to_string(), "step failed");
        }
        Ok(())
    }

    #[test]
    fn filesystem_identity_includes_the_requested_primary_group() -> Result<()> {
        let uid = rustix::process::geteuid().as_raw();
        // A different primary group must be retained along with the NSS memberships.
        let identity = FsIdentity::resolve(uid, 424242)?;
        assert_eq!(identity.uid(), uid);
        assert_eq!(identity.gid(), 424242);
        assert!(identity.user.groups.contains(&424242));
        Ok(())
    }

    #[test]
    fn a_command_of_the_caller_finds_its_tools_on_its_own_path() {
        let own = std::env::var_os("PATH").unwrap();
        assert_eq!(child_path(501, 501), own, "the caller's own PATH");
        assert_eq!(child_path(0, 501), std::ffi::OsString::from(SERVICE_PATH), "a service runs it with the fixed PATH");
        if cfg!(target_os = "macos") {
            assert!(SERVICE_PATH.starts_with("/opt/homebrew/bin:"));
        }
    }
}
