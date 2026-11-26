//! The remote bridge to git, and the `.git` folder of each line for tools that call git.
//!
//! The project git object store is `<project>/git` (a bare repository, readable by the readers).
//! Import maps the full Git DAG to states: each commit is a record with its exact commit/tree
//! objects (encrypted records). The tree of a state is built from git when it is read (`build_tree`).
//! Export restores imported objects; for new states it builds a git tree
//! from the tree of the previous mapped commit plus the changed paths, makes one commit with
//! fixed author and committer data, records the exact commit id, then pushes it. After a lost
//! response, a repeated push first reads the remote ref.

use crate::changes;
use crate::ctx::Ctx;
use crate::fsutil;
use crate::ids;
use crate::model::{Line, StateKind, StateRec};
use crate::repo::{skip_path, Repo};
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn store(repo: &Repo) -> PathBuf {
    let s = repo.p.git_dir();
    crate::gitpack::serve_if_used(&repo.p, &s);
    s
}

pub fn has_store(repo: &Repo) -> bool {
    store(repo).join("objects").is_dir()
}

#[cfg(test)]
thread_local! {
    /// The git commands this thread made through `base_cmd`, for tests that count them.
    static GIT_COMMANDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn base_cmd(git_dir: &Path) -> Command {
    #[cfg(test)]
    GIT_COMMANDS.with(|n| n.set(n.get() + 1));
    let mut c = Command::new("git");
    c.arg("-c").arg(format!("safe.directory={}", git_dir.display()));
    c.arg("-c").arg("safe.directory=*");
    c.env("GIT_DIR", git_dir);
    c.env_remove("GIT_WORK_TREE");
    c.env_remove("GIT_INDEX_FILE");
    c.env("GIT_TERMINAL_PROMPT", "0");
    c
}

pub fn git_out(git_dir: &Path, args: &[&str]) -> Result<String> {
    let out = base_cmd(git_dir).args(args).stdin(Stdio::null()).output().context("run git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Who can read the git object store: the objects are the project history too. When everyone
/// may read the project the store is 0755; otherwise 0750 for root, plus an access list (POSIX
/// ACL) with read access for the users and groups with a role of reader or more, and the owners
/// of lines on this machine. Only the store folder decides: everything below it is readable by
/// whoever enters it (`open_content`).
pub fn sync_store_access(repo: &Repo) -> Result<()> {
    sync_store_access_with(repo, &[])
}

/// `sync_store_access` that also admits `extra_owners`: the owners of lines that are about to be
/// recorded. A command calls it before its store lock, so the lines it will create are not in
/// `repo.lines()` yet.
pub fn sync_store_access_with(repo: &Repo, extra_owners: &[u32]) -> Result<()> {
    let s = store(repo);
    if !s.join("objects").is_dir() {
        return Ok(());
    }
    open_content(&s)?;
    let (everyone, users, groups) = repo.policy().readers();
    if everyone {
        std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o755))?;
        let _ = remove_acl(&s);
        return Ok(());
    }
    let mut uids: std::collections::BTreeSet<u32> = users.into_iter().collect();
    uids.extend(extra_owners.iter().copied());
    for l in repo.lines()? {
        if l.machine == repo.p.store.machine.id {
            uids.insert(l.owner);
        }
    }
    uids.remove(&0);
    let gids: std::collections::BTreeSet<u32> = groups.into_iter().collect();
    std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o750))?;
    set_acl(&s, &uids.into_iter().collect::<Vec<_>>(), &gids.into_iter().collect::<Vec<_>>())
}

/// `core.sharedRepository` of the store: git makes its files 0644 (objects 0444) and its folders
/// 0755, whatever the umask of the service (0077 in a systemd unit).
const SHARED: &str = "0644";

/// Make everything below the store folder readable for those who may enter the folder: the
/// readers use the objects through the `alternates` of their lines. git keeps new files readable
/// (`core.sharedRepository`); a store without that setting, written under the umask of the
/// service by an earlier layr, gets read access for its folders and files once.
fn open_content(s: &Path) -> Result<()> {
    if git_out(s, &["config", "--get", "core.sharedRepository"]).map(|v| v.trim() == SHARED).unwrap_or(false) {
        return Ok(());
    }
    let mut modes = Vec::new();
    fsutil::walk(s, "", &mut |rel, m| {
        let add = match m.ftype {
            fsutil::FType::Dir => 0o055,
            fsutil::FType::File => 0o044,
            _ => 0,
        };
        if m.mode & add != add {
            modes.push((rel.to_string(), m.mode | add));
        }
        Ok(true)
    })?;
    for (rel, mode) in modes {
        std::fs::set_permissions(s.join(rel), std::fs::Permissions::from_mode(mode))?;
    }
    git_out(s, &["config", "core.sharedRepository", SHARED])?;
    Ok(())
}

fn acl_entry(buf: &mut Vec<u8>, tag: u16, perm: u16, id: u32) {
    buf.extend_from_slice(&tag.to_le_bytes());
    buf.extend_from_slice(&perm.to_le_bytes());
    buf.extend_from_slice(&id.to_le_bytes());
}

fn set_acl(path: &Path, users: &[u32], groups: &[u32]) -> Result<()> {
    const UNDEFINED: u32 = u32::MAX;
    let mut buf = 2u32.to_le_bytes().to_vec();
    acl_entry(&mut buf, 0x01, 7, UNDEFINED); // owner rwx
    for u in users {
        acl_entry(&mut buf, 0x02, 5, *u); // listed users r-x
    }
    acl_entry(&mut buf, 0x04, 5, UNDEFINED); // group r-x
    for g in groups {
        acl_entry(&mut buf, 0x08, 5, *g); // listed groups r-x
    }
    acl_entry(&mut buf, 0x10, 5, UNDEFINED); // mask r-x
    acl_entry(&mut buf, 0x20, 0, UNDEFINED); // others none
    rustix::fs::setxattr(path, "system.posix_acl_access", &buf, rustix::fs::XattrFlags::empty())
        .map_err(|e| anyhow!("set access list on {}: {e}", path.display()))?;
    Ok(())
}

fn remove_acl(path: &Path) -> Result<()> {
    let _ = rustix::fs::removexattr(path, "system.posix_acl_access");
    Ok(())
}

pub fn init_store(repo: &Repo) -> Result<PathBuf> {
    let s = store(repo);
    if !has_store(repo) {
        let shared = format!("--shared={SHARED}");
        let out = Command::new("git").args(["init", "--bare", "-q", "-b", "main", &shared]).arg(&s).output()?;
        if !out.status.success() {
            bail!("git init failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        git_out(&s, &["config", "core.logAllRefUpdates", "false"])?;
        git_out(&s, &["config", "gc.auto", "0"])?;
        std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o750))?;
        sync_store_access(repo)?;
    }
    std::fs::create_dir_all(s.join("layr-index"))?;
    Ok(s)
}

// ----- shallow history -----
//
// A shallow clone has commits whose parents it does not have. Git names them in the file
// `shallow` of a repository and reads each one as a commit without parents. The git store of a
// project keeps such a file when a fetch brought a history that ends this way (`init --from-git`
// of a shallow clone); the temporary repository of a fetch and the `.git` folder of each line get
// its commits too, because they borrow the objects of the store. The state of such a commit has
// no parents, and its record keeps the exact commit (with the names of its parents).

/// The commit ids of the lines of a `shallow` file.
fn shallow_ids(text: &str) -> BTreeSet<String> {
    text.lines().map(str::trim).filter(|l| l.len() == 40 && l.bytes().all(|b| b.is_ascii_hexdigit())).map(String::from).collect()
}

fn shallow_text(ids: &BTreeSet<String>) -> String {
    ids.iter().map(|c| format!("{c}\n")).collect()
}

/// The commits of the git store `s` whose parents the store does not have.
fn store_shallow(s: &Path) -> BTreeSet<String> {
    shallow_ids(&std::fs::read_to_string(s.join("shallow")).unwrap_or_default())
}

/// Name `commits` in the `shallow` file of the git store `s`. The caller holds the lock of the
/// Git refs (`fetch_write_lock`).
fn add_shallow_locked(s: &Path, commits: &BTreeSet<String>) -> Result<()> {
    let mut all = store_shallow(s);
    let before = all.len();
    all.extend(commits.iter().cloned());
    if all.len() == before {
        return Ok(());
    }
    fsutil::atomic_write(&s.join("shallow"), shallow_text(&all).as_bytes(), 0o644)
}

/// The `shallow` file of the `.git` folder of `line` names the shallow commits of the store (and
/// keeps the ones that git added in a fast line). A folder that the line owner made is left alone.
fn refresh_shallow(repo: &Repo, line: &Line) -> Result<()> {
    let want = store_shallow(&store(repo));
    if want.is_empty() {
        return Ok(());
    }
    let w = git_base(repo, line);
    let Ok(dir) = fsutil::open_dir_nofollow(&w.join(".git")) else { return Ok(()) };
    if !fsutil::is_layer(&w, ".git", &fsutil::meta_of(&dir.metadata()?)) {
        return Ok(());
    }
    let mut all = shallow_ids(&fsutil::read_at(&dir, "shallow").map(|c| String::from_utf8_lossy(&c).into_owned()).unwrap_or_default());
    if want.is_subset(&all) {
        return Ok(());
    }
    all.extend(want);
    let text = shallow_text(&all);
    let fast = follows_git(repo, line);
    if !fast {
        fsutil::git_writable(&w, true);
    }
    let r = fsutil::replace_at(&dir, "shallow", &mut |f| Ok((&*f).write_all(text.as_bytes())?), Some(repo.owner_ids(line)));
    if !fast {
        fsutil::git_writable(&w, false);
    }
    r
}

// ----- network operations as the caller -----

/// Environment passed to git network commands from the caller.
const NET_ENV: &[&str] = &[
    "SSH_AUTH_SOCK",
    "GIT_SSH_COMMAND",
    "GIT_SSH",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
    "https_proxy",
    "http_proxy",
    "no_proxy",
    "GIT_CONFIG_GLOBAL",
];

/// Network git commands run as the caller: its uid, groups and selected environment (ssh agent,
/// tokens, proxies), never the service's environment.
fn as_caller(ctx: &Ctx, c: &mut Command, git_dir: &Path) -> Result<()> {
    let mut env = crate::privs::pick_env(ctx, NET_ENV);
    if let Some(p) = ctx.env("PATH") {
        env.push(("PATH".into(), p.to_string()));
    }
    env.push(("GIT_DIR".into(), git_dir.to_string_lossy().into_owned()));
    env.push(("GIT_TERMINAL_PROMPT".into(), "0".into()));
    crate::privs::command_as_caller(c, ctx, &env)
}

pub fn remote_url(repo: &Repo, remote: &str) -> Result<String> {
    if let Some(v) = repo.p.config_value(&format!("remote.{remote}.url"))? {
        if let Some(s) = v.as_str() {
            return Ok(s.to_string());
        }
    }
    bail!("remote '{remote}' is not configured (layr git remote add {remote} <url>)")
}

fn fetched_refs(ctx: &Ctx, tmp: &Path, remote: &str) -> Result<Vec<(String, String)>> {
    let mut command = base_cmd(tmp);
    command.args(["for-each-ref", "--format=%(objectname) %(refname)", &format!("refs/remotes/{remote}/"), "refs/tags/"]);
    as_caller(ctx, &mut command, tmp)?;
    let output = command.stdin(Stdio::null()).output()?;
    if !output.status.success() {
        bail!("cannot list fetched refs: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let mut refs = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let (oid, name) = line.split_once(' ').ok_or_else(|| anyhow!("bad fetched ref"))?;
        if oid.len() != 40 || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("bad fetched object id");
        }
        if !name.starts_with(&format!("refs/remotes/{remote}/")) && !name.starts_with("refs/tags/") {
            bail!("bad fetched ref name");
        }
        refs.push((name.to_string(), oid.to_string()));
    }
    Ok(refs)
}

/// The lock of the Git refs of a project: one fetch writes its refs at a time. No project lock.
fn fetch_write_lock(repo: &Repo) -> Result<std::fs::File> {
    let f = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(repo.p.meta_dir().join("git-refs.lock"))?;
    rustix::fs::flock(&f, rustix::fs::FlockOperation::LockExclusive)?;
    Ok(f)
}

/// True when `ancestor` is an ancestor of (or equal to) `descendant` in the Git store.
fn is_ancestor(store: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    Ok(base_cmd(store)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success())
}

