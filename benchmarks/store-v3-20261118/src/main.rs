//! THROWAWAY prototype: store v3 (source states as git objects plus a times table kept in git)
//! against store v2 (layr-objects). Same corpus, same edits, same steps. No layr integration.
//!
//! v3 layout of one state:
//! - a git commit whose tree is the source tree (blobs, trees, modes 100644/100755/120000/40000);
//! - a "times tree" that mirrors the folders: each folder has a blob `.layr-times` (its own time,
//!   then one i64 per entry of the source tree in git order, 0 for a folder) and one subtree per
//!   subfolder. Both are refs, so `git repack` keeps them.
//!
//! usage: store-v3-prototype <v2-hot|v2-cold|v3> <work folder> <scratch> <rounds> <batch>
//! The work folder is a private copy of the corpus: the edits go there.
use anyhow::{bail, ensure, Context, Result};
use gix::objs::Write as _;
use gix::ObjectId;
use layr_objects::{Id, Keying, Node, StateRef, Store, Temperature, WriteOptions};
use rayon::prelude::*;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn mtime(m: &fs::Metadata) -> i64 {
    m.mtime() * 1_000_000_000 + m.mtime_nsec()
}

/// Logical bytes, allocated bytes and file count of a folder.
fn usage(root: &Path) -> (u64, u64, u64) {
    let mut n = (0, 0, 0);
    let Ok(rd) = fs::read_dir(root) else { return n };
    for e in rd.flatten() {
        let Ok(m) = e.metadata() else { continue };
        if m.is_dir() {
            let a = usage(&e.path());
            n.0 += a.0;
            n.1 += a.1;
            n.2 += a.2;
        } else {
            n.0 += m.len();
            n.1 += m.blocks() * 512;
            n.2 += 1;
        }
    }
    n
}

/// Every regular file of the corpus (relative paths), sorted. `.git` is left out.
fn files(root: &Path, rel: &str, out: &mut Vec<String>) -> Result<()> {
    let mut names: Vec<_> = fs::read_dir(root.join(rel))?.collect::<std::io::Result<_>>()?;
    names.sort_by_key(|e| e.file_name());
    for e in names {
        let name = e.file_name().to_string_lossy().into_owned();
        if rel.is_empty() && name == ".git" {
            continue;
        }
        let path = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        let ft = e.file_type()?;
        if ft.is_dir() {
            files(root, &path, out)?;
        } else if ft.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- v3: git objects

const TIMES: &[u8] = b".layr-times";

#[derive(Clone)]
struct GEntry {
    mode: &'static str,
    name: Vec<u8>,
    oid: ObjectId,
}

fn is_tree(mode: &str) -> bool {
    mode == "40000"
}

/// Git order: a folder sorts as if its name ended with '/'.
fn git_sort(entries: &mut [GEntry]) {
    entries.sort_by(|a, b| {
        let ka: Vec<u8> = if is_tree(a.mode) { [a.name.as_slice(), b"/"].concat() } else { a.name.clone() };
        let kb: Vec<u8> = if is_tree(b.mode) { [b.name.as_slice(), b"/"].concat() } else { b.name.clone() };
        ka.cmp(&kb)
    });
}

fn encode_tree(entries: &[GEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(entries.len() * 40);
    for e in entries {
        out.extend_from_slice(e.mode.as_bytes());
        out.push(b' ');
        out.extend_from_slice(&e.name);
        out.push(0);
        out.extend_from_slice(e.oid.as_bytes());
    }
    out
}

fn decode_tree(data: &[u8]) -> Result<Vec<GEntry>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let sp = i + data[i..].iter().position(|b| *b == b' ').context("tree mode")?;
        let mode = match &data[i..sp] {
            b"100644" => "100644",
            b"100755" => "100755",
            b"120000" => "120000",
            b"40000" => "40000",
            b"160000" => "160000",
            m => bail!("tree mode {}", String::from_utf8_lossy(m)),
        };
        let nul = sp + 1 + data[sp + 1..].iter().position(|b| *b == 0).context("tree name")?;
        let name = data[sp + 1..nul].to_vec();
        let oid = ObjectId::try_from(&data[nul + 1..nul + 21]).context("tree oid")?;
        out.push(GEntry { mode, name, oid });
        i = nul + 21;
    }
    Ok(out)
}

fn put(repo: &gix::Repository, kind: gix::objs::Kind, data: &[u8]) -> Result<ObjectId> {
    Ok(repo.objects.write_buf(kind, data)?)
}

fn get(repo: &gix::Repository, oid: ObjectId) -> Result<Vec<u8>> {
    Ok(repo.find_object(oid)?.detach().data)
}

/// The meta table of a folder: its own time, then per entry its time and its exact mode (git
/// keeps only 644, 755 and link).
type Rec = (i64, u32);

fn encode_times(own: i64, records: &[Rec]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + records.len() * 12);
    out.extend_from_slice(&own.to_le_bytes());
    for (t, m) in records {
        out.extend_from_slice(&t.to_le_bytes());
        out.extend_from_slice(&m.to_le_bytes());
    }
    out
}

