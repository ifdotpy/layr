//! Experimental virtual lines (macOS, `LAYR_EXPERIMENTAL_VIEWS=1`, docs/views.md).
//!
//! The working folder of a virtual line is a LayrFS mount (tools/layrfs). The mount merges an
//! upper folder over the complete tree of the state the line started from (the lower tree, which
//! never changes). The upper folder is a thin layer (docs/apfs-thin-states.md): changed files,
//! new files, and the removed paths in its `com.layr.removed` attribute. Status, diff and save
//! read the upper folder and the head; they never scan the merged tree.
//!
//! The kind of view is local to this machine: `meta/vlines/<line id>/view.json`, never a record.
use crate::changes::{self, EntryChange};
use crate::ctx::Ctx;
use crate::errors::{error, Kind as ErrorKind};
use crate::fsutil::{self, Meta};
use crate::ids;
use crate::model::{Line, LineChange, StateKind, StateRec};
use crate::repo::{skip_path, Repo, Status};
use crate::store::Project;
use crate::thin::{self, Layered};
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The switch of the experiment.
pub const ENV: &str = "LAYR_EXPERIMENTAL_VIEWS";
/// The settings file that makes LayrFS mount a folder as an overlay.
const CONF: &str = "layrfs-view.conf";
const FS_TYPE: &str = "layrfs";
/// Folders that macOS services make at the root of a mount. LayrFS refuses them; older upper
/// folders may hold them. They are no content of the line.
pub(crate) const SYSTEM_ROOT_NAMES: &[&str] =
    &[".fseventsd", ".Spotlight-V100", ".Trashes", ".TemporaryItems", ".DocumentRevisions-V100"];

/// The local record of a virtual line.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct View {
    /// The line id.
    pub line: String,
    /// The line name when the view was made (lines with a view cannot be renamed).
    pub name: String,
    /// The state whose complete tree is the lower tree.
    pub base: String,
    /// The complete tree of `base` (`states/<base>`).
    pub lower: PathBuf,
    /// False after `layr line show <line> --none`: the layer stays, the mount is gone.
    pub mounted: bool,
    /// A fast line (`line new --fast`): the same LayrFS mount, but every command takes it through
    /// the folder paths (`of` gives no view for it; `fast_of` does).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fast: bool,
}

/// Why a fast line cannot start at `start` here, if it cannot.
pub fn fast_available(repo: &Repo, start: &str) -> std::result::Result<(), String> {
    if repo.p.is_overlay() {
        return Err("an overlay project has no fast lines".into());
    }
    // LayrFS copies one name of a hard link up on a write, and the shadow copies each name: a
    // save would split the link. The objects of the state name its link groups.
    if let Ok(Some((store, r))) = crate::objstore::verified(&repo.p, start) {
        if store.get_tree(&r.tree).is_ok_and(|t| !t.links.is_empty()) {
            return Err(format!("state {} has hard links, which fast lines do not keep", crate::ids::short(start)));
        }
    }
    // The LayrFS file system extension (FSKit) serves the folder of a fast line.
    let Some(app) = fskit_host_app() else {
        return Err(format!(
            "the LayrFS file system extension ({FSKIT_MODULE}) is not installed: install Layr.app, open it once, \
             and turn LayrFS on in System Settings > General > Login Items & Extensions > File System Extensions"
        ));
    };
    // pluginkit lists the extension also when it is off; the host app asks FSKit.
    match host_app_status(&app).as_deref() {
        Some("disabled") => Err(format!(
            "LayrFS is off: turn it on in System Settings > General > Login Items & Extensions > \
             File System Extensions (open {} shows the pane)",
            app.display()
        )),
        Some("missing") => Err(format!(
            "FSKit does not know the LayrFS extension of {}: open the app once, then turn LayrFS on in System \
             Settings > General > Login Items & Extensions > File System Extensions",
            app.display()
        )),
        // "enabled", or no answer: an older app without `--status` (the mount then tells).
        _ => Ok(()),
    }
}

/// The app that carries the registered LayrFS extension (pluginkit gives the path of the
/// extension, `<app>/Contents/Extensions/LayrFS.appex`).
fn fskit_host_app() -> Option<PathBuf> {
    let out = std::process::Command::new("/usr/bin/pluginkit")
        .args(["-m", "-v", "-p", "com.apple.fskit.fsmodule", "-i", FSKIT_MODULE])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().find(|l| l.contains(FSKIT_MODULE))?;
    let appex = PathBuf::from(line.rsplit('\t').next()?.trim());
    appex.ancestors().nth(3).map(Path::to_path_buf)
}

/// `Layr --status` of the host app: "enabled", "disabled" or "missing". None when it
/// fails or gives no answer in five seconds (an older app without the option opens a window).
fn host_app_status(app: &Path) -> Option<String> {
    use std::time::{Duration, Instant};
    // Layr.app; apps up to 0.53 were LayrFSHost.app.
    let bin = ["Contents/MacOS/Layr", "Contents/MacOS/LayrFSHost"].iter().map(|b| app.join(b)).find(|b| b.is_file())?;
    let mut child = std::process::Command::new(bin)
        .arg("--status")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    // The read runs in its own thread: a descendant that keeps the pipe open cannot hold it.
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = String::new();
        let _ = tx.send(std::io::Read::read_to_string(&mut stdout, &mut out).map(|_| out));
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let out = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).ok()?.ok()?;
    status.success().then(|| out.trim().to_string())
}

/// The bundle id of the LayrFS extension (tools/layrfs).
const FSKIT_MODULE: &str = "dev.layr.fshost.fs";

/// A fast line: a LayrFS (FSKit) mount whose commands take the folder paths.
pub fn create_fast(ctx: &Ctx, repo: &Repo, name: &str, start: &str, owner: u32) -> Result<Line> {
    create_inner(ctx, repo, name, start, owner, true)
}

/// `line move` of a fast line: the mount goes to the new folder (a mount cannot be renamed).
pub fn fast_move(repo: &Repo, line: &Line, from: &Path, to: &Path) -> Result<()> {
    hold_unmounted(line);
    // The caller holds the project lock (a line lock would have to come before it).
    std::fs::create_dir(to)?;
    std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o700))?;
    if is_mounted(from) {
        unmount(from)?;
    }
    if let Err(e) = mount(&dir(&repo.p, &line.id), to) {
        let _ = mount(&dir(&repo.p, &line.id), from);
        let _ = std::fs::remove_dir(to);
        return Err(e);
    }
    let _ = std::fs::remove_dir(from);
    Ok(())
}

/// The view of a fast line.
pub fn fast_of(repo: &Repo, line: &Line) -> Option<View> {
    of_line(&repo.p, &line.id).filter(|v| v.fast)
}