/// Import objects through a pipe. Root never opens the temporary repository that belongs to
/// the caller, so links or alternates in it cannot make root read another path. Runs under the
/// Git refs lock, not the project lock. Returns the refs to record: a ref that would move back to
/// an older commit (a concurrent fetch stored a newer one) keeps the newer commit.
fn import_fetched(
    ctx: &Ctx,
    repo: &Repo,
    store: &Path,
    tmp: &Path,
    refs: &[(String, String)],
    shallow: &BTreeSet<String>,
) -> Result<Vec<(String, String)>> {
    if refs.is_empty() {
        return Ok(Vec::new());
    }
    let _refs_lock = fetch_write_lock(repo)?;
    let current: BTreeMap<String, String> = git_out(store, &["for-each-ref", "--format=%(objectname) %(refname)"])?
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(oid, name)| (name.to_string(), oid.to_string()))
        .collect();
    let mut pack = base_cmd(tmp);
    pack.args(["pack-objects", "--stdout", "--revs", "--thin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    as_caller(ctx, &mut pack, tmp)?;
    let mut pack = pack.spawn().context("start git pack-objects")?;
    let pack_out = pack.stdout.take().ok_or_else(|| anyhow!("git pack-objects has no output"))?;

    let mut index = base_cmd(store);
    let mut index = index
        .args(["index-pack", "--stdin", "--fix-thin"])
        .stdin(Stdio::from(pack_out))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("start git index-pack")?;
    let write_result = (|| -> Result<()> {
        let input = pack.stdin.as_mut().ok_or_else(|| anyhow!("git pack-objects has no input"))?;
        for (_, oid) in refs {
            writeln!(input, "{oid}")?;
        }
        for oid in current.values() {
            writeln!(input, "^{oid}")?;
        }
        Ok(())
    })();
    drop(pack.stdin.take());
    let pack_status = pack.wait()?;
    let index_status = index.wait()?;
    write_result?;
    if !pack_status.success() || !index_status.success() {
        bail!("cannot import fetched git objects");
    }
    // The fetched history ends at commits without their parents (a shallow source): the store
    // names them before a ref shows them, so no command walks into a missing parent.
    let known = store_shallow(store);
    let ends: BTreeSet<String> = shallow
        .iter()
        .filter(|c| !known.contains(*c) && git_out(store, &["cat-file", "-e", &format!("{c}^{{commit}}")]).is_ok())
        .cloned()
        .collect();
    add_shallow_locked(store, &ends)?;

    let mut out = Vec::with_capacity(refs.len());
    for (name, oid) in refs {
        let keep = match current.get(name) {
            Some(cur) if cur != oid && is_ancestor(store, oid, cur)? => cur.clone(),
            _ => oid.clone(),
        };
        out.push((name.clone(), keep));
    }
    let mut update = base_cmd(store);
    update.args(["update-ref", "--stdin"]).stdin(Stdio::piped()).stderr(Stdio::piped());
    let mut update = update.spawn()?;
    {
        let input = update.stdin.as_mut().ok_or_else(|| anyhow!("git update-ref has no input"))?;
        for (name, oid) in &out {
            writeln!(input, "update {name} {oid}")?;
        }
    }
    drop(update.stdin.take());
    let output = update.wait_with_output()?;
    if !output.status.success() {
        bail!("cannot update fetched refs: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(out)
}

/// `git fetch` of a remote into the store. Network access runs as the caller, into a temporary
/// repository that borrows the store objects; the service then imports a pack through a pipe.
/// No project lock is held here.
pub fn fetch(ctx: &Ctx, repo: &Repo, remote: &str) -> Result<Vec<(String, String)>> {
    cache_retry(repo, || fetch_once(ctx, repo, remote))
}

fn fetch_once(ctx: &Ctx, repo: &Repo, remote: &str) -> Result<Vec<(String, String)>> {
    let url = remote_url(repo, remote)?;
    fetch_refspec(ctx, repo, &url, remote, &format!("+refs/heads/*:refs/remotes/{remote}/*"))
}

/// The remote-tracking branches of `remote` in a local Git checkout (`refs/remotes/<remote>/*`),
/// read from the folder, with no network: `layr init --from-git` keeps the branches that the
/// checkout knows. They are imported on first use.
pub fn fetch_tracking(ctx: &Ctx, repo: &Repo, checkout: &Path, remote: &str) -> Result<Vec<(String, String)>> {
    let url = checkout.to_string_lossy().into_owned();
    fetch_refspec(ctx, repo, &url, remote, &format!("+refs/remotes/{remote}/*:refs/remotes/{remote}/*"))
}

fn fetch_refspec(ctx: &Ctx, repo: &Repo, url: &str, remote: &str, refspec: &str) -> Result<Vec<(String, String)>> {
    let s = init_store(repo)?;
    let tmp = std::env::temp_dir().join(format!("layr-fetch-{}", ids::new_id()));
    std::fs::create_dir_all(&tmp)?;
    let out = Command::new("git").args(["init", "--bare", "-q"]).arg(&tmp).output()?;
    if !out.status.success() {
        bail!("git init failed");
    }
    std::fs::write(tmp.join("objects/info/alternates"), format!("{}\n", s.join("objects").display()))?;
    // Known refs of the store as "have" lines for a small transfer.
    let refs = git_out(&s, &["for-each-ref", "--format=%(objectname) %(refname)"])?;
    let mut packed = String::new();
    for l in refs.lines() {
        if let Some((oid, name)) = l.split_once(' ') {
            packed.push_str(&format!("{oid} refs/layr-have/{}\n", name.trim_start_matches("refs/")));
        }
    }
    std::fs::write(tmp.join("packed-refs"), packed)?;
    // The repository borrows the objects of the store, so it has the store's shallow commits.
    let known = store_shallow(&s);
    if !known.is_empty() {
        std::fs::write(tmp.join("shallow"), shallow_text(&known))?;
    }
    if rustix::process::geteuid().is_root() && ctx.caller.uid != 0 {
        let (u, g) = (ctx.caller.uid, ctx.caller.gid);
        let mut paths = Vec::new();
        fsutil::walk(&tmp, "", &mut |rel, _| {
            paths.push(rel.to_string());
            Ok(true)
        })?;
        fsutil::chown_nofollow(&tmp, u, g)?;
        for p in paths {
            fsutil::chown_nofollow(&tmp.join(p), u, g)?;
        }
    }
    let mut c = base_cmd(&tmp);
    // `--update-shallow`: a shallow source gives its branches (git skips them without it), and
    // the repository then names the commits where the history ends.
    c.args(["fetch", "-q", "--no-tags", "--no-write-fetch-head", "--update-shallow"])
        .arg(url)
        .arg(refspec)
        .arg("+refs/tags/*:refs/tags/*");
    as_caller(ctx, &mut c, &tmp)?;
    let out = c.stdin(Stdio::null()).output().context("run git fetch")?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        bail!("git fetch {url} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    test_sleep("LAYR_TEST_FETCH_SLEEP_MS");
    let refs = fetched_refs(ctx, &tmp, remote);
    // The repository belongs to the caller: its `shallow` file is read through a descriptor of
    // the folder, never through a link.
    let shallow = fsutil::open_dir_nofollow(&tmp)
        .ok()
        .and_then(|dir| fsutil::read_at(&dir, "shallow"))
        .map(|c| shallow_ids(&String::from_utf8_lossy(&c)))
        .unwrap_or_default();
    let r = refs.and_then(|refs| import_fetched(ctx, repo, &s, &tmp, &refs, &shallow));
    let _ = std::fs::remove_dir_all(&tmp);
    let prefix = format!("refs/remotes/");
    Ok(r?.into_iter().filter_map(|(name, oid)| name.strip_prefix(&prefix).map(|name| (name.to_string(), oid))).collect())
}

/// Read a remote ref without fetching (`git ls-remote`), as the caller.
pub fn ls_remote(ctx: &Ctx, repo: &Repo, url: &str, branch: &str) -> Result<Option<String>> {
    let s = init_store(repo)?;
    let mut c = base_cmd(&s);
    c.args(["ls-remote", url, &format!("refs/heads/{branch}")]);
    as_caller(ctx, &mut c, &s)?;
    let out = c.stdin(Stdio::null()).output()?;
    if !out.status.success() {
        bail!("git ls-remote failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).split_whitespace().next().map(|s| s.to_string()))
}

// ----- import -----

/// One cat-file process bounds memory to one object and avoids a process per file/tree.
struct CatFile {
    #[cfg(test)]
    blob_requests: usize,
    #[cfg(test)]
    decoded_blob_bytes: usize,
    #[cfg(test)]
    tree_requests: usize,
    child: std::process::Child,
    input: std::process::ChildStdin,
    output: std::io::BufReader<std::process::ChildStdout>,
}

impl CatFile {
    fn new(s: &Path) -> Result<Self> {
        let mut child = base_cmd(s).args(["cat-file", "--batch"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
        let input = child.stdin.take().unwrap();
        let output = std::io::BufReader::new(child.stdout.take().unwrap());
        Ok(Self {
            child,
            input,
            output,
            #[cfg(test)]
            blob_requests: 0,
            #[cfg(test)]
            decoded_blob_bytes: 0,
            #[cfg(test)]
            tree_requests: 0,
        })
    }

    fn object(&mut self, oid: &str, kind: &str) -> Result<Vec<u8>> {
        #[cfg(test)]
        match kind {
            "blob" => self.blob_requests += 1,
            "tree" => self.tree_requests += 1,
            _ => {}
        }
        writeln!(self.input, "{oid}")?;
        self.input.flush()?;
        let mut header = String::new();
        self.output.read_line(&mut header)?;
        let fields: Vec<&str> = header.split_whitespace().collect();
        if fields.len() != 3 || fields[0] != oid || (!kind.is_empty() && fields[1] != kind) {
            bail!("cannot read Git {kind} {oid}: {}", header.trim());
        }
        let mut data = vec![0; fields[2].parse()?];
        self.output.read_exact(&mut data)?;
        #[cfg(test)]
        if kind == "blob" {
            self.decoded_blob_bytes += data.len();
        }
        let mut newline = [0];
        self.output.read_exact(&mut newline)?;
        use layr_hashers::{Digest, GitCompatible};
        let mut hash = GitCompatible::new();
        hash.update(format!("{} {}\0", fields[1], data.len()).as_bytes());
        hash.update(&data);
        if hex::encode(hash.finalize()) != oid {
            bail!("Git cache object {oid} has a hash mismatch");
        }
        Ok(data)
    }
}

impl Drop for CatFile {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// These exact bytes travel in the existing signed/encrypted record stream. Trees are
/// deduplicated against the first parent, so every branch can be restored independently.
#[derive(serde::Serialize, serde::Deserialize)]
struct GitImport {
    format: u32,
    commit: String,
    trees: BTreeMap<String, String>,
}

fn tree_id(commit: &[u8]) -> Result<&str> {
    std::str::from_utf8(commit.split(|b| *b == b'\n').next().unwrap_or_default())?
        .strip_prefix("tree ")
        .context("Git commit has no tree")
}

/// One Git tree entry: its mode as Git prints it, and its object id.
#[derive(PartialEq)]
struct TreeEntry {
    mode: String,
    oid: String,
}

impl TreeEntry {
    /// The entry of a tree record (`tree_records`): its canonical mode and its ID.
    fn new(mode: u32, oid: &[u8]) -> Self {
        Self { mode: format!("{mode:o}"), oid: hex::encode(oid) }
    }
    fn is_tree(&self) -> bool {
        self.mode == "40000"
    }
    fn is_link(&self) -> bool {
        self.mode == "160000"
    }
    /// A file or a symbolic link: not a folder, not a submodule.
    fn is_blob(&self) -> bool {
        !self.is_tree() && !self.is_link()
    }
}

/// The mode Git reads for the raw octal mode of a tree record (`decode_tree_entry`, `canon_mode`):
/// the file type bits decide, so `040000` and `40755` are folders, `120777` is a symbolic link and
/// `160755` a submodule; a regular file is 100755 when its owner may execute it, else 100644.
/// Like Git, a long mode keeps its low 32 bits. `None` for an empty mode or one that is not octal.
fn canon_mode(raw: &[u8]) -> Option<u32> {
    if raw.is_empty() {
        return None;
    }
    let mut mode = 0u32;
    for digit in raw {
        if !(b'0'..=b'7').contains(digit) {
            return None;
        }
        mode = (mode << 3) | u32::from(digit - b'0');
    }
    Some(match mode & 0o170000 {
        0o100000 if mode & 0o100 != 0 => 0o100755,
        0o100000 => 0o100644,
        kind @ (0o040000 | 0o120000) => kind,
        _ => 0o160000,
    })
}

/// The records of a raw Git tree in order: canonical mode (`canon_mode`), name and the 20 bytes of
/// the object ID. A bad record is the last item.
fn tree_records(data: &[u8]) -> impl Iterator<Item = Result<(u32, &[u8], &[u8])>> {
    let mut rest = data;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let record = match (rest.iter().position(|b| *b == b' '), rest.iter().position(|b| *b == 0)) {
            (Some(space), Some(nul)) if space < nul && nul + 21 <= rest.len() => {
                let (record, tail) = rest.split_at(nul + 21);
                rest = tail;
                canon_mode(&record[..space])
                    .map(|mode| (mode, &record[space + 1..nul], &record[nul + 1..]))
                    .context("bad Git tree mode")
            }
            _ => Err(anyhow!("bad Git tree")),
        };
        if record.is_err() {
            rest = &[];
        }
        Some(record)
    })
}

fn parse_tree(data: &[u8]) -> Result<BTreeMap<String, TreeEntry>> {
    tree_records(data)
        .map(|record| {
            let (mode, name, oid) = record?;
            Ok((String::from_utf8_lossy(name).into_owned(), TreeEntry::new(mode, oid)))
        })
        .collect()
}

/// What one commit changes against its first parent, read from the Git trees that differ.
/// The cost follows the changed paths, not the size of the tree.
#[derive(Default)]
struct Changes {
    /// Files, links and submodules to write: path, mode, object id.
    write: Vec<(String, String, String)>,
    /// Topmost paths that the commit removes or replaces with another type.
    removed: BTreeSet<String>,
    /// The paths of every entry of a changed folder, to check for names that differ in case only.
    names: Vec<String>,
    /// The exact bytes of the changed trees (the new root tree first).
    trees: BTreeMap<String, String>,
}

fn walk_trees(cat: &mut CatFile, old: Option<&str>, new: &str, prefix: &str, c: &mut Changes) -> Result<()> {
    let raw = cat.object(new, "tree")?;
    let now = parse_tree(&raw)?;
    c.trees.insert(new.to_string(), STANDARD.encode(&raw));
    let before = match old {
        Some(o) => parse_tree(&cat.object(o, "tree")?)?,
        None => BTreeMap::new(),
    };
    let at = |name: &str| {
        if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        }
    };
    for (name, e) in &now {
        let path = at(name);
        c.names.push(path.clone());
        let o = before.get(name);
        if o == Some(e) {
            continue;
        }
        if e.is_tree() {
            if o.is_some_and(|o| !o.is_tree()) {
                c.removed.insert(path.clone());
            }
            walk_trees(cat, o.filter(|o| o.is_tree()).map(|o| o.oid.as_str()), &e.oid, &path, c)?;
            continue;
        }
        match o {
            // A submodule is an empty folder: only its own change shows nothing.
            Some(o) if o.is_link() && e.is_link() => continue,
            Some(o) if o.is_tree() || o.is_link() != e.is_link() => {
                c.removed.insert(path.clone());
            }
            _ => {}
        }
        c.write.push((path, e.mode.clone(), e.oid.clone()));
    }
    for name in before.keys().filter(|n| !now.contains_key(*n)) {
        c.removed.insert(at(name));
    }
    Ok(())
}

/// The changes of `commit` against `parent` (the whole tree without a parent), and the exact
/// objects that restore the commit: trees that the first parent does not have at that place.
fn import_changes(commit: &[u8], parent: Option<&str>, cat: &mut CatFile) -> Result<(GitImport, Changes)> {
    let new = tree_id(commit)?;
    let old = match parent {
        Some(p) => Some(tree_id(&cat.object(p, "commit")?)?.to_string()),
        None => None,
    };
    let mut c = Changes::default();
    if old.as_deref() != Some(new) {
        walk_trees(cat, old.as_deref(), new, "", &mut c)?;
    }
    let git = GitImport { format: 1, commit: STANDARD.encode(commit), trees: std::mem::take(&mut c.trees) };
    Ok((git, c))
}

/// Author name, email, message and time (milliseconds) of a raw Git commit.
fn commit_info(raw: &[u8]) -> (String, String, String, i64) {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text.split_once("\n\n").unwrap_or((&text, ""));
    let author = head.lines().find_map(|l| l.strip_prefix("author ")).unwrap_or("");
    let (name, rest) = author.split_once(" <").unwrap_or((author, ""));
    let (email, rest) = rest.split_once("> ").unwrap_or((rest.trim_end_matches('>'), ""));
    let time: i64 = rest.split_whitespace().next().and_then(|t| t.parse().ok()).unwrap_or(0);
    (name.to_string(), email.to_string(), body.trim_end().to_string(), time * 1000)
}

/// Write the files of `commit` for `paths` (all when `None`) into `dst`, deleting paths that
/// are not in the commit.
fn materialize(
    s: &Path,
    commit: &str,
    dst: &Path,
    paths: Option<&[String]>,
    own: Option<(u32, u32)>,
    source: Option<&Path>,
) -> Result<()> {
    let source = source.map(crate::tree::Tree::open).transpose()?;
    // ls-tree with many paths: call in batches.
    let mut entries: Vec<(String, String, String)> = Vec::new();
    let batch = |ps: &[&str]| -> Result<Vec<(String, String, String)>> {
        let mut a = vec!["ls-tree", "-r", "-z", "--full-tree", commit];
        if !ps.is_empty() {
            a.push("--");
            a.extend_from_slice(ps);
        }
        let out = base_cmd(s).args(&a).output()?;
        if !out.status.success() {
            bail!("git ls-tree failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        let mut v = Vec::new();
        for rec in out.stdout.split(|b| *b == 0) {
            if rec.is_empty() {
                continue;
            }
            let r = String::from_utf8_lossy(rec);
            let (meta, path) = r.split_once('\t').ok_or_else(|| anyhow!("bad ls-tree line"))?;
            let f: Vec<&str> = meta.split(' ').collect();
            v.push((f[0].to_string(), f[2].to_string(), path.to_string()));
        }
        Ok(v)
    };
    match paths {
        None => entries = batch(&[])?,
        Some(p) => {
            for chunk in p.chunks(500) {
                let refs: Vec<&str> = chunk.iter().map(|s| s.as_str()).collect();
                entries.extend(batch(&refs)?);
            }
        }
    }
    if let Some(p) = paths {
        let present: std::collections::HashSet<&str> = entries.iter().map(|(_, _, path)| path.as_str()).collect();
        for path in p {
            if !present.contains(path.as_str()) && fsutil::lmeta(dst, path).is_some() {
                fsutil::remove_entry(dst, path)?;
            }
        }
    }
    // Content through one `git cat-file --batch`.
    let mut child = base_cmd(s).args(["cat-file", "--batch"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let ids: Vec<String> = entries.iter().filter(|e| e.0 != "160000").map(|e| e.1.clone()).collect();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        for id in ids {
            writeln!(stdin, "{id}")?;
        }
        Ok(())
    });
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    use std::io::{BufRead, Read};
    for (mode, oid, path) in &entries {
        if mode == "160000" {
            fsutil::make_parents(dst, &format!("{path}/x"), own)?;
            continue;
        }
        let mut header = String::new();
        stdout.read_line(&mut header)?;
        let size: usize = header
            .split_whitespace()
            .nth(2)
            .and_then(|x| x.parse().ok())
            .ok_or_else(|| anyhow!("bad cat-file header {header}"))?;
        let mut data = vec![0u8; size];
        stdout.read_exact(&mut data)?;
        let mut nl = [0u8; 1];
        stdout.read_exact(&mut nl)?;
        if fsutil::git_blob_id(&data) != *oid {
            bail!("Git cache blob {oid} has a hash mismatch");
        }
        place_blob(dst, mode, path, &data, own, source.as_ref())?;
    }
    writer.join().map_err(|_| anyhow!("writer thread"))??;
    child.wait()?;
    Ok(())
}

/// Write one file or link of a commit into `dst`, replacing what is at the path.
fn place_blob(
    dst: &Path,
    mode: &str,
    path: &str,
    data: &[u8],
    own: Option<(u32, u32)>,
    source: Option<&crate::tree::Tree>,
) -> Result<()> {
    if fsutil::lmeta(dst, path).is_some() {
        fsutil::remove_entry(dst, path)?;
    }
    if mode == "120000" {
        fsutil::make_parents(dst, path, own)?;
        let target = String::from_utf8_lossy(data).into_owned();
        let p = fsutil::safe_join(dst, path)?;
        std::os::unix::fs::symlink(target, &p)?;
        if let Some((u, g)) = own {
            fsutil::chown_nofollow(&p, u, g)?;
        }
    } else {
        let m = if mode == "100755" { 0o755 } else { 0o644 };
        let cloned = match source {
            Some(source) => clone_matching_blob(source, path, data, dst, m, own)?,
            None => false,
        };
        if !cloned {
            fsutil::write_file(dst, path, data, m, own)?;
        }
    }
    Ok(())
}

/// Clone only exact blob bytes. Missing files, links and working edits use the Git blob.
fn clone_matching_blob(
    source: &crate::tree::Tree,
    path: &str,
    data: &[u8],
    dst: &Path,
    mode: u32,
    own: Option<(u32, u32)>,
) -> Result<bool> {
    use std::io::Read;
    let mut file = match source.open_read(path) {
        Ok(file) => file,
        Err(_) => return Ok(false),
    };
    if file.metadata()?.len() != data.len() as u64 {
        return Ok(false);
    }
    let mut buf = [0u8; 64 * 1024];
    for expected in data.chunks(buf.len()) {
        file.read_exact(&mut buf[..expected.len()])?;
        if &buf[..expected.len()] != expected {
            return Ok(false);
        }
    }
    // The caller-owned folder was copied with caller permissions before this snapshot.
    // It cannot change between this comparison and the descriptor-safe clone.
    fsutil::copy_entry(source.path.as_path(), path, dst, path, own)?;
    fsutil::set_mode(dst, path, mode)?;
    Ok(true)
}

/// Commits imported between two progress lines on standard error. A long import prints them, so a
/// user sees that it still runs.
const IMPORT_PROGRESS_EVERY: usize = 50;
/// Records written per project lock hold of an import. A hold then lasts a few milliseconds.
const IMPORT_CHUNK: usize = 16;

/// A remote ref or a commit moved while an import was planned. The caller plans again.
#[derive(Debug)]
pub struct Moved(pub String);

impl std::fmt::Display for Moved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Moved {}

/// The records of one import, planned with no project lock. `commit_import` writes them.
pub struct ImportPlan {
    /// Each planned record with the Git commit it imports, parents first.
    records: Vec<(String, crate::model::DraftRecord)>,
    /// The Git commit that was asked for, its state, and the remote ref to move (if any).
    commit: String,
    tip: String,
    refname: String,
}

/// Import the entire reachable DAG, parents first. A retry reuses completed imports. Each commit
/// becomes a state record without a tree; its tree is built from git when a command reads it
/// (`build_tree`, through `Repo::check_state`).
pub fn import_commit(ctx: &Ctx, repo: &Repo, commit: &str, refname: &str) -> Result<(String, bool)> {
    import_with_repair(ctx, repo, commit, refname, None)
}

/// Import `commit` as the new tip of the remote ref `refname`, which holds `prev` (a state, or none).
/// The ref update is written only while the ref still holds `prev`; otherwise the import fails with
/// `Moved` and the caller plans it again.
pub fn import_ref(ctx: &Ctx, repo: &Repo, commit: &str, refname: &str, prev: Option<String>) -> Result<String> {
    import_with_repair(ctx, repo, commit, refname, Some(prev)).map(|(st, _)| st)
}

/// `guard`: `None` for an import with no remote ref to move, `Some(prev)` for a remote ref that
/// holds `prev` (`import_ref`).
fn import_with_repair(
    ctx: &Ctx,
    repo: &Repo,
    commit: &str,
    refname: &str,
    guard: Option<Option<String>>,
) -> Result<(String, bool)> {
    match import_planned(ctx, repo, commit, refname, guard.clone()) {
        Ok(value) => Ok(value),
        Err(error) => {
            if error.is::<Moved>() || !repair_cache(repo)? {
                return Err(error);
            }
            let remote = refname.split_once('/').context("import ref has no remote")?.0;
            fetch(ctx, repo, remote)?;
            import_planned(ctx, repo, commit, refname, guard)
        }
    }
}

fn import_planned(ctx: &Ctx, repo: &Repo, commit: &str, refname: &str, guard: Option<Option<String>>) -> Result<(String, bool)> {
    let plan = plan_import(ctx, repo, commit, refname)?;
    test_sleep("LAYR_TEST_PLAN_SLEEP_MS");
    commit_import(ctx, repo, plan, guard)
}

/// Test hook: sleep for the milliseconds named by `var` (tests hold a phase open to race another command).
fn test_sleep(var: &str) {
    if let Some(ms) = std::env::var(var).ok().and_then(|v| v.parse::<u64>().ok()) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// The remote-tracking refs of `remote` in the Git store, named as `fetch` returns them. A retried
/// import plans from these, so a concurrent fetch's newer tip is used.
pub fn stored_remote_refs(repo: &Repo, remote: &str) -> Result<Vec<(String, String)>> {
    let out = git_out(&store(repo), &["for-each-ref", "--format=%(objectname) %(refname)", &format!("refs/remotes/{remote}/")])?;
    Ok(out
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter_map(|(oid, name)| name.strip_prefix("refs/remotes/").map(|n| (n.to_string(), oid.to_string())))
        .collect())
}

/// The state of a Git commit that the store maps through Git, native trees, or object store v2.
pub(crate) fn mapped_state(repo: &Repo, oid: &str) -> Result<Option<String>> {
    for st in repo.p.db.state_of_commit(oid)? {
        // An imported state is mapped by its record, which holds the Git objects of the commit
        // (earlier versions imported only the tip and their records lack them). Other states are
        // mapped when their native tree or verified object tree is available.
        let Some(rec) = repo.p.db.state(&st)? else {
            continue;
        };
        let mapped = if rec.kind == StateKind::Import {
            repo.p.db.has_git_import(&st)?
        } else {
            repo.p.has_state(&st) || crate::objstore::verified(&repo.p, &st)?.is_some()
        };
        if mapped {
            return Ok(Some(st));
        }
    }
    Ok(None)
}

/// Plan the records of the commits of `commit` that are not imported yet. Reads the Git objects and
/// the records only: no project lock, no write.
pub fn plan_import(ctx: &Ctx, repo: &Repo, commit: &str, refname: &str) -> Result<ImportPlan> {
    plan_import_over(ctx, repo, commit, refname, &[])
}

/// The state of a commit outside the walk of `plan_import_over`: a commit that a record maps, or
/// a commit that layr made for a state without a record (`state_of_history_commit`).
fn state_of_known_commit(repo: &Repo, oid: &str) -> Result<Option<String>> {
    match mapped_state(repo, oid)? {
        Some(st) => Ok(Some(st)),
        None => state_of_history_commit(repo, oid),
    }
}

/// `plan_import` without a walk of the whole history: the commits that `over` (commits with their
/// states) reach are imported already. A parent outside the walk is found by its commit.
fn plan_import_over(ctx: &Ctx, repo: &Repo, commit: &str, refname: &str, over: &[(String, String)]) -> Result<ImportPlan> {
    let s = init_store(repo)?;
    let mut args: Vec<String> = ["rev-list", "--topo-order", "--reverse", "--parents", commit].map(String::from).to_vec();
    args.extend(over.iter().map(|(c, _)| format!("^{c}")));
    let revs = git_out(&s, &args.iter().map(String::as_str).collect::<Vec<_>>())?;
    let mut states: BTreeMap<String, String> = over.iter().cloned().collect();
    let mut cat = CatFile::new(&s)?;
    let mut records = Vec::new();
    let mut imported = 0usize;
    for line in revs.lines() {
        let mut fields = line.split_whitespace();
        let oid = fields.next().context("empty Git revision")?;
        let git_parents: Vec<&str> = fields.collect();
        let state = match mapped_state(repo, oid)? {
            Some(st) => st,
            None => {
                let mut parents: Vec<String> = Vec::new();
                for p in &git_parents {
                    let known = match states.get(*p) {
                        Some(st) => Some(st.clone()),
                        None if !over.is_empty() => state_of_known_commit(repo, p)?,
                        None => None,
                    };
                    let st = known.ok_or_else(|| anyhow!("Git parent {p} was not imported"))?;
                    states.insert(p.to_string(), st.clone());
                    parents.push(st);
                }
                let raw = cat.object(oid, "commit")?;
                let (objects, changes) = import_changes(&raw, git_parents.first().copied(), &mut cat)?;
                // A commit that git made in a fast line: its state gets objects now, from the
                // objects of its first parent and the changed paths. Without them the state has
                // no tree until git builds one (`build_tree`), and no fast line starts from it.
                let refs = match parents.first().filter(|_| !over.is_empty()) {
                    Some(parent) => match commit_objects(repo, parent, &changes, &mut cat) {
                        Ok(refs) => refs,
                        Err(e) => {
                            drop(crate::trace::named(|| format!("commit {oid} gets no objects: {e:#}")));
                            None
                        }
                    },
                    None => None,
                };
                let (st, mut rec) = import_record(ctx, repo, oid, &raw, refname, parents, objects, changes)?;
                if let (Some((store, refs)), Some(state)) = (refs, rec.states.first_mut()) {
                    store.set_state(&state.id, &refs)?;
                    state.tree = Some(refs.tree.hex());
                    state.times = Some(refs.times.hex());
                }
                records.push((oid.to_string(), rec));
                imported += 1;
                if imported % IMPORT_PROGRESS_EVERY == 0 {
                    eprintln!("layr: imported {imported} commits of {refname}");
                }
                st
            }
        };
        states.insert(oid.to_string(), state);
    }
    // The tip of a walk without commits (git went back to an older commit) has its state already.
    let tip = match states.remove(commit) {
        Some(st) => Some(st),
        None if !over.is_empty() => state_of_known_commit(repo, commit)?,
        None => None,
    };
    Ok(ImportPlan { records, commit: commit.to_string(), tip: tip.context("Git tip was not imported")?, refname: refname.to_string() })
}

/// The paths that a commit changes, as a source of an object write over the objects of its
/// first parent: `root` holds only the changed files (and the folders on the way to a change),
/// and a folder lists the names of the parent's tree, without the removed ones, with its own.
struct CommitSource<'a> {
    root: &'a Path,
    store: &'a layr_objects::Store,
    base: layr_objects::Id,
    removed: &'a BTreeSet<String>,
}

impl CommitSource<'_> {
    /// The tree of the folder `relative` in the parent's objects; None when it has no such folder.
    fn base_tree(&self, relative: &[u8]) -> Result<Option<layr_objects::Tree>> {
        let mut tree = self.store.get_tree(&self.base)?;
        for part in relative.split(|b| *b == b'/').filter(|p| !p.is_empty()) {
            let next = tree.entries.iter().find(|e| e.name == part).and_then(|e| match &e.node {
                layr_objects::Node::Dir { tree } => Some(*tree),
                _ => None,
            });
            match next {
                Some(id) => tree = self.store.get_tree(&id)?,
                None => return Ok(None),
            }
        }
        Ok(Some(tree))
    }
}

impl layr_objects::write::Source for CommitSource<'_> {
    fn path(&self, relative: &[u8]) -> Result<PathBuf> {
        use std::os::unix::ffi::OsStrExt;
        Ok(self.root.join(std::ffi::OsStr::from_bytes(relative)))
    }

    fn names(&self, relative: &[u8]) -> Result<Vec<Vec<u8>>> {
        use std::os::unix::ffi::OsStrExt;
        let mut names: BTreeSet<Vec<u8>> = BTreeSet::new();
        if let Some(tree) = self.base_tree(relative)? {
            for e in tree.entries {
                let child = if relative.is_empty() { e.name.clone() } else { [relative, b"/", &e.name].concat() };
                if !std::str::from_utf8(&child).is_ok_and(|c| self.removed.contains(c)) {
                    names.insert(e.name);
                }
            }
        }
        match std::fs::read_dir(self.path(relative)?) {
            Ok(entries) => {
                for e in entries {
                    names.insert(e?.file_name().as_bytes().to_vec());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(names.into_iter().collect())
    }
}

/// The objects of the state of a commit: the objects of the state `parent` of its first parent,
/// with the changed paths read from the git objects (the files as `build_tree` writes them). The
/// cost follows the changed paths. None when the parent has no objects.
fn commit_objects(
    repo: &Repo,
    parent: &str,
    changes: &Changes,
    cat: &mut CatFile,
) -> Result<Option<(std::sync::Arc<layr_objects::Store>, layr_objects::StateRef)>> {
    let store = match crate::objstore::verified(&repo.p, parent)? {
        Some((store, _)) => store,
        // The state of an earlier commit of the same plan: its objects are not in a record yet.
        None if crate::objstore::exists(&repo.p) => std::sync::Arc::new(crate::objstore::open(&repo.p)?),
        None => return Ok(None),
    };
    let Some(base) = store.state_ref(parent)? else { return Ok(None) };
    if !store.has(&base.tree) || !store.has(&base.times) {
        return Ok(None);
    }
    let _t = crate::trace::span("objects of a git commit");
    std::fs::create_dir_all(repo.p.work_dir())?;
    let tmp = crate::repo::Temp::plain(repo.p.work_dir().join(crate::store::temp_name()));
    std::fs::create_dir(&tmp.path)?;
    for (path, mode, oid) in &changes.write {
        if mode == "160000" {
            fsutil::make_parents(&tmp.path, &format!("{path}/x"), None)?;
            continue;
        }
        let data = cat.object(oid, "blob")?;
        place_blob(&tmp.path, mode, path, &data, None, None)?;
    }
    // The folder of a removed path stays (git has no folder without files), so it is read.
    for path in &changes.removed {
        fsutil::make_parents(&tmp.path, path, None)?;
    }
    let changed: Vec<Vec<u8>> =
        changes.write.iter().map(|w| w.0.clone().into_bytes()).chain(changes.removed.iter().map(|p| p.clone().into_bytes())).collect();
    let options =
        layr_objects::WriteOptions { temperature: layr_objects::Temperature::Hot, previous: None, exclude: Vec::new(), similar: false };
    let source = CommitSource { root: &tmp.path, store: &store, base: base.tree, removed: &changes.removed };
    let written = layr_objects::write::write_changed_from(
        &store,
        &source,
        &layr_objects::write::Base { tree: base.tree, times: base.times },
        &changed,
        &options,
    )?;
    let refs = layr_objects::StateRef {
        tree: written.tree.context("object write gave no tree")?,
        times: written.times.context("object write gave no times")?,
    };
    Ok(Some((store, refs)))
}

/// Objects (a tree and a times table in the object store) for the imported states that have none:
/// from the objects of the first parent and the changed paths when that parent is an imported
/// state with objects, else from the whole tree of the commit. Parents first; `state.tree`
/// records name them (docs/git-on-objects.md). Returns the number of states that got objects.
pub fn build_import_objects(ctx: &Ctx, repo: &Repo) -> Result<usize> {
    if !has_store(repo) || !crate::objstore::exists(&repo.p) {
        return Ok(0);
    }
    let p = &repo.p;
    let mut pending: BTreeMap<String, String> = BTreeMap::new();
    for id in p.db.kept_state_ids()? {
        let Some(rec) = p.db.state(&id)? else { continue };
        if rec.kind != StateKind::Import || rec.tree.is_some() || !p.db.has_git_import(&id)? {
            continue;
        }
        if let Some(c) = stored_commit(repo, &id)? {
            pending.insert(id, c);
        }
    }
    if pending.is_empty() {
        return Ok(0);
    }
    let s = store(repo);
    let store = crate::objstore::open(p)?;
    let mut cat = CatFile::new(&s)?;
    let mut made: BTreeMap<String, layr_objects::StateRef> = BTreeMap::new();
    let mut batch = serde_json::Map::new();
    let options =
        layr_objects::WriteOptions { temperature: layr_objects::Temperature::Hot, previous: None, exclude: Vec::new(), similar: false };
    let flush = |batch: &mut serde_json::Map<String, serde_json::Value>| -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let locked = repo.p.lock_or_borrow()?;
        let mut rec = crate::model::DraftRecord::new("state.tree");
        rec.data = serde_json::json!({ "states": std::mem::take(batch) });
        repo.commit_record(&locked, ctx, rec)?;
        Ok(())
    };
    loop {
        let mut progress = false;
        let ids: Vec<String> = pending.keys().cloned().collect();
        for id in ids {
            let rec = repo.state(&id)?;
            let first = rec.parents.first().cloned();
            // The objects of the first parent, when it is an imported state: its tree is the tree of
            // the parent commit, so the changed paths give this one.
            let base = match &first {
                Some(parent) if pending.contains_key(parent) => continue,
                Some(parent) => {
                    let imported = p.db.state(parent)?.is_some_and(|s| s.kind == StateKind::Import);
                    match (made.get(parent).copied(), imported) {
                        (Some(r), _) => Some((parent.clone(), r)),
                        (None, true) => crate::objstore::verified(p, parent)?.map(|(_, r)| (parent.clone(), r)),
                        _ => None,
                    }
                }
                None => None,
            };
            let commit = pending.remove(&id).expect("pending state");
            let raw = cat.object(&commit, "commit")?;
            let parent_commit = base.as_ref().and_then(|(ps, _)| stored_commit(repo, ps).ok().flatten());
            let refs = match (&base, parent_commit) {
                (Some((ps, r)), Some(pc)) => {
                    store.set_state(ps, r)?;
                    let (_, changes) = import_changes(&raw, Some(&pc), &mut cat)?;
                    commit_objects(repo, ps, &changes, &mut cat)?.map(|(_, refs)| refs)
                }
                _ => None,
            };
            let refs = match refs {
                Some(r) => r,
                None => {
                    let _t = crate::trace::span(format!("objects of commit {} (whole tree)", &commit[..10]));
                    std::fs::create_dir_all(p.work_dir())?;
                    let tmp = crate::repo::Temp::plain(p.work_dir().join(crate::store::temp_name()));
                    std::fs::create_dir(&tmp.path)?;
                    materialize(&s, &commit, &tmp.path, None, None, None)?;
                    let w = layr_objects::write_tree(&store, &tmp.path, &options)?;
                    layr_objects::StateRef {
                        tree: w.tree.context("object write gave no tree")?,
                        times: w.times.context("object write gave no times")?,
                    }
                }
            };
            store.set_state(&id, &refs)?;
            made.insert(id.clone(), refs);
            batch.insert(id, serde_json::json!({"tree": refs.tree.hex(), "times": refs.times.hex()}));
            if batch.len() >= 64 {
                flush(&mut batch)?;
            }
            progress = true;
        }
        if pending.is_empty() || !progress {
            break;
        }
    }
    flush(&mut batch)?;
    Ok(made.len())
}

/// The lock of the Git refs of a project (`fetch_write_lock`): no fetch adds objects meanwhile.
pub fn refs_lock(repo: &Repo) -> Result<std::fs::File> {
    fetch_write_lock(repo)
}

/// After the git store replaced its packs (`gitpack::drop_git_blobs`): the clones of the old packs
/// in the `.git` folder of each fast line of this machine go, and the line gets clones of the new
/// ones. It runs as the line owner (the folder is the owner's). A line that fails keeps its
/// clones: they only hold space.
pub fn refresh_line_packs(repo: &Repo, removed: &[String]) {
    let s = store(repo);
    let Ok(lines) = repo.lines() else { return };
    let new: Vec<String> = std::fs::read_dir(s.join("objects/pack"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("pack-") && (n.ends_with(".pack") || n.ends_with(".idx") || n.ends_with(".rev")))
        .collect();
    for line in lines {
        if line.machine != repo.p.store.machine.id || !follows_git(repo, &line) {
            continue;
        }
        let Some((g, _)) = fast_git_folder(repo, &line) else { continue };
        // `$1`: the pack folder of the line; `$2`: the pack folder of the store; then the names to
        // remove, "--", and the names to clone (packs before their indexes).
        let mut script = String::from("cd \"$1\" || exit 1; src=\"$2\"; shift 2; while [ \"$1\" != -- ]; do rm -f -- \"$1\"; shift; done; shift; ");
        script.push_str("for f in \"$@\"; do [ -e \"$f\" ] || cp --reflink=auto -- \"$src/$f\" \"$f\" || rm -f -- \"$f\"; done");
        let mut c = Command::new("sh");
        c.arg("-c").arg(&script).arg("sh").arg(g.join("objects/pack")).arg(s.join("objects/pack"));
        for r in removed {
            for ext in ["pack", "idx", "rev", "bitmap"] {
                c.arg(format!("{r}.{ext}"));
            }
        }
        c.arg("--");
        let mut order = new.clone();
        order.sort_by_key(|n| (n.ends_with(".idx"), n.clone()));
        c.args(order);
        if crate::privs::command_as(&mut c, repo.owner_ids(&line).0, &[]).is_err() {
            continue;
        }
        if let Err(e) = c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status() {
            eprintln!("warning: the git packs of line {} stay as they were: {e}", line.name);
        }
    }
}

/// Write a plan in chunks of `IMPORT_CHUNK` records. Each chunk holds the project lock for its
/// writes only. With a guard, the remote ref must still hold the expected state before each chunk,
/// and it ends at the tip. A commit that another command imported meanwhile is `Moved`, so the
/// records never repeat it. Returns the tip state and whether this plan imported the tip.
pub fn commit_import(ctx: &Ctx, repo: &Repo, plan: ImportPlan, guard: Option<Option<String>>) -> Result<(String, bool)> {
    let fresh = plan.records.iter().any(|(oid, _)| *oid == plan.commit);
    let ImportPlan { records, commit, tip, refname } = plan;
    let mut expected: Option<String> = guard.clone().flatten();
    let mut pending = records.into_iter();
    loop {
        let locked = repo.p.lock_or_borrow()?;
        if guard.is_some() {
            let current = repo.p.db.remote_ref(&refname)?.map(|r| r.0);
            if current != expected {
                bail_moved(format!("remote ref {refname} moved during the fetch; run it again"))?;
            }
        }
        let mut written = 0;
        while written < IMPORT_CHUNK {
            let Some((oid, rec)) = pending.next() else {
                break;
            };
            if mapped_state(repo, &oid)?.is_some() {
                bail_moved(format!("commit {} of {refname} was imported by another command; run it again", &oid[..10]))?;
            }
            let st = rec.states.first().map(|s| s.id.clone()).unwrap_or_default();
            repo.commit_record(&locked, ctx, rec)?;
            expected = Some(st);
            written += 1;
        }
        if written == IMPORT_CHUNK {
            continue;
        }
        if guard.is_some() && expected.as_deref() != Some(tip.as_str()) {
            let mut rec = crate::model::DraftRecord::new("remote.ref");
            rec.data = serde_json::json!({"ref": refname, "state": tip, "commit": commit});
            repo.commit_record(&locked, ctx, rec)?;
        }
        return Ok((tip, fresh));
    }
}

/// A reflink clone of the whole of `src` into the empty file `dst` (Linux); an error where the
/// file system or the system has none, and the caller copies.
fn clone_fd(src: &std::fs::File, dst: &std::fs::File) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    return rustix::fs::ioctl_ficlone(dst, src).map_err(std::io::Error::from);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (src, dst);
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }
}

fn bail_moved(message: String) -> Result<()> {
    Err(Moved(message).into())
}

/// Record one imported commit as a state without a tree. The record holds the exact Git objects
/// of the commit (`GitImport`); the tree is built from git on the first read (`build_tree`).
#[allow(clippy::too_many_arguments)]
fn import_record(
    ctx: &Ctx,
    repo: &Repo,
    commit: &str,
    raw: &[u8],
    refname: &str,
    parents: Vec<String>,
    objects: GitImport,
    changes: Changes,
) -> Result<(String, crate::model::DraftRecord)> {
    // On a store volume that ignores case, README and Readme would become one file. The earlier
    // state passed this check: only the entries of changed folders can collide.
    fsutil::refuse_case_collisions(&repo.p.work_dir(), changes.names.iter().map(|n| n.as_str()))?;
    let (name, email, msg, time) = commit_info(raw);
    let mut st = repo.state_record(ctx, ids::new_id(), StateKind::Import, parents, &msg, None, String::new());
    st.author = crate::model::Author { name, email, uid: 0, session: None };
    st.time = time.max(1);
    let mut rec = crate::model::DraftRecord::new("import");
    let id = st.id.clone();
    rec.states.push(st);
    rec.data = serde_json::json!({"ref": refname, "commit": commit, "state": id, "git": objects});
    Ok((id, rec))
}

/// The Git commit of an imported state that the store holds: the commit its record names, not a
/// squash commit.
fn stored_commit(repo: &Repo, state: &str) -> Result<Option<String>> {
    let squash = repo.p.db.squash_commits_of(state)?;
    let s = store(repo);
    for c in repo.p.db.git_commits_of(state)?.into_iter().filter(|c| !squash.contains(c)) {
        if git_out(&s, &["cat-file", "-e", &format!("{c}^{{commit}}")]).is_ok() {
            return Ok(Some(c));
        }
    }
    Ok(None)
}

/// The fast verification of `layr migrate` for a state that `build_tree` builds from Git (docs/objects.md,
/// section 13). The "snapshot" of such a state is itself made from the Git commit, so the v2 tree can be
/// compared with the commit's tree directly instead of checking both out: names, kinds and the modes that
/// `materialize` writes (0644, 0755, 0755 folders, empty folders for submodules), file bytes by their Git
/// blob id, and link targets. A subtree pair (v2 tree, Git tree) that matched once is not walked again.
/// Times follow the rule of `build_tree`: an entry that is the same as in the v2 state of the first parent
/// keeps its time there; a changed entry has a new time. `verify` answers false when it cannot show the
/// state equal (an unusual mode, an attribute, a hard link, a time that moved, no first parent); the caller
/// then runs the full comparison, which decides.
pub(crate) struct GitVerifier {
    cat: CatFile,
    pairs: std::collections::HashSet<(layr_objects::Id, String)>,
    blobs: std::collections::HashMap<layr_objects::Id, String>,
}

impl GitVerifier {
    pub(crate) fn new(repo: &Repo) -> Result<Self> {
        Ok(Self { cat: CatFile::new(&store(repo))?, pairs: Default::default(), blobs: Default::default() })
    }

    pub(crate) fn verify(
        &mut self,
        repo: &Repo,
        objects: &layr_objects::Store,
        state: &str,
        r: &layr_objects::StateRef,
        parent: Option<&layr_objects::StateRef>,
    ) -> Result<bool> {
        let Some(parent) = parent else { return Ok(false) };
        let Some(commit) = stored_commit(repo, state)? else { return Ok(false) };
        let raw = self.cat.object(&commit, "commit")?;
        let root = tree_id(&raw)?.to_string();
        Ok(self.same_tree(objects, &r.tree, &root)? && times_follow(objects, &r.tree, &r.times, &parent.tree, &parent.times)?)
    }

    fn same_tree(&mut self, objects: &layr_objects::Store, tree: &layr_objects::Id, git: &str) -> Result<bool> {
        use layr_objects::tree::Node;
        if self.pairs.contains(&(*tree, git.to_string())) {
            return Ok(true);
        }
        let v2 = objects.get_tree(tree)?;
        let entries = parse_tree(&self.cat.object(git, "tree")?)?;
        if !v2.links.is_empty() || v2.entries.len() != entries.len() {
            return Ok(false);
        }
        for e in &v2.entries {
            let Ok(name) = std::str::from_utf8(&e.name) else { return Ok(false) };
            let Some(g) = entries.get(name) else { return Ok(false) };
            if !e.xattrs.is_empty() {
                return Ok(false);
            }
            let same = match (g.mode.as_str(), &e.node) {
                ("100644", Node::File { blob, .. }) => e.mode == 0o644 && self.blob_id(objects, blob)? == g.oid,
                ("100755", Node::File { blob, .. }) => e.mode == 0o755 && self.blob_id(objects, blob)? == g.oid,
                ("120000", Node::Symlink { target }) => *target == self.cat.object(&g.oid, "blob")?,
                ("40000", Node::Dir { tree }) => e.mode == 0o755 && self.same_tree(objects, tree, &g.oid)?,
                ("160000", Node::Dir { tree }) => {
                    let sub = objects.get_tree(tree)?;
                    e.mode == 0o755 && sub.entries.is_empty() && sub.links.is_empty()
                }
                _ => false,
            };
            if !same {
                return Ok(false);
            }
        }
        self.pairs.insert((*tree, git.to_string()));
        Ok(true)
    }

    fn blob_id(&mut self, objects: &layr_objects::Store, blob: &layr_objects::Id) -> Result<String> {
        if let Some(id) = self.blobs.get(blob) {
            return Ok(id.clone());
        }
        let id = fsutil::git_blob_id(&objects.read_blob(blob)?);
        self.blobs.insert(*blob, id.clone());
        Ok(id)
    }
}

/// The times rule of `build_tree` (see `GitVerifier`): every entry of `tree` that equals the entry of the
/// same name in `base_tree` (same node and mode) has the same time record in `times` as in `base_times`; a
/// folder that changed is checked the same way inside. Changed and new entries may have any time.
fn times_follow(
    objects: &layr_objects::Store,
    tree: &layr_objects::Id,
    times: &layr_objects::Id,
    base_tree: &layr_objects::Id,
    base_times: &layr_objects::Id,
) -> Result<bool> {
    use layr_objects::times::Record;
    use layr_objects::tree::Node;
    if tree == base_tree && times == base_times {
        return Ok(true);
    }
    let (t, b) = (objects.get_tree(tree)?, objects.get_tree(base_tree)?);
    let (m, bm) = (objects.get_times(times)?, objects.get_times(base_times)?);
    if m.records.len() != t.entries.len() || bm.records.len() != b.entries.len() {
        return Ok(false);
    }
    let base: BTreeMap<&[u8], (&layr_objects::tree::Entry, Record)> =
        b.entries.iter().zip(bm.records.iter().copied()).map(|(e, r)| (e.name.as_slice(), (e, r))).collect();
    for (e, record) in t.entries.iter().zip(m.records.iter().copied()) {
        let Some((be, base_record)) = base.get(e.name.as_slice()) else { continue };
        let unchanged = be.node == e.node && be.mode == e.mode && be.xattrs == e.xattrs;
        match (&e.node, &be.node, record, *base_record) {
            (Node::Dir { tree }, Node::Dir { tree: bt }, Record::Folder(x), Record::Folder(y)) => {
                if unchanged && x != y {
                    return Ok(false);
                }
                if !unchanged && !times_follow(objects, tree, &x, bt, &y)? {
                    return Ok(false);
                }
            }
            _ if unchanged => {
                if record != *base_record {
                    return Ok(false);
                }
            }
            _ => {}
        }
    }
    Ok(true)
}

/// The states of `states` that `build_tree` can build (`buildable`), checked with one `git cat-file`
/// process for all of them: one process per state took minutes on a long imported history.
pub fn buildable_set(repo: &Repo, states: &[String]) -> Result<BTreeSet<String>> {
    buildable_states(&repo.p, states)
}

/// `buildable_set` for a project (the free step has no `Repo`).
pub fn buildable_states(p: &crate::store::Project, states: &[String]) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    if states.is_empty() || !p.git_dir().join("objects").is_dir() {
        return Ok(out);
    }
    let mut cat: Option<CatFile> = None;
    for state in states {
        let imported = matches!(p.db.state(state), Ok(Some(s)) if s.kind == StateKind::Import);
        if !imported || !p.db.has_git_import(state).unwrap_or(false) {
            continue;
        }
        let squash = p.db.squash_commits_of(state)?;
        for c in p.db.git_commits_of(state)?.into_iter().filter(|c| !squash.contains(c)) {
            let cat = match &mut cat {
                Some(c) => c,
                None => cat.insert(CatFile::new(&p.git_dir())?),
            };
            // A missing object answers "<oid> missing" and the process stays usable.
            if cat.object(&c, "commit").is_ok() {
                out.insert(state.clone());
                break;
            }
        }
    }
    Ok(out)
}

/// True when `state` is an imported state that `build_tree` can build: its record holds the Git
/// objects, and the store holds its commit.
pub fn buildable(repo: &Repo, state: &str) -> bool {
    let imported = matches!(repo.p.db.state(state), Ok(Some(s)) if s.kind == StateKind::Import);
    imported
        && has_store(repo)
        && repo.p.db.has_git_import(state).unwrap_or(false)
        && stored_commit(repo, state).ok().flatten().is_some()
}

/// The nearest imported ancestor of `state` whose tree is on disk, with its commit. Breadth first,
/// so the first parent wins a tie.
fn nearest_built_import(repo: &Repo, state: &str) -> Result<Option<(String, String)>> {
    let mut seen = BTreeSet::new();
    let mut queue: std::collections::VecDeque<String> = repo.state(state)?.parents.into();
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let Some(s) = repo.p.db.state(&id)? else {
            continue;
        };
        if s.kind == StateKind::Import && repo.p.has_state(&id) {
            if let Some(c) = stored_commit(repo, &id)? {
                return Ok(Some((id, c)));
            }
        }
        queue.extend(s.parents);
    }
    Ok(None)
}

/// Recover a save input into creator-owned storage, without publication, retention or a project
/// lock. A complete checkout also avoids introducing a thin dependency on missing native storage.
pub(crate) fn prepare_tree(repo: &Repo, state: &str, dst: &Path) -> Result<()> {
    let commit = stored_commit(repo, state)?.context("imported state has no local Git commit")?;
    let temp = crate::repo::Temp::empty(repo.p.work_dir().join(crate::store::temp_name()))?;
    crate::repo::save_barrier("git_recovery")?;
    materialize(&store(repo), &commit, &temp.path, None, None, None)?;
    crate::fs::freeze_path_owned(&temp.path, dst, crate::fs::CapturePurpose::State)?.release();
    Ok(())
}

/// Build the tree of an imported state from git and mark it present. The tree is a thin layer
/// over the nearest imported ancestor on disk, holding the changed paths (APFS), or a copy of that
/// ancestor with the changed paths (other file systems). Without such an ancestor it is the full
/// tree of the commit. `source` is a folder with the content of the commit (`init --from`); its
/// files are cloned where their bytes match the Git objects. The caller holds the project lock, or
/// this function takes it. `own` normalizes a new private import before sealing it for the
/// initial line; rebuilding an existing shared state keeps its recorded metadata.
pub fn build_tree(repo: &Repo, state: &str, source: Option<&Path>, own: Option<(u32, u32)>) -> Result<()> {
    let _lock = repo.p.lock_or_borrow()?;
    if repo.p.has_state(state) {
        return Ok(());
    }
    let rec = repo.state(state)?;
    if rec.kind != StateKind::Import {
        bail!("state {} is not an imported state", ids::short(state));
    }
    let commit =
        stored_commit(repo, state)?.with_context(|| format!("state {} has no Git commit in the store", ids::short(state)))?;
    let s = store(repo);
    let _t = crate::trace::span(format!("build {} from git", ids::short(state)));
    let mut cat = CatFile::new(&s)?;
    let raw = cat.object(&commit, "commit")?;
    let base = nearest_built_import(repo, state)?;
    let (_, changes) = import_changes(&raw, base.as_ref().map(|(_, c)| c.as_str()), &mut cat)?;
    let work = repo.p.work_dir().join(crate::store::temp_name());
    let prepared = match &base {
        Some((base, _)) if own.is_none() => thin_build(repo, &mut cat, &changes, &work, base, source, state)?,
        _ => None,
    };
    let captured = match prepared {
        Some(captured) => captured,
        None => {
            let temporary = match &base {
                Some((base, _)) => {
                    crate::repo::Temp::source(&repo.state_src(base)?, work.clone(), crate::fs::Protection::Writable)?
                }
                None => crate::repo::Temp::empty(work.clone())?,
            };
            let prepared = (|| -> Result<_> {
                let paths = base.as_ref().map(|_| {
                    changes.write.iter().map(|w| w.0.clone()).chain(changes.removed.iter().cloned()).collect::<Vec<_>>()
                });
                materialize(&s, &commit, &work, paths.as_deref(), None, source)?;
                if let Some((uid, gid)) = own {
                    crate::repo::give_tree(&work, uid, gid)?;
                }
                crate::fs::freeze_path_owned(&work, &repo.p.state_path(state), crate::fs::CapturePurpose::State)
            })();
            match prepared {
                Ok(captured) => {
                    if let Err(error) = temporary.discard() {
                        return Err(captured.abort(error));
                    }
                    captured
                }
                Err(error) => return Err(temporary.abort(error)),
            }
        }
    };
    match repo.p.lock_or_borrow()?.write(|tx| tx.set_present(state, true)) {
        Ok(()) => captured.release(),
        Err(crate::db::CommitResolution::NotCommitted(error)) => return Err(captured.abort(error)),
        Err(error) => {
            captured.release();
            return Err(error.into());
        }
    }
    if let Err(e) = keep_newest_built(&repo.p, state) {
        eprintln!("warning: older built Git trees stay: {e:#}");
    }
    Ok(())
}

/// The trees that git built for states that no tree owner keeps: the newest `import.trees` stay, so a second
/// `try` or `bisect` of the same commit reads its tree (`meta/built/<state>`).
pub const BUILT_TREES: u64 = 16;

fn built_dir(p: &crate::store::Project) -> PathBuf {
    p.meta_dir().join("built")
}

/// Whether `state` is among the trees that git built last (`keep_newest_built`).
pub fn recently_built(p: &crate::store::Project, state: &str) -> bool {
    built_dir(p).join(state).is_file()
}

/// Marks `state` as built now, and frees the trees of the built states beyond the newest `import.trees` that no
/// tree owner keeps and no thin layer uses as its base: git builds them again on read. The caller holds the
/// project lock.
fn keep_newest_built(p: &crate::store::Project, state: &str) -> Result<()> {
    let dir = built_dir(p);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(state), b"")?;
    let max = p.config_u64("import.trees", BUILT_TREES) as usize;
    let mut stamps: Vec<(std::time::SystemTime, String)> = std::fs::read_dir(&dir)?
        .flatten()
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.file_name().to_string_lossy().into_owned())))
        .filter(|(_, id)| crate::model::is_uuid(id))
        .collect();
    if stamps.len() <= max {
        return Ok(());
    }
    stamps.sort_by(|a, b| b.0.cmp(&a.0));
    let bases: BTreeSet<String> = p.db.present_layer_bases()?.into_iter().collect();
    let mut drop = Vec::new();
    for (_, id) in stamps.into_iter().skip(max) {
        // A base of a live layer keeps its mark: once the layer goes, a later build drops it here. Without the
        // mark nothing would look at the tree again, and only `gc --repair` would free it.
        if id == state || bases.contains(&id) {
            continue;
        }
        // A tree owner keeps it: the free step drops it when that owner goes.
        if !p.db.has_tree_owner(&id)? && p.has_state(&id) {
            drop.push(id.clone());
        }
        let _ = std::fs::remove_file(dir.join(&id));
    }
    for id in p.delete_states(&drop) {
        if !p.has_state(&id) {
            p.lock_or_borrow()?.write(|tx| tx.set_present(&id, false))?;
        }
    }
    Ok(())
}

/// A thin layer of the changes over the state `base`, written from the changed files only, as the
/// tree of `state`. False when the chain of `base` is long enough for a complete tree
/// (`thin.chain`), so reads stay short, and for a project that keeps complete trees.
fn thin_build(
    repo: &Repo,
    cat: &mut CatFile,
    changes: &Changes,
    work: &Path,
    base: &str,
    source: Option<&Path>,
    state: &str,
) -> Result<Option<crate::fs::OwnedCapture>> {
    let Some(storage) = repo.state_src(base)?.layered_storage().and_then(|storage| storage.for_import(&repo.p, base)) else {
        return Ok(None);
    };
    let temporary = crate::repo::Temp::directory(work.to_path_buf(), 0o755)?;
    let prepared = (|| -> Result<_> {
        let source = source.map(crate::tree::Tree::open).transpose()?;
        for (path, mode, oid) in &changes.write {
            if mode == "160000" {
                fsutil::make_parents(work, &format!("{path}/x"), None)?;
                continue;
            }
            let data = cat.object(oid, "blob")?;
            place_blob(work, mode, path, &data, None, source.as_ref())?;
        }
        storage.write_import(
            &repo.p,
            work,
            base,
            state,
            changes.write.iter().map(|w| w.0.clone()).collect(),
            changes.removed.clone(),
        )
    })();
    match prepared {
        Ok(captured) => {
            if let Err(error) = temporary.discard() {
                return Err(captured.abort(error));
            }
            Ok(Some(captured))
        }
        Err(error) => Err(temporary.abort(error)),
    }
}

/// Removes the given paths from an index before they are added again. A copied index keeps the
/// stat data of the base state, and git takes a listed path as unchanged when its size and mtime
/// second match. An edit of the same size in the same second then kept the old blob. The copy
/// also gives the index a new mtime, which turns off git's own check for such racy entries.
const FORGET_CHANGED: [&str; 4] = ["update-index", "--force-remove", "-z", "--stdin"];

/// The folder that git reads `paths` of a state from: its complete tree, or on APFS a private
/// copy of only those paths when the state is a thin layer.
fn work_tree(repo: &Repo, source: &crate::repo::Snap, paths: &[String], keep: &mut Vec<crate::repo::Temp>) -> Result<PathBuf> {
    use crate::fsutil::Src;
    if let Some(root) = source.folder() {
        return Ok(root.to_path_buf());
    }
    let path = repo.p.scan_dir().join(crate::store::temp_name());
    #[cfg(test)]
    crate::repo::MATERIALIZATIONS.with(|count| count.set(count.get() + 1));
    let temporary = crate::repo::Temp::selected(source, paths, path)?;
    let path = temporary.path.clone();
    keep.push(temporary);
    Ok(path)
}

/// A checkout borrowed from one immutable source, owning any temporary trees until Git has
/// finished both the index command and its refresh. It cannot survive or mutate its source.
struct ExportCheckout<'source> {
    source: &'source crate::repo::Snap,
    root: PathBuf,
    _keep: Vec<crate::repo::Temp>,
}