fn decode_times(data: &[u8]) -> (i64, Vec<Rec>) {
    let own = i64::from_le_bytes(data[..8].try_into().unwrap());
    let recs = data[8..]
        .chunks_exact(12)
        .map(|c| (i64::from_le_bytes(c[..8].try_into().unwrap()), u32::from_le_bytes(c[8..].try_into().unwrap())))
        .collect();
    (own, recs)
}

#[derive(Clone, Copy)]
struct V3State {
    commit: ObjectId,
    tree: ObjectId,
    times: ObjectId,
}

/// Write one folder of `work` (relative `rel`): its source tree and its times tree. `blobs` holds
/// the blob of every regular file, written before in parallel.
fn v3_write_dir(repo: &gix::Repository, work: &Path, rel: &str, blobs: &BTreeMap<String, ObjectId>) -> Result<(ObjectId, ObjectId)> {
    let dir = work.join(rel);
    let own = mtime(&fs::metadata(&dir)?);
    let mut entries = Vec::new();
    let mut times: BTreeMap<Vec<u8>, Rec> = BTreeMap::new();
    let mut subtimes = Vec::new();
    for e in fs::read_dir(&dir)? {
        let e = e?;
        let name = e.file_name().as_bytes().to_vec();
        if rel.is_empty() && name == b".git" {
            continue;
        }
        let path = if rel.is_empty() { String::from_utf8_lossy(&name).into_owned() } else { format!("{rel}/{}", String::from_utf8_lossy(&name)) };
        let m = fs::symlink_metadata(e.path())?;
        if m.is_dir() {
            let (t, tt) = v3_write_dir(repo, work, &path, blobs)?;
            entries.push(GEntry { mode: "40000", name: name.clone(), oid: t });
            subtimes.push(GEntry { mode: "40000", name: name.clone(), oid: tt });
            times.insert(name, (0, m.permissions().mode() & 0o7777));
        } else if m.file_type().is_symlink() {
            let target = fs::read_link(e.path())?;
            let oid = put(repo, gix::objs::Kind::Blob, target.as_os_str().as_bytes())?;
            entries.push(GEntry { mode: "120000", name: name.clone(), oid });
            times.insert(name, (mtime(&m), 0));
        } else if m.is_file() {
            let exec = m.permissions().mode() & 0o111 != 0;
            entries.push(GEntry { mode: if exec { "100755" } else { "100644" }, name: name.clone(), oid: match blobs.get(&path) {
                Some(o) => *o,
                None => put(repo, gix::objs::Kind::Blob, &fs::read(e.path())?)?,
            } });
            times.insert(name, (mtime(&m), m.permissions().mode() & 0o7777));
        }
    }
    git_sort(&mut entries);
    let records: Vec<Rec> = entries.iter().map(|e| times[&e.name]).collect();
    let tree = put(repo, gix::objs::Kind::Tree, &encode_tree(&entries))?;
    let tblob = put(repo, gix::objs::Kind::Blob, &encode_times(own, &records))?;
    subtimes.push(GEntry { mode: "100644", name: TIMES.to_vec(), oid: tblob });
    git_sort(&mut subtimes);
    let ttree = put(repo, gix::objs::Kind::Tree, &encode_tree(&subtimes))?;
    Ok((tree, ttree))
}

fn v3_commit(repo: &gix::Repository, tree: ObjectId, parent: Option<ObjectId>, n: usize) -> Result<ObjectId> {
    let mut c = format!("tree {tree}\n");
    if let Some(p) = parent {
        c.push_str(&format!("parent {p}\n"));
    }
    let when = 1_700_000_000 + n;
    c.push_str(&format!("author Layr <layr@example.invalid> {when} +0000\ncommitter Layr <layr@example.invalid> {when} +0000\n\nstate {n}\n"));
    put(repo, gix::objs::Kind::Commit, c.as_bytes())
}

fn v3_ref(git: &Path, name: &str, oid: ObjectId) -> Result<()> {
    let p = git.join("refs/layr").join(name);
    fs::create_dir_all(p.parent().unwrap())?;
    fs::write(p, format!("{oid}\n"))?;
    Ok(())
}

/// Change the files `changes` (path -> blob, mode, time) in the folder (`tree`, `times`). Reads
/// the trees on the paths from the store and writes the new ones bottom up.
fn v3_update(repo: &gix::Repository, tree: ObjectId, ttree: ObjectId, changes: &BTreeMap<&str, (ObjectId, Rec)>) -> Result<(ObjectId, ObjectId)> {
    let mut entries = decode_tree(&get(repo, tree)?)?;
    let mut tentries = decode_tree(&get(repo, ttree)?)?;
    let ti = tentries.iter().position(|e| e.name == TIMES).context("times blob")?;
    let (own, mut records) = decode_times(&get(repo, tentries[ti].oid)?);
    let mut here: BTreeMap<&str, BTreeMap<&str, (ObjectId, Rec)>> = BTreeMap::new();
    for (path, v) in changes {
        match path.split_once('/') {
            Some((first, rest)) => {
                here.entry(first).or_default().insert(rest, *v);
            }
            None => {
                let i = entries.iter().position(|e| e.name == path.as_bytes()).context("changed file not in tree")?;
                entries[i].oid = v.0;
                records[i] = v.1;
            }
        }
    }
    for (dir, sub) in here {
        let i = entries.iter().position(|e| e.name == dir.as_bytes() && is_tree(e.mode)).context("folder")?;
        let j = tentries.iter().position(|e| e.name == dir.as_bytes()).context("times folder")?;
        let (t, tt) = v3_update(repo, entries[i].oid, tentries[j].oid, &sub)?;
        entries[i].oid = t;
        tentries[j].oid = tt;
    }
    let tree = put(repo, gix::objs::Kind::Tree, &encode_tree(&entries))?;
    tentries[ti].oid = put(repo, gix::objs::Kind::Blob, &encode_times(own, &records))?;
    let ttree = put(repo, gix::objs::Kind::Tree, &encode_tree(&tentries))?;
    Ok((tree, ttree))
}