/// The fast lines that this command unmounts on purpose (delete, move): no remount.
static UNMOUNTING: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

pub(crate) fn hold_unmounted(line: &Line) {
    UNMOUNTING.lock().unwrap_or_else(|e| e.into_inner()).push(line.id.clone());
}

/// The file that tells other processes (commands, the user service) that a live process unmounted
/// a fast line on purpose (`delete`): they do not mount it again. It names that process.
fn unmounting_mark(p: &crate::store::Project, id: &str) -> PathBuf {
    dir(p, id).join("unmounting")
}

pub(crate) fn mark_unmounting(p: &crate::store::Project, id: &str) -> Result<()> {
    fsutil::atomic_write(&unmounting_mark(p, id), std::process::id().to_string().as_bytes(), 0o600)
}

pub(crate) fn unmark_unmounting(p: &crate::store::Project, id: &str) {
    let _ = std::fs::remove_file(unmounting_mark(p, id));
}

/// True when another live process holds the line unmounted (`mark_unmounting`).
fn unmounted_elsewhere(p: &crate::store::Project, id: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(unmounting_mark(p, id)) else { return false };
    text.trim().parse::<i32>().is_ok_and(|pid| {
        pid as u32 != std::process::id()
            // EPERM: a live process of another user.
            && rustix::process::Pid::from_raw(pid)
                .is_some_and(|p| !matches!(rustix::process::test_kill_process(p), Err(rustix::io::Errno::SRCH)))
    })
}

/// Mount a fast line of the caller again when its mount is gone (a restart of the machine, a
/// crash of the file system extension): its folder would read as empty. Only the owner mounts
/// (an FSKit mount belongs to a user), and only over an empty folder. The user service does it
/// for every line at its start; a command does it for the lines it reads (`Repo::working_src`).
pub fn ensure_fast_mounted(repo: &Repo, line: &Line) -> Result<()> {
    if line.machine != repo.p.store.machine.id
        || rustix::process::geteuid().as_raw() != line.owner
        || UNMOUNTING.lock().unwrap_or_else(|e| e.into_inner()).contains(&line.id)
        || unmounted_elsewhere(&repo.p, &line.id)
    {
        return Ok(());
    }
    if !fast_of(repo, line).is_some_and(|v| v.mounted) {
        return Ok(());
    }
    let at = repo.working(line);
    if is_mounted(&at) {
        return Ok(());
    }
    match std::fs::symlink_metadata(&at) {
        Ok(m) if m.is_dir() && std::fs::read_dir(&at)?.next().is_none() => {}
        // Files written into the folder while the mount was gone: status and save read the
        // shadow, so they would not see them.
        Ok(_) => anyhow::bail!(
            "the fast line {} is not mounted and its folder {} is not empty: move its content away, then run the command again",
            line.name,
            at.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(&at)?;
            std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o700))?;
        }
        Err(e) => return Err(e.into()),
    }
    let _t = crate::trace::span(format!("mount fast line {} again", line.name));
    mount(&dir(&repo.p, &line.id), &at)
}

/// `storage::snapshot` of a fast line's mount (None when `src` is not one): a copy of the line's
/// shadow (`vlines/<id>/shadow`, a clone of the lower tree), brought up to the view first: the
/// entries of the upper folder that changed since the last time are cloned in, the removed paths
/// go, and paths that left the upper folder come back from the lower tree.
/// The view folder and the view of the fast line whose mount is `src`.
fn fast_mount_view(src: &Path) -> Option<(PathBuf, View)> {
    if !is_mounted(src) {
        return None;
    }
    let st = rustix::fs::statfs(src).ok()?;
    // The mount itself, not a folder in it: a copy of `mount/sub` is not the whole line.
    let on: Vec<u8> = st.f_mntonname.iter().take_while(|b| **b != 0).map(|b| *b as u8).collect();
    let on = PathBuf::from(std::ffi::OsString::from(String::from_utf8_lossy(&on).into_owned()));
    if std::fs::canonicalize(src).ok()? != std::fs::canonicalize(&on).ok()? {
        return None;
    }
    let from: Vec<u8> = st.f_mntfromname.iter().take_while(|b| **b != 0).map(|b| *b as u8).collect();
    let vdir = mount_source_path(&from);
    let v = read_view(&vdir.join("view.json")).filter(|v| v.fast)?;
    Some((vdir, v))
}

