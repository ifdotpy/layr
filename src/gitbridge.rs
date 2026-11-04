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
use crate::model::{Line, Record, StateKind, StateRec};
use crate::repo::{skip_path, Repo};
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn store(repo: &Repo) -> PathBuf {
    repo.p.git_dir()
}

pub fn has_store(repo: &Repo) -> bool {
    store(repo).join("objects").is_dir()
}

fn base_cmd(git_dir: &Path) -> Command {
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
    crate::privs::command_as(c, ctx.caller.uid, &env)
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
fn import_fetched(ctx: &Ctx, repo: &Repo, store: &Path, tmp: &Path, refs: &[(String, String)]) -> Result<Vec<(String, String)>> {
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
    let s = init_store(repo)?;
    let url = remote_url(repo, remote)?;
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
    c.args(["fetch", "-q", "--no-tags", "--no-write-fetch-head"])
        .arg(&url)
        .arg(format!("+refs/heads/*:refs/remotes/{remote}/*"))
        .arg("+refs/tags/*:refs/tags/*");
    as_caller(ctx, &mut c, &tmp)?;
    let out = c.stdin(Stdio::null()).output().context("run git fetch")?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        bail!("git fetch {url} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    test_sleep("LAYR_TEST_FETCH_SLEEP_MS");
    let refs = fetched_refs(ctx, &tmp, remote);
    let r = refs.and_then(|refs| import_fetched(ctx, repo, &s, &tmp, &refs));
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
    child: std::process::Child,
    input: std::process::ChildStdin,
    output: std::io::BufReader<std::process::ChildStdout>,
}

impl CatFile {
    fn new(s: &Path) -> Result<Self> {
        let mut child = base_cmd(s).args(["cat-file", "--batch"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
        let input = child.stdin.take().unwrap();
        let output = std::io::BufReader::new(child.stdout.take().unwrap());
        Ok(Self { child, input, output })
    }

    fn object(&mut self, oid: &str, kind: &str) -> Result<Vec<u8>> {
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
        let mut newline = [0];
        self.output.read_exact(&mut newline)?;
        use sha1::{Digest, Sha1};
        let mut hash = Sha1::new();
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
    fn is_tree(&self) -> bool {
        self.mode == "40000"
    }
    fn is_link(&self) -> bool {
        self.mode == "160000"
    }
}

fn parse_tree(data: &[u8]) -> Result<BTreeMap<String, TreeEntry>> {
    let mut out = BTreeMap::new();
    let mut rest = data;
    while !rest.is_empty() {
        let space = rest.iter().position(|b| *b == b' ').context("bad Git tree")?;
        let nul = rest.iter().position(|b| *b == 0).context("bad Git tree")?;
        if nul < space || rest.len() < nul + 21 {
            bail!("bad Git tree");
        }
        let mode = String::from_utf8_lossy(&rest[..space]).into_owned();
        let name = String::from_utf8_lossy(&rest[space + 1..nul]).into_owned();
        out.insert(name, TreeEntry { mode, oid: hex::encode(&rest[nul + 1..nul + 21]) });
        rest = &rest[nul + 21..];
    }
    Ok(out)
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
    let at = |name: &str| if prefix.is_empty() { name.to_string() } else { format!("{prefix}/{name}") };
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
    let mut args = vec!["ls-tree", "-r", "-z", "--full-tree", commit];
    let owned: Vec<String>;
    if let Some(p) = paths {
        args.push("--");
        owned = p.to_vec();
        args.extend(owned.iter().map(|s| s.as_str()));
    }
    // ls-tree with many paths: call in batches.
    let mut entries: Vec<(String, String, String)> = Vec::new();
    let mut present = std::collections::HashSet::new();
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
    let _ = args;
    for (_, _, p) in &entries {
        present.insert(p.clone());
    }
    if let Some(p) = paths {
        for path in p {
            if !present.contains(path) && fsutil::lmeta(dst, path).is_some() {
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
    records: Vec<(String, Record)>,
    /// The Git commit that was asked for, its state, and the remote ref to move (if any).
    commit: String,
    tip: String,
    refname: String,
}

/// Import the entire reachable DAG, parents first. A retry reuses completed imports. Each commit
/// becomes a state record without a tree; its tree is built from git when a command reads it
/// (`build_tree`, through `Repo::check_state`).
pub fn import_commit(ctx: &Ctx, repo: &Repo, commit: &str, refname: &str, author_uid: u32) -> Result<(String, bool)> {
    let _ = author_uid; // Git author metadata is preserved; the record actor remains the caller.
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

/// The state of a Git commit that the store maps (an imported state, or a state with a tree here).
fn mapped_state(repo: &Repo, oid: &str) -> Result<Option<String>> {
    for st in repo.p.db.state_of_commit(oid)? {
        // An imported state is mapped by its record, which holds the Git objects of the commit
        // (earlier versions imported only the tip and their records lack them). Other states are
        // mapped when their tree is on disk.
        let Some(rec) = repo.p.db.state(&st)? else { continue };
        let mapped = if rec.kind == StateKind::Import { repo.p.db.has_git_import(&st)? } else { repo.p.has_state(&st) };
        if mapped {
            return Ok(Some(st));
        }
    }
    Ok(None)
}

/// Plan the records of the commits of `commit` that are not imported yet. Reads the Git objects and
/// the records only: no project lock, no write.
pub fn plan_import(ctx: &Ctx, repo: &Repo, commit: &str, refname: &str) -> Result<ImportPlan> {
    let s = init_store(repo)?;
    let revs = git_out(&s, &["rev-list", "--topo-order", "--reverse", "--parents", commit])?;
    let mut states: BTreeMap<String, String> = BTreeMap::new();
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
                let parents: Vec<String> = git_parents
                    .iter()
                    .map(|p| states.get(*p).cloned().ok_or_else(|| anyhow!("Git parent {p} was not imported")))
                    .collect::<Result<_>>()?;
                let raw = cat.object(oid, "commit")?;
                let (objects, changes) = import_changes(&raw, git_parents.first().copied(), &mut cat)?;
                let (st, rec) = import_record(ctx, repo, oid, &raw, refname, parents, objects, changes)?;
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
    Ok(ImportPlan {
        records,
        commit: commit.to_string(),
        tip: states.remove(commit).context("Git tip was not imported")?,
        refname: refname.to_string(),
    })
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
        let _lock = (!repo.p.holds_lock()).then(|| repo.p.lock()).transpose()?;
        if guard.is_some() {
            let current = repo.p.db.remote_ref(&refname)?.map(|r| r.0);
            if current != expected {
                bail_moved(format!("remote ref {refname} moved during the fetch; run it again"))?;
            }
        }
        let mut written = 0;
        while written < IMPORT_CHUNK {
            let Some((oid, rec)) = pending.next() else { break };
            if mapped_state(repo, &oid)?.is_some() {
                bail_moved(format!("commit {} of {refname} was imported by another command; run it again", &oid[..10]))?;
            }
            let st = rec.states.first().map(|s| s.id.clone()).unwrap_or_default();
            repo.commit_record(ctx, rec)?;
            expected = Some(st);
            written += 1;
        }
        if written == IMPORT_CHUNK {
            continue;
        }
        if guard.is_some() && expected.as_deref() != Some(tip.as_str()) {
            let mut rec = Record::new("remote.ref");
            rec.data = serde_json::json!({"ref": refname, "state": tip, "commit": commit});
            repo.commit_record(ctx, rec)?;
        }
        return Ok((tip, fresh));
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
) -> Result<(String, Record)> {
    // On a store volume that ignores case, README and Readme would become one file. The earlier
    // state passed this check: only the entries of changed folders can collide.
    fsutil::refuse_case_collisions(&repo.p.work_dir(), changes.names.iter().map(|n| n.as_str()))?;
    let (name, email, msg, time) = commit_info(raw);
    let mut st = repo.state_record(ctx, ids::new_id(), StateKind::Import, parents, &msg, None, String::new());
    st.author = crate::model::Author { name, email, uid: 0, session: None };
    st.time = time.max(1);
    let mut rec = Record::new("import");
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

/// The states of `states` that `build_tree` can build (`buildable`), checked with one `git cat-file`
/// process for all of them: one process per state took minutes on a long imported history.
pub fn buildable_set(repo: &Repo, states: &[String]) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    if states.is_empty() || !has_store(repo) {
        return Ok(out);
    }
    let mut cat: Option<CatFile> = None;
    for state in states {
        let imported = matches!(repo.p.db.state(state), Ok(Some(s)) if s.kind == StateKind::Import);
        if !imported || !repo.p.db.has_git_import(state).unwrap_or(false) {
            continue;
        }
        let squash = repo.p.db.squash_commits_of(state)?;
        for c in repo.p.db.git_commits_of(state)?.into_iter().filter(|c| !squash.contains(c)) {
            let cat = match &mut cat {
                Some(c) => c,
                None => cat.insert(CatFile::new(&store(repo))?),
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
        let Some(s) = repo.p.db.state(&id)? else { continue };
        if s.kind == StateKind::Import && repo.p.has_state(&id) {
            if let Some(c) = stored_commit(repo, &id)? {
                return Ok(Some((id, c)));
            }
        }
        queue.extend(s.parents);
    }
    Ok(None)
}

/// Build the tree of an imported state from git and mark it present. The tree is a thin layer
/// over the nearest imported ancestor on disk, holding the changed paths (APFS), or a copy of that
/// ancestor with the changed paths (other file systems). Without such an ancestor it is the full
/// tree of the commit. `source` is a folder with the content of the commit (`init --from`); its
/// files are cloned where their bytes match the Git objects. The caller holds the project lock, or
/// this function takes it.
pub fn build_tree(repo: &Repo, state: &str, source: Option<&Path>) -> Result<()> {
    let _lock = (!repo.p.holds_lock()).then(|| repo.p.lock()).transpose()?;
    if repo.p.has_state(state) {
        return Ok(());
    }
    let rec = repo.state(state)?;
    if rec.kind != StateKind::Import {
        bail!("state {} is not an imported state", ids::short(state));
    }
    let commit = stored_commit(repo, state)?
        .with_context(|| format!("state {} has no Git commit in the store", ids::short(state)))?;
    let s = store(repo);
    let _t = crate::trace::span(format!("build {} from git", ids::short(state)));
    let mut cat = CatFile::new(&s)?;
    let raw = cat.object(&commit, "commit")?;
    let base = nearest_built_import(repo, state)?;
    let (_, changes) = import_changes(&raw, base.as_ref().map(|(_, c)| c.as_str()), &mut cat)?;
    let work = repo.p.work_dir().join(crate::store::temp_name());
    let made = (|| -> Result<()> {
        match &base {
            Some((b, _)) => {
                if thin_build(repo, &mut cat, &changes, &work, b, source, state)? {
                    return Ok(());
                }
                repo.snapshot_state(b, &work, false)?;
                let paths: Vec<String> =
                    changes.write.iter().map(|w| w.0.clone()).chain(changes.removed.iter().cloned()).collect();
                materialize(&s, &commit, &work, Some(&paths), None, source)?;
            }
            None => {
                crate::storage::create_subvolume(&work)?;
                materialize(&s, &commit, &work, None, None, source)?;
            }
        }
        crate::storage::snapshot(&work, &repo.p.state_path(state), true)
    })();
    let _ = crate::storage::delete_tree(&work);
    if let Err(e) = made {
        let _ = crate::storage::delete_tree(&repo.p.state_path(state));
        let _ = crate::storage::delete_tree(&repo.p.delta_path(state));
        return Err(e);
    }
    repo.p.db.set_present(state, true)?;
    Ok(())
}

/// A thin layer of the changes over the state `base`, written from the changed files only, as the
/// tree of `state`. False when the chain of `base` is long enough for a complete tree
/// (`thin.chain`), so reads stay short, and for a project that keeps complete trees.
#[cfg(target_os = "macos")]
fn thin_build(
    repo: &Repo,
    cat: &mut CatFile,
    changes: &Changes,
    work: &Path,
    base: &str,
    source: Option<&Path>,
    state: &str,
) -> Result<bool> {
    let max = repo.p.config_u64("thin.chain", crate::thin::MAX_CHAIN) as usize;
    let len = crate::thin::chain_len(&repo.p, base);
    if repo.p.is_overlay() || len == 0 || len >= max {
        return Ok(false);
    }
    std::fs::create_dir(work)?;
    std::fs::set_permissions(work, std::fs::Permissions::from_mode(0o755))?;
    let source = source.map(crate::tree::Tree::open).transpose()?;
    for (path, mode, oid) in &changes.write {
        if mode == "160000" {
            fsutil::make_parents(work, &format!("{path}/x"), None)?;
            continue;
        }
        let data = cat.object(oid, "blob")?;
        place_blob(work, mode, path, &data, None, source.as_ref())?;
    }
    let d = crate::thin::Diff {
        write: changes.write.iter().map(|w| w.0.clone()).collect(),
        removed: changes.removed.clone(),
        whole: Default::default(),
    };
    crate::thin::write_layer(work, &repo.p.delta_path(state), &d, base, false, None)?;
    repo.p.db.record_base(state, base)?;
    Ok(true)
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
fn thin_build(_: &Repo, _: &mut CatFile, _: &Changes, _: &Path, _: &str, _: Option<&Path>, _: &str) -> Result<bool> {
    Ok(false)
}

/// Removes the given paths from an index before they are added again. A copied index keeps the
/// stat data of the base state, and git takes a listed path as unchanged when its size and mtime
/// second match. An edit of the same size in the same second then kept the old blob. The copy
/// also gives the index a new mtime, which turns off git's own check for such racy entries.
const FORGET_CHANGED: [&str; 4] = ["update-index", "--force-remove", "-z", "--stdin"];

/// The folder that git reads `paths` of a state from: its complete tree, or on APFS a private
/// copy of only those paths when the state is a thin layer.
fn work_tree(repo: &Repo, src: &crate::repo::Snap, paths: &[String], keep: &mut Vec<crate::repo::Temp>) -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        if src.is_complete() {
            return Ok(src.bottom().to_path_buf());
        }
        let t = crate::repo::Temp { path: repo.p.scan_dir().join(crate::store::temp_name()) };
        crate::thin::extract(src, paths.iter().map(|p| p.as_str()), &t.path)?;
        let p = t.path.clone();
        keep.push(t);
        Ok(p)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (repo, paths, keep);
        Ok(src.clone())
    }
}

/// True when the state is a thin layer: git reads only changed paths of it.
fn is_thin(repo: &Repo, state: &str) -> bool {
    #[cfg(target_os = "macos")]
    return repo.state_src(state).map(|s| !s.is_complete()).unwrap_or(false);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (repo, state);
        false
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
                for p in [Some(c.path.clone()), c.old_path.clone()].into_iter().flatten() {
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
    let s = store(repo);
    let objects = git_out(&s, &["rev-list", "--objects", "--no-object-names", commit])?;
    let mut cat = CatFile::new(&s)?;
    for oid in objects.lines() {
        check_object(&s, &mut cat, oid, "")?;
    }
    Ok(())
}

/// Git objects are a derived local cache. Validate the exact object identity before publishing.
fn write_object(s: &Path, kind: &str, data: &[u8], expected: &str) -> Result<()> {
    let _write = crate::trace::span("Git write object");
    use sha1::{Digest, Sha1};
    let header = format!("{kind} {}\0", data.len());
    let mut hash = Sha1::new();
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

fn restore_import(repo: &Repo, state: &StateRec, parents: &[String], saved: serde_json::Value) -> Result<String> {
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
    if git_parents != parents {
        bail!("stored Git parents do not match state {}", ids::short(&state.id));
    }
    let s = init_store(repo)?;
    for (oid, bytes) in saved.trees {
        write_object(&s, "tree", &STANDARD.decode(bytes)?, &oid)?;
    }
    let tree = tree_id(&raw)?;
    let listed = base_cmd(&s).args(["ls-tree", "-r", "-z", tree]).output()?;
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
    let mut check = base_cmd(&s).args(["cat-file", "--batch-check"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
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
            write_object(&s, "blob", &data, &oid)?;
        }
    }
    writer.join().map_err(|_| anyhow!("Git object check writer failed"))??;
    if !check.wait()?.success() {
        bail!("Git object check failed");
    }
    let commit = repo.p.db.git_commit_of(&state.id)?.context("Git import has no commit ID")?;
    write_object(&s, "commit", &raw, &commit)?;
    git_out(&s, &["update-ref", &history_ref(&state.id), &commit])?;
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
                let (c, _) = export_history(repo, &a)?;
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
    let c = commit_tree(repo, state, anc.as_ref(), &parents, message, &format!("refs/layr/squash/{}", ids::display(&state.id)))?;
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
            None => export_history(repo, state)?.0,
        },
    };
    let mut cat = CatFile::new(&s)?;
    let raw = cat.object(&commit, "commit")?;
    let mut oid = tree_id(&raw)?.to_string();
    let mut kind = "tree";
    for component in path.trim_end_matches('/').split('/').filter(|_| !path.is_empty()) {
        if kind != "tree" {
            return Ok(None);
        }
        let bytes = cat.object(&oid, "tree")?;
        let mut rest = bytes.as_slice();
        let mut found = None;
        while !rest.is_empty() {
            let space = rest.iter().position(|b| *b == b' ').context("bad Git tree mode")?;
            let nul = rest.iter().position(|b| *b == 0).context("bad Git tree name")?;
            let end = nul.checked_add(21).filter(|end| *end <= rest.len()).context("short Git tree ID")?;
            if rest.get(space + 1..nul) == Some(component.as_bytes()) {
                let kind = match &rest[..space] {
                    b"40000" | b"040000" => "tree",
                    b"160000" => "commit",
                    _ => "blob",
                };
                found = Some((hex::encode(&rest[nul + 1..end]), kind));
                break;
            }
            rest = &rest[end..];
        }
        let Some((next, next_kind)) = found else { return Ok(None) };
        oid = next;
        kind = next_kind;
    }
    // Gitlinks name commits in another repository; their containing tree authenticates the ID.
    if kind != "commit" {
        check_object(&s, &mut cat, &oid, kind)?;
    }
    Ok(Some(oid))
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
pub fn export_history(repo: &Repo, state: &str) -> Result<(String, Vec<(String, String)>)> {
    cache_retry(repo, || export_history_once(repo, state))
}

fn export_history_once(repo: &Repo, state: &str) -> Result<(String, Vec<(String, String)>)> {
    repo.source_state(state)?;
    let mut known: BTreeMap<String, String> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<(String, bool)> = vec![(state.to_string(), false)];
    let mut made = Vec::new();
    while let Some((s, parents_done)) = stack.pop() {
        if parents_done {
            order.push(s);
            continue;
        }
        if !seen.insert(s.clone()) {
            continue;
        }
        if let Some(c) = recorded_commit(repo, &s)? {
            known.insert(s, c);
            continue;
        }
        // A history commit made earlier without a record: a record of this export names it.
        if let Some(c) = history_commit(repo, &s)? {
            made.push((s.clone(), c.clone()));
            known.insert(s.clone(), c);
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
    for s in order {
        let rec = repo.state(&s)?;
        let parents: Vec<String> = rec
            .parents
            .iter()
            .map(|p| known.get(p).cloned().ok_or_else(|| anyhow!("no commit for state {}", ids::short(p))))
            .collect::<Result<_>>()?;
        let base = rec.parents.first().map(|p| (p.clone(), known[p].clone()));
        let c = if let Some(saved) = repo.p.db.git_import(&s)? {
            restore_import(repo, &rec, &parents, saved)?
        } else {
            commit_tree(repo, &rec, base.as_ref(), &parents, None, &history_ref(&s))?
        };
        known.insert(s.clone(), c.clone());
        made.push((s, c));
    }
    let tip = known.get(state).cloned().ok_or_else(|| anyhow!("no commit for state {}", ids::short(state)))?;
    Ok((tip, made))
}

/// Write the commit of a state: the tree from the index of `base` (a state with its commit) plus
/// the paths that changed since, the given parent commits, author and committer from the state.
/// `refname` is set to the commit.
fn commit_tree(
    repo: &Repo,
    state: &StateRec,
    base: Option<&(String, String)>,
    parents: &[String],
    message: Option<&str>,
    refname: &str,
) -> Result<String> {
    let s = init_store(repo)?;
    let src = repo.state_src(&state.id)?;
    let tmp_idx = std::env::temp_dir().join(format!("layr-index-{}", ids::new_id()));
    let mut keep: Vec<crate::repo::Temp> = Vec::new();
    let res = (|| -> Result<String> {
        let mut paths: Vec<String> = Vec::new();
        let mut full: Option<PathBuf> = None;
        // True when the index is a copy of the index of the base: its entries for unchanged
        // files have valid stat data, so no refresh of every tracked file is needed.
        let mut inherited = false;
        match base {
            Some((ast, ac)) => {
                let ai = index_path(repo, ast);
                // gc may remove a kept index at any time: then the tree of the commit is read.
                inherited = ai.exists() && std::fs::copy(&ai, &tmp_idx).is_ok();
                if !inherited {
                    let out = base_cmd(&s).env("GIT_INDEX_FILE", &tmp_idx).args(["read-tree", ac]).output()?;
                    if !out.status.success() {
                        bail!("git read-tree failed");
                    }
                }
                let aroot = repo.state_src(ast)?;
                let rules = crate::ignore_rules::Rules::of(&src);
                for c in changes::file_changes(&aroot, &src)? {
                    for p in [Some(c.path.clone()), c.old_path.clone()].into_iter().flatten() {
                        if skip_path(&p) {
                            continue;
                        }
                        // Ignored paths are never exported, unless the ancestor tracked them.
                        if c.old.is_none() && rules.ignored(&p, false) {
                            continue;
                        }
                        paths.push(p);
                    }
                }
            }
            None => {
                let root = repo.complete(src.clone(), &mut keep)?;
                full = Some(root.clone());
                let rules = crate::ignore_rules::Rules::new(&root);
                fsutil::walk(&root, "", &mut |rel, m| {
                    if skip_path(rel) || rules.ignored(rel, m.is_dir()) {
                        return Ok(false);
                    }
                    if !m.is_dir() {
                        paths.push(rel.to_string());
                    }
                    Ok(true)
                })?;
            }
        }
        let mut input = Vec::new();
        for p in &paths {
            input.extend_from_slice(p.as_bytes());
            input.push(0);
        }
        let root = match full {
            Some(root) => root,
            None => work_tree(repo, &src, &paths, &mut keep)?,
        };
        // Changed paths are hashed again: see FORGET_CHANGED.
        let _t = crate::trace::span(format!("git index update of {} changed paths", paths.len()));
        for args in [&FORGET_CHANGED[..], &["update-index", "--add", "--remove", "-z", "--stdin"]] {
            let mut c = base_cmd(&s);
            c.env("GIT_INDEX_FILE", &tmp_idx).env("GIT_WORK_TREE", &root).args(args);
            c.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
            let mut ch = c.spawn()?;
            ch.stdin.take().unwrap().write_all(&input)?;
            let out = ch.wait_with_output()?;
            if !out.status.success() {
                bail!("git update-index failed: {}", String::from_utf8_lossy(&out.stderr));
            }
        }
        let tree = String::from_utf8_lossy(&base_cmd(&s).env("GIT_INDEX_FILE", &tmp_idx).args(["write-tree"]).output()?.stdout)
            .trim()
            .to_string();
        if tree.is_empty() {
            bail!("git write-tree failed");
        }
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
            let mut c = base_cmd(&s);
            c.args(["commit-tree", &tree]);
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
        let sign = git_out(&s, &["config", "--bool", "commit.gpgsign"]).is_ok_and(|v| v.trim() == "true")
            && git_out(&s, &["config", "user.email"]).is_ok_and(|v| v.trim() == state.author.email);
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
        let commit = String::from_utf8_lossy(&out.stdout).trim().to_string();
        git_out(&s, &["update-ref", refname, &commit])?;
        // Keep the index for the next export and for lines made from this state.
        keep_index(repo, &state.id, &tmp_idx, inherited)?;
        Ok(commit)
    })();
    let _ = std::fs::remove_file(&tmp_idx);
    res
}

/// Keep `idx` as the index of `state` unless it has one. An index that is not a copy of an
/// earlier index gets its stat data from one refresh. The file appears complete or not at
/// all: other processes copy kept indexes without the project lock.
fn keep_index(repo: &Repo, state: &str, idx: &Path, inherited: bool) -> Result<()> {
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
        let _ = refresh_index(repo, state);
    }
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
    let (c, _) = export_history(repo, state)?;
    if !index_path(repo, state).exists() {
        let prev = repo.state(state)?.parents.first().cloned().filter(|p| index_path(repo, p).exists());
        build_index(repo, state, &c, prev.as_deref())?;
    }
    Ok(c)
}

fn refresh_index(repo: &Repo, state: &str) -> Result<()> {
    // An index made from the previous index and the changed paths of a thin state has valid
    // stat data already; a refresh would need a complete tree.
    if is_thin(repo, state) {
        return Ok(());
    }
    let _t = crate::trace::span("git index refresh");
    let s = store(repo);
    let root = repo.state_root(state)?;
    base_cmd(&s)
        .env("GIT_INDEX_FILE", index_path(repo, state))
        .env("GIT_WORK_TREE", &root)
        .args(["-c", "core.checkStat=minimal", "-c", "core.trustctime=false", "update-index", "-q", "--refresh"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(())
}

/// Push a prepared commit to `refs/heads/<branch>` of the remote, as the caller.
pub fn push(ctx: &Ctx, repo: &Repo, url: &str, commit: &str, branch: &str, force: bool) -> Result<()> {
    if let Err(error) = check_reachable(repo, commit) {
        if !repair_cache(repo)? {
            return Err(error);
        }
        // The damaged cache went aside. Imported states have no trees to rebuild their commits from,
        // so the Git remotes of the project give the objects back (git is the source).
        let mut remotes: Vec<String> = repo.p.db.remote_refs()?.into_iter().filter_map(|(name, _, _)| name.split('/').next().map(str::to_owned)).collect();
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
            if repo.tree_available(&state) && export_history(repo, &state)?.0 == commit {
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
    let before = ls_remote(ctx, repo, url, branch)?;
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
    let mut c = base_cmd(&s);
    c.args(["push", "-q", url]);
    let spec = format!("{}{commit}:refs/heads/{branch}", if force { "+" } else { "" });
    c.arg(spec);
    as_caller(ctx, &mut c, &s)?;
    let out = c.stdin(Stdio::null()).output().context("run git push")?;
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
    let g = w.join(".git");
    if let Ok(m) = std::fs::symlink_metadata(&g) {
        if m.is_dir() && !crate::storage::is_subvolume(&g) && std::fs::read_dir(&g)?.next().is_none() {
            std::fs::remove_dir(&g)?;
        } else {
            return Ok(());
        }
    }
    crate::storage::create_subvolume(&g)?;
    let s = store(repo);
    for d in ["objects/info", "refs/heads", "refs/tags", "info"] {
        std::fs::create_dir_all(g.join(d))?;
    }
    std::fs::write(g.join("objects/info/alternates"), format!("{}\n", s.join("objects").display()))?;
    std::fs::write(
        g.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tcheckStat = minimal\n\ttrustctime = false\n\tlogallrefupdates = false\n",
    )?;
    std::fs::write(g.join("info/exclude"), "")?;
    // The state's own commit; if it cannot be made, the nearest commit a record names.
    let anc = match line_commit(repo, state) {
        Ok(c) => Some((state.to_string(), c)),
        Err(e) => {
            eprintln!("warning: .git folder shows an older commit: {e:#}");
            mapped_ancestor(repo, state).ok().flatten()
        }
    };
    match &anc {
        Some((_, c)) => std::fs::write(g.join("HEAD"), format!("{c}\n"))?,
        None => std::fs::write(g.join("HEAD"), "ref: refs/heads/main\n")?,
    }
    std::fs::write(g.join("packed-refs"), packed_refs_text(repo).unwrap_or_else(|_| PACKED_HEADER.to_string()))?;
    if let Some((ast, _)) = &anc {
        let idx = index_path(repo, ast);
        if idx.exists() {
            let src = std::fs::File::open(&idx)?;
            let dst = std::fs::File::create(g.join("index"))?;
            if crate::storage::clone_file(&src, &dst).is_err() {
                std::fs::copy(&idx, g.join("index"))?;
            }
        }
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
    // Read only for git: a git write needs lock files and new objects; layr writes through descriptors.
    fsutil::git_writable(w, false);
    Ok(())
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
    let w = repo.working(line);
    let dir = match fsutil::open_dir_nofollow(&w.join(".git")) {
        Ok(d) => d,
        Err(_) => return Ok(()),
    };
    if !fsutil::is_layer(&w, ".git", &fsutil::meta_of(&dir.metadata()?)) {
        return Ok(());
    }
    let want = packed_refs_text(repo)?;
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
    let g = repo.working(line).join(".git");
    let dir = match fsutil::open_dir_nofollow(&g) {
        Ok(d) => d,
        Err(_) => return Ok(()),
    };
    // Only the subvolume layr made; a .git folder the line owner made is its own.
    {
        if !fsutil::is_layer(&repo.working(line), ".git", &fsutil::meta_of(&dir.metadata()?)) {
            return Ok(());
        }
    }
    // A HEAD that is not a regular file (for example a link) is replaced.
    let cur = fsutil::read_at(&dir, "HEAD").map(|c| String::from_utf8_lossy(&c).trim().to_string()).unwrap_or_default();
    let c = line_commit(repo, &line.head)?;
    let w = repo.working(line);
    if cur == c {
        fsutil::git_writable(&w, false);
        return Ok(());
    }
    fsutil::git_writable(&w, true);
    let r = write_git_folder(repo, line, &dir, &c);
    fsutil::git_writable(&w, false);
    r
}

fn write_git_folder(repo: &Repo, line: &Line, dir: &std::fs::File, c: &str) -> Result<()> {
    let own = Some(repo.owner_ids(line));
    let head = format!("{c}\n");
    fsutil::replace_at(dir, "HEAD", &mut |f| Ok((&*f).write_all(head.as_bytes())?), own)?;
    // The index of the head state: its stat data matches files copied from that state.
    let idx = index_path(repo, &line.head);
    if idx.exists() {
        fsutil::replace_at(
            dir,
            "index",
            &mut |f| {
                let src = std::fs::File::open(&idx)?;
                if crate::storage::clone_file(&src, f).is_err() {
                    std::io::copy(&mut std::fs::File::open(&idx)?, &mut &*f)?;
                }
                Ok(())
            },
            own,
        )?;
    } else {
        fsutil::unlink_at(dir, "index");
    }
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
    let w = repo.working(line);
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
    // Without a commit for the head, refresh_git_folder keeps HEAD and only warns: no refusal.
    let Ok(expected) = line_commit(repo, &line.head) else {
        return Ok(());
    };
    if cur != expected {
        bail!(
            "A git commit, merge, reset or checkout ran in this line ({cur}). layr did not touch the files.\n\
             To keep the files as they are and continue, run:\n  printf '%s\\n' {expected} > .git/HEAD\n\
             and then:\n  layr save -a -m \"<message>\"\n\
             Use layr merge, layr git fetch and layr save instead of git."
        );
    }
    Ok(())
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
        changes.push((path, Side { mode: f[0].to_string(), id: f[2].to_string() }, Side { mode: f[1].to_string(), id: f[3].to_string() }));
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
        keep.push(crate::repo::Temp { path: scratch_folder(repo)? });
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
    // `btrfs send` sends read-only subvolumes only.
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    for k in &keep {
        crate::storage::set_readonly(&k.path)?;
    }
    Ok(Pair { old: crate::repo::snap(&keep[0].path), new: crate::repo::snap(&keep[1].path), keep })
}

/// An empty folder for one side of a sparse pair. On btrfs it is a subvolume: a comparison of two
/// trees sends them (`btrfs send`), and that needs subvolumes.
fn scratch_folder(repo: &Repo) -> Result<PathBuf> {
    let path = repo.p.scan_dir().join(crate::store::temp_name());
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    crate::storage::create_subvolume(&path)?;
    #[cfg(not(all(target_os = "linux", not(feature = "overlayfs"))))]
    std::fs::create_dir_all(&path)?;
    Ok(path)
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

/// The entry at `path` of `commit` ("" is the root), read from the git objects.
pub fn entry(repo: &Repo, commit: &str, path: &str) -> Result<Entry> {
    let s = store(repo);
    let path = path.trim_matches('/');
    if path.is_empty() {
        return Ok(Entry::Dir(folder_names(&s, &format!("{commit}^{{tree}}"))?));
    }
    let out = git_out_bytes(&s, &["ls-tree", "-z", "--full-tree", commit, "--", path])?;
    let Some(rec) = out.split(|b| *b == 0).find(|r| !r.is_empty()) else { return Ok(Entry::Missing) };
    let rec = String::from_utf8_lossy(rec).into_owned();
    let (meta, _) = rec.split_once('\t').context("bad Git tree entry")?;
    let fields: Vec<&str> = meta.split(' ').collect();
    match (fields[0], fields[1]) {
        ("040000", _) | (_, "tree") => Ok(Entry::Dir(folder_names(&s, fields[2])?)),
        (_, "commit") => Ok(Entry::Dir(Vec::new())),
        ("120000", _) => Ok(Entry::Link(git_out_bytes(&s, &["cat-file", "blob", fields[2]])?)),
        _ => Ok(Entry::File(git_out_bytes(&s, &["cat-file", "blob", fields[2]])?)),
    }
}

fn folder_names(s: &Path, tree: &str) -> Result<Vec<(String, bool)>> {
    let out = git_out_bytes(s, &["ls-tree", "-z", tree])?;
    let mut names = Vec::new();
    for rec in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let rec = String::from_utf8_lossy(rec).into_owned();
        let (meta, name) = rec.split_once('\t').context("bad Git tree entry")?;
        let is_dir = meta.split(' ').nth(1).is_some_and(|t| t == "tree" || t == "commit");
        names.push((name.to_string(), is_dir));
    }
    names.sort();
    Ok(names)
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
        let size: usize = header.split_whitespace().nth(2).and_then(|x| x.parse().ok()).with_context(|| format!("bad cat-file header for {oid}: {header}"))?;
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
    let mut child = base_cmd(&s)
        .args(["pack-objects", "--stdout", "--revs", "-q"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = child.stdin.take().context("pack-objects has no input")?;
    let lines: Vec<String> = new.iter().cloned().chain(covered.iter().map(|c| format!("^{c}"))).collect();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        for line in lines {
            writeln!(input, "{line}")?;
        }
        Ok(())
    });
    let out = child.wait_with_output()?;
    writer.join().map_err(|_| anyhow!("pack-objects writer failed"))??;
    if !out.status.success() {
        bail!("git pack-objects failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// Add the objects of a pack to the git store as loose objects, as a restored import writes them
/// (`write_object`). A pack of a whole history is unpacked this way too.
pub fn install_pack(repo: &Repo, pack: &[u8]) -> Result<()> {
    let s = init_store(repo)?;
    let mut child = base_cmd(&s)
        .args(["unpack-objects", "-q"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
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
        f(cat)
    })
}

/// True when a read can take the state's changes: its tree is on disk, or it is an imported state
/// whose commit is known (the record names it). Reads no git object; a missing object shows when the
/// read happens.
pub fn readable(repo: &Repo, state: &str) -> Result<bool> {
    Ok(repo.p.has_state(state) || imported_commit(repo, state)?.is_some())
}

/// Forgets the object reader (the store changed on disk: a damaged cache went aside).
fn forget_objects() {
    OBJECTS.with(|cell| cell.borrow_mut().take());
}

/// One changed file of a commit against its base: the (mode, object id) of each side, None when
/// the side has no file there (absent, a folder, or a submodule).
struct Delta {
    path: String,
    old: Option<(String, String)>,
    new: Option<(String, String)>,
}

fn file_side(e: Option<&TreeEntry>) -> Option<(String, String)> {
    e.filter(|e| !e.is_tree() && !e.is_link()).map(|e| (e.mode.clone(), e.oid.clone()))
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
        if o == e {
            continue;
        }
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        let o_tree = o.filter(|x| x.is_tree()).map(|x| x.oid.as_str());
        let e_tree = e.filter(|x| x.is_tree()).map(|x| x.oid.as_str());
        if o_tree.is_some() || e_tree.is_some() {
            tree_deltas(cat, o_tree, e_tree, &path, out)?;
        }
        let (old_side, new_side) = (file_side(o), file_side(e));
        if old_side != new_side {
            out.push(Delta { path, old: old_side, new: new_side });
        }
    }
    Ok(())
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
    pub changes: Vec<crate::changes::Change>,
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

pub fn sides_of(repo: &Repo, old: Option<&str>, new: &str) -> Result<Sides> {
    let (deltas, blobs) = with_objects(repo, |cat| {
        let new_tree = tree_id(&cat.object(new, "commit")?)?.to_string();
        let old_tree = match old {
            Some(o) => Some(tree_id(&cat.object(o, "commit")?)?.to_string()),
            None => None,
        };
        let mut deltas = Vec::new();
        tree_deltas(cat, old_tree.as_deref(), Some(&new_tree), "", &mut deltas)?;
        deltas.sort_by(|a, b| a.path.cmp(&b.path));
        let mut blobs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for d in &deltas {
            for (_, oid) in d.old.iter().chain(d.new.iter()) {
                if !blobs.contains_key(oid) {
                    blobs.insert(oid.clone(), cat.object(oid, "blob")?);
                }
            }
        }
        Ok((deltas, blobs))
    })?;
    let (mut old_files, mut new_files) = (BTreeMap::new(), BTreeMap::new());
    let mut changes = Vec::new();
    for d in &deltas {
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
        changes.push(crate::changes::Change { path: d.path.clone(), old_path: None, old: old_meta, new: new_meta, ranges: None });
    }
    Ok(Sides { old: GitFiles { files: old_files }, new: GitFiles { files: new_files }, changes })
}