impl<'source> ExportCheckout<'source> {
    fn new(repo: &Repo, source: &'source crate::repo::Snap, paths: Option<&[String]>) -> Result<Self> {
        let _source = crate::trace::span("git export source materialization");
        let mut keep = Vec::new();
        let root = match paths {
            None => repo.complete(source.clone(), &mut keep)?,
            Some(paths) => work_tree(repo, source, paths, &mut keep)?,
        };
        Ok(Self { source, root, _keep: keep })
    }

    fn is_thin(&self) -> bool {
        use crate::fsutil::Src;
        self.source.folder().is_none()
    }
}

fn index_path(repo: &Repo, state: &str) -> PathBuf {
    store(repo).join("layr-index").join(state)
}

/// A git index of `commit` whose stat data matches the files of `state`.
pub fn build_index(repo: &Repo, state: &str, commit: &str, prev_state: Option<&str>) -> Result<()> {
    let s = store(repo);
    let idx = index_path(repo, state);
    let src = repo.state_src(state)?;
    let prev_idx = prev_state.map(|p| index_path(repo, p)).filter(|p| p.exists());
    let mut keep: Vec<crate::repo::Temp> = Vec::new();
    let run = |root: &Path, args: &[&str], input: Option<&[u8]>| -> Result<()> {
        let mut c = base_cmd(&s);
        c.env("GIT_INDEX_FILE", &idx)
            .env("GIT_WORK_TREE", root)
            .args(["-c", "core.checkStat=minimal", "-c", "core.trustctime=false"])
            .args(args);
        c.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::null()).stderr(Stdio::piped());
        let mut ch = c.spawn()?;
        if let Some(d) = input {
            ch.stdin.take().unwrap().write_all(d)?;
        }
        let out = ch.wait_with_output()?;
        if !out.status.success() && args[0] != "update-index" {
            bail!("git {} failed: {}", args[0], String::from_utf8_lossy(&out.stderr));
        }
        Ok(())
    };
    match (prev_idx, prev_state) {
        (Some(pi), Some(ps)) => {
            std::fs::copy(&pi, &idx)?;
            let prev_src = repo.state_src(ps)?;
            let mut paths = Vec::new();
            for c in changes::file_changes(&prev_src, &src)? {
                for p in [Some(c.path().clone()), c.old_path().cloned()].into_iter().flatten() {
                    if !skip_path(&p) {
                        paths.push(p);
                    }
                }
            }
            let mut input = Vec::new();
            for p in &paths {
                input.extend_from_slice(p.as_bytes());
                input.push(0);
            }
            let root = work_tree(repo, &src, &paths, &mut keep)?;
            // Changed paths are hashed again: see FORGET_CHANGED.
            let _t = crate::trace::span(format!("git index update of {} changed paths", paths.len()));
            run(&root, &FORGET_CHANGED, Some(&input))?;
            run(&root, &["update-index", "--add", "--remove", "-z", "--stdin"], Some(&input))?;
            // Unchanged files share size and time with the previous state, and the index
            // compares only those (core.checkStat=minimal, core.trustctime=false): their stat
            // data stay valid across a snapshot, so no refresh of every tracked file is needed.
        }
        _ => {
            let root = repo.complete(src, &mut keep)?;
            run(&root, &["read-tree", commit], None)?;
            let _t = crate::trace::span("git index refresh");
            run(&root, &["update-index", "-q", "--refresh"], None)?;
        }
    }
    Ok(())
}