/// The folder that an FSKit mount names as its source: a `file://` URL (`file:///a%20b/`), or a
/// plain path.
fn mount_source_path(from: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    let Some(rest) = from.strip_prefix(b"file://") else { return PathBuf::from(std::ffi::OsString::from_vec(from.to_vec())) };
    let mut out = Vec::with_capacity(rest.len());
    let mut i = 0;
    while i < rest.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (rest[i], rest.get(i + 1).copied().and_then(hex), rest.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    PathBuf::from(std::ffi::OsString::from_vec(out))
}

/// True when `src` is the mount of a fast line.
pub(crate) fn is_fast_mount(src: &Path) -> bool {
    fast_mount_view(src).is_some()
}

/// The folder of a fast line as the source of a copy (`crate::fs::source_of`): None when `src`
/// is not the mount of one.
pub(crate) fn fast_source(src: &Path) -> Option<Result<FastSource>> {
    let (vdir, v) = fast_mount_view(src)?;
    Some((|| {
        // The shadow lock stays for the command: the update and the copy are one step.
        lock_shadow(&vdir)?;
        let (paths, removed) = sync_shadow(&vdir, &v)?;
        Ok(FastSource { shadow: vdir.join("shadow"), paths, removed })
    })())
}

/// The folder of a fast line as the source of a copy (`crate::fs`): its shadow, brought up to
/// the view. The mount has no APFS clones, file ids or layr attributes.
pub(crate) struct FastSource {
    pub shadow: PathBuf,
    paths: BTreeSet<String>,
    removed: BTreeSet<String>,
}

impl FastSource {
    /// A copy of the shadow at `dst` differs from the start state at the same paths.
    pub(crate) fn finish(self, dst: &Path, _readonly: bool) -> Result<()> {
        if let Some(k) = known_of(&self.shadow) {
            add_known(dst, std::sync::Arc::new(Known { lower: k.lower.clone(), paths: self.paths, removed: self.removed }));
        }
        Ok(())
    }
}

/// The shadow of a fast line, brought up to its view (`Repo::working_src`). The paths where it
/// can differ from the view's start state are known (`known_of`), so a compare with an object
/// tree reads only them.
pub fn fast_shadow(p: &Project, v: &View) -> Result<PathBuf> {
    let vdir = dir(p, &v.line);
    lock_shadow(&vdir)?;
    let (paths, removed) = sync_shadow(&vdir, v)?;
    let shadow = vdir.join("shadow");
    if let Ok(Some((store, refs))) = crate::objstore::verified(p, &v.base) {
        if let Ok(lower) = crate::object_tree::ObjectTree::new(store, refs, &p.meta_dir().join("object-reads")) {
            add_known(&shadow, std::sync::Arc::new(Known { lower, paths, removed }));
        }
    }
    Ok(shadow)
}

/// The links to cache trees in the folder of fast line `line` (`fast_cache`): path and target.
/// They are entries of its upper folder.
pub fn cache_links(repo: &Repo, line: &Line) -> Result<Vec<(String, PathBuf)>> {
    cache_links_in(&upper(&repo.p, &line.id))
}

/// `cache_links` of the upper folder `up` of a fast line.
pub fn cache_links_in(up: &Path) -> Result<Vec<(String, PathBuf)>> {
    let up = up.to_path_buf();
    let mut out = Vec::new();
    fsutil::walk(up.as_path(), "", &mut |rel, m| {
        if system_path(rel) {
            return Ok(false);
        }
        if cache_link(&up, rel, m) {
            out.push((rel.to_string(), std::fs::read_link(up.join(rel))?));
        }
        Ok(true)
    })?;
    Ok(out)
}

/// The scan copy of a fast line (`Repo::scan`): a second tree that follows the shadow by its
/// known paths, so a command reads a still copy without a clone of the whole tree. One command
/// at a time holds it (a lock file); another one gets a full copy. `release_scan` frees it.
pub fn fast_scan(p: &Project, v: &View) -> Result<Option<PathBuf>> {
    let shadow = fast_shadow(p, v)?;
    let Some(k) = known_of(&shadow) else { return Ok(None) };
    let vdir = dir(p, &v.line);
    let scan = vdir.join("scan");
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(vdir.join("scan.lock"))?;
    if rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err() {
        return Ok(None);
    }
    {
        let mut held = SCANS.lock().unwrap_or_else(|e| e.into_inner());
        if held.iter().any(|(s, _)| s == &scan) {
            return Ok(None);
        }
        held.push((scan.clone(), lock));
    }
    let made = (|| -> Result<()> {
        let _t = crate::trace::span("fast line scan copy");
        let mark = vdir.join("scan.json");
        // The paths where the copy can differ from the start state since its last update.
        let old: Option<BTreeSet<String>> = std::fs::read(&mark).ok().and_then(|d| serde_json::from_slice(&d).ok());
        let old = match old {
            Some(o) if scan.is_dir() => o,
            _ => {
                let _ = std::fs::remove_file(&mark);
                if scan.symlink_metadata().is_ok() {
                    p.trash(&scan)?;
                }
                crate::fs::snapshot(&shadow, &scan, false)?;
                fsutil::atomic_write(&mark, &serde_json::to_vec(&k.paths)?, 0o600)?;
                return Ok(());
            }
        };
        let all: BTreeSet<&String> = old.iter().chain(k.paths.iter()).collect();
        // Removals first (children before parents), then the entries of the shadow, parents first.
        for rel in all.iter().rev() {
            if fsutil::lmeta(scan.as_path(), rel).is_some() {
                fsutil::remove_entry(&scan, rel)?;
            }
        }
        for rel in &all {
            let Some(m) = fsutil::lmeta(shadow.as_path(), rel) else { continue };
            if fsutil::lmeta(scan.as_path(), rel).is_some() {
                continue;
            }
            fsutil::copy_entry(shadow.as_path(), rel, &scan, rel, None)?;
            // A folder of the start state that came back: its content too.
            if m.is_dir() {
                fsutil::walk(shadow.as_path(), rel, &mut |sub, _| {
                    if fsutil::lmeta(scan.as_path(), sub).is_none() {
                        fsutil::copy_entry(shadow.as_path(), sub, &scan, sub, None)?;
                    }
                    Ok(true)
                })?;
            }
        }
        fsutil::atomic_write(&mark, &serde_json::to_vec(&k.paths)?, 0o600)?;
        Ok(())
    })();
    if let Err(e) = made {
        release_scan(&scan);
        let _ = std::fs::remove_file(vdir.join("scan.json"));
        return Err(e);
    }
    add_known(&scan, std::sync::Arc::new(Known { lower: k.lower.clone(), paths: k.paths.clone(), removed: k.removed.clone() }));
    Ok(Some(scan))
}

static SCANS: std::sync::Mutex<Vec<(PathBuf, std::fs::File)>> = std::sync::Mutex::new(Vec::new());

/// Frees the scan copy `path` of a fast line; false when `path` is no such copy (the caller
/// discards it).
pub fn release_scan(path: &Path) -> bool {
    let mut held = SCANS.lock().unwrap_or_else(|e| e.into_inner());
    let before = held.len();
    held.retain(|(s, _)| s != path);
    held.len() != before
}

/// The shadows that this command holds. A command reads the shadow of a fast line for its whole
/// run, so another command on the line waits for its end before it brings the shadow up to the view.
static SHADOW_LOCKS: std::sync::Mutex<Vec<(PathBuf, std::fs::File)>> = std::sync::Mutex::new(Vec::new());
static COMMAND_DEPTH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn lock_shadow(vdir: &Path) -> Result<()> {
    let mut held = SHADOW_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    if held.iter().any(|(d, _)| d == vdir) {
        return Ok(());
    }
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(vdir.join("shadow.lock"))?;
    let _t = crate::trace::span("fast line shadow lock");
    rustix::fs::flock(&f, rustix::fs::FlockOperation::LockExclusive)?;
    held.push((vdir.to_path_buf(), f));
    Ok(())
}

/// Free the shadow of the fast line `line_id` if this command holds it, and what it knew of it;
/// a later read locks and brings the shadow up to its view again. A command frees it before it
/// gives up a line lock that it took it under: the lock of a line comes before the lock of its
/// shadow, so a command that holds a shadow never waits for that line (`cmd::adopt_git`).
/// Copies of the shadow stay known: only layr writes them.
pub fn release_shadow(p: &Project, line_id: &str) {
    let vdir = dir(p, line_id);
    let shadow = vdir.join("shadow");
    KNOWN.lock().unwrap_or_else(|e| e.into_inner()).retain(|(k, _)| k != &shadow);
    MOUNT_SHADOWS.lock().unwrap_or_else(|e| e.into_inner()).retain(|(_, (s, _))| s != &shadow);
    SHADOW_LOCKS.lock().unwrap_or_else(|e| e.into_inner()).retain(|(d, _)| d != &vdir);
}

/// One command (`cmd::run`): at the end of the outermost one, its shadows and what it knew of
/// them are free.
pub struct CommandScope;

impl CommandScope {
    pub fn enter() -> Self {
        COMMAND_DEPTH.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        CommandScope
    }
}

impl Drop for CommandScope {
    fn drop(&mut self) {
        if COMMAND_DEPTH.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 1 {
            KNOWN.lock().unwrap_or_else(|e| e.into_inner()).clear();
            MOUNT_SHADOWS.lock().unwrap_or_else(|e| e.into_inner()).clear();
            UNMOUNTING.lock().unwrap_or_else(|e| e.into_inner()).clear();
            SHADOW_LOCKS.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
    }
}

/// A folder that differs from the object tree `lower` only at `paths` (and below them): the
/// shadow of a fast line and its copies. Only layr writes them.
pub struct Known {
    pub lower: std::sync::Arc<crate::object_tree::ObjectTree>,
    pub paths: BTreeSet<String>,
    /// The removed paths among `paths`: whatever the compared tree holds below them can differ.
    pub removed: BTreeSet<String>,
}

static KNOWN: std::sync::Mutex<Vec<(PathBuf, std::sync::Arc<Known>)>> = std::sync::Mutex::new(Vec::new());

fn add_known(path: &Path, k: std::sync::Arc<Known>) {
    let mut all = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    all.retain(|(p, _)| p != path);
    all.push((path.to_path_buf(), k));
}

/// A copy of the known folder `from` at `to` is known the same way.
pub fn copy_known(from: &Path, to: &Path) {
    if let Some(k) = known_of(from) {
        add_known(to, k);
    }
}

/// What is known of the folder `path` (`Known`), if it is a fast line's shadow or a copy of it.
pub fn known_of(path: &Path) -> Option<std::sync::Arc<Known>> {
    KNOWN.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(p, _)| p == path).map(|(_, k)| k.clone())
}

/// The shadow and the upper folder of each fast line that this command read
/// (`Repo::working_src`), by mount.
#[allow(clippy::type_complexity)]
static MOUNT_SHADOWS: std::sync::Mutex<Vec<(PathBuf, (PathBuf, PathBuf))>> = std::sync::Mutex::new(Vec::new());

pub fn note_mount_shadow(repo: &Repo, line: &Line, shadow: &Path) {
    let mount = repo.working(line);
    let mut all = MOUNT_SHADOWS.lock().unwrap_or_else(|e| e.into_inner());
    all.retain(|(m, _)| m != &mount);
    all.push((mount, (shadow.to_path_buf(), upper(&repo.p, &line.id))));
}

/// The shadow and the upper folder of the fast line mounted at `mount`, when this command read it.
/// The shadow has the entries of the mount but its cache links (`cache_link`); the upper folder
/// has those.
pub fn mount_shadow(mount: &Path) -> Option<(PathBuf, PathBuf)> {
    MOUNT_SHADOWS.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(m, _)| m == mount).map(|(_, s)| s.clone())
}