/// Every file of a state: (path, blob, mode). Folders in walk order with their own times.
struct V3Listing {
    files: Vec<(String, ObjectId, &'static str, Rec)>,
    dirs: Vec<(String, i64, u32)>,
}

fn v3_list(repo: &gix::Repository, tree: ObjectId, ttree: ObjectId, rel: &str, mode_of_dir: u32, out: &mut V3Listing) -> Result<()> {
    let entries = decode_tree(&get(repo, tree)?)?;
    let tentries = decode_tree(&get(repo, ttree)?)?;
    let tblob = tentries.iter().find(|e| e.name == TIMES).context("times blob")?.oid;
    let (own, records) = decode_times(&get(repo, tblob)?);
    out.dirs.push((rel.to_string(), own, mode_of_dir));
    for (i, e) in entries.iter().enumerate() {
        let name = String::from_utf8_lossy(&e.name);
        let path = if rel.is_empty() { name.into_owned() } else { format!("{rel}/{name}") };
        if is_tree(e.mode) {
            let tt = tentries.iter().find(|t| t.name == e.name).context("times folder")?.oid;
            v3_list(repo, e.oid, tt, &path, records[i].1, out)?;
        } else {
            out.files.push((path, e.oid, e.mode, records[i]));
        }
    }
    Ok(())
}

fn v3_diff(repo: &gix::Repository, a: ObjectId, b: ObjectId, rel: &str, out: &mut Vec<String>) -> Result<()> {
    if a == b {
        return Ok(());
    }
    let ea = decode_tree(&get(repo, a)?)?;
    let eb = decode_tree(&get(repo, b)?)?;
    let ma: BTreeMap<&[u8], &GEntry> = ea.iter().map(|e| (e.name.as_slice(), e)).collect();
    for e in &eb {
        let path = format!("{rel}/{}", String::from_utf8_lossy(&e.name));
        match ma.get(e.name.as_slice()) {
            Some(o) if o.oid == e.oid && o.mode == e.mode => {}
            Some(o) if is_tree(o.mode) && is_tree(e.mode) => v3_diff(repo, o.oid, e.oid, &path, out)?,
            _ => out.push(path),
        }
    }
    Ok(())
}

fn v3_checkout(ts: &gix::ThreadSafeRepository, st: V3State, dst: &Path) -> Result<usize> {
    let repo = ts.to_thread_local();
    let mut l = V3Listing { files: Vec::new(), dirs: Vec::new() };
    v3_list(&repo, st.tree, st.times, "", 0o755, &mut l)?;
    for (d, _, _) in &l.dirs {
        fs::create_dir_all(dst.join(d))?;
    }
    l.files.par_iter().try_for_each_init(
        || ts.to_thread_local(),
        |repo, (path, oid, mode, (time, perm))| -> Result<()> {
            let data = get(repo, *oid)?;
            let p = dst.join(path);
            if *mode == "120000" {
                std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&data), &p)?;
                filetime::set_symlink_file_times(&p, ft(*time), ft(*time))?;
            } else {
                fs::write(&p, &data)?;
                fs::set_permissions(&p, fs::Permissions::from_mode(*perm))?;
                filetime::set_file_mtime(&p, ft(*time))?;
            }
            Ok(())
        },
    )?;
    for (d, t, perm) in l.dirs.iter().rev() {
        if !d.is_empty() {
            fs::set_permissions(dst.join(d), fs::Permissions::from_mode(*perm))?;
        }
        filetime::set_file_mtime(dst.join(d), ft(*t))?;
    }
    Ok(l.files.len())
}

fn ft(ns: i64) -> filetime::FileTime {
    filetime::FileTime::from_unix_time(ns.div_euclid(1_000_000_000), ns.rem_euclid(1_000_000_000) as u32)
}

// ---------------------------------------------------------------- v2 helpers

fn v2_list(store: &Store, tree: &Id, rel: &str, out: &mut Vec<(String, Id)>) -> Result<()> {
    for e in store.get_tree(tree)?.entries {
        let name = String::from_utf8_lossy(&e.name);
        let path = if rel.is_empty() { name.into_owned() } else { format!("{rel}/{name}") };
        match e.node {
            Node::File { blob, .. } => out.push((path, blob)),
            Node::Dir { tree } => v2_list(store, &tree, &path, out)?,
            Node::Symlink { .. } => {}
        }
    }
    Ok(())
}