// ----- export -----

// Full scanning is reserved for a failed object read; normal commands hash only objects they use.
thread_local! {
    static VERIFIED_OBJECTS: std::cell::RefCell<BTreeSet<(PathBuf, String)>> = const { std::cell::RefCell::new(BTreeSet::new()) };
}
fn repair_cache(repo: &Repo) -> Result<bool> {
    if !has_store(repo) {
        return Ok(false);
    }
    let s = store(repo);
    if base_cmd(&s)
        .args(["fsck", "--full", "--no-reflogs", "--no-dangling"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success()
    {
        return Ok(false);
    }
    let damaged = repo.p.dir.join(format!("git-damaged-{}", ids::new_id()));
    crate::gitpack::unmount(&s);
    forget_objects();
    std::fs::rename(&s, &damaged)?;
    std::fs::set_permissions(&damaged, std::fs::Permissions::from_mode(0o700))?;
    VERIFIED_OBJECTS.with(|objects| objects.borrow_mut().retain(|(path, _)| path != &s));
    init_store(repo)?;
    Ok(true)
}
fn cache_retry<T>(repo: &Repo, mut read: impl FnMut() -> Result<T>) -> Result<T> {
    match read() {
        Ok(value) => Ok(value),
        Err(error) => {
            if repair_cache(repo)? {
                read()
            } else {
                Err(error)
            }
        }
    }
}
fn check_object(s: &Path, cat: &mut CatFile, oid: &str, kind: &str) -> Result<()> {
    let key = (s.to_path_buf(), oid.to_string());
    if !VERIFIED_OBJECTS.with(|objects| objects.borrow().contains(&key)) {
        cat.object(oid, kind)?;
        VERIFIED_OBJECTS.with(|objects| objects.borrow_mut().insert(key));
    }
    Ok(())
}
fn check_commit(repo: &Repo, commit: &str) -> Result<()> {
    let s = store(repo);
    let mut cat = CatFile::new(&s)?;
    let raw = cat.object(commit, "commit")?;
    let tree = tree_id(&raw)?;
    check_object(&s, &mut cat, tree, "tree")?;
    for line in git_out(&s, &["ls-tree", "-r", "-t", "-d", tree])?.lines() {
        let fields: Vec<_> = line.split_whitespace().take(3).collect();
        if fields.get(1) == Some(&"tree") {
            check_object(&s, &mut cat, fields[2], "tree")?;
        }
    }
    Ok(())
}
fn check_reachable(repo: &Repo, commit: &str) -> Result<()> {
    let _verify = crate::trace::span("git reachable-object verification");
    let s = store(repo);
    let objects = {
        let _order = crate::trace::span("git reachable-object enumeration");
        git_out(&s, &["rev-list", "--objects", "--no-object-names", commit])?
    };
    let mut cat = CatFile::new(&s)?;
    for oid in objects.lines() {
        check_object(&s, &mut cat, oid, "")?;
    }
    Ok(())
}

/// Git objects are a derived local cache. Validate the exact object identity before publishing.
fn write_object(s: &Path, kind: &str, data: &[u8], expected: &str) -> Result<()> {
    let _write = crate::trace::span("Git write object");
    use layr_hashers::{Digest, GitCompatible};
    let header = format!("{kind} {}\0", data.len());
    let mut hash = GitCompatible::new();
    hash.update(header.as_bytes());
    hash.update(data);
    let oid = hex::encode(hash.finalize());
    if oid != expected {
        bail!("stored Git {kind} hash mismatch: expected {expected}, got {oid}");
    }
    let path = s.join("objects").join(&oid[..2]).join(&oid[2..]);
    if path.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::set_permissions(path.parent().unwrap(), std::fs::Permissions::from_mode(0o755))?;
    // Git objects are a rebuildable cache of durable records and state files. Publish complete
    // objects atomically, without forcing the whole filesystem twice for every loose object.
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension(format!("tmp-{}", ids::new_id()));
    let result = (|| -> Result<()> {
        let file = std::fs::OpenOptions::new().create_new(true).write(true).mode(0o444).open(&tmp)?;
        let mut compressed = flate2::write::ZlibEncoder::new(file, flate2::Compression::default());
        compressed.write_all(header.as_bytes())?;
        compressed.write_all(data)?;
        let file = compressed.finish()?;
        file.set_permissions(std::fs::Permissions::from_mode(0o444))?;
        drop(file);
        std::fs::rename(&tmp, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn restore_import(
    repo: &Repo,
    export: &ExportContext,
    state: &StateRec,
    parents: &[String],
    saved: serde_json::Value,
) -> Result<String> {
    let _restore = crate::trace::span("Git restore import");
    let saved: GitImport = serde_json::from_value(saved).context("invalid stored Git import")?;
    if saved.format != 1 {
        bail!("unsupported stored Git import format {}", saved.format);
    }
    let raw = STANDARD.decode(saved.commit)?;
    // The state DAG must still describe exactly the Git commit DAG.
    let mut git_parents = Vec::new();
    for line in raw.split(|b| *b == b'\n').take_while(|l| !l.is_empty()) {
        if let Some(parent) = line.strip_prefix(b"parent ") {
            git_parents.push(std::str::from_utf8(parent)?.to_string());
        }
    }
    // A state without parents for a commit with parents: the history ends at this commit (the
    // import came from a shallow clone). The store names it as a shallow commit below.
    let ends_history = parents.is_empty() && !git_parents.is_empty();
    if git_parents != parents && !ends_history {
        bail!("stored Git parents do not match state {}", ids::short(&state.id));
    }
    let s = &export.store;
    for (oid, bytes) in saved.trees {
        write_object(s, "tree", &STANDARD.decode(bytes)?, &oid)?;
    }
    let tree = tree_id(&raw)?;
    let listed = base_cmd(s).args(["ls-tree", "-r", "-z", tree]).output()?;
    if !listed.status.success() {
        bail!("cannot restore Git tree {tree}: {}", String::from_utf8_lossy(&listed.stderr));
    }
    let mut entries = Vec::new();
    for entry in listed.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let text = std::str::from_utf8(entry).context("Git path is not UTF-8")?;
        let (meta, path) = text.split_once('\t').context("bad Git tree entry")?;
        let fields: Vec<&str> = meta.split_whitespace().collect();
        if fields[1] == "blob" {
            entries.push((fields[0].to_string(), fields[2].to_string(), path.to_string()));
        }
    }
    // Check the whole tree in one process, then read only blobs absent from the local cache.
    let mut check = base_cmd(s).args(["cat-file", "--batch-check"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
    let mut input = check.stdin.take().unwrap();
    let ids: Vec<String> = entries.iter().map(|(_, oid, _)| oid.clone()).collect();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        for oid in ids {
            writeln!(input, "{oid}")?;
        }
        Ok(())
    });
    let mut output = std::io::BufReader::new(check.stdout.take().unwrap());
    let src = repo.state_src(&state.id)?;
    use crate::fsutil::Src;
    for (mode, oid, path) in entries {
        let mut result = String::new();
        output.read_line(&mut result)?;
        if result.trim_end().ends_with(" missing") {
            let file = src.at(&path)?;
            let data = if mode == "120000" {
                use std::os::unix::ffi::OsStrExt;
                std::fs::read_link(&file)?.as_os_str().as_bytes().to_vec()
            } else {
                let mut data = Vec::new();
                use std::os::unix::fs::OpenOptionsExt;
                let mut f =
                    std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&file)?;
                if !f.metadata()?.is_file() {
                    bail!("{} is not a regular file", file.display());
                }
                f.read_to_end(&mut data)?;
                data
            };
            write_object(s, "blob", &data, &oid)?;
        }
    }
    writer.join().map_err(|_| anyhow!("Git object check writer failed"))??;
    if !check.wait()?.success() {
        bail!("Git object check failed");
    }
    let commit = repo.p.db.git_commit_of(&state.id)?.context("Git import has no commit ID")?;
    write_object(s, "commit", &raw, &commit)?;
    if ends_history {
        let _refs_lock = fetch_write_lock(repo)?;
        add_shallow_locked(s, &BTreeSet::from([commit.clone()]))?;
    }
    git_out(s, &["update-ref", &history_ref(&state.id), &commit])?;
    Ok(commit)
}

/// The nearest ancestor state with a git commit.
pub fn mapped_ancestor(repo: &Repo, state: &str) -> Result<Option<(String, String)>> {
    cache_retry(repo, || mapped_ancestor_once(repo, state))
}

fn mapped_ancestor_once(repo: &Repo, state: &str) -> Result<Option<(String, String)>> {
    for a in repo.ancestors(state)? {
        if let Some(c) = repo.p.db.git_commit_of(&a)? {
            if git_out(&store(repo), &["cat-file", "-e", &format!("{c}^{{commit}}")]).is_ok() {
                check_commit(repo, &c)?;
                return Ok(Some((a, c)));
            }
            if repo.p.db.git_import(&a)?.is_some() {
                let c = export_commit(repo, &a)?;
                return Ok(Some((a, c)));
            }
        }
    }
    Ok(None)
}

/// One commit for a state on top of the nearest exported ancestor (`push --squash`): the tree of
/// the state, one parent. The same state gives the same commit.
pub fn make_commit(repo: &Repo, state: &StateRec, message: Option<&str>) -> Result<(String, Option<String>)> {
    cache_retry(repo, || make_commit_once(repo, state, message))
}

fn make_commit_once(repo: &Repo, state: &StateRec, message: Option<&str>) -> Result<(String, Option<String>)> {
    repo.source_state(&state.id)?;
    let anc = mapped_ancestor(repo, &state.id)?;
    if let Some((ast, ac)) = &anc {
        if ast == &state.id {
            return Ok((ac.clone(), None));
        }
    }
    let parents: Vec<String> = anc.iter().map(|(_, c)| c.clone()).collect();
    let export = ExportContext::new(repo)?;
    let c = commit_tree(
        repo,
        &export,
        state,
        anc.as_ref(),
        &parents,
        message,
        &format!("refs/layr/squash/{}", ids::display(&state.id)),
    )?;
    Ok((c, parents.into_iter().next()))
}

/// The git tree hash of a state: the content as git stores it, with the paths a push exports.
/// It is the same on every machine and in git, unlike the state ID. A state without a commit
/// gets one in the git store (an object only; no record, no ref).
pub fn tree_of(repo: &Repo, state: &str) -> Result<String> {
    object_of(repo, state, "")?.ok_or_else(|| anyhow!("the commit of state {} has no tree", ids::display(state)))
}

/// The git object id of a path in a state, as `git rev-parse <commit>:<path>` prints it for the
/// commit a push exports: a tree for a folder, a blob for a file, the root tree for an empty path.
/// `None` when the exported tree has no such path (absent, or a path a push does not export).
pub fn object_of(repo: &Repo, state: &str, path: &str) -> Result<Option<String>> {
    cache_retry(repo, || object_of_once(repo, state, path))
}

fn object_of_once(repo: &Repo, state: &str, path: &str) -> Result<Option<String>> {
    let s = store(repo);
    // A path query reads only its commit and ancestor trees. Avoid walking every unrelated
    // subtree merely to return the root tree ID of an already imported state.
    let mapped = repo.p.db.git_commit_of(state)?.filter(|c| git_out(&s, &["cat-file", "-e", &format!("{c}^{{commit}}")]).is_ok());
    let commit = match mapped {
        Some(c) => c,
        None => match commit_of(repo, state)? {
            Some(c) => c,
            None => export_commit(repo, state)?,
        },
    };
    with_objects(repo, |cat| {
        let Some(found) = path_entry(cat, &commit, path)? else {
            return Ok(None);
        };
        // Gitlinks name commits in another repository; their containing tree authenticates the ID.
        if !found.is_link() {
            check_object(&s, cat, &found.oid, if found.is_tree() { "tree" } else { "blob" })?;
        }
        Ok(Some(found.oid))
    })
}

/// The tree entry at `path` of `commit` (the root tree for ""), read through `cat`: only the trees
/// on the path, one per component; each costs O(its entries). `None` when no entry is there.
fn path_entry(cat: &mut CatFile, commit: &str, path: &str) -> Result<Option<TreeEntry>> {
    let mut found = Some(root_entry(cat, commit)?);
    for component in components(path) {
        found = child_entry(cat, found, component)?;
    }
    Ok(found)
}

/// The entries at `path` of the trees of two commits (`old` None: an empty tree) when they differ, read down both
/// together: None as soon as they hold the same folder above it.
fn entry_pair(
    cat: &mut CatFile,
    old: Option<&str>,
    new: &str,
    path: &str,
) -> Result<Option<(Option<TreeEntry>, Option<TreeEntry>)>> {
    let mut pair = (old.map(|o| root_entry(cat, o)).transpose()?, Some(root_entry(cat, new)?));
    for component in components(path) {
        if pair.0 == pair.1 {
            return Ok(None);
        }
        pair = (child_entry(cat, pair.0, component)?, child_entry(cat, pair.1, component)?);
    }
    Ok((pair.0 != pair.1).then_some(pair))
}

/// The root folder of a commit, as a tree entry.
fn root_entry(cat: &mut CatFile, commit: &str) -> Result<TreeEntry> {
    Ok(TreeEntry { mode: "40000".into(), oid: tree_id(&cat.object(commit, "commit")?)?.to_string() })
}

/// The names of a path below the root ("" and "/" are the root).
fn components(path: &str) -> impl Iterator<Item = &str> {
    let path = path.trim_matches('/');
    path.split('/').filter(move |_| !path.is_empty())
}

/// The entry `name` of the folder `entry`; None when it is no folder or has no such entry.
fn child_entry(cat: &mut CatFile, entry: Option<TreeEntry>, name: &str) -> Result<Option<TreeEntry>> {
    match entry {
        Some(folder) if folder.is_tree() => tree_find(&cat.object(&folder.oid, "tree")?, name.as_bytes()),
        _ => Ok(None),
    }
}

/// The entry `name` of the raw Git tree `tree`, scanning up to it.
fn tree_find(tree: &[u8], name: &[u8]) -> Result<Option<TreeEntry>> {
    for record in tree_records(tree) {
        let (mode, found, oid) = record?;
        if found == name {
            return Ok(Some(TreeEntry::new(mode, oid)));
        }
    }
    Ok(None)
}

/// The commit of a state in the git store, if it has one: the commit a record names (import,
/// push, git-sync), or else the history commit made for the state without a record
/// (`history_commit`).
pub fn commit_of(repo: &Repo, state: &str) -> Result<Option<String>> {
    if let Some(c) = recorded_commit(repo, state)? {
        return Ok(Some(c));
    }
    history_commit(repo, state)
}

/// The commit that a record names for a state (import, push, git-sync), if it is in the store.
/// A squash commit is never one: it has the parent of an earlier export, not the history.
fn recorded_commit(repo: &Repo, state: &str) -> Result<Option<String>> {
    let commit = stored_commit(repo, state)?;
    if let Some(c) = &commit {
        check_commit(repo, c)?;
    }
    Ok(commit)
}

/// The ref of the history commit of a state: the commit `export_history` makes for it, with the
/// commits of its parent states as parents. The same state always gives the same commit, so the
/// ref only saves the work of making it again; it is not a record and leaves the machine only
/// in a push. (Commits of `push --squash` have other parents: `refs/layr/squash/<state>`.)
fn history_ref(state: &str) -> String {
    format!("refs/layr/states/{}", ids::display(state))
}

fn history_commit(repo: &Repo, state: &str) -> Result<Option<String>> {
    let spec = format!("{}^{{commit}}", history_ref(state));
    let commit = git_out(&store(repo), &["rev-parse", "--verify", "-q", &spec])
        .ok()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty());
    if let Some(commit) = &commit {
        check_commit(repo, commit)?;
    }
    Ok(commit)
}

/// The state of a git commit that layr made without a record (a history commit, for example the
/// `HEAD` of a line's `.git` folder). `name` is a full commit id or a prefix.
pub fn state_of_history_commit(repo: &Repo, name: &str) -> Result<Option<String>> {
    if !has_store(repo) {
        return Ok(None);
    }
    let s = store(repo);
    let commit = match git_out(&s, &["rev-parse", "--verify", "-q", &format!("{name}^{{commit}}")]) {
        Ok(c) => c.trim().to_string(),
        Err(_) => return Ok(None),
    };
    let refs = git_out(&s, &["for-each-ref", "--format=%(refname)", "--points-at", &commit, "refs/layr/states/"])?;
    for r in refs.lines() {
        let display = r.trim_start_matches("refs/layr/states/");
        if let Some(st) = repo.p.db.states_by_prefix(display)?.into_iter().find(|st| ids::display(st) == display) {
            return Ok(Some(st));
        }
    }
    Ok(None)
}

/// Commits for a state and every ancestor state without one, parents first, each with the same
/// parents as its state (a merge stays a merge). The same states give the same commits. Returns
/// the commit of `state` and the new (state, commit) pairs.
pub fn export_history(repo: &Repo, state: &crate::repo::SourceStateId) -> Result<(String, Vec<(String, String)>)> {
    cache_retry(repo, || export_history_once(repo, state, true))
}

/// The commit of `state`, as `export_history` makes it, without naming the history commits that
/// no record names yet: a history commit's parents have theirs, so the walk stops there. A line's
/// `.git` folder needs only this; without the stop each save walks every earlier unpushed save.
pub fn export_commit(repo: &Repo, state: &str) -> Result<String> {
    let source = repo.source_id(state)?;
    cache_retry(repo, || export_history_once(repo, &source, false)).map(|(c, _)| c)
}

fn export_history_once(
    repo: &Repo,
    state: &crate::repo::SourceStateId,
    name_history: bool,
) -> Result<(String, Vec<(String, String)>)> {
    let _export = crate::trace::span("git export history");
    let dag = crate::trace::span("git export DAG ordering");
    let mut known: BTreeMap<String, String> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<(String, bool)> = vec![(state.as_str().to_string(), false)];
    let mut made = Vec::new();
    while let Some((s, parents_done)) = stack.pop() {
        if parents_done {
            order.push(s);
            continue;
        }
        if !seen.insert(s.clone()) {
            continue;
        }
        repo.source_state(&s)?;
        if let Some(c) = recorded_commit(repo, &s)? {
            known.insert(s, c);
            continue;
        }
        // A history commit made earlier without a record: a record of this export names it.
        if let Some(c) = history_commit(repo, &s)? {
            made.push((s.clone(), c.clone()));
            known.insert(s.clone(), c);
            if !name_history {
                continue;
            }
            // Its parents have history commits too. The record names them all, so later pushes,
            // their secret check and `foreign_base` stop at them.
            for p in repo.state(&s)?.parents.iter().rev() {
                stack.push((p.clone(), false));
            }
            continue;
        }
        let rec = repo.state(&s)?;
        stack.push((s, true));
        for p in rec.parents.iter().rev() {
            stack.push((p.clone(), false));
        }
    }
    drop(dag);
    if !order.is_empty() {
        let export = ExportContext::new(repo)?;
        for s in order {
            let rec = repo.state(&s)?;
            let parents: Vec<String> = rec
                .parents
                .iter()
                .map(|p| known.get(p).cloned().ok_or_else(|| anyhow!("no commit for state {}", ids::short(p))))
                .collect::<Result<_>>()?;
            let base = rec.parents.first().map(|p| (p.clone(), known[p].clone()));
            let c = if let Some(saved) = repo.p.db.git_import(&s)? {
                restore_import(repo, &export, &rec, &parents, saved)?
            } else {
                commit_tree(repo, &export, &rec, base.as_ref(), &parents, None, &history_ref(&s))?
            };
            // A state that changed only the parts of overlays has the tree of its parent: git gets
            // no empty commit, the state maps to its parent's commit (docs/overlays.md section 1a).
            let c = match (rec.parents.as_slice(), base.as_ref()) {
                ([parent], Some((_, parent_commit)))
                    if !rec.parts.is_empty()
                        && repo.state(parent)?.parts != rec.parts
                        && same_tree(&export.store, &c, parent_commit) =>
                {
                    // The history ref of the state names the parent's commit too, so a later export
                    // (`history_commit`) gives the same commit as this one.
                    git_out(&export.store, &["update-ref", &history_ref(&s), parent_commit])?;
                    parent_commit.clone()
                }
                _ => c,
            };
            known.insert(s.clone(), c.clone());
            made.push((s, c));
        }
    }
    let tip = known.get(state.as_str()).cloned().ok_or_else(|| anyhow!("no commit for state {}", ids::short(state.as_str())))?;
    Ok((tip, made))
}

/// Whether two commits of the git store have the same tree.
fn same_tree(store: &Path, a: &str, b: &str) -> bool {
    match (git_out(store, &["rev-parse", &format!("{a}^{{tree}}")]), git_out(store, &["rev-parse", &format!("{b}^{{tree}}")])) {
        (Ok(x), Ok(y)) => x.trim() == y.trim(),
        _ => false,
    }
}

/// Store and signing configuration shared by all commits in one export attempt.
struct ExportContext {
    store: PathBuf,
    signing_email: Option<String>,
}

impl ExportContext {
    fn new(repo: &Repo) -> Result<Self> {
        Ok(Self::load(init_store(repo)?))
    }

    fn load(store: PathBuf) -> Self {
        let signing_email = if git_out(&store, &["config", "--bool", "commit.gpgsign"]).is_ok_and(|v| v.trim() == "true") {
            git_out(&store, &["config", "user.email"]).ok().map(|v| v.trim().to_string())
        } else {
            None
        };
        Self { store, signing_email }
    }

    fn signs(&self, email: &str) -> bool {
        self.signing_email.as_deref() == Some(email)
    }

    fn commit(&self, state: &StateRec, tree: &str, parents: &[String], message: Option<&str>) -> Result<String> {
        let s = &self.store;
        let msg = message.map(|m| m.to_string()).unwrap_or_else(|| {
            if state.message.is_empty() {
                "layr state".to_string()
            } else {
                state.message.clone()
            }
        });
        let secs = state.time / 1000;
        let date = format!("{secs} +0000");
        let commit_tree = |sign: bool| -> Result<std::process::Output> {
            let mut c = base_cmd(s);
            c.args(["commit-tree", tree]);
            if sign {
                c.arg("-S");
            }
            for p in parents {
                c.args(["-p", p]);
            }
            c.env("GIT_AUTHOR_NAME", &state.author.name)
                .env("GIT_AUTHOR_EMAIL", &state.author.email)
                .env("GIT_AUTHOR_DATE", &date)
                .env("GIT_COMMITTER_NAME", &state.author.name)
                .env("GIT_COMMITTER_EMAIL", &state.author.email)
                .env("GIT_COMMITTER_DATE", &date);
            c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut ch = c.spawn()?;
            ch.stdin.take().unwrap().write_all(msg.as_bytes())?;
            Ok(ch.wait_with_output()?)
        };
        // `git commit-tree` ignores commit.gpgSign. Sign as git commit would, but only the
        // caller's own states: another person's state must not carry the caller's signature.
        let sign = self.signs(&state.author.email);
        let mut out = commit_tree(sign)?;
        if sign && !out.status.success() {
            eprintln!(
                "warning: could not sign the commit of state {} ({}); it is written unsigned",
                state.id,
                String::from_utf8_lossy(&out.stderr).trim()
            );
            out = commit_tree(false)?;
        }
        if !out.status.success() {
            bail!("git commit-tree failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

/// Fast-import owns pack encoding, compression and atomic pack publication. Keep the final
/// commit in `ExportContext::commit`: fast-import must not reinterpret its identity or signing.
/// The private branch is reset before `done`, so even a successful import publishes no ref.
fn pack_tree(s: &Path, root: &Path, paths: &[String]) -> Result<String> {
    use std::os::unix::ffi::OsStrExt;
    let _pack = crate::trace::span("git export pack stream");
    let branch = format!("refs/layr/pack/{}", ids::new_id());
    let mut child = base_cmd(s)
        .args(["-c", "fastimport.unpackLimit=0", "fast-import", "--quiet", "--done"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stream = (|| -> Result<()> {
        let mut input = std::io::BufWriter::new(child.stdin.take().context("Git import has no input")?);
        writeln!(input, "commit {branch}\ncommitter Layr <layr@example.invalid> 0 +0000\ndata 0")?;
        for path in paths {
            let file = root.join(path);
            let meta = std::fs::symlink_metadata(&file)?;
            // update-index uses the owner execute bit, even when other execute bits differ.
            let mode = if meta.file_type().is_symlink() {
                0o120000
            } else if meta.permissions().mode() & 0o100 != 0 {
                0o100755
            } else {
                0o100644
            };
            // Git's C quoting is byte based (JSON's Unicode escapes are not accepted).
            write!(input, "M {mode:o} inline \"")?;
            for byte in path.bytes() {
                match byte {
                    b'"' | b'\\' => write!(input, "\\{}", char::from(byte))?,
                    0x20..=0x7e => input.write_all(&[byte])?,
                    _ => write!(input, "\\{byte:03o}")?,
                }
            }
            writeln!(input, "\"")?;
            if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&file)?;
                let data = target.as_os_str().as_bytes();
                writeln!(input, "data {}", data.len())?;
                input.write_all(data)?;
            } else {
                let fd = rustix::fs::open(
                    &file,
                    rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK,
                    rustix::fs::Mode::empty(),
                )?;
                let mut file = std::fs::File::from(fd);
                let meta = file.metadata()?;
                if !meta.is_file() {
                    bail!("{} is not a regular file", path);
                }
                let size = meta.len();
                writeln!(input, "data {size}")?;
                if std::io::copy(&mut Read::by_ref(&mut file).take(size), &mut input)? != size {
                    bail!("{path} changed during Git export");
                }
            }
            writeln!(input)?;
        }
        writeln!(input, "ls \"\"\n\nreset {branch}\n\ndone")?;
        input.flush()?;
        Ok(())
    })();
    if stream.is_err() {
        let _ = child.kill();
    }
    let output = child.wait_with_output()?;
    stream?;
    if !output.status.success() {
        bail!("git fast-import failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    let listing = std::str::from_utf8(&output.stdout)?;
    let tree = listing
        .strip_prefix("040000 tree ")
        .and_then(|l| l.split_once('\t'))
        .map(|(oid, _)| oid)
        .context("Git import did not return its root tree")?;
    Ok(tree.to_string())
}

/// Only initial exports use a pack. Later exports retain changed-path/index reuse. Git's
/// index applies attributes and clean/EOL conversions; keep that path whenever they may apply.
fn can_pack_tree(s: &Path, root: &Path, paths: &[u8]) -> Result<bool> {
    if git_out(s, &["config", "--get", "core.autocrlf"]).is_ok_and(|v| v.trim() != "false") {
        return Ok(false);
    }
    if git_out(s, &["config", "--bool", "core.filemode"]).is_ok_and(|v| v.trim() == "false")
        || git_out(s, &["config", "--bool", "core.ignorecase"]).is_ok_and(|v| v.trim() == "true")
    {
        return Ok(false);
    }
    let mut child = base_cmd(s)
        .env("GIT_WORK_TREE", root)
        .args(["check-attr", "--all", "-z", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = child.stdin.take().context("Git attributes has no input")?;
    // Attribute output may exceed a pipe: consume it while writing the path list.
    let output = std::thread::scope(|scope| -> Result<_> {
        let writer = scope.spawn(move || input.write_all(paths));
        let output = child.wait_with_output()?;
        writer.join().map_err(|_| anyhow!("Git attributes writer failed"))??;
        Ok(output)
    })?;
    if !output.status.success() {
        bail!("git check-attr failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(output.stdout.is_empty())
}

/// Write the commit of a state: the tree from the index of `base` (a state with its commit) plus
/// the paths that changed since, the given parent commits, author and committer from the state.
/// `refname` is set to the commit.
fn commit_tree(
    repo: &Repo,
    export: &ExportContext,
    state: &StateRec,
    base: Option<&(String, String)>,
    parents: &[String],
    message: Option<&str>,
    refname: &str,
) -> Result<String> {
    let _state = crate::trace::span("git export state");
    let s = &export.store;
    let src = repo.state_src(&state.id)?;
    let tmp_idx = std::env::temp_dir().join(format!("layr-index-{}", ids::new_id()));
    let res = (|| -> Result<String> {
        let mut paths: Vec<String> = Vec::new();
        let mut full = None;
        // True when the index is a copy of the index of the base: its entries for unchanged
        // files have valid stat data, so no refresh of every tracked file is needed.
        let mut inherited = false;
        match base {
            Some((ast, ac)) => {
                let ai = index_path(repo, ast);
                // gc may remove a kept index at any time: then the tree of the commit is read.
                inherited = ai.exists() && std::fs::copy(&ai, &tmp_idx).is_ok();
                if !inherited {
                    let out = base_cmd(s).env("GIT_INDEX_FILE", &tmp_idx).args(["read-tree", ac]).output()?;
                    if !out.status.success() {
                        bail!("git read-tree failed");
                    }
                }
                let aroot = repo.state_src(ast)?;
                let rules = crate::ignore_rules::Rules::of(&src);
                for c in changes::file_changes(&aroot, &src)? {
                    for p in [Some(c.path().clone()), c.old_path().cloned()].into_iter().flatten() {
                        if skip_path(&p) {
                            continue;
                        }
                        // Ignored paths are never exported, unless the ancestor tracked them.
                        if c.old().is_none() && rules.ignored(&p, false) {
                            continue;
                        }
                        paths.push(p);
                    }
                }
            }
            None => {
                let checkout = ExportCheckout::new(repo, &src, None)?;
                let rules = crate::ignore_rules::Rules::new(&checkout.root);
                fsutil::walk(&checkout.root, "", &mut |rel, m| {
                    if skip_path(rel) || rules.ignored(rel, m.is_dir()) {
                        return Ok(false);
                    }
                    if !m.is_dir() {
                        paths.push(rel.to_string());
                    }
                    Ok(true)
                })?;
                full = Some(checkout);
            }
        }
        let mut input = Vec::new();
        for p in &paths {
            input.extend_from_slice(p.as_bytes());
            input.push(0);
        }
        let checkout = match full {
            Some(checkout) => checkout,
            None => ExportCheckout::new(repo, &src, Some(&paths))?,
        };
        // Changed paths are hashed again: see FORGET_CHANGED.
        let _t = crate::trace::span(format!("git index update of {} changed paths", paths.len()));
        let tree = if base.is_none() && can_pack_tree(s, &checkout.root, &input)? {
            let tree = pack_tree(s, &checkout.root, &paths)?;
            let _index = crate::trace::span("git export index command");
            let out = base_cmd(s).env("GIT_INDEX_FILE", &tmp_idx).args(["read-tree", &tree]).output()?;
            if !out.status.success() {
                bail!("git read-tree failed: {}", String::from_utf8_lossy(&out.stderr));
            }
            tree
        } else {
            let index_command = crate::trace::span("git export index command");
            for args in [&FORGET_CHANGED[..], &["update-index", "--add", "--remove", "-z", "--stdin"]] {
                let mut c = base_cmd(s);
                c.env("GIT_INDEX_FILE", &tmp_idx).env("GIT_WORK_TREE", &checkout.root).args(args);
                c.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
                let mut ch = c.spawn()?;
                ch.stdin.take().unwrap().write_all(&input)?;
                let out = ch.wait_with_output()?;
                if !out.status.success() {
                    bail!("git update-index failed: {}", String::from_utf8_lossy(&out.stderr));
                }
            }
            drop(index_command);
            let tree_write = crate::trace::span("git export tree write");
            let tree =
                String::from_utf8_lossy(&base_cmd(s).env("GIT_INDEX_FILE", &tmp_idx).args(["write-tree"]).output()?.stdout)
                    .trim()
                    .to_string();
            if tree.is_empty() {
                bail!("git write-tree failed");
            }
            drop(tree_write);
            tree
        };
        let commit_write = crate::trace::span("git export commit and ref write");
        let commit = export.commit(state, &tree, parents, message)?;
        git_out(s, &["update-ref", refname, &commit])?;
        drop(commit_write);
        // Keep the index for the next export and for lines made from this state.
        keep_index(repo, &state.id, &tmp_idx, inherited, &checkout)?;
        Ok(commit)
    })();
    let _ = std::fs::remove_file(&tmp_idx);
    res
}

/// Keep `idx` as the index of `state` unless it has one. An index that is not a copy of an
/// earlier index gets its stat data from one refresh (none for a thin state: its index already has
/// them). The file appears complete or not at all: other processes copy kept indexes without the
/// project lock. Without it, the next export of every line read the whole tree of its commit
/// (main's behavior since 0.47; the git-view merge had dropped it, and the apfs check failed).
fn keep_index(repo: &Repo, state: &str, idx: &Path, inherited: bool, checkout: &ExportCheckout<'_>) -> Result<()> {
    let _retain = crate::trace::span("git export index retention");
    let dst = index_path(repo, state);
    if dst.exists() {
        return Ok(());
    }
    let tmp = dst.with_file_name(format!("{state}.{}.tmp", ids::new_id()));
    let r = std::fs::copy(idx, &tmp).map(|_| ()).and_then(|_| std::fs::rename(&tmp, &dst));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r.context("keep the git index of the exported state")?;
    if !inherited {
        let _ = refresh_index(repo, state, checkout);
    }
    Ok(())
}

fn refresh_index(repo: &Repo, state: &str, checkout: &ExportCheckout<'_>) -> Result<()> {
    // An index made from the previous index and the changed paths of a thin state has valid
    // stat data already; a refresh would need a complete tree.
    if checkout.is_thin() {
        return Ok(());
    }
    let _t = crate::trace::span("git index refresh");
    let s = store(repo);
    base_cmd(&s)
        .env("GIT_INDEX_FILE", index_path(repo, state))
        .env("GIT_WORK_TREE", &checkout.root)
        .args(["-c", "core.checkStat=minimal", "-c", "core.trustctime=false", "update-index", "-q", "--refresh"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(())
}

/// Remove kept indexes that nothing needs soon: a state keeps its index while it is the head of
/// a line or a record names its commit (imports and exports). Others are made again when needed
/// (`line_commit`). Leftover temporary files of an interrupted write go after an hour.
pub fn prune_indexes(repo: &Repo) -> Result<usize> {
    let dir = store(repo).join("layr-index");
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(0),
    };
    let heads: BTreeSet<String> = repo.p.db.lines()?.into_iter().map(|l| l.head).collect();
    let mut n = 0;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let remove = if name.ends_with(".tmp") {
            e.metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| t.elapsed().map(|x| x.as_secs() > 3600).unwrap_or(false))
                .unwrap_or(false)
        } else {
            !heads.contains(&name) && repo.p.db.git_commit_of(&name)?.is_none()
        };
        if remove && std::fs::remove_file(e.path()).is_ok() {
            n += 1;
        }
    }
    Ok(n)
}

/// The commit that the `.git` folder of a line shows for `state`: the commit that `layr git push`
/// would send for exactly this state (made in the git store when it has none, without a record),
/// and a kept index of that state whose stat data matches files copied from it.
pub fn line_commit(repo: &Repo, state: &str) -> Result<String> {
    let c = export_commit(repo, state)?;
    if !index_path(repo, state).exists() {
        let prev = repo.state(state)?.parents.first().cloned().filter(|p| index_path(repo, p).exists());
        build_index(repo, state, &c, prev.as_deref())?;
    }
    Ok(c)
}

/// Push a prepared commit to `refs/heads/<branch>` of the remote, as the caller.
pub fn push(ctx: &Ctx, repo: &Repo, url: &str, commit: &str, branch: &str, force: bool) -> Result<()> {
    let _push = crate::trace::span("git push prepared commit");
    if let Err(error) = check_reachable(repo, commit) {
        if !repair_cache(repo)? {
            return Err(error);
        }
        // The damaged cache went aside. Imported states have no trees to rebuild their commits from,
        // so the Git remotes of the project give the objects back (git is the source).
        let mut remotes: Vec<String> =
            repo.p.db.remote_refs()?.into_iter().filter_map(|(name, _, _)| name.split('/').next().map(str::to_owned)).collect();
        remotes.sort();
        remotes.dedup();
        for remote in remotes {
            if repo.p.config_value(&format!("remote.{remote}.url"))?.is_some() {
                fetch(ctx, repo, &remote)?;
            }
        }
        crate::store_remote::restore_git_objects(ctx, repo)?;
        let mut rebuilt = false;
        for state in repo.p.db.state_of_commit(commit)? {
            if repo.tree_available(&state) && export_commit(repo, &state)? == commit {
                rebuilt = true;
                break;
            }
        }
        if !rebuilt {
            bail!("cannot rebuild prepared Git commit {commit}; prepare the export again");
        }
        check_reachable(repo, commit)?;
    }
    let s = store(repo);
    let before = {
        let _remote = crate::trace::span("git push remote discovery");
        ls_remote(ctx, repo, url, branch)?
    };
    if !force {
        if let Some(remote) = &before {
            if remote != commit
                && !base_cmd(&s)
                    .args(["merge-base", "--is-ancestor", remote, commit])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()?
                    .success()
            {
                bail!("push rejected: non-fast-forward update of '{branch}' from {remote} to {commit}.\nRun 'layr git pull' (fetch and merge), then push again, or use --force.");
            }
        }
    }
    // Blobs that the git store reads from the object store (`gitpack`) are not compressed, and
    // git sends a reused entry as it is: a compressed pack of the push goes first (git prefers a
    // local pack).
    if crate::gitpack::uses_folder(&s) {
        compressed_pack(&s, commit, before.as_deref())?;
    }
    let mut c = base_cmd(&s);
    c.args(["push", "-q", url]);
    let spec = format!("{}{commit}:refs/heads/{branch}", if force { "+" } else { "" });
    c.arg(spec);
    as_caller(ctx, &mut c, &s)?;
    let out = {
        let _subprocess = crate::trace::span("git push subprocess");
        c.stdin(Stdio::null()).output().context("run git push")?
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("non-fast-forward") || err.contains("fetch first") || err.contains("rejected") {
            let remote = ls_remote(ctx, repo, url, branch)?.or(before).unwrap_or_else(|| "(absent)".into());
            bail!("push rejected: non-fast-forward update of '{branch}' from {remote} to {commit}.\nRun 'layr git pull' (fetch and merge), then push again, or use --force.\n{}", err.trim());
        }
        bail!("git push failed: {}", err.trim());
    }
    Ok(())
}

/// A compressed pack in the git store of the objects of `commit` that `have` (when the store has
/// it) does not reach.
fn compressed_pack(s: &Path, commit: &str, have: Option<&str>) -> Result<()> {
    let mut c = base_cmd(s);
    c.args(["pack-objects", "--revs", "--no-reuse-object", "--local", "-q"])
        .arg(s.join("objects/pack/pack"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = c.spawn().context("start git pack-objects")?;
    {
        let mut input = child.stdin.take().context("git pack-objects has no input")?;
        writeln!(input, "{commit}")?;
        if let Some(h) = have.filter(|h| git_out(s, &["cat-file", "-e", &format!("{h}^{{commit}}")]).is_ok()) {
            writeln!(input, "^{h}")?;
        }
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("git pack-objects failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

// ----- the .git folder of a line -----

/// Give a new working folder a `.git` folder (a nested subvolume) so tools that call git see the
/// commit of the folder's state (`line_commit`). Objects come from the project store through
/// `alternates`.
pub fn make_git_folder(repo: &Repo, line: &Line, state: &str, w: &Path) -> Result<()> {
    if !has_store(repo) {
        if !repo.p.db.has_git_history()? {
            return Ok(());
        }
        init_store(repo)?;
    }
    // The folder of a fast line is a LayrFS mount, which holds no nested subvolume: its `.git`
    // folder lives in the cache root of the line, and the mount shows it as a link.
    let base = repo.cache_base(line, w);
    let fast = base != w;
    if fast {
        crate::fast_cache::ensure_root(repo, line)?;
    }
    let g = base.join(".git");
    if let Ok(m) = std::fs::symlink_metadata(&g) {
        if m.is_dir() && !crate::fs::host().namespace.managed(&g) && std::fs::read_dir(&g)?.next().is_none() {
            std::fs::remove_dir(&g)?;
        } else {
            return Ok(());
        }
    }
    crate::fs::create(&g)?;
    let s = store(repo);
    for d in ["objects/info", "refs/heads", "refs/tags", "info"] {
        std::fs::create_dir_all(g.join(d))?;
    }
    std::fs::write(g.join("objects/info/alternates"), format!("{}\n", s.join("objects").display()))?;
    // The folder borrows the objects of the store, so it has the store's shallow commits.
    let shallow = store_shallow(&s);
    if !shallow.is_empty() {
        std::fs::write(g.join("shallow"), shallow_text(&shallow))?;
    }
    let mut config =
        String::from("[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tcheckStat = minimal\n\ttrustctime = false\n");
    if fast {
        // Git writes this folder, and the line follows (`git_moved`).
        config.push_str(&fast_config());
        // A split index: git writes the changed entries to `index` and keeps the rest in a shared
        // file, so a commit or a status in a large tree writes kilobytes, not the whole index.
        // Layr replaces `index` with a whole index; git splits it again at its next write. A
        // shared file that no index uses goes after an hour.
        config.push_str("\tsplitIndex = true\n[splitIndex]\n\tsharedIndexExpire = 1.hour.ago\n");
        // Each file that a checkout writes costs several requests to the view (unlink, create,
        // writes, getattr): writers in parallel hide their wait (git uses them from 100 files).
        // Kernel tree, 1,260 files: 2.4 s with one writer, 1.7 s with one per core.
        config.push_str("[checkout]\n\tworkers = 0\n");
        write_hooks(&g)?;
    } else {
        config.push_str("\tlogallrefupdates = false\n");
    }
    std::fs::write(g.join("config"), config)?;
    // The project's local ignore patterns (`ignore.local`, from `init --from-git`): git and layr read them.
    let mut local: Vec<String> =
        repo.p.config_value("ignore.local")?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    // The cache links of a fast line are links for git; it leaves them out (`fast_cache::link`).
    if fast {
        if let Some(root) = crate::fast_cache::root_for(repo, line, w) {
            local.extend(crate::fast_cache::links(&root, w).into_iter().map(|rel| format!("/{rel}")));
        }
    }
    std::fs::write(g.join("info/exclude"), local.iter().map(|p| format!("{p}\n")).collect::<String>())?;
    // The state's own commit; if it cannot be made, the nearest commit a record names.
    let anc = match line_commit(repo, state) {
        Ok(c) => Some((state.to_string(), c)),
        Err(e) => {
            let ancestor = mapped_ancestor(repo, state).ok().flatten();
            let fallback = if ancestor.is_some() { "shows an older commit" } else { "has an unborn HEAD" };
            eprintln!("warning: .git folder {fallback}: {e:#}");
            ancestor
        }
    };
    // A fast line is on a branch with the name of the line, so `git commit` moves that branch.
    let branch = Some(line.name.as_str()).filter(|n| fast && branch_name_ok(n));
    match (&anc, branch) {
        (anc, Some(b)) => {
            if let Some((_, c)) = anc {
                if let Some(parent) = Path::new(b).parent() {
                    std::fs::create_dir_all(g.join("refs/heads").join(parent))?;
                }
                std::fs::write(g.join("refs/heads").join(b), format!("{c}\n"))?;
            }
            std::fs::write(g.join("HEAD"), format!("ref: refs/heads/{b}\n"))?;
        }
        (Some((_, c)), None) => std::fs::write(g.join("HEAD"), format!("{c}\n"))?,
        (None, None) => std::fs::write(g.join("HEAD"), "ref: refs/heads/main\n")?,
    }
    if anc.as_ref().is_none_or(|(ancestor, _)| ancestor != state) {
        let head = std::fs::read_to_string(g.join("HEAD"))?;
        std::fs::write(g.join(FALLBACK_HEAD), format!("{state}\n{head}"))?;
    }
    std::fs::write(g.join("packed-refs"), packed_refs_text(repo).unwrap_or_else(|_| PACKED_HEADER.to_string()))?;
    if let Some((ast, _)) = &anc {
        let idx = index_path(repo, ast);
        if idx.exists() {
            let src = std::fs::File::open(&idx)?;
            let dst = std::fs::File::create(g.join("index"))?;
            layr_fs_unix::copy_contents(&src, &dst)?;
        }
    }
    if let Some((_, c)) = &anc {
        std::fs::write(g.join(INDEX_OF), format!("{c}\n"))?;
    }
    if fast {
        clone_store_packs(&s, &g);
    }
    let (u, gid) = repo.owner_ids(line);
    let mut paths = Vec::new();
    fsutil::walk(&g, "", &mut |rel, _| {
        paths.push(rel.to_string());
        Ok(true)
    })?;
    fsutil::chown_nofollow(&g, u, gid)?;
    for p in paths {
        fsutil::chown_nofollow(&g.join(p), u, gid)?;
    }
    if fast {
        crate::fast_cache::link(w, ".git", &g, u, gid)?;
        return Ok(());
    }
    // Read only for git: a git write needs lock files and new objects; layr writes through descriptors.
    fsutil::git_writable(&base, false);
    Ok(())
}

/// Where the last move of HEAD came from, when it was a checkout (`git checkout`, `git switch`)
/// from the branch or the detached commit that is `c` now: `refs/heads/<branch>`, or the commit.
/// Git's reflog of HEAD names it ("checkout: moving from <from> to <to>"). None for every other
/// last move (a commit, a reset, a move by layr).
fn checkout_source(repo: &Repo, line: &Line, g: &Path, c: &str) -> Option<String> {
    let out = owner_git(repo, line, g).ok()?.args(["reflog", "-1", "--format=%gs", "HEAD"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let from = text.trim().strip_prefix("checkout: moving from ")?.rsplit_once(" to ")?.0.to_string();
    if from.len() == 40 && from.bytes().all(|b| b.is_ascii_hexdigit()) {
        return (from == c).then_some(from);
    }
    if !branch_name_ok(&from) {
        return None;
    }
    let r = format!("refs/heads/{from}");
    let out = owner_git(repo, line, g).ok()?.args(["rev-parse", "--verify", "-q", &format!("{r}^{{commit}}")]).output().ok()?;
    (String::from_utf8_lossy(&out.stdout).trim() == c).then_some(r)
}

/// Clones (reflinks) of the packs of the git store in `objects/pack` of the `.git` folder `g` of a
/// fast line, which then belong to the line owner. Before git skips the write of an object that it
/// has, it touches the file that holds it (freshen); a pack of the store (root's) cannot be
/// touched, so git wrote a copy of every object: `add -A` into a new index (a checkpoint of T3
/// Code) wrote 96,705 loose objects for the Linux tree (1.6 GB, 45 s of CPU). No clone, no copy:
/// a plain copy of the packs would cost more than it saves.
fn clone_store_packs(store: &Path, g: &Path) {
    let (src, dst) = (store.join("objects/pack"), g.join("objects/pack"));
    let Ok(rd) = std::fs::read_dir(&src) else { return };
    if std::fs::create_dir_all(&dst).is_err() {
        return;
    }
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(base) = name.strip_suffix(".idx") else { continue };
        // The index last: git finds a pack by its index.
        let parts = [format!("{base}.pack"), format!("{base}.rev"), name.clone()];
        let mut made = Vec::new();
        let cloned = parts.iter().all(|part| {
            let from = src.join(part);
            if !from.exists() && !part.ends_with(".pack") && !part.ends_with(".idx") {
                return true;
            }
            let to = dst.join(part);
            let ok = std::fs::File::open(&from)
                .and_then(|f| std::fs::File::create_new(&to).map(|t| (f, t)))
                .and_then(|(f, t)| {
                    made.push(to.clone());
                    clone_fd(&f, &t)
                })
                .is_ok();
            ok
        });
        if !cloned {
            for p in made {
                let _ = std::fs::remove_file(p);
            }
            return;
        }
    }
}

// ----- git in a fast line -----
//
// The `.git` folder of a fast line is writable for git. Each side follows the other:
// - layr moves the head of the line (save, merge, reset, undo, pull): `refresh_git_folder` moves
//   git's HEAD (the branch it is on) to the commit of the new head.
// - git moves HEAD (commit, amend, reset, checkout, merge, rebase): `git_moved` brings the new
//   commits into the project's git store and imports them as states; the caller then moves the
//   head of the line to the state of HEAD and keeps the files (a mixed reset). A hook of the
//   folder runs `layr git adopt` after such a move, and each layr command of the line owner in
//   the line checks for it first.
// The file `layr-index` of the folder names the commit of the last time the two sides agreed.
//
// The folder belongs to the line owner. Root reads and writes only files at its top through
// descriptors; everything below goes through git commands that run as the line owner.

thread_local! {
    /// The commit of git's HEAD while the head of a fast line moves to its state (`following_git`).
    static FOLLOWING: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Run `f`, which moves the head of a fast line to the state of git's HEAD `commit`. During it
/// the checks of the line's `.git` folder take `commit` as agreed: the move leaves HEAD and
/// git's index as they are, and `layr-index` names the commit once the head has moved. A command
/// that reads `layr-index` without the line lock never sees the commit before the head is there.
pub fn following_git<T>(commit: &str, f: impl FnOnce() -> T) -> T {
    FOLLOWING.with(|c| *c.borrow_mut() = Some(commit.to_string()));
    let out = f();
    FOLLOWING.with(|c| *c.borrow_mut() = None);
    out
}

fn following(commit: &str) -> bool {
    FOLLOWING.with(|c| c.borrow().as_deref() == Some(commit))
}

/// True when git may write the `.git` folder of `line`, and the line follows it: a fast line.
pub fn follows_git(repo: &Repo, line: &Line) -> bool {
    crate::views::fast_of(repo, line).is_some()
}

/// A line name that git takes as a branch name (a subset of `git check-ref-format`).
fn branch_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
        && !name.contains("..")
        && !name.contains("//")
        && name.split('/').all(|part| !part.is_empty() && !part.starts_with(['.', '-']) && !part.ends_with('.') && !part.ends_with(".lock"))
}

/// This program for a line of git's config or of a hook: quoted for a shell. None for a path
/// that cannot stand in either.
fn exe_for_shell() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.to_str()?;
    (!exe.contains(['\'', '"', '\\', '\n'])).then(|| format!("'{exe}'"))
}

/// The settings of the `.git` folder of a fast line. On Linux git asks the view which paths
/// changed (docs/layrfs-fuse.md), so `git status` reads no folder that did not change.
fn fast_config() -> String {
    match exe_for_shell().filter(|_| cfg!(target_os = "linux")) {
        Some(exe) => format!("\tfsmonitor = {exe} fs view fsmonitor\n\tuntrackedCache = true\n"),
        None => String::new(),
    }
}

/// The hooks of the `.git` folder of a fast line: after git moves HEAD or a branch, the head of
/// the line follows. A hook never fails git: the next layr command in the line follows too.
fn write_hooks(g: &Path) -> Result<()> {
    let Some(exe) = exe_for_shell() else { return Ok(()) };
    std::fs::create_dir_all(g.join("hooks"))?;
    let head = "#!/bin/sh\n# Made by layr: the head of the line follows git (layr git adopt).\n";
    // The command runs on after the hook ends: git does not wait for the head of the line.
    let moved = format!(
        "{head}[ \"$1\" = committed ] || exit 0\nwhile read -r old new ref; do\n\tcase \"$ref\" in\n\tHEAD|refs/heads/*) {exe} git adopt --hook </dev/null >/dev/null 2>&1 & exit 0 ;;\n\tesac\ndone\nexit 0\n"
    );
    // A rebase or an amend ends here; its last ref update came while it was still in progress.
    let rewritten = format!("{head}{exe} git adopt --hook </dev/null >/dev/null 2>&1 &\nexit 0\n");
    for (name, text) in [("reference-transaction", moved), ("post-rewrite", rewritten)] {
        std::fs::write(g.join("hooks").join(name), text)?;
        std::fs::set_permissions(g.join("hooks").join(name), std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// A git command in the `.git` folder `g` of `line`, run as the line owner without hooks and
/// without the owner's own git settings.
fn owner_git(repo: &Repo, line: &Line, g: &Path) -> Result<Command> {
    let mut c = Command::new("git");
    c.args(["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false"]);
    let env = [
        ("GIT_DIR".to_string(), g.to_string_lossy().into_owned()),
        ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
        ("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string()),
        ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
    ];
    crate::privs::command_as(&mut c, repo.owner_ids(line).0, &env)?;
    c.current_dir(g).stdin(Stdio::null());
    Ok(c)
}

/// The commit of git's HEAD in the `.git` folder `g` of a fast line. None for a branch without
/// a commit, and when git cannot read the folder.
fn owner_head(repo: &Repo, line: &Line, g: &Path) -> Option<String> {
    let out = owner_git(repo, line, g).ok()?.args(["rev-parse", "--verify", "-q", "HEAD^{commit}"]).output().ok()?;
    let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && head.len() == 40 && head.bytes().all(|b| b.is_ascii_hexdigit())).then_some(head)
}

/// The `.git` folder that layr made for the fast line `line`: its path and an open descriptor.
fn fast_git_folder(repo: &Repo, line: &Line) -> Option<(PathBuf, std::fs::File)> {
    if !has_store(repo) || !follows_git(repo, line) {
        return None;
    }
    let base = git_base(repo, line);
    let dir = fsutil::open_dir_nofollow(&base.join(".git")).ok()?;
    fsutil::is_layer(&base, ".git", &fsutil::meta_of(&dir.metadata().ok()?)).then(|| (base.join(".git"), dir))
}

/// The commit that `layr-index` names: where git's HEAD was when the two sides last agreed.
fn agreed_commit(dir: &std::fs::File) -> String {
    fsutil::read_at(dir, INDEX_OF).map(|c| String::from_utf8_lossy(&c).trim().to_string()).unwrap_or_default()
}

/// True when the folder has a regular file `name` (a link does not count).
fn has_file_at(dir: &std::fs::File, name: &str) -> bool {
    rustix::fs::statat(dir, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .is_ok_and(|st| rustix::fs::FileType::from_raw_mode(st.st_mode) == rustix::fs::FileType::RegularFile)
}

/// `git reset --no-refresh` in the `.git` folder `g` of the fast line `line`, as its owner: the
/// index becomes the tree of HEAD. False when git fails.
fn owner_index_reset(repo: &Repo, line: &Line, g: &Path) -> bool {
    let Ok(mut c) = owner_git(repo, line, g) else { return false };
    c.env("GIT_WORK_TREE", repo.working(line)).args(["reset", "-q", "--no-refresh"]);
    c.stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// True while a git command of several steps runs in the folder (a rebase, a pick of several
/// commits, a bisect): HEAD moves at each step, and the line follows when the command ends.
fn git_in_progress(dir: &std::fs::File) -> bool {
    ["rebase-merge", "rebase-apply", "sequencer", "BISECT_LOG"]
        .iter()
        .any(|n| rustix::fs::statat(dir, *n, rustix::fs::AtFlags::SYMLINK_NOFOLLOW).is_ok())
}

/// True when git's HEAD in the fast line `line` is not where the two sides last agreed (one git
/// command, no lock): `git_moved` then says what to do.
pub fn git_head_differs(repo: &Repo, line: &Line) -> bool {
    let Some((g, dir)) = fast_git_folder(repo, line) else { return false };
    !git_in_progress(&dir) && owner_head(repo, line, &g).is_some_and(|head| agreed_commit(&dir) != head)
}

/// Git moved HEAD of the fast line `line` to a commit that is not the commit of the line's head:
/// the commit and its state, after the new commits came into the project's git store and their
/// states were recorded (`plan_import_over`). None when HEAD did not move, or a git command of
/// several steps is in progress. The caller holds the line lock; it moves the head of the line
/// to the state and then calls `git_followed`.
pub fn git_moved(ctx: &Ctx, repo: &Repo, line: &Line) -> Result<Option<(String, String)>> {
    let Some((g, dir)) = fast_git_folder(repo, line) else { return Ok(None) };
    if git_in_progress(&dir) {
        return Ok(None);
    }
    let Some(head) = owner_head(repo, line, &g) else { return Ok(None) };
    // The usual case costs one git command: HEAD is where layr left it.
    if agreed_commit(&dir) == head {
        return Ok(None);
    }
    let expected = export_commit(repo, &line.head).ok();
    if expected.as_deref() == Some(head.as_str()) || repo.p.db.git_commits_of(&line.head)?.contains(&head) {
        git_followed(repo, line, &head)?;
        return Ok(None);
    }
    let _t = crate::trace::span("follow git");
    take_objects(repo, line, &g, &head, expected.as_deref())?;
    let over: Vec<(String, String)> = expected.into_iter().map(|c| (c, line.head.clone())).collect();
    let plan = plan_import_over(ctx, repo, &head, &format!("git/{}", line.name), &over)?;
    let made: Vec<(String, String)> =
        plan.records.iter().filter_map(|(oid, rec)| rec.states.first().map(|st| (st.id.clone(), oid.clone()))).collect();
    let (state, _) = commit_import(ctx, repo, plan, None)?;
    // The snapshot of each new state, from the snapshot of its first parent (parents first),
    // and the snapshot of the state of HEAD when git went to an earlier commit, from the old
    // head of the line. The line still keeps the snapshot of its old head now; the next save in
    // the line starts from the snapshot of the new head.
    #[cfg(not(target_os = "macos"))]
    {
        let mut near: Vec<(String, String)> = Vec::new();
        for (st, _) in &made {
            if let Some(parent) = repo.state(st)?.parents.first() {
                near.push((st.clone(), parent.clone()));
            }
        }
        near.push((state.clone(), line.head.clone()));
        for (st, from) in &near {
            if let Err(e) = crate::objstore::build_state_near(repo, st, from) {
                drop(crate::trace::named(|| format!("state {} gets no snapshot: {e:#}", ids::short(st))));
            }
        }
    }
    // The index of each new state, from the index of its first parent and the changed paths
    // (parents first). A later save in the line copies it; without it the save reads the tree
    // of the commit and every file of the state.
    for (st, commit) in &made {
        let prev = repo.state(st)?.parents.first().cloned().filter(|p| index_path(repo, p).exists());
        if let (Some(prev), false) = (prev, index_path(repo, st).exists()) {
            if let Err(e) = build_index(repo, st, commit, Some(&prev)) {
                drop(crate::trace::named(|| format!("state {} gets no git index: {e:#}", ids::short(st))));
            }
        }
    }
    Ok(Some((head, state)))
}

/// Record that the head of the fast line `line` is at the state of git's HEAD `commit`. Git's
/// index stays as git left it.
pub fn git_followed(repo: &Repo, line: &Line, commit: &str) -> Result<()> {
    let Some((_, dir)) = fast_git_folder(repo, line) else { return Ok(()) };
    let text = format!("{commit}\n");
    fsutil::replace_at(&dir, INDEX_OF, &mut |f| Ok((&*f).write_all(text.as_bytes())?), Some(repo.owner_ids(line)))
}

/// Bring the objects of `want` that `have` does not reach from the `.git` folder `g` of a fast
/// line into the project's git store. The pack comes through a pipe from a git that runs as the
/// line owner: root never opens the folder (`import_fetched`).
fn take_objects(repo: &Repo, line: &Line, g: &Path, want: &str, have: Option<&str>) -> Result<()> {
    let s = init_store(repo)?;
    if git_out(&s, &["cat-file", "-e", &format!("{want}^{{commit}}")]).is_ok() {
        return Ok(());
    }
    // Objects that the folder borrows from the store (its alternate) stay out of the pack.
    let mut pack = owner_git(repo, line, g)?;
    pack.args(["pack-objects", "--stdout", "--revs", "--local", "-q"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut pack = pack.spawn().context("start git pack-objects")?;
    {
        let mut input = pack.stdin.take().context("git pack-objects has no input")?;
        writeln!(input, "{want}")?;
        if let Some(h) = have {
            writeln!(input, "^{h}")?;
        }
    }
    let mut out = pack.stdout.take().context("git pack-objects has no output")?;
    // A pack without objects is no input for index-pack.
    let mut header = [0u8; 12];
    let complete = out.read_exact(&mut header).is_ok();
    let count = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
    let stored = if complete && &header[..4] == b"PACK" && count > 0 {
        let mut index = base_cmd(&s)
            .args(["index-pack", "--stdin", "--fix-thin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("start git index-pack")?;
        let copied = (|| -> Result<()> {
            let mut input = index.stdin.take().context("git index-pack has no input")?;
            input.write_all(&header)?;
            std::io::copy(&mut out, &mut input)?;
            Ok(())
        })();
        let done = index.wait_with_output()?;
        copied.is_ok() && done.status.success()
    } else {
        let _ = std::io::copy(&mut out, &mut std::io::sink());
        true
    };
    let packed = pack.wait_with_output()?;
    if !packed.status.success() || !stored {
        bail!("cannot read the commits of the line's .git folder: {}", String::from_utf8_lossy(&packed.stderr).trim());
    }
    if git_out(&s, &["cat-file", "-e", &format!("{want}^{{commit}}")]).is_err() {
        bail!("commit {want} of the line's .git folder did not reach the project's git store");
    }
    Ok(())
}

/// The folder that holds the `.git` folder of `line`: its working folder, or for a fast line its
/// cache root (`make_git_folder`).
fn git_base(repo: &Repo, line: &Line) -> PathBuf {
    let w = repo.working(line);
    repo.cache_base(line, &w)
}

const PACKED_HEADER: &str = "# pack-refs with: peeled fully-peeled sorted \n";

/// The `packed-refs` of a `.git` folder: the tags and remote refs of the bridge store.
fn packed_refs_text(repo: &Repo) -> Result<String> {
    let refs = git_out(&store(repo), &["for-each-ref", "--format=%(objectname) %(refname)", "refs/tags", "refs/remotes"])?;
    let mut packed = String::from(PACKED_HEADER);
    for l in refs.lines() {
        packed.push_str(l);
        packed.push('\n');
    }
    Ok(packed)
}

/// Delete the tracking refs of a removed remote (`refs/remotes/<remote>/*`) from the bridge store.
/// Then rewrite the `packed-refs` of every line, so no line lists or exports them.
pub fn drop_remote_refs(repo: &Repo, remote: &str) -> Result<()> {
    if !has_store(repo) {
        return Ok(());
    }
    let s = store(repo);
    for r in git_out(&s, &["for-each-ref", "--format=%(refname)", &format!("refs/remotes/{remote}/")])?.lines() {
        git_out(&s, &["update-ref", "-d", r])?;
    }
    for line in repo.lines()? {
        if let Err(e) = refresh_packed_refs(repo, &line) {
            eprintln!("warning: .git folder of line {} not updated: {e:#}", line.name);
        }
    }
    Ok(())
}

/// Rewrite the `packed-refs` of a line's `.git` folder when it differs from the bridge store. A
/// folder the line owner made is left alone.
pub fn refresh_packed_refs(repo: &Repo, line: &Line) -> Result<()> {
    refresh_packed_refs_to(repo, line, &packed_refs_text(repo)?)
}

/// After a fetch: the `packed-refs` of the `.git` folder of every line of this machine, so git in a
/// line sees the remote refs that layr has, not the ones of the line's start. A line that fails
/// keeps its old refs, with a warning.
pub fn refresh_all_packed_refs(repo: &Repo) -> Result<()> {
    let want = packed_refs_text(repo)?;
    for line in repo.lines()? {
        if line.machine != repo.p.store.machine.id {
            continue;
        }
        if let Err(e) = refresh_packed_refs_to(repo, &line, &want) {
            eprintln!("warning: the git refs of line {} stay as they were: {e:#}", line.name);
        }
    }
    Ok(())
}

fn refresh_packed_refs_to(repo: &Repo, line: &Line, want: &str) -> Result<()> {
    refresh_shallow(repo, line)?;
    if follows_git(repo, line) {
        return refresh_fast_refs(repo, line, want);
    }
    let w = git_base(repo, line);
    let dir = match fsutil::open_dir_nofollow(&w.join(".git")) {
        Ok(d) => d,
        Err(_) => return Ok(()),
    };
    if !fsutil::is_layer(&w, ".git", &fsutil::meta_of(&dir.metadata()?)) {
        return Ok(());
    }
    let cur = fsutil::read_at(&dir, "packed-refs").map(|c| String::from_utf8_lossy(&c).into_owned()).unwrap_or_default();
    if cur == want {
        return Ok(());
    }
    fsutil::git_writable(&w, true);
    let own = Some(repo.owner_ids(line));
    let r = fsutil::replace_at(&dir, "packed-refs", &mut |f| Ok((&*f).write_all(want.as_bytes())?), own);
    fsutil::git_writable(&w, false);
    r
}

/// Point the `.git` folder of a line at the commit of its head (`line_commit`) after every move
/// of the head: commit, amend, merge, reset, undo, pull, push. So `git rev-parse HEAD` in a line
/// is the commit of the line's head, the same id that `layr git push` sends. When the commit cannot
/// be made, `HEAD` stays as it was and the error is returned.
/// The folder belongs to the line owner, so all access goes through descriptors of the `.git`
/// folder and never follows a link the owner made.
pub fn refresh_git_folder(repo: &Repo, line: &Line) -> Result<()> {
    let w = git_base(repo, line);
    let g = w.join(".git");
    let dir = match fsutil::open_dir_nofollow(&g) {
        Ok(d) => d,
        Err(_) => return Ok(()),
    };
    // Only the subvolume layr made; a .git folder the line owner made is its own.
    {
        if !fsutil::is_layer(&w, ".git", &fsutil::meta_of(&dir.metadata()?)) {
            return Ok(());
        }
    }
    if follows_git(repo, line) {
        return refresh_fast_git_folder(repo, line, &g, &dir);
    }
    // A HEAD that is not a regular file (for example a link) is replaced.
    let cur = fsutil::read_at(&dir, "HEAD").map(|c| String::from_utf8_lossy(&c).trim().to_string()).unwrap_or_default();
    let c = line_commit(repo, &line.head)?;
    // HEAD is written before the index: a command killed between the two leaves HEAD at the commit
    // and the old index, so the folder is complete only when INDEX_OF also names it (feedback item 62).
    let indexed = fsutil::read_at(&dir, INDEX_OF).map(|c| String::from_utf8_lossy(&c).trim().to_string()).unwrap_or_default();
    if cur == c && indexed == c {
        fsutil::git_writable(&w, true);
        fsutil::unlink_at(&dir, FALLBACK_HEAD);
        fsutil::git_writable(&w, false);
        return Ok(());
    }
    fsutil::git_writable(&w, true);
    let r = write_git_folder(repo, line, &dir, &c);
    fsutil::git_writable(&w, false);
    r
}

/// `refresh_packed_refs` of a fast line. Git owns the `packed-refs` file there (it can hold the
/// branches of the line owner), so the tags and remote refs of the bridge store go in through
/// `git update-ref`: each one is set, and a remote ref that the store no longer has goes. Tags
/// that the store does not have stay: the line owner made them.
fn refresh_fast_refs(repo: &Repo, line: &Line, want: &str) -> Result<()> {
    let Some((g, _)) = fast_git_folder(repo, line) else { return Ok(()) };
    let parse = |text: &str| -> BTreeMap<String, String> {
        text.lines()
            .filter(|l| !l.starts_with('#') && !l.starts_with('^'))
            .filter_map(|l| l.split_once(' '))
            .map(|(oid, name)| (name.to_string(), oid.to_string()))
            .collect()
    };
    let want = parse(want);
    let out = owner_git(repo, line, &g)?.args(["for-each-ref", "--format=%(objectname) %(refname)", "refs/tags", "refs/remotes"]).output()?;
    if !out.status.success() {
        bail!("git for-each-ref failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let have = parse(&String::from_utf8_lossy(&out.stdout));
    let mut input = String::new();
    for (name, oid) in &want {
        if have.get(name) != Some(oid) {
            input.push_str(&format!("update {name} {oid}\n"));
        }
    }
    for name in have.keys().filter(|n| n.starts_with("refs/remotes/") && !want.contains_key(*n)) {
        input.push_str(&format!("delete {name}\n"));
    }
    if input.is_empty() {
        return Ok(());
    }
    let mut c = owner_git(repo, line, &g)?;
    let mut child = c.args(["update-ref", "--stdin"]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?;
    child.stdin.take().context("git update-ref has no input")?.write_all(input.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("git update-ref failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// `refresh_git_folder` of a fast line: the branch that git's HEAD is on moves to the commit of
/// the line's head, and the index is the one of that state. Nothing is written when git's HEAD
/// is at that commit already and the two sides agreed on it (git made the commit: its index stays).
fn refresh_fast_git_folder(repo: &Repo, line: &Line, g: &Path, dir: &std::fs::File) -> Result<()> {
    let c = {
        let _t = crate::trace::span("git: export commit");
        export_commit(repo, &line.head)?
    };
    let head = {
        let _t = crate::trace::span("git: HEAD of the folder");
        owner_head(repo, line, g)
    };
    let agreed = agreed_commit(dir);
    // The head of the line follows git (`following_git`): git's refs and index stay. Git may
    // have moved HEAD again meanwhile; the next `git_moved` sees that.
    if FOLLOWING.with(|f| f.borrow().is_some()) {
        if head.as_deref() == Some(c.as_str()) && agreed != c {
            let text = format!("{c}\n");
            fsutil::replace_at(dir, INDEX_OF, &mut |f| Ok((&*f).write_all(text.as_bytes())?), Some(repo.owner_ids(line)))?;
        }
        return Ok(());
    }
    if head.as_deref() == Some(c.as_str()) && agreed == c {
        return Ok(());
    }
    // Git makes the index from its own one below (`owner_index_reset`); the index that layr keeps
    // for the state (a walk that hashes every file of a state it has none for) only when git has
    // no index.
    let have_index = has_file_at(dir, "index");
    let c = {
        let _t = crate::trace::span("git: commit of the state");
        if have_index { export_commit(repo, &line.head)? } else { line_commit(repo, &line.head)? }
    };
    if head.as_deref() != Some(c.as_str()) {
        // Git's branch moves only from the commit that the two sides agreed on. Git moved HEAD
        // while this command ran: its commit stays on the branch, and the line follows it next.
        let moved_by_git = !agreed.is_empty() && head.as_deref().is_some_and(|h| h != agreed);
        if moved_by_git {
            bail!(
                "git moved HEAD to {} while the command ran; the branch of git stays there (run 'layr git adopt')",
                head.as_deref().unwrap_or_default()
            );
        }
        // Back to where a checkout came from (`layr undo` after `git checkout`): git checks that
        // branch (or commit) out again, so its files come back and the branch that HEAD is on now
        // stays. When git refuses (a local change in the way), HEAD only goes back.
        let back = checkout_source(repo, line, g, &c);
        if let Some(b) = &back {
            let mut co = owner_git(repo, line, g)?;
            co.env("GIT_WORK_TREE", repo.working(line)).args(["-c", "checkout.workers=0", "checkout", "-q"]);
            match b.strip_prefix("refs/heads/") {
                Some(name) => co.arg(name),
                None => co.args(["--detach", b.as_str()]),
            };
            let out = co.output()?;
            if out.status.success() {
                return finish_fast_refresh(repo, line, g, dir, &c, have_index);
            }
            eprintln!(
                "warning: git did not check out {} again ({}); HEAD goes back, the files stay",
                b.trim_start_matches("refs/heads/"),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let mut update = owner_git(repo, line, g)?;
        match &back {
            Some(b) if b.starts_with("refs/heads/") => {
                update.args(["symbolic-ref", "-m", "layr: back to the branch of the head", "HEAD", b]);
            }
            Some(_) => {
                update.args(["update-ref", "--no-deref", "-m", "layr: back to the commit of the head", "HEAD", &c]);
            }
            None => {
                update.args(["update-ref", "-m", "layr: the head of the line", "HEAD", &c]);
            }
        }
        if back.as_deref().is_none_or(|b| !b.starts_with("refs/heads/")) {
            if let Some(old) = head.as_deref() {
                update.arg(old);
            }
        }
        let out = update.output()?;
        if !out.status.success() {
            bail!("git update-ref HEAD failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
    }
    finish_fast_refresh(repo, line, g, dir, &c, have_index)
}

/// The end of `refresh_fast_git_folder`: the index of HEAD, and the agreed commit `c`.
fn finish_fast_refresh(repo: &Repo, line: &Line, g: &Path, dir: &std::fs::File, c: &str, have_index: bool) -> Result<()> {
    crate::store::failpoint("git-folder-half");
    let own = Some(repo.owner_ids(line));
    // Git makes the index of HEAD from its own index: entries that stay keep their stat data
    // (the files did not change), and a split index stays split, so git writes kilobytes and
    // not the whole index of a large tree. Without an index of git, the index that layr keeps.
    let _t = crate::trace::span("git: index of HEAD");
    if !(have_index && owner_index_reset(repo, line, g)) {
        if have_index {
            line_commit(repo, &line.head)?;
        }
        let idx = index_path(repo, &line.head);
        if idx.exists() {
            fsutil::replace_at(
                dir,
                "index",
                &mut |f| {
                    let src = std::fs::File::open(&idx)?;
                    if clone_fd(&src, f).is_err() {
                        std::io::copy(&mut std::fs::File::open(&idx)?, &mut &*f)?;
                    }
                    Ok(())
                },
                own,
            )?;
        } else {
            fsutil::unlink_at(dir, "index");
        }
    }
    let text = format!("{c}\n");
    fsutil::replace_at(dir, INDEX_OF, &mut |f| Ok((&*f).write_all(text.as_bytes())?), own)
}

/// Carry an unchanged, verified fallback across a Layr head move whose Git refresh failed.
pub fn advance_fallback_head(repo: &Repo, line: &Line, new_head: &str) -> Result<()> {
    let w = repo.working(line);
    let dir = match fsutil::open_dir_nofollow(&w.join(".git")) {
        Ok(d) => d,
        Err(_) => return Ok(()),
    };
    if !fsutil::is_layer(&w, ".git", &fsutil::meta_of(&dir.metadata()?)) {
        return Ok(());
    }
    let Some(head) = fsutil::read_at(&dir, "HEAD").and_then(|bytes| String::from_utf8(bytes).ok()) else {
        return Ok(());
    };
    let expected = format!("{}\n{head}", line.head);
    if fsutil::read_at(&dir, FALLBACK_HEAD).as_deref() != Some(expected.as_bytes())
        || (head.starts_with("ref: ") && git_out(&w.join(".git"), &["rev-parse", "--verify", "HEAD"]).is_ok())
    {
        return Ok(());
    }
    let marker = format!("{new_head}\n{head}");
    fsutil::git_writable(&w, true);
    let r = fsutil::replace_at(&dir, FALLBACK_HEAD, &mut |f| Ok((&*f).write_all(marker.as_bytes())?), Some(repo.owner_ids(line)));
    fsutil::git_writable(&w, false);
    r
}

/// The file of a line's `.git` folder that names the commit whose index layr wrote last. Git ignores it.
const INDEX_OF: &str = "layr-index";

/// The current state and exact fallback HEAD, retained until a successful refresh.
/// Kept locally in the managed .git folder; an ancestor must never become a mapping of the head state.
const FALLBACK_HEAD: &str = "layr-fallback";

fn write_git_folder(repo: &Repo, line: &Line, dir: &std::fs::File, c: &str) -> Result<()> {
    let own = Some(repo.owner_ids(line));
    let head = format!("{c}\n");
    fsutil::replace_at(dir, "HEAD", &mut |f| Ok((&*f).write_all(head.as_bytes())?), own)?;
    crate::store::failpoint("git-folder-half");
    // The index of the head state: its stat data matches files copied from that state.
    let idx = index_path(repo, &line.head);
    if idx.exists() {
        fsutil::replace_at(
            dir,
            "index",
            &mut |f| {
                let src = std::fs::File::open(&idx)?;
                layr_fs_unix::copy_contents(&src, f)?;
                Ok(())
            },
            own,
        )?;
    } else {
        fsutil::unlink_at(dir, "index");
    }
    fsutil::replace_at(dir, INDEX_OF, &mut |f| Ok((&*f).write_all(head.as_bytes())?), own)?;
    fsutil::unlink_at(dir, FALLBACK_HEAD);
    Ok(())
}

/// Refuse a move of a line's head while the `.git` folder of the line shows another commit than
/// `refresh_git_folder` wrote: a git commit, merge or reset made in the line. The next export
/// would make its commit a child of layr's last export and drop git's commit from HEAD. Nothing
/// moves on refusal. A `.git` folder that the line owner made is left alone.
pub fn check_git_head(repo: &Repo, line: &Line) -> Result<()> {
    if !has_store(repo) {
        return Ok(());
    }
    if follows_git(repo, line) {
        return check_fast_git_head(repo, line);
    }
    let w = git_base(repo, line);
    let dir = match fsutil::open_dir_nofollow(&w.join(".git")) {
        Ok(d) => d,
        Err(_) => return Ok(()),
    };
    if !fsutil::is_layer(&w, ".git", &fsutil::meta_of(&dir.metadata()?)) {
        return Ok(());
    }
    // A HEAD that is missing or not a regular file is replaced by refresh_git_folder, as before.
    let Some(cur) = fsutil::read_at(&dir, "HEAD") else {
        return Ok(());
    };
    let cur = String::from_utf8_lossy(&cur).trim().to_string();
    // A clone can deliberately write an ancestor (or an unborn HEAD) while history is absent.
    // Accept only what Layr wrote for this state, including when history later becomes available.
    let fallback = fsutil::read_at(&dir, FALLBACK_HEAD).and_then(|bytes| String::from_utf8(bytes).ok());
    let fallback = fallback
        .as_deref()
        .and_then(|text| text.split_once('\n'))
        .filter(|(state, _)| *state == line.head)
        .map(|(_, head)| head.trim().to_string());
    let expected = if let Some(head) = fallback {
        head
    } else {
        // Without a commit for the head, refresh_git_folder keeps HEAD and only warns: no refusal.
        let Ok(commit) = line_commit(repo, &line.head) else { return Ok(()) };
        commit
    };
    // Git keeps a symbolic HEAD unchanged when it creates the first commit on an unborn branch.
    let untouched =
        cur == expected && (!cur.starts_with("ref: ") || git_out(&w.join(".git"), &["rev-parse", "--verify", "HEAD"]).is_err());
    // A state can have more than one commit: an import from Git and a later export of the same state, or
    // the exports of two layr versions. A clone writes one of them. Any commit that layr maps to the head
    // names the same files, so no git command ran.
    if !untouched && !repo.p.db.git_commits_of(&line.head)?.contains(&cur) {
        // Only undo the first commit on the unborn fallback, never a different or redirected branch.
        // The observed value makes deletion fail atomically if the ref changes after this diagnostic.
        let reset_ref = expected
            .strip_prefix("ref: ")
            .filter(|_| cur == expected)
            .filter(|branch| git_out(&w.join(".git"), &["symbolic-ref", "-q", branch]).is_err())
            .and_then(|branch| {
                let observed = git_out(&w.join(".git"), &["rev-parse", "--verify", branch]).ok()?;
                let branch = branch.replace('\'', "'\\''");
                Some(format!(" && git update-ref --no-deref -d '{branch}' '{}'", observed.trim()))
            })
            .unwrap_or_default();
        let expected = expected.replace('\'', "'\\''");
        bail!(
            "A git commit, merge, reset or checkout ran in this line ({cur}). layr did not touch the files.\n\
             To keep the files as they are and continue, run:\n  printf '%s\\n' '{expected}' > .git/HEAD{reset_ref}\n\
             and then:\n  layr save -a -m \"<message>\"\n\
             Use layr merge, layr git fetch and layr save instead of git."
        );
    }
    Ok(())
}

/// `check_git_head` of a fast line. Git may move HEAD there, and the line follows before a
/// command runs (`git_moved`). When it could not follow, the head of the line does not move: a
/// new state would become a child of the old head and git's commit would leave HEAD.
fn check_fast_git_head(repo: &Repo, line: &Line) -> Result<()> {
    let Some((g, dir)) = fast_git_folder(repo, line) else { return Ok(()) };
    let Some(head) = owner_head(repo, line, &g) else { return Ok(()) };
    if agreed_commit(&dir) == head || following(&head) {
        return Ok(());
    }
    let Ok(expected) = export_commit(repo, &line.head) else { return Ok(()) };
    if head == expected || repo.p.db.git_commits_of(&line.head)?.contains(&head) {
        return Ok(());
    }
    if git_in_progress(&dir) {
        bail!("A git rebase, pick or bisect is in progress in this line. Finish it or stop it (for example 'git rebase --abort'), then run the command again.");
    }
    bail!(
        "git moved HEAD of this line to {head}, and the head of the line did not follow.\n\
         Run 'layr git adopt' to see why. To put HEAD back, run:\n  git reset --soft {expected}"
    );
}

// ----- reads of imported states from git -----
//
// An imported state keeps its record. Its tree is on disk only when a command needs it (`build_tree`).
// A read that needs only the changes or some files takes them from the git objects instead: a
// diff, a file, a listing or a search builds no tree. These functions own that read.

/// The git commit of an imported state, whether its tree is on disk or not.
pub fn imported_commit(repo: &Repo, state: &str) -> Result<Option<String>> {
    match repo.p.db.state(state)? {
        Some(s) if s.kind == StateKind::Import => {
            let squash = repo.p.db.squash_commits_of(state)?;
            Ok(repo.p.db.git_commits_of(state)?.into_iter().find(|c| !squash.contains(c)))
        }
        _ => Ok(None),
    }
}

/// The git commit of an imported state whose tree is not on disk: a read takes it from git.
pub fn unbuilt_commit(repo: &Repo, state: &str) -> Result<Option<String>> {
    if repo.p.has_state(state) {
        return Ok(None);
    }
    imported_commit(repo, state)
}

/// The selected regular file of an import that would otherwise be rebuilt from Git.
pub(crate) fn selected_import_file(
    repo: &Repo,
    state: &str,
    path: &str,
) -> Result<Option<(crate::repo::Snap, Option<crate::repo::Temp>)>> {
    use layr_fs_core::{RelDir, TreeRead};

    if repo.p.has_state(state) || crate::objstore::recorded_refs(&repo.p, state)?.is_some() {
        return Ok(None);
    }
    let Some(commit) = stored_commit(repo, state)? else { return Ok(None) };
    let Some(entry) = with_objects(repo, |cat| path_entry(cat, &commit, path))? else { return Ok(None) };
    if !matches!(entry.mode.as_str(), "100644" | "100755") {
        return Ok(None);
    }
    if let Some((base, base_commit)) = nearest_built_import(repo, state)? {
        let pin = repo.pin(&[&base])?;
        if !repo.p.has_state(&base) {
            return Ok(None);
        }
        let source = repo.state_src(&base)?;
        // A rebuild keeps unmodified files from the base, including their stored attributes.
        if with_objects(repo, |cat| path_entry(cat, &base_commit, path))?.as_ref() == Some(&entry) {
            if fsutil::lmeta_checked(&source, path)?.is_some_and(|m| m.is_file()) {
                repo.hold(pin);
                return Ok(Some((source, None)));
            }
            return Ok(None);
        }
        // A new file in the rebuilt tree could inherit attributes from its directory.
        let parent = RelDir::from_bytes(path.rsplit_once('/').map_or("", |(dir, _)| dir).as_bytes())?;
        let attrs = source.metadata(&parent)?;
        if !attrs.is_some_and(|m| m.kind == fsutil::FType::Dir && m.attributes.acl.is_none() && m.attributes.xattrs.is_empty()) {
            return Ok(None);
        }
    }
    let data = blob(repo, &entry.oid)?;
    let tmp = crate::repo::Temp::empty(repo.p.work_dir().join(crate::store::temp_name()))?;
    place_blob(&tmp.path, &entry.mode, path, &data, None, None)?;
    Ok(Some((crate::repo::snap(&tmp.path), Some(tmp))))
}

/// True when `state` is an imported state whose record holds Git objects but whose commit the git
/// store lacks (a machine that got the record without the Git pack).
pub fn missing_objects(repo: &Repo, state: &str) -> bool {
    let imported = matches!(repo.p.db.state(state), Ok(Some(s)) if s.kind == StateKind::Import);
    imported && repo.p.db.has_git_import(state).unwrap_or(false) && stored_commit(repo, state).ok().flatten().is_none()
}

/// Two folders that hold the paths where commits `old` and `new` differ, with their bytes from the
/// git store. Both folders hold the same paths, so a comparison of them finds the same changes as a
/// comparison of the two full trees. `old` None: the first commit, against an empty folder.
pub struct Pair {
    pub old: crate::repo::Snap,
    pub new: crate::repo::Snap,
    pub keep: Vec<crate::repo::Temp>,
}

pub fn sparse_pair(repo: &Repo, old: Option<&str>, new: &str) -> Result<Pair> {
    let s = store(repo);
    let mut args = vec!["diff-tree", "-r", "-z", "--no-renames", "--no-commit-id"];
    if old.is_none() {
        args.push("--root");
    }
    args.extend(old);
    args.push(new);
    // Raw records: ":<old mode> <new mode> <old id> <new id> <status>" NUL "<path>" NUL.
    let out = git_out_bytes(&s, &args)?;
    let mut fields = out.split(|b| *b == 0);
    let mut changes: Vec<(String, Side, Side)> = Vec::new();
    while let Some(meta) = fields.next() {
        if meta.is_empty() {
            continue;
        }
        let meta = String::from_utf8_lossy(meta).into_owned();
        let meta = meta.strip_prefix(':').context("bad diff-tree record")?;
        let path = String::from_utf8_lossy(fields.next().context("diff-tree record without a path")?).into_owned();
        let f: Vec<&str> = meta.split_whitespace().collect();
        if f.len() != 5 {
            bail!("bad diff-tree record for {path}");
        }
        changes.push((
            path,
            Side { mode: f[0].to_string(), id: f[2].to_string() },
            Side { mode: f[1].to_string(), id: f[3].to_string() },
        ));
    }
    let zero = "0".repeat(40);
    let mut ids: Vec<String> = Vec::new();
    for (_, o, n) in &changes {
        for side in [o, n] {
            if side.mode != "160000" && side.id != zero {
                ids.push(side.id.clone());
            }
        }
    }
    ids.sort();
    ids.dedup();
    let data: BTreeMap<String, Vec<u8>> = ids.iter().cloned().zip(blobs(repo, &ids)?).collect();
    let mut keep = Vec::new();
    for _ in 0..2 {
        keep.push(crate::repo::Temp::empty(repo.p.scan_dir().join(crate::store::temp_name()))?);
    }
    for (path, o, n) in &changes {
        for (side, dst) in [(o, &keep[0].path), (n, &keep[1].path)] {
            if side.id == zero {
                continue;
            }
            if side.mode == "160000" {
                fsutil::make_parents(dst, &format!("{path}/x"), None)?;
                continue;
            }
            let bytes = data.get(&side.id).with_context(|| format!("Git object {} is missing", side.id))?;
            place_blob(dst, &side.mode, path, bytes, None, None)?;
        }
    }
    Ok(Pair { old: crate::repo::snap(&keep[0].path), new: crate::repo::snap(&keep[1].path), keep })
}

/// One side of a raw diff record: the mode and the object id that git prints for it.
struct Side {
    mode: String,
    id: String,
}

/// Output of a git command as bytes.
fn git_out_bytes(git_dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = base_cmd(git_dir).args(args).stdin(Stdio::null()).output().context("run git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// What a path of an imported commit is.
pub enum Entry {
    Missing,
    /// The names of a folder, and whether each is a folder (submodules count as folders).
    Dir(Vec<(String, bool)>),
    File(Vec<u8>),
    Link(Vec<u8>),
}

/// The entry at `path` of `commit` ("" is the root), read from the git objects through the
/// object reader of the command.
pub fn entry(repo: &Repo, commit: &str, path: &str) -> Result<Entry> {
    with_objects(repo, |cat| {
        let Some(found) = path_entry(cat, commit, path)? else {
            return Ok(Entry::Missing);
        };
        Ok(if found.is_tree() {
            let mut names = tree_records(&cat.object(&found.oid, "tree")?)
                .map(|record| {
                    let (mode, name, _) = record?;
                    Ok((String::from_utf8_lossy(name).into_owned(), matches!(mode, 0o040000 | 0o160000)))
                })
                .collect::<Result<Vec<_>>>()?;
            names.sort();
            Entry::Dir(names)
        } else if found.is_link() {
            Entry::Dir(Vec::new())
        } else if found.mode == "120000" {
            Entry::Link(cat.object(&found.oid, "blob")?)
        } else {
            Entry::File(cat.object(&found.oid, "blob")?)
        })
    })
}

/// The blob id of the file or symbolic link at `path` of `commit`; `None` when the path is
/// missing, a folder or a submodule. Reads only the trees on the path.
pub fn file_blob(repo: &Repo, commit: &str, path: &str) -> Result<Option<String>> {
    with_objects(repo, |cat| Ok(path_entry(cat, commit, path)?.filter(TreeEntry::is_blob).map(|e| e.oid)))
}

/// The bytes of a blob, through the object reader of the command.
pub fn blob(repo: &Repo, oid: &str) -> Result<Vec<u8>> {
    with_objects(repo, |cat| cat.object(oid, "blob"))
}

/// Every regular file of `commit` (no links, no submodules): path and blob id.
pub fn regular_files(repo: &Repo, commit: &str) -> Result<Vec<(String, String)>> {
    let out = git_out_bytes(&store(repo), &["ls-tree", "-r", "-z", "--full-tree", commit])?;
    let mut files = Vec::new();
    for rec in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let rec = String::from_utf8_lossy(rec).into_owned();
        let (meta, path) = rec.split_once('\t').context("bad Git tree entry")?;
        let fields: Vec<&str> = meta.split(' ').collect();
        if fields[0] == "100644" || fields[0] == "100755" {
            files.push((path.to_string(), fields[2].to_string()));
        }
    }
    Ok(files)
}

/// The contents of blobs, in the order of `oids`, through one `cat-file --batch`.
pub fn blobs(repo: &Repo, oids: &[String]) -> Result<Vec<Vec<u8>>> {
    let s = store(repo);
    let mut child = base_cmd(&s).args(["cat-file", "--batch"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
    let mut input = child.stdin.take().context("cat-file has no input")?;
    let ids = oids.to_vec();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        for id in ids {
            writeln!(input, "{id}")?;
        }
        Ok(())
    });
    let mut stdout = std::io::BufReader::new(child.stdout.take().context("cat-file has no output")?);
    let mut out = Vec::with_capacity(oids.len());
    for oid in oids {
        let mut header = String::new();
        stdout.read_line(&mut header)?;
        let size: usize = header
            .split_whitespace()
            .nth(2)
            .and_then(|x| x.parse().ok())
            .with_context(|| format!("bad cat-file header for {oid}: {header}"))?;
        let mut data = vec![0u8; size];
        stdout.read_exact(&mut data)?;
        let mut nl = [0u8; 1];
        stdout.read_exact(&mut nl)?;
        out.push(data);
    }
    writer.join().map_err(|_| anyhow!("cat-file writer failed"))??;
    child.wait()?;
    Ok(out)
}

// ----- packs of the git store (store remote, docs/remote-format.md "Git packs") -----

/// The commits that the git store holds of `commits`, in order.
pub fn has_commits(repo: &Repo, commits: &[String]) -> Result<Vec<bool>> {
    if commits.is_empty() {
        return Ok(Vec::new());
    }
    let s = init_store(repo)?;
    let mut child = base_cmd(&s).args(["cat-file", "--batch-check"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
    let mut input = child.stdin.take().context("cat-file has no input")?;
    let ids = commits.to_vec();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        for id in ids {
            writeln!(input, "{id}")?;
        }
        Ok(())
    });
    let out = child.wait_with_output()?;
    writer.join().map_err(|_| anyhow!("cat-file writer failed"))??;
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() != commits.len() {
        bail!("git cat-file answered {} lines for {} commits", lines.len(), commits.len());
    }
    // "<id> commit <size>" for a commit, "<id> missing" otherwise.
    Ok(lines.iter().map(|l| l.split_whitespace().nth(1) == Some("commit")).collect())
}

/// A pack of the objects of `new` that `covered` does not reach: the objects of the commits a remote
/// store lacks, without the ones it already has.
pub fn pack_commits(repo: &Repo, new: &[String], covered: &[String]) -> Result<Vec<u8>> {
    let s = store(repo);
    // Entries that the object store serves are not compressed: make them again.
    let reuse = if crate::gitpack::uses_folder(&s) { "--no-reuse-object" } else { "--reuse-object" };
    revision_output(&s, "pack-objects", &["--stdout", "--revs", "-q", reuse], new, covered)
}

fn revision_output(git_dir: &Path, subcommand: &str, args: &[&str], new: &[String], covered: &[String]) -> Result<Vec<u8>> {
    let mut command = base_cmd(git_dir);
    command.arg(subcommand).args(args);
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let input = child.stdin.take().with_context(|| format!("{subcommand} has no input"))?;
    // Drain both output pipes while the writer streams revisions and closes stdin.
    let out = std::thread::scope(|scope| -> Result<_> {
        let writer = scope.spawn(move || {
            let mut input = std::io::BufWriter::new(input);
            for id in new {
                writeln!(input, "{id}")?;
            }
            for id in covered {
                writeln!(input, "^{id}")?;
            }
            input.flush()
        });
        let out = child.wait_with_output();
        let written = writer.join();
        let out = out?;
        written.map_err(|_| anyhow!("{subcommand} writer failed"))??;
        Ok(out)
    })?;
    if !out.status.success() {
        bail!("git {subcommand} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// The commits of `new` that a pack of `new` without the history of `covered` holds: `git rev-list new ^covered`.
/// A commit of `new` that a covered commit reaches is in the pack of that commit, not in this one.
pub fn commits_outside(repo: &Repo, new: &[String], covered: &[String]) -> Result<Vec<String>> {
    if new.is_empty() {
        return Ok(Vec::new());
    }
    let stdout = revision_output(&store(repo), "rev-list", &["--stdin"], new, covered)?;
    let listed: std::collections::BTreeSet<&str> = std::str::from_utf8(&stdout)?.lines().collect();
    Ok(new.iter().filter(|c| listed.contains(c.as_str())).cloned().collect())
}

/// Add the objects of a pack to the git store as loose objects, as a restored import writes them
/// (`write_object`). A pack of a whole history is unpacked this way too.
pub fn install_pack(repo: &Repo, pack: &[u8]) -> Result<()> {
    let s = init_store(repo)?;
    let mut child =
        base_cmd(&s).args(["unpack-objects", "-q"]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?;
    {
        let mut input = child.stdin.take().context("unpack-objects has no input")?;
        input.write_all(pack)?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("git unpack-objects failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

// ----- changes of many commits in one object reader -----
//
// `log -p` and `show` read the changes of every commit of a range. One process per commit (diff-tree,
// cat-file) costs more than the diffs. This reader keeps one `cat-file --batch` process for the
// command and walks the trees of each commit itself (the import walk, `walk_trees`, with both sides).

thread_local! {
    /// The object reader of this thread, with the store it reads.
    static OBJECTS: std::cell::RefCell<Option<(PathBuf, CatFile)>> = const { std::cell::RefCell::new(None) };
}

/// Runs `f` with the object reader of the project's git store (one `cat-file --batch` process).
fn with_objects<T>(repo: &Repo, f: impl FnOnce(&mut CatFile) -> Result<T>) -> Result<T> {
    let s = store(repo);
    OBJECTS.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.as_ref().map(|(path, _)| path != &s).unwrap_or(true) {
            *slot = Some((s.clone(), CatFile::new(&s)?));
        }
        let (_, cat) = slot.as_mut().context("object reader")?;
        let result = f(cat);
        // A failed read can leave the reader out of step with its output (a body not read, a
        // process that ended): the next read starts a new one.
        if result.is_err() {
            *slot = None;
        }
        result
    })
}

/// True when a read can take the state's changes: a native tree, retained portable objects,
/// or an imported commit known by its record. Reads no Git blobs or portable file content;
/// a missing blob shows when the read happens.
pub fn readable(repo: &Repo, state: &str) -> Result<bool> {
    Ok(repo.p.has_state(state)
        || (!matches!(repo.p.db.state_flags(state)?, Some((_, true))) && crate::objstore::verified(&repo.p, state)?.is_some())
        || imported_commit(repo, state)?.is_some())
}

/// Forgets the object reader (the store changed on disk: a damaged cache went aside).
fn forget_objects() {
    OBJECTS.with(|cell| cell.borrow_mut().take());
}

/// One changed file of a commit against its base: the (mode, object id) of each side, None when
/// the side has no file there (absent, a folder, or a submodule).
pub(crate) struct Delta {
    pub path: String,
    old: Option<(String, String)>,
    new: Option<(String, String)>,
}

fn file_side(e: Option<&TreeEntry>) -> Option<(String, String)> {
    e.filter(|e| e.is_blob()).map(|e| (e.mode.clone(), e.oid.clone()))
}

/// The files that differ between two trees, below `prefix` (`old`/`new` None: the side is empty).
fn tree_deltas(cat: &mut CatFile, old: Option<&str>, new: Option<&str>, prefix: &str, out: &mut Vec<Delta>) -> Result<()> {
    let before = match old {
        Some(o) => parse_tree(&cat.object(o, "tree")?)?,
        None => BTreeMap::new(),
    };
    let now = match new {
        Some(n) => parse_tree(&cat.object(n, "tree")?)?,
        None => BTreeMap::new(),
    };
    let names: BTreeSet<&String> = before.keys().chain(now.keys()).collect();
    for name in names {
        let (o, e) = (before.get(name), now.get(name));
        if o != e {
            entry_deltas(cat, o, e, if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") }, out)?;
        }
    }
    Ok(())
}

/// The files that differ between two different entries at `path` (None: no entry): below them when either is a
/// folder, and the entry itself when either is a file.
fn entry_deltas(
    cat: &mut CatFile,
    o: Option<&TreeEntry>,
    e: Option<&TreeEntry>,
    path: String,
    out: &mut Vec<Delta>,
) -> Result<()> {
    let o_tree = o.filter(|x| x.is_tree()).map(|x| x.oid.as_str());
    let e_tree = e.filter(|x| x.is_tree()).map(|x| x.oid.as_str());
    if o_tree.is_some() || e_tree.is_some() {
        tree_deltas(cat, o_tree, e_tree, &path, out)?;
    }
    let (old_side, new_side) = (file_side(o), file_side(e));
    if old_side != new_side {
        out.push(Delta { path, old: old_side, new: new_side });
    }
    Ok(())
}

/// The path of `roots` that holds all the others, if one does.
fn only_root(roots: &BTreeSet<String>) -> Option<&String> {
    let mut outermost = roots.iter().filter(|path| !path.match_indices('/').any(|(at, _)| roots.contains(&path[..at])));
    outermost.next().filter(|_| outermost.next().is_none())
}

/// The bytes of the changed files of one side, held in memory, by path.
pub struct GitFiles {
    files: BTreeMap<String, Vec<u8>>,
}

impl GitFiles {
    /// The bytes of the file at `path`; empty when the side has no file there.
    pub fn bytes(&self, path: &str) -> Result<Vec<u8>> {
        Ok(self.files.get(path).cloned().unwrap_or_default())
    }
}

/// The changes of `new` against `old` (its first parent; None for the first commit), and the bytes
/// of the changed files on both sides, held in memory. No folder is written and no tree is built.
pub struct Sides {
    pub old: GitFiles,
    pub new: GitFiles,
    pub changes: Vec<crate::changes::EntryChange>,
}

/// Metadata of a changed file of a git side: the type and mode of its tree entry, and its size.
fn git_meta(mode: &str, size: usize) -> crate::fsutil::Meta {
    use crate::fsutil::{FType, Meta};
    Meta {
        ftype: if mode == "120000" { FType::Symlink } else { FType::File },
        mode: if mode == "100755" { 0o755 } else { 0o644 },
        size: size as u64,
        ino: 0,
        uid: 0,
        gid: 0,
        mtime: 0,
    }
}

/// Discover changed paths and typed tree entries without requesting any blobs. With `roots` (literal paths), the
/// caller keeps the paths that `pathspec::matches` selects for them; one root (inside the others) is read alone,
/// down to it. Several are read in one walk of the trees, which reads each folder once.
pub(crate) fn deltas_of(repo: &Repo, old: Option<&str>, new: &str, roots: Option<&BTreeSet<String>>) -> Result<Vec<Delta>> {
    with_objects(repo, |cat| {
        let mut deltas = Vec::new();
        match roots.and_then(only_root) {
            Some(path) => {
                if let Some((before, now)) = entry_pair(cat, old, new, path)? {
                    entry_deltas(cat, before.as_ref(), now.as_ref(), path.clone(), &mut deltas)?;
                }
            }
            None => {
                let new_tree = tree_id(&cat.object(new, "commit")?)?.to_string();
                let old_tree = match old {
                    Some(o) => Some(tree_id(&cat.object(o, "commit")?)?.to_string()),
                    None => None,
                };
                tree_deltas(cat, old_tree.as_deref(), Some(&new_tree), "", &mut deltas)?;
            }
        }
        deltas.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(deltas)
    })
}

/// Load only the supplied tree deltas. Callers select paths before requesting any file bytes.
pub(crate) fn sides_of(repo: &Repo, deltas: &[Delta]) -> Result<Sides> {
    let blobs = with_objects(repo, |cat| {
        let mut blobs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for d in deltas {
            for (_, oid) in d.old.iter().chain(d.new.iter()) {
                if !blobs.contains_key(oid) {
                    blobs.insert(oid.clone(), cat.object(oid, "blob")?);
                }
            }
        }
        Ok(blobs)
    })?;

    let (mut old_files, mut new_files) = (BTreeMap::new(), BTreeMap::new());
    let mut changes = Vec::new();
    for d in deltas {
        let old_meta = d.old.as_ref().map(|(mode, oid)| {
            let data = &blobs[oid];
            old_files.insert(d.path.clone(), data.clone());
            git_meta(mode, data.len())
        });
        let new_meta = d.new.as_ref().map(|(mode, oid)| {
            let data = &blobs[oid];
            new_files.insert(d.path.clone(), data.clone());
            git_meta(mode, data.len())
        });
        if let Some(change) =
            crate::changes::EntryChange::observed(d.path.clone(), old_meta, new_meta, crate::compare::DataEvidence::Unknown)
        {
            changes.push(change);
        }
    }
    Ok(Sides { old: GitFiles { files: old_files }, new: GitFiles { files: new_files }, changes })
}

#[cfg(test)]
#[path = "gitbridge_history_tests.rs"]
mod history_tests;

#[cfg(test)]
#[path = "gitbridge_pack_tests.rs"]
mod pack_tests;

#[cfg(test)]
mod export_tests {
    use super::*;
    use layr_hashers::{Digest, GitCompatible};

    fn object_id(kind: &str, data: &[u8]) -> String {
        let mut hash = GitCompatible::new();
        hash.update(format!("{kind} {}\0", data.len()).as_bytes());
        hash.update(data);
        hex::encode(hash.finalize())
    }

    fn scratch() -> Result<PathBuf> {
        let root = std::env::temp_dir().join(format!("layr-export-test-{}", ids::new_id()));
        std::fs::create_dir(&root)?;
        Ok(root)
    }

    #[test]
    fn git_packs_and_commit_lists_exclude_covered_history() -> Result<()> {
        let root = scratch()?;
        let result = (|| -> Result<()> {
            let store = crate::store::Store::open(&root)?;
            let p = crate::store::Project {
                lease: Default::default(),
                store: &store,
                name: "pack-test".into(),
                dir: root.clone(),
                id: ids::new_id(),
                db: crate::db::Db::open(&root.join("pack-test.sqlite"))?,
            };
            let repo = Repo { p, ctx: None, restore_from_remote: false };
            let git = super::store(&repo);
            assert!(Command::new("git").args(["init", "--bare", "-q"]).arg(&git).status()?.success());
            let tree = git_out(&git, &["mktree"])?.trim().to_owned();
            let commit = |args: &[&str]| -> Result<String> {
                let mut command = vec![
                    "-c",
                    "user.name=Pack Test",
                    "-c",
                    "user.email=test@example.test",
                    "-c",
                    "commit.gpgsign=false",
                    "commit-tree",
                    &tree,
                ];
                command.extend_from_slice(args);
                Ok(git_out(&git, &command)?.trim().to_owned())
            };
            let first = commit(&["-m", "first"])?;
            let second = commit(&["-p", &first, "-m", "second"])?;
            let third = commit(&["-p", &second, "-m", "third"])?;
            let new = [second.clone(), first.clone(), third.clone(), second.clone()];
            let covered = [first];
            assert_eq!(commits_outside(&repo, &new, &covered)?, [second.clone(), third, second]);
            let pack = pack_commits(&repo, &new, &covered)?;
            assert!(pack.starts_with(b"PACK"));
            assert_eq!(&pack[8..12], &2_u32.to_be_bytes(), "only the two uncovered commits");
            let invalid = ["missing-revision".into()];
            assert!(commits_outside(&repo, &[], &invalid)?.is_empty());
            for (subcommand, error) in [
                ("rev-list", commits_outside(&repo, &invalid, &[]).err()),
                ("pack-objects", pack_commits(&repo, &invalid, &[]).err()),
            ] {
                let error = error.context("Git accepted an unknown revision")?.to_string();
                assert!(error.starts_with(&format!("git {subcommand} failed: ")) && error.contains(&invalid[0]), "{error}");
            }
            Ok(())
        })();
        std::fs::remove_dir_all(root)?;
        result
    }

    #[test]
    fn loose_objects_match_buffered_bytes_and_reject_bad_identity() -> Result<()> {
        let root = scratch()?;
        let result = (|| -> Result<()> {
            for data in [Vec::new(), b"hello\n".to_vec(), vec![b'x'; 1024 * 1024]] {
                let oid = object_id("blob", &data);
                write_object(&root, "blob", &data, &oid)?;
                let path = root.join("objects").join(&oid[..2]).join(&oid[2..]);
                let mut reference = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
                reference.write_all(format!("blob {}\0", data.len()).as_bytes())?;
                reference.write_all(&data)?;
                assert_eq!(std::fs::read(&path)?, reference.finish()?);
                assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o777, 0o444);
                write_object(&root, "blob", &data, &oid)?;
                assert!(write_object(&root, "blob", b"different", &oid).is_err());
                assert_eq!(std::fs::read_dir(path.parent().unwrap())?.count(), 1);
            }
            Ok(())
        })();
        std::fs::remove_dir_all(root)?;
        result
    }

    #[test]
    fn export_commit_identity_parents_and_signing_fallback() -> Result<()> {
        let root = scratch()?;
        let result = (|| -> Result<()> {
            assert!(Command::new("git").args(["init", "--bare", "-q"]).arg(&root).status()?.success());
            git_out(&root, &["config", "commit.gpgsign", "false"])?;
            git_out(&root, &["config", "user.email", "author@example.test"])?;
            let state: StateRec = serde_json::from_value(serde_json::json!({
                "id": "test", "kind": "commit", "subvol": "", "time": 1700000000000i64,
                "author": {"name": "Author", "email": "author@example.test"}, "message": "message"
            }))?;
            let tree_data = b"";
            let tree = object_id("tree", tree_data);
            write_object(&root, "tree", tree_data, &tree)?;
            let export = ExportContext::load(root.clone());
            let first = export.commit(&state, &tree, &[], None)?;
            let expected = format!("tree {tree}\nauthor Author <author@example.test> 1700000000 +0000\ncommitter Author <author@example.test> 1700000000 +0000\n\nmessage");
            assert_eq!(first, object_id("commit", expected.as_bytes()));
            let second = export.commit(&state, &tree, std::slice::from_ref(&first), Some("second"))?;
            let parents = vec![first.clone(), second.clone()];
            let merged = export.commit(&state, &tree, &parents, Some("merge"))?;
            let raw = git_out(&root, &["cat-file", "commit", &merged])?;
            assert!(raw.starts_with(&format!("tree {tree}\nparent {first}\nparent {second}\n")));
            git_out(&root, &["config", "commit.gpgsign", "true"])?;
            git_out(&root, &["config", "gpg.program", "/nonexistent/layr-signing-test"])?;
            // A user gpg.format of ssh signs with ssh-keygen, not gpg.program: the fallback needs openpgp here.
            git_out(&root, &["config", "gpg.format", "openpgp"])?;
            let signing = ExportContext::load(root.clone());
            assert!(signing.signs(&state.author.email));
            assert!(!signing.signs("other@example.test"));
            assert!(!export.signs(&state.author.email)); // One operation keeps its config snapshot.
            assert_eq!(signing.commit(&state, &tree, &[], None)?, first);
            // Exercise successful signing through the same commit writer as exports.
            let key = root.join("signing-key");
            assert!(Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key).status()?.success());
            git_out(&root, &["config", "gpg.format", "ssh"])?;
            git_out(&root, &["config", "user.signingkey", key.to_str().unwrap()])?;
            let allowed = root.join("allowed-signers");
            std::fs::write(&allowed, format!("author@example.test {}", std::fs::read_to_string(key.with_extension("pub"))?))?;
            git_out(&root, &["config", "gpg.ssh.allowedSignersFile", allowed.to_str().unwrap()])?;
            let signed = ExportContext::load(root.clone()).commit(&state, &tree, &parents, None)?;
            let raw = git_out(&root, &["cat-file", "commit", &signed])?;
            assert!(raw.starts_with(&format!("tree {tree}\nparent {first}\nparent {second}\n")));
            assert!(raw.contains("gpgsig -----BEGIN SSH SIGNATURE-----"));
            git_out(&root, &["verify-commit", &signed])?;
            git_out(&root, &["config", "commit.gpgsign", "invalid"])?;
            assert!(!ExportContext::load(root.clone()).signs(&state.author.email));
            Ok(())
        })();
        std::fs::remove_dir_all(root)?;
        result
    }

    #[test]
    #[ignore = "requires a filesystem test volume (btrfs, APFS or OverlayFS)"]
    fn object_exports_materialize_once_per_state() -> Result<()> {
        let base = std::env::var_os("LAYR_APFS_TEST_ROOT").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let root = base.join(format!("layr-export-test-{}", ids::new_id()));
        std::fs::create_dir(&root)?;
        let result = (|| -> Result<()> {
            let store = crate::store::Store::open(&root.join("store"))?;
            let dir = store.project_dir("export")?;
            for path in ["meta/db", "meta/scan", "meta/work", "states"] {
                std::fs::create_dir_all(dir.join(path))?;
            }
            let p = crate::store::Project {
                lease: Default::default(),
                store: &store,
                name: "export".into(),
                dir: dir.clone(),
                id: ids::new_id(),
                db: crate::db::Db::open(&crate::store::db_file(&dir))?,
            };
            let actor =
                crate::model::Actor { uid: rustix::process::getuid().as_raw(), name: "Export Test".into(), session: None };
            let mut project = crate::model::DraftRecord::new("project");
            project.data = serde_json::json!({"name": "export", "id": p.id, "config": {}});
            p.lock_maintenance()?.append(project, &actor)?;
            let repo = Repo { restore_from_remote: true, p, ctx: None };
            let objects = crate::objstore::open(&repo.p)?;
            let source = root.join("source");
            std::fs::create_dir(&source)?;
            std::fs::write(source.join(".gitignore"), "*.log\n")?;
            std::fs::write(source.join("ignored.log"), "ignored\n")?;
            std::fs::write(source.join("run"), "#!/bin/sh\n")?;
            std::fs::set_permissions(source.join("run"), std::fs::Permissions::from_mode(0o755))?;
            std::os::unix::fs::symlink("text", source.join("link"))?;
            let mut parents = Vec::<String>::new();
            let mut commits = Vec::<String>::new();
            for version in 0..3 {
                let contents = format!("version {version}\n");
                std::fs::write(source.join("text"), &contents)?;
                // Equal size and mtime must still hash the changed path again.
                layr_objects::times::set(&source.join("text"), 1_700_000_000_000_000_000)?;
                let written = layr_objects::write_tree(
                    &objects,
                    &source,
                    &layr_objects::WriteOptions {
                        temperature: layr_objects::Temperature::Hot,
                        previous: None,
                        exclude: Vec::new(),
                        similar: false,
                    },
                )?;
                let id = ids::new_id();
                let refs = layr_objects::StateRef { tree: written.tree.unwrap(), times: written.times.unwrap() };
                objects.set_state(&id, &refs)?;
                let state: StateRec = serde_json::from_value(serde_json::json!({
                    "id": id, "kind": "commit", "subvol": "", "parents": parents,
                    "time": 1700000000000i64 + version * 1000,
                    "author": {"name": "Author", "email": "author@example.test"}, "message": contents,
                    "tree": refs.tree.to_string(), "times": refs.times.to_string()
                }))?;
                let mut record = crate::model::DraftRecord::new("state");
                record.states.push(state);
                repo.p.lock_maintenance()?.append(record, &actor)?;
                if version == 2 {
                    // Missing ancestor indexes retain the read-tree/full-refresh fallback.
                    std::fs::remove_file(index_path(&repo, &parents[0]))?;
                }
                crate::repo::MATERIALIZATIONS.with(|n| n.set(0));
                let (commit, made) = export_history(&repo, &repo.source_id(&id)?)?;
                assert_eq!(crate::repo::MATERIALIZATIONS.with(|n| n.get()), 1, "version {version}");
                assert_eq!(made.last(), Some(&(id.clone(), commit.clone())));
                assert!(!repo.p.state_path(&id).exists(), "export must not rebuild a second native state");
                assert!(index_path(&repo, &id).exists());
                if version == 0 {
                    let counts = git_out(&super::store(&repo), &["count-objects", "-v"])?;
                    assert!(counts.lines().any(|l| l == "packs: 1"), "initial export streams a pack");
                    assert!(counts.lines().any(|l| l == "count: 1"), "only the final commit is loose: {counts}");
                }
                let git = repo.p.git_dir();
                assert_eq!(git_out(&git, &["show", &format!("{commit}:text")])?, contents);
                let listing = git_out(&git, &["ls-tree", &commit])?;
                assert!(listing.contains("100755 blob") && listing.contains("120000 blob"));
                assert!(!listing.contains("ignored.log"));
                if let Some(parent) = commits.last() {
                    assert_eq!(git_out(&git, &["rev-parse", &format!("{commit}^")])?.trim(), parent);
                }
                // A cached export neither checks out the source nor changes commit identity.
                crate::repo::MATERIALIZATIONS.with(|n| n.set(0));
                assert_eq!(export_commit(&repo, &id)?, commit);
                assert_eq!(crate::repo::MATERIALIZATIONS.with(|n| n.get()), 0);
                check_reachable(&repo, &commit)?;
                parents = vec![id];
                commits.push(commit);
            }
            // Corruption of a packed blob must still rebuild the complete derived cache.
            forget_objects();
            VERIFIED_OBJECTS.with(|v| v.borrow_mut().clear());
            let packs = super::store(&repo).join("objects/pack");
            let pack = std::fs::read_dir(&packs)?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .find(|p| p.extension().is_some_and(|e| e == "pack"))
                .context("no export pack")?;
            std::fs::set_permissions(&pack, std::fs::Permissions::from_mode(0o644))?;
            std::fs::OpenOptions::new().write(true).open(&pack)?.set_len(12)?;
            // As at the push boundary, tree lookup alone is insufficient: verify all blobs.
            let repaired = cache_retry(&repo, || {
                let commit = export_commit(&repo, &parents[0])?;
                check_reachable(&repo, &commit)?;
                Ok(commit)
            })?;
            assert_eq!(Some(&repaired), commits.last());
            git_out(&super::store(&repo), &["fsck", "--full", "--no-reflogs", "--no-dangling"])?;
            Ok(())
        })();
        // Unprivileged btrfs cannot drop read-only scan subvolumes. Unseal test-owned
        // leftovers before removing the fixture (also works for sealed APFS trees).
        let scans = root.join("store/projects/export/meta/scan");
        for entry in std::fs::read_dir(scans)? {
            let path = entry?.path();
            crate::fs::set_writable(&path)?;
            crate::fs::remove(&path)?;
        }
        std::fs::remove_dir_all(root)?;
        result
    }

    fn write_object_buffered(s: &Path, kind: &str, data: &[u8], expected: &str) -> Result<()> {
        let _write = crate::trace::span("Git write object");
        use layr_hashers::{Digest, GitCompatible};
        let header = format!("{kind} {}\0", data.len());
        let mut hash = GitCompatible::new();
        hash.update(header.as_bytes());
        hash.update(data);
        let oid = hex::encode(hash.finalize());
        if oid != expected {
            bail!("stored Git {kind} hash mismatch: expected {expected}, got {oid}");
        }
        let path = s.join("objects").join(&oid[..2]).join(&oid[2..]);
        if path.exists() {
            return Ok(());
        }
        let mut compressed = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        compressed.write_all(header.as_bytes())?;
        compressed.write_all(data)?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::set_permissions(path.parent().unwrap(), std::fs::Permissions::from_mode(0o755))?;
        // Git objects are a rebuildable cache of durable records and state files. Publish complete
        // objects atomically, without forcing the whole filesystem twice for every loose object.
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = path.with_extension(format!("tmp-{}", ids::new_id()));
        let result = (|| -> Result<()> {
            let mut file = std::fs::OpenOptions::new().create_new(true).write(true).mode(0o444).open(&tmp)?;
            file.write_all(&compressed.finish()?)?;
            file.set_permissions(std::fs::Permissions::from_mode(0o444))?;
            drop(file);
            std::fs::rename(&tmp, &path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// Run each ignored case in a fresh process to compare peak RSS on incompressible objects.
    fn large_object(buffered: bool) -> Result<()> {
        use rand::{RngCore, SeedableRng};
        let root = scratch()?;
        let mut data = vec![0; 64 * 1024 * 1024];
        rand::rngs::StdRng::seed_from_u64(0).fill_bytes(&mut data);
        let oid = object_id("blob", &data);
        let result =
            if buffered { write_object_buffered(&root, "blob", &data, &oid) } else { write_object(&root, "blob", &data, &oid) };
        std::fs::remove_dir_all(root)?;
        result
    }

    #[test]
    #[ignore = "large-object measurement; run in an isolated temporary environment"]
    fn large_object_buffered() -> Result<()> {
        large_object(true)
    }

    #[test]
    #[ignore = "large-object measurement; run in an isolated temporary environment"]
    fn large_object_streamed() -> Result<()> {
        large_object(false)
    }
    fn export_chain(reuse: bool) -> Result<()> {
        let root = scratch()?;
        let result = (|| -> Result<()> {
            assert!(Command::new("git").args(["init", "--bare", "-q"]).arg(&root).status()?.success());
            git_out(&root, &["config", "commit.gpgsign", "true"])?;
            git_out(&root, &["config", "user.email", "someone-else@example.test"])?;
            let state: StateRec = serde_json::from_value(serde_json::json!({
                "id": "test", "kind": "commit", "subvol": "", "time": 1700000000000i64,
                "author": {"name": "Author", "email": "author@example.test"}, "message": "message"
            }))?;
            let tree = object_id("tree", b"");
            write_object(&root, "tree", b"", &tree)?;
            let shared = reuse.then(|| ExportContext::load(root.clone()));
            let mut parents = Vec::new();
            // A short history with one process per commit and two config queries per context.
            for _ in 0..32 {
                let per_commit;
                let export = if let Some(shared) = &shared {
                    shared
                } else {
                    per_commit = ExportContext::load(root.clone());
                    &per_commit
                };
                parents = vec![export.commit(&state, &tree, &parents, None)?];
            }
            println!("tip={}", parents[0]);
            Ok(())
        })();
        std::fs::remove_dir_all(root)?;
        result
    }

    #[test]
    #[ignore = "export context measurement; run in an isolated temporary environment"]
    fn export_context_per_commit() -> Result<()> {
        export_chain(false)
    }

    #[test]
    #[ignore = "export context measurement; run in an isolated temporary environment"]
    fn export_context_shared() -> Result<()> {
        export_chain(true)
    }
}