/// Brings the shadow up to the view; returns the paths where it can differ from the lower tree:
/// the entries of the upper folder and the removed paths.
fn sync_shadow(vdir: &Path, v: &View) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let _t = crate::trace::span("fast line shadow");
    let up = vdir.join("upper");
    let shadow = vdir.join("shadow");
    let mark = vdir.join("shadow.json");
    if shadow.symlink_metadata().is_err() {
        let _t = crate::trace::span("shadow clone");
        let _ = std::fs::remove_file(&mark);
        crate::fs::snapshot(&v.lower, &shadow, false)?;
    }
    // The upper entries of the last update: (size, mtime ns, ctime ns, mode) by path.
    let old: std::collections::BTreeMap<String, (u64, i64, i64, u32)> =
        std::fs::read(&mark).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default();
    let mut now = std::collections::BTreeMap::new();
    let removed = thin::removed_of(&up)?;
    fsutil::walk(up.as_path(), "", &mut |rel, m| {
        if system_path(rel) || cache_link(&up, rel, m) {
            return Ok(false);
        }
        let m = std::fs::symlink_metadata(up.join(rel))?;
        now.insert(rel.to_string(), (m.size(), m.mtime() * 1_000_000_000 + m.mtime_nsec(), m.ctime() * 1_000_000_000 + m.ctime_nsec(), m.mode()));
        Ok(true)
    })?;
    // Paths that left the upper folder (and are not removed): the lower tree's entry again.
    for p in old.keys().rev() {
        if !now.contains_key(p) && !removed.contains(p) {
            fsutil::remove_entry(&shadow, p)?;
            if fsutil::lmeta(&v.lower, p).is_some() {
                fsutil::copy_entry(&v.lower, p, &shadow, p, None)?;
            }
        }
    }
    for p in &removed {
        if !now.contains_key(p) {
            fsutil::remove_entry(&shadow, p)?;
        }
    }
    for (p, meta) in &now {
        if old.get(p) != Some(meta) {
            let is_dir = std::fs::symlink_metadata(up.join(p)).is_ok_and(|m| m.is_dir());
            if is_dir {
                if std::fs::symlink_metadata(shadow.join(p)).is_ok_and(|m| !m.is_dir()) {
                    fsutil::remove_entry(&shadow, p)?;
                }
                fsutil::copy_entry(&up, p, &shadow, p, None)?;
            } else {
                fsutil::remove_entry(&shadow, p)?;
                fsutil::copy_entry(&up, p, &shadow, p, None)?;
            }
            // The mount hides the quarantine mark that the extension's sandbox puts on its files:
            // the shadow's copy (whose mark the system rewrote) goes too.
            const Q: &std::ffi::CStr = c"com.apple.quarantine";
            if let Ok(v) = crate::sys::apfs_attribute(&up.join(p), Q, None) {
                if String::from_utf8_lossy(&v).contains(";LayrFS;") {
                    let _ = crate::sys::apfs_remove_attribute(&shadow.join(p), Q);
                }
            }
        }
    }
    // A removed folder that the upper folder made again hides the lower one: the shadow keeps
    // only the upper entries below it.
    for p in removed.iter().filter(|p| now.contains_key(*p)) {
        // A file made again has nothing below it.
        if !fsutil::lmeta(shadow.as_path(), p).is_some_and(|m| m.is_dir()) {
            continue;
        }
        let mut gone = Vec::new();
        fsutil::walk(shadow.as_path(), p, &mut |rel, _| {
            if now.contains_key(rel) {
                return Ok(true);
            }
            gone.push(rel.to_string());
            Ok(false)
        })?;
        for rel in gone {
            fsutil::remove_entry(&shadow, &rel)?;
        }
    }
    fsutil::atomic_write(&mark, &serde_json::to_vec(&now)?, 0o600)?;
    let paths: BTreeSet<String> = now.into_keys().chain(removed.iter().cloned()).collect();
    Ok((paths, removed))
}

