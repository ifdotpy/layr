//! The root service (`layr daemon`) and the client side of `layr` for other users.
//!
//! A client sends its arguments, working folder and environment over a Unix socket, with its
//! standard input, output and error and an open handle of its working folder as `SCM_RIGHTS`.
//! The service forks one process per request (the parent stays single threaded), takes the
//! caller's uid from `SO_PEERCRED`, runs the command with the caller's streams and returns the
//! exit code. Commands check the caller against line owners and the project lead.

use crate::ctx::{Caller, Ctx};
use crate::store::Store;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

pub fn socket_path() -> PathBuf {
    std::env::var_os("LAYR_SOCKET").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/run/layr/layr.sock"))
}

const MAX_REQ: usize = 4 << 20;

fn send_with_fds(sock: &UnixStream, data: &[u8], fds: &[RawFd]) -> Result<()> {
    unsafe {
        let space = libc::CMSG_SPACE(std::mem::size_of_val(fds) as u32) as usize;
        let mut cbuf = vec![0u8; space];
        let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut _, iov_len: data.len() };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut _;
        msg.msg_controllen = space as _;
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds) as u32) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, libc::CMSG_DATA(c), std::mem::size_of_val(fds));
        let n = libc::sendmsg(sock.as_raw_fd(), &msg, 0);
        if n < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let n = n as usize;
        if n < data.len() {
            (&*sock).write_all(&data[n..])?;
        }
    }
    Ok(())
}

fn recv_with_fds(sock: &UnixStream, buf: &mut [u8], max_fds: usize) -> Result<(usize, Vec<RawFd>)> {
    unsafe {
        let space = libc::CMSG_SPACE((max_fds * std::mem::size_of::<RawFd>()) as u32) as usize;
        let mut cbuf = vec![0u8; space];
        let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut _, iov_len: buf.len() };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut _;
        msg.msg_controllen = space as _;
        let n = libc::recvmsg(sock.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC);
        if n < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut fds = Vec::new();
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let count = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<RawFd>();
                let p = libc::CMSG_DATA(c) as *const RawFd;
                for i in 0..count {
                    fds.push(*p.add(i));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
        Ok((n as usize, fds))
    }
}

/// Client: run a command through the service. Returns the exit code.
pub fn forward(argv: &[String]) -> Result<i32> {
    let path = socket_path();
    let sock = UnixStream::connect(&path).map_err(|e| {
        anyhow!(
            "cannot reach the layr service at {} ({e}). Start it as root with 'layr daemon', or run layr as root.",
            path.display()
        )
    })?;
    let cwd = std::env::current_dir()?;
    let env: HashMap<String, String> = std::env::vars().collect();
    let req = serde_json::to_vec(&serde_json::json!({"argv": argv, "cwd": cwd, "env": env}))?;
    let dir = std::fs::File::open(&cwd)?;
    let mut data = (req.len() as u32).to_be_bytes().to_vec();
    data.extend_from_slice(&req);
    // The service answers "B" at once when it is busy, or "R" and the exit code at the end.
    let _ = send_with_fds(&sock, &data, &[0, 1, 2, dir.as_raw_fd()]);
    let mut kind = [0u8; 1];
    (&sock).read_exact(&mut kind).context("the layr service closed the connection")?;
    if kind[0] == b'B' {
        bail!("the layr service is busy (too many running requests); try again");
    }
    let mut code = [0u8; 4];
    (&sock).read_exact(&mut code).context("the layr service closed the connection")?;
    Ok(i32::from_be_bytes(code))
}

fn peer_uid(sock: &UnixStream) -> Result<(u32, u32, i32)> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(sock.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cred as *mut _ as *mut _, &mut len)
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok((cred.uid, cred.gid, cred.pid))
}