fn v2_diff(store: &Store, a: &Id, b: &Id, rel: &str, out: &mut Vec<String>) -> Result<()> {
    if a == b {
        return Ok(());
    }
    let ta = store.get_tree(a)?;
    let tb = store.get_tree(b)?;
    let ma: BTreeMap<&[u8], &layr_objects::tree::Entry> = ta.entries.iter().map(|e| (e.name.as_slice(), e)).collect();
    for e in &tb.entries {
        let path = format!("{rel}/{}", String::from_utf8_lossy(&e.name));
        match (ma.get(e.name.as_slice()), &e.node) {
            (Some(o), _) if o.node == e.node && o.mode == e.mode => {}
            (Some(o), Node::Dir { tree: nb }) => match &o.node {
                Node::Dir { tree: na } => v2_diff(store, na, nb, &path, out)?,
                _ => out.push(path),
            },
            _ => out.push(path),
        }
    }
    Ok(())
}


// ---------------------------------------------------------------- history replay

/// One changed path of a commit: None when it is gone from the work folder.
type Change = Option<(ObjectId, &'static str, Rec)>;

fn perm(m: &fs::Metadata) -> u32 {
    m.permissions().mode() & 0o7777
}

/// The change of `path` as the work folder has it now (a blob is written).
fn v3_change(repo: &gix::Repository, work: &Path, path: &str) -> Result<Change> {
    let p = work.join(path);
    let Ok(m) = fs::symlink_metadata(&p) else { return Ok(None) };
    // A gitlink is an empty folder here: `v3_apply` writes a folder for a None that is a folder.
    if m.is_dir() {
        return Ok(None);
    }
    if m.file_type().is_symlink() {
        let t = fs::read_link(&p)?;
        return Ok(Some((put(repo, gix::objs::Kind::Blob, t.as_os_str().as_bytes())?, "120000", (mtime(&m), 0))));
    }
    let mode = if m.permissions().mode() & 0o111 != 0 { "100755" } else { "100644" };
    Ok(Some((put(repo, gix::objs::Kind::Blob, &fs::read(&p)?)?, mode, (mtime(&m), perm(&m)))))
}

/// Apply `changes` (paths under the folder `rel`) to the folder (`base`: its tree and times tree,
/// None for a new folder). None when the folder is gone (git keeps no empty folder).
fn v3_apply(
    repo: &gix::Repository,
    work: &Path,
    rel: &str,
    base: Option<(ObjectId, ObjectId)>,
    changes: &BTreeMap<String, Change>,
) -> Result<Option<(ObjectId, ObjectId)>> {
    let mut ents: BTreeMap<Vec<u8>, (GEntry, Rec)> = BTreeMap::new();
    let mut tsub: BTreeMap<Vec<u8>, ObjectId> = BTreeMap::new();
    if let Some((tree, ttree)) = base {
        let entries = decode_tree(&get(repo, tree)?)?;
        let tentries = decode_tree(&get(repo, ttree)?)?;
        let tblob = tentries.iter().find(|e| e.name == TIMES).context("times blob")?.oid;
        let (_, records) = decode_times(&get(repo, tblob)?);
        for (e, r) in entries.into_iter().zip(records) {
            ents.insert(e.name.clone(), (e, r));
        }
        for e in tentries {
            if e.name != TIMES {
                tsub.insert(e.name, e.oid);
            }
        }
    }
    let mut direct: Vec<(&str, &Change)> = Vec::new();
    let mut sub: BTreeMap<&str, BTreeMap<String, Change>> = BTreeMap::new();
    for (path, c) in changes {
        match path.split_once('/') {
            Some((first, rest)) => {
                sub.entry(first).or_default().insert(rest.to_string(), *c);
            }
            None => direct.push((path.as_str(), c)),
        }
    }
    for (dir, subchanges) in sub {
        let name = dir.as_bytes().to_vec();
        let path = if rel.is_empty() { dir.to_string() } else { format!("{rel}/{dir}") };
        let base = match ents.get(&name) {
            Some((e, _)) if is_tree(e.mode) => Some((e.oid, *tsub.get(&name).context("times folder")?)),
            _ => None,
        };
        match v3_apply(repo, work, &path, base, &subchanges)? {
            Some((t, tt)) => {
                let m = fs::metadata(work.join(&path))?;
                ents.insert(name.clone(), (GEntry { mode: "40000", name: name.clone(), oid: t }, (0, perm(&m))));
                tsub.insert(name, tt);
            }
            None => {
                ents.remove(&name);
                tsub.remove(&name);
            }
        }
    }
    for (name, c) in direct {
        let name = name.as_bytes().to_vec();
        let path = if rel.is_empty() { String::from_utf8_lossy(&name).into_owned() } else { format!("{rel}/{}", String::from_utf8_lossy(&name)) };
        match c {
            Some((oid, mode, r)) => {
                tsub.remove(&name);
                ents.insert(name.clone(), (GEntry { mode, name, oid: *oid }, *r));
            }
            None => {
                // A gitlink (or another folder) shows as an empty folder in the work folder.
                let p = work.join(&path);
                if fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) {
                    let (t, tt) = v3_write_dir(repo, work, &path, &BTreeMap::new())?;
                    ents.insert(name.clone(), (GEntry { mode: "40000", name: name.clone(), oid: t }, (0, perm(&fs::metadata(&p)?))));
                    tsub.insert(name, tt);
                } else {
                    ents.remove(&name);
                    tsub.remove(&name);
                }
            }
        }
    }
    let dir = work.join(rel);
    let Ok(dm) = fs::metadata(&dir) else { return Ok(None) };
    if ents.is_empty() && !rel.is_empty() {
        return Ok(None);
    }
    let mut entries: Vec<GEntry> = ents.values().map(|(e, _)| e.clone()).collect();
    git_sort(&mut entries);
    let records: Vec<Rec> = entries.iter().map(|e| ents[&e.name].1).collect();
    let tree = put(repo, gix::objs::Kind::Tree, &encode_tree(&entries))?;
    let tblob = put(repo, gix::objs::Kind::Blob, &encode_times(mtime(&dm), &records))?;
    let mut tentries: Vec<GEntry> = tsub.into_iter().map(|(name, oid)| GEntry { mode: "40000", name, oid }).collect();
    tentries.push(GEntry { mode: "100644", name: TIMES.to_vec(), oid: tblob });
    git_sort(&mut tentries);
    let ttree = put(repo, gix::objs::Kind::Tree, &encode_tree(&tentries))?;
    Ok(Some((tree, ttree)))
}