pub fn enabled(ctx: &Ctx) -> bool {
    ctx.env(ENV) == Some("1")
}

pub(crate) fn require_enabled(ctx: &Ctx) -> Result<()> {
    if !enabled(ctx) {
        return Err(error(ErrorKind::Unsupported, format!("virtual lines are experimental: set {ENV}=1 (docs/views.md)")));
    }
    Ok(())
}

fn views_dir(meta: &Path) -> PathBuf {
    meta.join("vlines")
}

pub(crate) fn dir(p: &Project, line_id: &str) -> PathBuf {
    views_dir(&p.meta_dir()).join(line_id)
}

/// The upper folder of a virtual line.
pub fn upper(p: &Project, line_id: &str) -> PathBuf {
    dir(p, line_id).join("upper")
}

fn read_view(file: &Path) -> Option<View> {
    std::fs::read(file).ok().and_then(|d| serde_json::from_slice(&d).ok())
}

/// The view of a line, when it is a virtual line.
pub fn of_line(p: &Project, line_id: &str) -> Option<View> {
    read_view(&dir(p, line_id).join("view.json"))
}

/// Every virtual line of a project.
pub fn all_in(meta: &Path) -> Vec<View> {
    let Ok(rd) = std::fs::read_dir(views_dir(meta)) else { return Vec::new() };
    rd.flatten().filter_map(|e| read_view(&e.path().join("view.json"))).collect()
}

/// The native trees of fast line starts stay a day after their last use (`LAYR_FAST_SEED_HOURS`):
/// a new fast line from the same state then mounts at once, with no build from the objects.
fn seeds_dir(p: &Project) -> PathBuf {
    p.meta_dir().join("fast-seeds")
}

fn note_seed(p: &Project, state: &str) {
    let dir = seeds_dir(p);
    let _ = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join(state), b""));
}

/// The fast line starts whose native trees stay (`objstore::retire_native`); older marks go.
pub fn kept_seeds(p: &Project) -> BTreeSet<String> {
    let hours = std::env::var("LAYR_FAST_SEED_HOURS").ok().and_then(|h| h.parse::<u64>().ok()).unwrap_or(24);
    let ttl = std::time::Duration::from_secs(hours * 3600);
    let mut out = BTreeSet::new();
    let Ok(rd) = std::fs::read_dir(seeds_dir(p)) else { return out };
    for e in rd.flatten() {
        let fresh = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|d| d < ttl);
        if fresh {
            out.insert(e.file_name().to_string_lossy().into_owned());
        } else {
            let _ = std::fs::remove_file(e.path());
        }
    }
    out
}

/// The base states of the virtual lines of a project: their trees must stay (`thin` upkeep).
pub fn protected_states(p: &Project) -> BTreeSet<String> {
    all_in(&p.meta_dir()).into_iter().map(|v| v.base).collect()
}

pub(crate) fn write_view(p: &Project, v: &View) -> Result<()> {
    fsutil::atomic_write(&dir(p, &v.line).join("view.json"), &serde_json::to_vec_pretty(v)?, 0o600)
}

pub fn is_mounted(path: &Path) -> bool {
    crate::sys::fs_type_name(path).map(|t| t == FS_TYPE).unwrap_or(false)
}