fn limit(var: &str, default: usize) -> usize {
    std::env::var(var).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Child process: serve one request, then exit.
fn handle(sock: UnixStream, root: &Path) -> i32 {
    let res = (|| -> Result<i32> {
        let (uid, _gid, _pid) = peer_uid(&sock)?;
        // A client that does not send its request in time does not hold a process.
        sock.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
        let mut first = vec![0u8; 65536];
        let (n, fds) = recv_with_fds(&sock, &mut first, 8)?;
        if fds.len() != 4 {
            bail!("bad request: expected 4 descriptors");
        }
        if n < 4 {
            bail!("bad request");
        }
        let len = u32::from_be_bytes([first[0], first[1], first[2], first[3]]) as usize;
        if len > MAX_REQ {
            bail!("request too large");
        }
        let mut req = first[4..n].to_vec();
        while req.len() < len {
            let mut more = vec![0u8; len - req.len()];
            let k = (&sock).read(&mut more)?;
            if k == 0 {
                bail!("short request");
            }
            req.extend_from_slice(&more[..k]);
        }
        let v: serde_json::Value = serde_json::from_slice(&req)?;
        let argv: Vec<String> = serde_json::from_value(v["argv"].clone())?;
        let env: HashMap<String, String> = serde_json::from_value(v["env"].clone()).unwrap_or_default();
        unsafe {
            for (i, fd) in fds[..3].iter().enumerate() {
                libc::dup2(*fd, i as i32);
                libc::close(*fd);
            }
            if libc::fchdir(fds[3]) != 0 {
                bail!("cannot enter the working folder");
            }
            libc::close(fds[3]);
        }
        let cwd = std::env::current_dir()?;
        Ok(run_as(argv, Caller::from_uid(uid), cwd, env, root))
    })();
    let code = match res {
        Ok(c) => c,
        Err(e) => {
            eprintln!("layr service: {e:#}");
            128
        }
    };
    let mut reply = vec![b'R'];
    reply.extend_from_slice(&code.to_be_bytes());
    let _ = (&sock).write_all(&reply);
    code
}

/// Run a command for a caller and return its exit code (errors are printed to stderr).
pub fn run_as(argv: Vec<String>, caller: Caller, cwd: PathBuf, env: HashMap<String, String>, root: &Path) -> i32 {
    let store = match Store::open(root) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fatal: cannot open the layr store {}: {e:#}", root.display());
            return 128;
        }
    };
    let out: Box<dyn Write> = Box::new(std::io::BufWriter::new(std::io::stdout()));
    let ctx = Ctx { store, caller, cwd, env, out: std::cell::RefCell::new(out) };
    let r = crate::cmd::run(&ctx, &argv);
    let _ = ctx.out.borrow_mut().flush();
    match r {
        Ok(c) => c,
        Err(e) => {
            if let Some(x) = e.downcast_ref::<crate::cmd::Exit>() {
                if !x.1.is_empty() {
                    eprintln!("{}", x.1);
                }
                x.0
            } else {
                eprintln!("fatal: {e:#}");
                128
            }
        }
    }
}

/// Periodic work: automatic states of busy lines, retention, backups.
fn periodic(root: &Path, hourly: bool) {
    let now = crate::ids::now_ms();
    let store = match Store::open(root) {
        Ok(s) => s,
        Err(_) => return,
    };
    let caller = Caller::from_uid(0);
    let names = store.project_names().unwrap_or_default();
    drop(store);
    for name in names {
        let store = match Store::open(root) {
            Ok(s) => s,
            Err(_) => return,
        };
        let ctx = Ctx {
            store,
            caller: caller.clone(),
            cwd: PathBuf::from("/"),
            env: HashMap::new(),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
        };
        let repo = match crate::repo::Repo::open(&ctx, &name) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let interval = repo.p.config_u64("snapshot.interval", 600) as i64 * 1000;
        if let Ok(lines) = repo.lines() {
            for l in lines {
                if l.machine != repo.p.store.machine.id || !repo.working(&l).exists() {
                    continue;
                }
                let last = repo.last_auto(&l).ok().flatten().map(|s| s.time).unwrap_or(0);
                if now - last >= interval {
                    if let Ok(_lock) = repo.p.lock() {
                        let _ = repo.save(&ctx, &l, "periodic", true, false);
                    }
                }
            }
        }
        if hourly {
            let _ = crate::cmd::admin::run_gc(&ctx, &repo, false);
            if let Ok(Some(dir)) = repo.p.config_value("backup.dir") {
                if let Some(d) = dir.as_str() {
                    // Written as the lead of this machine, who configured the folder.
                    let _ = crate::cmd::transfer::run_backup(&ctx, &repo, Path::new(d), false, repo.lead_ids());
                }
            }
        }
    }
}