fn git(args: &[&str]) -> Result<String> {
    let o = std::process::Command::new("git").args(args).output()?;
    ensure!(o.status.success(), "git {} failed: {}", args.join(" "), String::from_utf8_lossy(&o.stderr));
    Ok(String::from_utf8(o.stdout)?)
}

/// usage: history <v2-cold|v3> <bare repo> <work> <scratch> <commits>
fn history(a: &[String]) -> Result<()> {
    let (engine, repo_dir, work, scratch) = (a[2].as_str(), a[3].as_str(), PathBuf::from(&a[4]), PathBuf::from(&a[5]));
    let n: usize = a[6].parse()?;
    ensure!(!scratch.exists() && !work.exists(), "scratch or work exists");
    fs::create_dir_all(&scratch)?;
    fs::create_dir_all(&work)?;
    let (scratch, work) = (fs::canonicalize(&scratch)?, fs::canonicalize(&work)?);
    let index = scratch.join("git-index");
    std::env::set_var("GIT_INDEX_FILE", &index);
    let gd = format!("--git-dir={repo_dir}");
    let wt = format!("--work-tree={}", work.display());
    let commits: Vec<String> = git(&[&gd, "rev-list", "--first-parent", "-n", &n.to_string(), "refs/heads/main"])?.lines().rev().map(String::from).collect();
    let checkout = |c: &str| git(&[&gd, &wt, "-c", "advice.detachedHead=false", "checkout", "-f", "-q", "--detach", c]).map(|_| ());
    let t = Instant::now();
    checkout(&commits[0])?;
    let mut out = json!({"engine": engine, "mode": "history", "commits": commits.len(), "first": commits[0], "last": commits[commits.len() - 1], "checkout_first_ms": ms(t)});
    let mut paths = Vec::new();
    files(&work, "", &mut paths)?;
    out["files_first"] = json!(paths.len());
    let store_dir = scratch.join("store");
    let mut saves = Vec::new();
    let mut changed_total = 0usize;
    let mut checkout_ms = 0.0;
    // Each step: the changed paths of the commit (git), git writes them into the work folder, then
    // the engine saves them. Only the save is timed.
    let mut steps = |save: &mut dyn FnMut(&[String]) -> Result<()>| -> Result<()> {
        for w in commits.windows(2) {
            let names = git(&[&gd, "diff-tree", "-r", "--name-only", "--no-renames", "--no-commit-id", &w[0], &w[1]])?;
            let changed: Vec<String> = names.lines().filter(|l| !l.is_empty()).map(String::from).collect();
            let t = Instant::now();
            checkout(&w[1])?;
            checkout_ms += ms(t);
            changed_total += changed.len();
            let t = Instant::now();
            save(&changed)?;
            saves.push(ms(t));
        }
        Ok(())
    };
    if engine == "v3" {
        let ts = gix::init_bare(&store_dir)?.into_sync();
        let repo = ts.to_thread_local();
        let t = Instant::now();
        let blobs: BTreeMap<String, ObjectId> = paths
            .par_iter()
            .map_init(|| ts.to_thread_local(), |r, p| -> Result<(String, ObjectId)> { Ok((p.clone(), put(r, gix::objs::Kind::Blob, &fs::read(work.join(p))?)?)) })
            .collect::<Result<_>>()?;
        let (tree, times) = v3_write_dir(&repo, &work, "", &blobs)?;
        let commit = v3_commit(&repo, tree, None, 0)?;
        v3_ref(&store_dir, "t0", times)?;
        out["init_ms"] = json!(ms(t));
        let mut st = V3State { commit, tree, times };
        let mut k = 0;
        let mut trees_equal = 0usize;
        steps(&mut |changed: &[String]| -> Result<()> {
            k += 1;
            let mut ch = BTreeMap::new();
            for p in changed {
                ch.insert(p.clone(), v3_change(&repo, &work, p)?);
            }
            let (tree, times) = v3_apply(&repo, &work, "", Some((st.tree, st.times)), &ch)?.context("root gone")?;
            let commit = v3_commit(&repo, tree, Some(st.commit), k)?;
            // Each state names its times tree, as a state record would.
            v3_ref(&store_dir, &format!("t{k}"), times)?;
            st = V3State { commit, tree, times };
            Ok(())
        })?;
        // The source tree of the last state must be git's own tree of the last commit.
        let real = git(&[&gd, "rev-parse", &format!("{}^{{tree}}", commits[commits.len() - 1])])?;
        if real.trim() == st.tree.to_string() {
            trees_equal += 1;
        }
        out["last_tree_is_git_tree"] = json!(trees_equal == 1);
        v3_ref(&store_dir, "last", st.commit)?;
        v3_ref(&store_dir, "last-times", st.times)?;
        let u = usage(&store_dir.join("objects"));
        out["store_bytes"] = json!({"logical": u.0, "allocated": u.1, "files": u.2});
        let t = Instant::now();
        let ok = std::process::Command::new("git").env_remove("GIT_INDEX_FILE").arg("-C").arg(&store_dir).args(["repack", "-a", "-d", "-q"]).status()?;
        ensure!(ok.success(), "git repack failed");
        let ok = std::process::Command::new("git").env_remove("GIT_INDEX_FILE").arg("-C").arg(&store_dir).args(["prune-packed", "-q"]).status()?;
        ensure!(ok.success(), "git prune-packed failed");
        out["pack_ms"] = json!(ms(t));
        let u = usage(&store_dir.join("objects"));
        out["packed_bytes"] = json!({"logical": u.0, "allocated": u.1, "files": u.2});
        let ts = gix::open(&store_dir)?.into_sync();
        let t = Instant::now();
        v3_checkout(&ts, st, &scratch.join("co"))?;
        out["checkout_packed_ms"] = json!(ms(t));
    } else {
        let mut store = Store::open(&store_dir, Keying::Plain)?;
        // LAYR_BLOCK_SIZE: the plain size of a pack block (default 4 MiB), for read tests.
        if let Ok(b) = std::env::var("LAYR_BLOCK_SIZE") {
            store.options.block_size = b.parse()?;
        }
        out["block_size"] = json!(store.options.block_size);
        let opts = WriteOptions { temperature: Temperature::Cold, previous: None, exclude: vec![], similar: false };
        let t = Instant::now();
        let w = layr_objects::write_tree(&store, &work, &opts)?;
        out["init_ms"] = json!(ms(t));
        let mut st = StateRef { tree: w.tree.unwrap(), times: w.times.unwrap() };
        store.set_state("s0", &st)?;
        let mut k = 0;
        steps(&mut |changed: &[String]| -> Result<()> {
            k += 1;
            let c: Vec<Vec<u8>> = changed.iter().map(|p| p.as_bytes().to_vec()).collect();
            let o = WriteOptions { temperature: Temperature::Cold, previous: Some(st.tree), exclude: vec![], similar: false };
            let w = layr_objects::write::write_changed(&store, &work, &layr_objects::write::Base { tree: st.tree, times: st.times }, &c, &o)?;
            st = StateRef { tree: w.tree.unwrap(), times: w.times.unwrap() };
            store.set_state(&format!("s{k}"), &st)?;
            Ok(())
        })?;
        let u = usage(&store_dir);
        out["store_bytes"] = json!({"logical": u.0, "allocated": u.1, "files": u.2});
        let t = Instant::now();
        store.evict_hot(&[])?;
        store.pack()?;
        out["pack_ms"] = json!(ms(t));
        let u = usage(&store_dir);
        out["packed_bytes"] = json!({"logical": u.0, "allocated": u.1, "files": u.2});
        drop(store);
        let store = Store::open_existing(&store_dir, Keying::Plain)?;
        let t = Instant::now();
        layr_objects::checkout_with(&store, &st.tree, Some(&st.times), &scratch.join("co"))?;
        out["checkout_packed_ms"] = json!(ms(t));
    }
    let mut sorted = saves.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    out["save_total_ms"] = json!(saves.iter().sum::<f64>());
    out["save_median_ms"] = json!(sorted.get(sorted.len() / 2));
    out["save_p95_ms"] = json!(sorted.get(sorted.len() * 95 / 100));
    out["changed_paths"] = json!(changed_total);
    out["git_checkout_ms"] = json!(checkout_ms);
    let c = layr_objects::compare::compare_with(&work, &scratch.join("co"), true)?;
    let diffs: Vec<_> = c.differences.iter().take(5).collect();
    out["checkout_differences"] = json!(c.differences.len());
    out["checkout_difference_samples"] = json!(diffs);
    println!("{out}");
    Ok(())
}
// ---------------------------------------------------------------- the run