pub(crate) fn mount(vdir: &Path, at: &Path) -> Result<()> {
    let _t = crate::trace::span("view mount");
    let out = Command::new("/sbin/mount")
        .args(["-F", "-t", FS_TYPE, "-o", "nobrowse"])
        .arg(vdir)
        .arg(at)
        .output()
        .context("run mount")?;
    if !out.status.success() || !is_mounted(at) {
        bail!(
            "cannot mount the virtual line at {}: {}\n(is the LayrFS file system extension installed and enabled? see tools/layrfs/README.md)",
            at.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

pub(crate) fn unmount(at: &Path) -> Result<()> {
    let _t = crate::trace::span("view unmount");
    // A new mount can be busy for a moment (the system looks at a new volume): try again.
    let mut out = Command::new("/sbin/umount").arg(at).output().context("run umount")?;
    for _ in 0..20 {
        if out.status.success() || !is_mounted(at) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        out = Command::new("/sbin/umount").arg(at).output().context("run umount")?;
    }
    if !out.status.success() && is_mounted(at) {
        bail!(
            "cannot unmount {}: {}\n(close the programs and shells that use the folder, then run the command again)",
            at.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// The settings file of the mount. `openclose=off`: FSKit sends no open and close calls (the
/// fastest reads measured, docs/views.md).
fn write_conf(vdir: &Path, lower: &Path, ctx: &Ctx) -> Result<()> {
    let mut conf = format!("# LayrFS overlay of a virtual line (docs/views.md)\nlower={}\nopenclose=off\n", lower.display());
    if ctx.env("LAYR_VIEW_PREFETCH") == Some("1") {
        conf.push_str("prefetch=1\n");
    }
    fsutil::atomic_write(&vdir.join(CONF), conf.as_bytes(), 0o600)
}

/// Serialize mount lifecycle changes with commands that need a working mount.
pub(crate) fn lifecycle_lock(repo: &Repo, line: &Line) -> Result<crate::store::LineLeaseSet> {
    repo.p.line_lock(&format!("view-{}", line.id), &line.name)
}

/// A hidden line gets a temporary mount for an in-place update. Its saved view stays hidden.
/// The lifecycle lock prevents show or delete from unmounting it during the command.
pub fn with_working<T>(ctx: &Ctx, repo: &Repo, line: &Line, run: impl FnOnce() -> Result<T>) -> Result<T> {
    if of(repo, line).is_none() {
        return run();
    }
    let _lifecycle = lifecycle_lock(repo, line)?;
    let hidden = {
        let _line = repo.line_lock(line)?;
        let view = of(repo, line).context("virtual line changed before pull")?;
        let at = repo.working(line);
        if !is_mounted(&at) {
            repo.authorize_line(ctx, line, crate::repo::LineOp::Manage)?;
            match std::fs::symlink_metadata(&at) {
                Ok(meta) => anyhow::ensure!(
                    meta.is_dir() && std::fs::read_dir(&at)?.next().is_none(),
                    "the virtual line mount path is not an empty directory: {}",
                    at.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir(&at)?;
                    std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o700))?;
                }
                Err(error) => return Err(error.into()),
            }
            mount(&dir(&repo.p, &line.id), &at)?;
        }
        !view.mounted
    };
    let result = run();
    if hidden {
        let _line = repo.line_lock(line)?;
        let at = repo.working(line);
        unmount(&at).context("the command finished, but its temporary view could not be hidden")?;
        std::fs::remove_dir(&at)?;
    }
    result
}

/// Make a virtual line `name` at `start`: the line record, its upper folder, and the mount at the
/// line's folder. The lower tree is the complete tree of `start`; a thin state gets one once.
pub fn create(ctx: &Ctx, repo: &Repo, name: &str, start: &str, owner: u32) -> Result<Line> {
    require_enabled(ctx)?;
    create_inner(ctx, repo, name, start, owner, false)
}

fn create_inner(ctx: &Ctx, repo: &Repo, name: &str, start: &str, owner: u32, fast: bool) -> Result<Line> {
    if owner != ctx.caller.uid {
        bail!("a virtual line belongs to the user who makes it (no --owner)");
    }
    if repo.p.is_overlay() {
        bail!("an overlay project has no virtual lines");
    }
    crate::store::check_name("line", name)?;
    if repo.p.db.line_by_name(name)?.is_some() {
        bail!("a line named '{name}' already exists");
    }
    let at = repo.p.line_path(name);
    // A folder that `line rename` kept for another line counts as taken, also when it is gone.
    if at.symlink_metadata().is_ok() || repo.p.kept_folder_line(name).is_some() {
        bail!("{} already exists", at.display());
    }
    repo.source_state(start)?;
    let lower = {
        let _t = crate::trace::span("view lower tree");
        repo.state_root(start)?
    };
    if fast {
        note_seed(&repo.p, start);
    }
    let line = Line {
        id: ids::new_id(),
        name: name.to_string(),
        head: start.to_string(),
        owner,
        machine: repo.p.store.machine.id.clone(),
    };
    let vdir = dir(&repo.p, &line.id);
    let up = vdir.join("upper");
    // Set once this command made the folder: another command can make the same name at once,
    // and a cleanup must not unmount or remove its folder.
    let made_at = std::cell::Cell::new(false);
    let made = (|| -> Result<()> {
        std::fs::create_dir_all(&up)?;
        std::fs::set_permissions(&vdir, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&up, std::fs::Permissions::from_mode(0o700))?;
        // LayrFS names entries by the inode numbers of both trees: one volume.
        if std::fs::metadata(&lower)?.dev() != std::fs::metadata(&up)?.dev() {
            bail!("the base tree {} and the project folder are on different volumes", lower.display());
        }
        write_conf(&vdir, &lower, ctx)?;
        std::fs::create_dir(&at)?;
        made_at.set(true);
        std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o700))?;
        mount(&vdir, &at)
    })();
    let cleanup = || {
        if made_at.get() {
            if is_mounted(&at) {
                let _ = unmount(&at);
            }
            let _ = std::fs::remove_dir(&at);
        }
        let _ = std::fs::remove_dir_all(&vdir);
    };
    if let Err(e) = made {
        cleanup();
        return Err(e);
    }
    let recorded = (|| -> Result<()> {
        let _lock = repo.p.lock_maintenance()?;
        if repo.p.db.line_by_name(name)?.is_some() {
            bail!("a line named '{name}' already exists");
        }
        repo.source_state(start)?;
        let mut rec = crate::model::DraftRecord::new("line.create");
        rec.data = serde_json::json!({"name": name, "start": start});
        rec.lines.insert(
            line.id.clone(),
            LineChange {
                transition: crate::model::LineTransition::Create { after: repo.line_state(&line, start) },
                working_before: None,
                working_after: None,
            },
        );
        _lock.append(rec, &ctx.actor())?;
        write_view(
            &repo.p,
            &View { line: line.id.clone(), name: name.to_string(), base: start.to_string(), lower, mounted: true, fast },
        )
    })();
    if let Err(e) = recorded {
        if repo.p.db.line_by_id(&line.id)?.is_none() {
            cleanup();
        }
        return Err(e);
    }
    Ok(line)
}

/// Removes the quarantine mark that the sandbox of the LayrFS extension puts on the files it
/// makes. The mount hides it; a saved state must not keep it.
pub(crate) fn strip_quarantine(up: &Path, paths: &BTreeSet<String>) {
    const Q: &std::ffi::CStr = c"com.apple.quarantine";
    for p in paths {
        let path = up.join(p);
        if let Ok(v) = crate::sys::apfs_attribute(&path, Q, None) {
            if String::from_utf8_lossy(&v).contains(";LayrFS;") {
                let _ = crate::sys::apfs_remove_attribute(&path, Q);
            }
        }
    }
}

/// The trees of a virtual line: the merged working content and the head.
pub struct Trees {
    pub upper: PathBuf,
    pub merged: Layered,
    pub head: Layered,
}

pub(crate) fn trees(repo: &Repo, line: &Line, v: &View) -> Result<Trees> {
    let up = upper(&repo.p, &line.id);
    let merged = Layered::over(&up, Layered::complete(&v.lower))?;
    Ok(Trees { upper: up, merged, head: repo.state_src(&line.head)?.into_layered() })
}

/// A link of a fast line to one of its cache trees (`fast_cache`): no content of the line.
/// `root` is the upper folder of a line, `vlines/<line id>/upper`.
fn cache_link(root: &Path, rel: &str, m: &Meta) -> bool {
    root.parent()
        .and_then(crate::fast_cache::root_of_view)
        .is_some_and(|caches| crate::fast_cache::is_cache_link(root, rel, m, &caches))
}

fn system_path(p: &str) -> bool {
    let top = p.split('/').next().unwrap_or("");
    SYSTEM_ROOT_NAMES.contains(&top)
}

/// The paths that can differ between the head and the merged view: the entries of the upper
/// folder, its removed paths, and what the saves of this line changed over the lower tree (the
/// layers of the head above it). None when the head no longer rests on the lower tree (a gc
/// keyframe): then a full compare decides.
fn candidates(t: &Trees, v: &View, rules: &crate::ignore_rules::Rules) -> Result<Option<BTreeSet<String>>> {
    let _s = crate::trace::span("view candidates");
    if t.head.bottom() != v.lower.as_path() {
        return Ok(None);
    }
    let mut set = BTreeSet::new();
    let head = &t.head;
    fsutil::walk(t.upper.as_path(), "", &mut |rel, m| {
        if skip_path(rel) || system_path(rel) || cache_link(&t.upper, rel, m) {
            return Ok(false);
        }
        // A new ignored folder (build output) is no content; its files are not listed.
        if m.is_dir() && rules.ignored(rel, true) && fsutil::lmeta(head, rel).is_none() {
            return Ok(false);
        }
        set.insert(rel.to_string());
        Ok(true)
    })?;
    let lower = Layered::complete(&v.lower);
    let mut removed: BTreeSet<String> = thin::removed_of(&t.upper)?;
    for l in &head.layers()[..head.layers().len() - 1] {
        fsutil::walk(l.root.as_path(), "", &mut |rel, _| {
            set.insert(rel.to_string());
            Ok(true)
        })?;
        removed.extend(l.removed.iter().cloned());
    }
    for r in removed {
        for tree in [head, &lower] {
            set.extend(tree.scan_at(&r)?.into_keys().filter(|k| !k.is_empty()));
        }
        set.insert(r);
    }
    Ok(Some(set))
}

/// The confirmed file changes of the merged view against the head, without ignored new files.
pub(crate) fn file_changes(t: &Trees, v: &View) -> Result<Vec<EntryChange>> {
    let rules = crate::ignore_rules::Rules::of(&crate::source::Source::from(t.merged.clone()));
    let list = match candidates(t, v, &rules)? {
        Some(set) => {
            strip_quarantine(&t.upper, &set);
            let raw: Vec<EntryChange> = set
                .into_iter()
                .filter_map(|p| {
                    let (old, new) = (fsutil::lmeta(&t.head, &p), fsutil::lmeta(&t.merged, &p));
                    EntryChange::observed(p, old, new, crate::compare::DataEvidence::Unknown)
                })
                .collect();
            changes::file_changes_of(&t.head, &t.merged, raw)?
        }
        None => {
            // Object heads use another root path, so the native-layer shortcut cannot apply.
            // Normalize the extension's marker on upper entries before the full comparison too.
            let mut paths = BTreeSet::new();
            fsutil::walk(t.upper.as_path(), "", &mut |path, meta| {
                if skip_path(path) || system_path(path) || cache_link(&t.upper, path, meta) {
                    return Ok(false);
                }
                if meta.is_dir() && rules.ignored(path, true) && fsutil::lmeta(&t.head, path).is_none() {
                    return Ok(false);
                }
                paths.insert(path.to_string());
                Ok(true)
            })?;
            strip_quarantine(&t.upper, &paths);
            changes::file_changes(&t.head, &t.merged)?
        }
    };
    Ok(list
        .into_iter()
        .filter(|c| !skip_path(c.path()) && !system_path(c.path()))
        .filter(|c| c.old().is_some() || !rules.ignored(c.path(), false))
        .filter(|c| !c.new().is_some_and(|m| cache_link(&t.upper, c.path(), m)))
        .collect())
}

/// `layr status` of a virtual line: every change is saved by the next `save -m`, so all are staged.
pub fn status(repo: &Repo, line: &Line, v: &View) -> Result<Status> {
    let t = trees(repo, line, v)?;
    let staged = changes::with_renames(&t.head, &t.merged, file_changes(&t, v)?);
    let mut conflicts = Vec::new();
    for conflict in repo.state(&line.head)?.conflicts {
        if crate::repo::conflict_open(&t.merged, &conflict)? {
            conflicts.push(conflict);
        }
    }
    Ok(Status { staged, conflicts, ..Default::default() })
}

/// Merge and replay treat unsaved view changes as local edits, not an explicit index.
pub fn working_status(repo: &Repo, line: &Line, v: &View) -> Result<Status> {
    let mut result = status(repo, line, v)?;
    result.unstaged = std::mem::take(&mut result.staged);
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn working_snapshot_at(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    v: &View,
    parents: Vec<String>,
    message: &str,
    base: &str,
    id: String,
    complete: &Path,
    delta: &Path,
) -> Result<StateRec> {
    let mut prior = line.clone();
    prior.head = base.to_string();
    let t = trees(repo, &prior, v)?;
    let list = file_changes(&t, v)?;
    crate::secrets::check_changes(ctx, repo, &t.head.clone().into(), &t.merged.clone().into(), &prior.head, &list)?;
    let mut state = write_state_at(ctx, repo, &prior, &t, &list, StateKind::Auto, message, id, complete, delta)?;
    state.parents = parents;
    Ok(state)
}

/// The trees and changes for `layr diff` of a virtual line against its head.
pub fn diff_input(
    repo: &Repo,
    line: &Line,
    v: &View,
    _keep: &mut Vec<crate::repo::Temp>,
) -> Result<(crate::repo::Snap, crate::repo::Snap, Vec<EntryChange>)> {
    let t = trees(repo, line, v)?;
    let list = file_changes(&t, v)?;
    Ok((t.head.into(), t.merged.into(), list))
}

fn topmost(paths: BTreeSet<String>) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for p in paths {
        let covered = p.match_indices('/').any(|(i, _)| out.contains(&p[..i]));
        if !covered {
            out.insert(p);
        }
    }
    out
}

/// A new state of the merged view: a thin layer over the head with the changed entries of the
/// upper folder. None when nothing changed. The record is the caller's.
pub(crate) fn write_state(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    t: &Trees,
    list: &[EntryChange],
    kind: StateKind,
    message: &str,
) -> Result<StateRec> {
    let id = ids::new_id();
    let st =
        write_state_at(ctx, repo, line, t, list, kind, message, id.clone(), &repo.p.state_path(&id), &repo.p.delta_path(&id))?;
    if repo.p.delta_path(&id).exists() {
        repo.p.lock_or_borrow()?.write(|tx| tx.record_base(&id, &line.head))?;
    }
    Ok(st)
}

#[allow(clippy::too_many_arguments)]
fn write_state_at(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    t: &Trees,
    list: &[EntryChange],
    kind: StateKind,
    message: &str,
    id: String,
    complete_path: &Path,
    delta: &Path,
) -> Result<StateRec> {
    let _s = crate::trace::span("view write state");
    let is_dir = |m: &Option<Meta>| m.as_ref().is_some_and(|m| m.is_dir());
    let mut d = thin::Diff::default();
    let mut gone = BTreeSet::new();
    for c in list {
        if c.new().is_some() {
            d.write.insert(c.path().clone());
        }
        if c.old().is_some() && (c.new().is_none() || c.old().map(|m| m.ftype) != c.new().map(|m| m.ftype)) {
            gone.insert(c.path().clone());
        }
        if let Some(o) = c.old_path() {
            gone.insert(o.clone());
        }
    }
    // Folders: a removed path that the head has as a folder goes as a whole, as on a native save;
    // a new folder of the upper folder is written, also when it is empty.
    for r in thin::removed_of(&t.upper)? {
        if is_dir(&fsutil::lmeta(&t.head, &r)) && !is_dir(&fsutil::lmeta(&t.merged, &r)) {
            gone.insert(r);
        }
    }
    let rules = crate::ignore_rules::Rules::of(&crate::source::Source::from(t.merged.clone()));
    fsutil::walk(t.upper.as_path(), "", &mut |rel, m| {
        if !m.is_dir() || skip_path(rel) || system_path(rel) || rules.ignored(rel, true) {
            return Ok(false);
        }
        if !is_dir(&fsutil::lmeta(&t.head, rel)) {
            d.write.insert(rel.to_string());
        }
        Ok(true)
    })?;
    // A path below a removed folder of the head that comes back is written; the folder is new.
    d.removed = topmost(gone);
    let dst = delta.to_path_buf();
    // A reverted entry can live only in the lower tree. Hard links also need a complete
    // snapshot so their identity survives. Ordinary edits still write only the upper layer.
    let missing_upper = d.write.iter().any(|p| std::fs::symlink_metadata(t.upper.join(p)).is_err());
    let complete = if missing_upper || thin::any_linked(&t.upper, &d.write) {
        true
    } else {
        match thin::write_layer(&t.upper, &dst, &d, &line.head, false, None) {
            Ok(()) => false,
            Err(e) if thin::needs_complete(&e) => true,
            Err(e) => return Err(e),
        }
    };
    if complete {
        let path = complete_path.to_path_buf();
        let made = (|| -> Result<()> {
            t.merged.materialize(&path, false)?;
            // A complete fallback obeys the same exclusions as the thin write. In particular,
            // ignored build output and untracked secrets must not enter a named state.
            let mut excluded = Vec::new();
            fsutil::walk(path.as_path(), "", &mut |rel, meta| {
                if skip_path(rel)
                    || system_path(rel)
                    || (rules.ignored(rel, meta.is_dir()) && fsutil::lmeta(&t.head, rel).is_none())
                {
                    excluded.push(rel.to_string());
                    return Ok(false);
                }
                Ok(true)
            })?;
            for rel in excluded {
                crate::fs::remove(&path.join(rel))?;
            }
            crate::apfs::write_info(
                &path,
                &crate::apfs::SubvolInfo {
                    uuid: ids::new_id(),
                    received_uuid: None,
                    parent_uuid: None,
                    readonly: true,
                    base: None,
                },
            )?;
            crate::apfs::seal(&path, true, false)
        })();
        if let Err(error) = made {
            let _ = crate::fs::remove(&path);
            return Err(error);
        }
    }
    let path = if complete { complete_path } else { delta };
    Ok(repo.state_record(
        ctx,
        id,
        kind,
        vec![line.head.clone()],
        message,
        Some(line),
        crate::fs::host().namespace.info(path)?.uuid,
    ))
}

/// Refuse a command that does not support virtual lines yet, when it runs on one.
pub fn guard(ctx: &Ctx, cmd: &str) -> Result<()> {
    const SUPPORTED: &[&str] = &[
        "help",
        "--help",
        "-h",
        "version",
        "--version",
        "status",
        "diff",
        "save",
        "line",
        "log",
        "show",
        "rev-parse",
        "merge-base",
        "projects",
        "gc",
        "du",
        "config",
        "usage",
        "init",
        "git",
    ];
    if SUPPORTED.contains(&cmd) {
        return Ok(());
    }
    let Ok(loc) = ctx.locate() else { return Ok(()) };
    let Some(name) = loc.line else { return Ok(()) };
    let Ok(pdir) = ctx.store.project_dir(&loc.project) else { return Ok(()) };
    if all_in(&pdir.join("meta")).iter().any(|v| v.name == name && !v.fast) {
        return Err(error(
            ErrorKind::Failed,
            format!(
                "layr {cmd} does not support virtual lines yet ({ENV}); convert the line first: layr line show {name} --folder"
            ),
        ));
    }
    Ok(())
}

/// The view of `line` when it is virtual, for commands that branch on it. A fast line counts as
/// a folder line here (`fast_of` names it).
pub fn of(repo: &Repo, line: &Line) -> Option<View> {
    of_line(&repo.p, &line.id).filter(|v| !v.fast)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_fskit_mount_source_is_a_file_url() {
        assert_eq!(mount_source_path(b"file:///store/meta/vlines/a%20b/"), PathBuf::from("/store/meta/vlines/a b/"));
        assert_eq!(mount_source_path(b"/store/meta/vlines/x"), PathBuf::from("/store/meta/vlines/x"));
    }

    #[test]
    fn object_head_does_not_report_the_fskit_quarantine_marker_as_a_change() -> Result<()> {
        let tmp = std::env::temp_dir().join(format!("layr-view-quarantine-{}", crate::ids::new_id()));
        let lower = tmp.join("lower");
        let upper = tmp.join("upper");
        std::fs::create_dir_all(lower.join("added"))?;
        std::fs::create_dir_all(upper.join("added"))?;
        let store = layr_objects::Store::open(&tmp.join("objects"), layr_objects::Keying::Plain)?;
        let written = layr_objects::write_tree(
            &store,
            &lower,
            &layr_objects::WriteOptions {
                temperature: layr_objects::Temperature::Hot,
                previous: None,
                exclude: Vec::new(),
                similar: false,
            },
        )?;
        let head = crate::object_tree::ObjectTree::new(
            store,
            layr_objects::StateRef { tree: written.tree.unwrap(), times: written.times.unwrap() },
            &tmp.join("reads"),
        )?;
        crate::sys::apfs_attribute(&upper.join("added"), c"com.apple.quarantine", Some(b"0086;00000000;LayrFS;"))?;
        let trees = Trees {
            upper: upper.clone(),
            head: Layered::objects(head),
            merged: Layered::over(&upper, Layered::complete(&lower))?,
        };
        let view = View { line: crate::ids::new_id(), name: "virtual".into(), base: crate::ids::new_id(), lower, mounted: false, fast: false };
        let changes = file_changes(&trees, &view)?;
        assert!(changes.is_empty(), "unexpected changes: {changes:?}");
        crate::sys::apfs_attribute(&upper.join("added"), c"com.apple.quarantine", Some(b"0086;00000000;OtherAgent;"))?;
        let real_changes = file_changes(&trees, &view)?;
        assert_eq!(real_changes.len(), 1);
        assert_eq!(real_changes[0].path(), "added");
        drop(trees);
        std::fs::remove_dir_all(tmp)?;
        Ok(())
    }

    #[test]
    fn topmost_keeps_only_the_highest_removed_paths() {
        let set: BTreeSet<String> = ["a", "a/b", "a/b/c", "ab", "d/e", "d/e/f", "g/h"].iter().map(|s| s.to_string()).collect();
        let want: BTreeSet<String> = ["a", "ab", "d/e", "g/h"].iter().map(|s| s.to_string()).collect();
        assert_eq!(topmost(set), want);
    }

    #[test]
    fn system_folders_are_only_root_names() {
        assert!(system_path(".fseventsd"));
        assert!(system_path(".fseventsd/fseventsd-uuid"));
        assert!(!system_path("src/.fseventsd"));
        assert!(!system_path("notes.txt"));
    }
}