pub fn daemon(args: &[String]) -> Result<i32> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("layr daemon must run as root");
    }
    let mut sock = socket_path();
    let mut tick = 60u64;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" => {
                sock = PathBuf::from(args.get(i + 1).ok_or_else(|| anyhow!("--socket needs a path"))?);
                i += 1;
            }
            "--tick" => {
                tick = args.get(i + 1).ok_or_else(|| anyhow!("--tick needs seconds"))?.parse()?;
                i += 1;
            }
            x => bail!("unknown option {x}"),
        }
        i += 1;
    }
    let root = crate::store::default_root();
    Store::open(&root)?;
    if let Some(d) = sock.parent() {
        std::fs::create_dir_all(d)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755))?;
    }
    if sock.exists() {
        if UnixStream::connect(&sock).is_ok() {
            bail!("another layr service is running on {}", sock.display());
        }
        std::fs::remove_file(&sock)?;
    }
    let listener = UnixListener::bind(&sock)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o666))?;
    }
    eprintln!("layr service: listening on {} (store {})", sock.display(), root.display());
    let mut last_hourly: i64 = 0;
    let mut next = std::time::Instant::now() + std::time::Duration::from_secs(tick);
    // Running request processes, by caller: limits keep one user from exhausting the service.
    let mut children: HashMap<i32, u32> = HashMap::new();
    let (max_total, max_per_user) = (limit("LAYR_MAX_REQUESTS", 256), limit("LAYR_MAX_REQUESTS_PER_USER", 32));
    loop {
        // Reap finished children.
        loop {
            let pid = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
            if pid <= 0 {
                break;
            }
            children.remove(&pid);
        }
        let mut pfd = libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let wait = next.saturating_duration_since(std::time::Instant::now()).as_millis().min(1000) as i32;
        let rc = unsafe { libc::poll(&mut pfd, 1, wait.max(0)) };
        if rc > 0 && pfd.revents & libc::POLLIN != 0 {
            if let Ok((conn, _)) = listener.accept() {
                let uid = peer_uid(&conn).map(|x| x.0).unwrap_or(u32::MAX);
                let mine = children.values().filter(|u| **u == uid).count();
                if children.len() >= max_total || (uid != 0 && mine >= max_per_user) {
                    let _ = (&conn).write_all(b"B");
                    drop(conn);
                    continue;
                }
                match unsafe { libc::fork() } {
                    0 => {
                        drop(listener);
                        let code = handle(conn, &root);
                        std::process::exit(if code >= 0 { 0 } else { 1 });
                    }
                    -1 => eprintln!("layr service: fork failed"),
                    pid => {
                        children.insert(pid, uid);
                        drop(conn);
                    }
                }
            }
        }
        if std::time::Instant::now() >= next {
            next = std::time::Instant::now() + std::time::Duration::from_secs(tick);
            // Periodic work in a child, so the service stays responsive and single threaded.
            let now = crate::ids::now_ms();
            let hourly = now - last_hourly >= 3_600_000;
            if hourly {
                last_hourly = now;
            }
            if unsafe { libc::fork() } == 0 {
                drop(listener);
                periodic(&root, hourly);
                std::process::exit(0);
            }
        }
    }
}