fn edit(work: &Path, path: &str, round: usize) -> Result<()> {
    let p = work.join(path);
    let mut data = fs::read(&p)?;
    data.extend_from_slice(format!("\n/* store-v3 edit {round} */\n").as_bytes());
    fs::write(&p, &data)?;
    filetime::set_file_mtime(&p, ft(1_700_000_000_000_000_000 + round as i64 * 1_000_000_000))?;
    Ok(())
}

fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().collect();
    if a.get(1).map(String::as_str) == Some("history") {
        ensure!(a.len() == 7, "usage: store-v3-prototype history <v2-cold|v3> <bare repo> <work> <scratch> <commits>");
        return history(&a);
    }
    ensure!(a.len() == 6, "usage: store-v3-prototype <v2-hot|v2-cold|v3> <work> <scratch> <rounds> <batch>");
    let (engine, work, scratch) = (a[1].as_str(), PathBuf::from(&a[2]), PathBuf::from(&a[3]));
    let (rounds, batch): (usize, usize) = (a[4].parse()?, a[5].parse()?);
    ensure!(!scratch.exists(), "scratch exists");
    fs::create_dir_all(&scratch)?;
    let mut paths = Vec::new();
    files(&work, "", &mut paths)?;
    // The same files in every engine: spread over the tree.
    let step = paths.len() / (rounds * batch + 1);
    let pick = |round: usize| -> Vec<String> { (0..batch).map(|k| paths[(round * batch + k + 1) * step].clone()).collect() };
    let mut out = json!({"engine": engine, "files": paths.len(), "rounds": rounds, "batch": batch});
    let store_dir = scratch.join("store");

    if engine == "v3" {
        let repo = gix::init_bare(&store_dir)?;
        let ts = repo.into_sync();
        let repo = ts.to_thread_local();
        // Initial write: blobs in parallel, then trees.
        let t = Instant::now();
        let blobs: BTreeMap<String, ObjectId> = paths
            .par_iter()
            .map_init(
                || ts.to_thread_local(),
                |r, p| -> Result<(String, ObjectId)> { Ok((p.clone(), put(r, gix::objs::Kind::Blob, &fs::read(work.join(p))?)?)) },
            )
            .collect::<Result<_>>()?;
        let blobs_ms = ms(t);
        let (tree, times) = v3_write_dir(&repo, &work, "", &blobs)?;
        let commit = v3_commit(&repo, tree, None, 0)?;
        v3_ref(&store_dir, "s0", commit)?;
        v3_ref(&store_dir, "t0", times)?;
        out["init_ms"] = json!(ms(t));
        out["init_blobs_ms"] = json!(blobs_ms);
        let first = V3State { commit, tree, times };
        let mut st = first;
        let mut saves = Vec::new();
        for round in 0..rounds {
            let sel = pick(round);
            for p in &sel {
                edit(&work, p, round)?;
            }
            let t = Instant::now();
            let mut changes = BTreeMap::new();
            for p in &sel {
                let fp = work.join(p);
                let m = fs::metadata(&fp)?;
                changes.insert(p.as_str(), (put(&repo, gix::objs::Kind::Blob, &fs::read(&fp)?)?, (mtime(&m), m.permissions().mode() & 0o7777)));
            }
            let (tree, times) = v3_update(&repo, st.tree, st.times, &changes)?;
            let commit = v3_commit(&repo, tree, Some(st.commit), round + 1)?;
            v3_ref(&store_dir, &format!("s{}", round + 1), commit)?;
            v3_ref(&store_dir, &format!("t{}", round + 1), times)?;
            saves.push(ms(t));
            st = V3State { commit, tree, times };
        }
        out["save_ms"] = json!(saves);
        let t = Instant::now();
        let mut changed = Vec::new();
        v3_diff(&repo, first.tree, st.tree, "", &mut changed)?;
        out["diff_ms"] = json!(ms(t));
        out["diff_paths"] = json!(changed.len());
        let read_all = |label: &str, out: &mut serde_json::Value| -> Result<()> {
            let repo = ts.to_thread_local();
            let mut l = V3Listing { files: Vec::new(), dirs: Vec::new() };
            v3_list(&repo, st.tree, st.times, "", 0o755, &mut l)?;
            let mut v = Vec::new();
            for _ in 0..2 {
                let t = Instant::now();
                let mut bytes = 0usize;
                for (_, oid, _, _) in &l.files {
                    bytes += get(&repo, *oid)?.len();
                }
                v.push(ms(t));
                out[format!("read_bytes{label}")] = json!(bytes);
            }
            out[format!("read_all_ms{label}")] = json!(v);
            Ok(())
        };
        read_all("", &mut out)?;
        let t = Instant::now();
        let n = v3_checkout(&ts, st, &scratch.join("co1"))?;
        out["checkout_ms"] = json!(ms(t));
        out["checkout_files"] = json!(n);
        let u = usage(&store_dir.join("objects"));
        out["store_bytes"] = json!({"logical": u.0, "allocated": u.1, "files": u.2});
        let t = Instant::now();
        let ok = std::process::Command::new("git").env_remove("GIT_INDEX_FILE").arg("-C").arg(&store_dir).args(["repack", "-a", "-d", "-q"]).status()?;
        ensure!(ok.success(), "git repack failed");
        let ok = std::process::Command::new("git").env_remove("GIT_INDEX_FILE").arg("-C").arg(&store_dir).args(["prune-packed", "-q"]).status()?;
        ensure!(ok.success(), "git prune-packed failed");
        out["pack_ms"] = json!(ms(t));
        let u = usage(&store_dir.join("objects"));
        out["packed_bytes"] = json!({"logical": u.0, "allocated": u.1, "files": u.2});
        // A fresh handle reads the packs.
        let ts = gix::open(&store_dir)?.into_sync();
        let read_all = |label: &str, out: &mut serde_json::Value| -> Result<()> {
            let repo = ts.to_thread_local();
            let mut l = V3Listing { files: Vec::new(), dirs: Vec::new() };
            v3_list(&repo, st.tree, st.times, "", 0o755, &mut l)?;
            let mut v = Vec::new();
            for _ in 0..2 {
                let t = Instant::now();
                for (_, oid, _, _) in &l.files {
                    std::hint::black_box(get(&repo, *oid)?);
                }
                v.push(ms(t));
            }
            out[format!("read_all_ms{label}")] = json!(v);
            Ok(())
        };
        read_all("_packed", &mut out)?;
        let t = Instant::now();
        v3_checkout(&ts, st, &scratch.join("co2"))?;
        out["checkout_packed_ms"] = json!(ms(t));
    } else {
        let temperature = if engine == "v2-hot" { Temperature::Hot } else { Temperature::Cold };
        let store = Store::open(&store_dir, Keying::Plain)?;
        let opts = WriteOptions { temperature, previous: None, exclude: vec![b".git".to_vec()], similar: false };
        let t = Instant::now();
        let w = layr_objects::write_tree(&store, &work, &opts)?;
        let first = StateRef { tree: w.tree.unwrap(), times: w.times.unwrap() };
        store.set_state("s0", &first)?;
        out["init_ms"] = json!(ms(t));
        let mut st = first;
        let mut saves = Vec::new();
        for round in 0..rounds {
            let sel = pick(round);
            for p in &sel {
                edit(&work, p, round)?;
            }
            let t = Instant::now();
            let changed: Vec<Vec<u8>> = sel.iter().map(|p| p.as_bytes().to_vec()).collect();
            let o = WriteOptions { temperature, previous: Some(st.tree), exclude: vec![b".git".to_vec()], similar: false };
            let w = layr_objects::write::write_changed(&store, &work, &layr_objects::write::Base { tree: st.tree, times: st.times }, &changed, &o)?;
            st = StateRef { tree: w.tree.unwrap(), times: w.times.unwrap() };
            store.set_state(&format!("s{}", round + 1), &st)?;
            saves.push(ms(t));
        }
        out["save_ms"] = json!(saves);
        let t = Instant::now();
        let mut changed = Vec::new();
        v2_diff(&store, &first.tree, &st.tree, "", &mut changed)?;
        out["diff_ms"] = json!(ms(t));
        out["diff_paths"] = json!(changed.len());
        let read_all = |store: &Store, label: &str, out: &mut serde_json::Value| -> Result<()> {
            let mut l = Vec::new();
            v2_list(store, &st.tree, "", &mut l)?;
            let mut v = Vec::new();
            for _ in 0..2 {
                let t = Instant::now();
                let mut bytes = 0usize;
                for (_, id) in &l {
                    bytes += store.read_blob(id)?.len();
                }
                v.push(ms(t));
                out[format!("read_bytes{label}")] = json!(bytes);
            }
            out[format!("read_all_ms{label}")] = json!(v);
            Ok(())
        };
        read_all(&store, "", &mut out)?;
        let t = Instant::now();
        layr_objects::checkout_with(&store, &st.tree, Some(&st.times), &scratch.join("co1"))?;
        out["checkout_ms"] = json!(ms(t));
        let u = usage(&store_dir);
        out["store_bytes"] = json!({"logical": u.0, "allocated": u.1, "files": u.2});
        let t = Instant::now();
        store.evict_hot(&[])?;
        store.pack()?;
        out["pack_ms"] = json!(ms(t));
        let u = usage(&store_dir);
        out["packed_bytes"] = json!({"logical": u.0, "allocated": u.1, "files": u.2});
        drop(store);
        let store = Store::open_existing(&store_dir, Keying::Plain)?;
        read_all(&store, "_packed", &mut out)?;
        let t = Instant::now();
        layr_objects::checkout_with(&store, &st.tree, Some(&st.times), &scratch.join("co2"))?;
        out["checkout_packed_ms"] = json!(ms(t));
    }
    // Both checkouts must equal the work folder: content, modes, links and times.
    for co in ["co1", "co2"] {
        let c = layr_objects::compare::compare_with(&work, &scratch.join(co), true)?;
        let diffs: Vec<_> = c.differences.iter().filter(|d| !d.contains(".git")).take(5).collect();
        ensure!(diffs.is_empty(), "{co} differs from the work folder: {diffs:?}");
    }
    out["verified"] = json!(true);
    println!("{out}");
    Ok(())
}
