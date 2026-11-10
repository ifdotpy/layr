//! Remove the build output of a profile folder (`target/<...>/<profile>`) that no standard command
//! needs. The rule is an allowlist: an entry stays only when a listed unit needs it. See docs/retest.md,
//! "Sweeping stale build output".

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Length of the hash that cargo appends to the name of a unit.
const HASH_LEN: usize = 16;
/// Cargo's build lock in a profile folder.
const LOCK: &str = ".cargo-lock";
/// Folders of a profile folder with one entry per unit: `<package>-<hash>`.
const UNIT_FOLDERS: [&str; 2] = [".fingerprint", "build"];
/// Folders with the output files of the units.
const OUTPUT_FOLDERS: [&str; 2] = ["deps", "examples"];
/// Folder of the incremental sessions: one folder per crate, named `<crate>-<stable id>`.
const INCREMENTAL: &str = "incremental";
/// Crate types whose outputs cargo names without a unit hash (`deps/libcrate.rlib`).
const LIB_KINDS: [&str; 6] = ["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"];

/// The cargo hash of a file or folder name: the 16 hex digits after the last `-` of the name before
/// its first `.`. `None` for a name without one, such as `incremental/<crate>-<base 36>`.
pub fn unit_hash(name: &str) -> Option<&str> {
    let stem = name.split('.').next()?;
    let (_, hash) = stem.rsplit_once('-')?;
    (hash.len() == HASH_LEN && hash.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hash)
}

/// The outputs that one listing needs in one profile folder.
#[derive(Debug, Default, PartialEq)]
pub struct Live {
    /// Unit hashes whose entries stay: `.fingerprint`, `build`, hashed `deps` and `examples` files.
    pub hashes: BTreeSet<String>,
    /// Outputs that cargo names, relative to the profile folder (`app`, `deps/libx-<hash>.rlib`).
    pub paths: BTreeSet<PathBuf>,
    /// Crates whose library outputs have no hash in their names (`deps/libx.rlib`, `libx.dylib`).
    pub crates: BTreeSet<String>,
}

impl Live {
    fn merge(&mut self, other: Live) {
        self.hashes.extend(other.hashes);
        self.paths.extend(other.paths);
        self.crates.extend(other.crates);
    }
}

/// The live set of each profile folder, keyed by the folder's path.
pub type Plans = BTreeMap<PathBuf, Live>;

/// Merges the live sets of several listings.
pub fn merge(into: &mut Plans, plans: Plans) {
    for (profile, live) in plans {
        into.entry(profile).or_default().merge(live);
    }
}

/// The target name of a `--message-format json` line that is a unit which is not fresh (`fresh: false`).
/// A listing that reports one has built that unit, so its live set is not a fact about the build.
pub fn not_fresh(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    let stale = v["reason"] == "compiler-artifact" && v["fresh"].as_bool() == Some(false);
    stale.then(|| v["target"]["name"].as_str().unwrap_or("?").to_string())
}

/// The live set of each profile folder under `target`, from the `--message-format json` lines of a listing.
/// Every output that cargo names is live, in the profile folder that holds it. An output whose hash is not in
/// its name gets the hash from its file: a copy of a bin or an example (`debug/app`) is matched to its hashed
/// file by size and modification time. An unhashed library output (`deps/libx.rlib`, `libx.dylib`) is live
/// with its crate, and the newest unit of that crate in `.fingerprint` is its unit.
pub fn plan(target: &Path, messages: &[u8]) -> Result<Plans> {
    let mut plans = Plans::new();
    let mut copies: Vec<Pending> = Vec::new();
    for line in String::from_utf8_lossy(messages).lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        match v["reason"].as_str() {
            Some("compiler-artifact") => {
                let name = v["target"]["name"].as_str().unwrap_or_default().to_string();
                let kinds: Vec<&str> = v["target"]["kind"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                let lib = kinds.iter().any(|k| LIB_KINDS.contains(k));
                let marker = if lib {
                    "lib"
                } else if kinds.contains(&"example") {
                    "example"
                } else {
                    "bin"
                };
                let files = v["filenames"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .chain(v["executable"].as_str().map(|_| &v["executable"]))
                    .filter_map(Value::as_str);
                for p in files {
                    let p = PathBuf::from(p);
                    let profile = profile_of(target, &p)?;
                    let live = plans.entry(profile.clone()).or_default();
                    live.paths.insert(p.strip_prefix(&profile)?.to_path_buf());
                    match hash_of_output(&p) {
                        Some(h) => {
                            live.hashes.insert(h);
                        }
                        None if lib => {
                            // The unit of an unhashed library is the one whose output has the same size and time
                            // (matched below). The crate is live too, for its object files.
                            live.crates.insert(name.clone());
                            copies.push(Pending { profile, path: p, name: name.clone(), marker });
                        }
                        None => copies.push(Pending { profile, path: p, name: name.clone(), marker }),
                    }
                }
            }
            Some("build-script-executed") => {
                let Some(out) = v["out_dir"].as_str().map(PathBuf::from) else { continue };
                let profile = profile_of(target, &out)?;
                let live = plans.entry(profile.clone()).or_default();
                live.paths.insert(out.strip_prefix(&profile)?.to_path_buf());
                // `build/<package>-<hash>/out`: the hash is in the name of the folder of the out dir.
                live.hashes.extend(out.parent().and_then(file_name).and_then(unit_hash).map(String::from));
            }
            _ => {}
        }
    }
    let mut outputs_of: BTreeMap<PathBuf, Vec<Output>> = BTreeMap::new();
    // Libraries whose unhashed output matched one unit: the newest-unit rule below does not apply to them.
    let mut matched: BTreeSet<(PathBuf, String)> = BTreeSet::new();
    for c in copies {
        let outputs = outputs_of.entry(c.profile.clone()).or_insert_with(|| outputs(&c.profile));
        let m = c.path.metadata().with_context(|| format!("cannot read the output {}", c.path.display()))?;
        let live = plans.entry(c.profile.clone()).or_default();
        let same: Vec<&Output> = outputs.iter().filter(|o| o.len == m.len() && o.mtime == (m.mtime(), m.mtime_nsec())).collect();
        if same.is_empty() {
            // No file matches: the outputs of that name and every unit of its kind stay.
            // A library with no matching file takes the newest-unit rule below, not every unit of its name.
            if c.marker != "lib" {
                let base = c.name.replace('-', "_");
                live.hashes
                    .extend(outputs.iter().filter(|o| unit_base(&o.name).is_some_and(|b| b == base)).map(|o| o.hash.clone()));
                live.hashes.extend(marked(&c.profile, c.marker, &base).into_iter().map(|(h, _)| h));
            }
        } else {
            live.hashes.extend(same.iter().map(|o| o.hash.clone()));
            if c.marker == "lib" {
                matched.insert((c.profile.clone(), c.name.clone()));
            }
        }
    }
    for (profile, live) in plans.iter_mut() {
        let outputs = outputs_of.entry(profile.clone()).or_insert_with(|| outputs(profile));
        let hashed: BTreeSet<&str> = outputs.iter().map(|o| o.hash.as_str()).collect();
        for name in live.crates.clone() {
            if matched.contains(&(profile.clone(), name.clone())) {
                continue;
            }
            // The newest unit of the crate is the one whose build wrote the unhashed output. A unit with a hashed
            // output (the `.rmeta` and `.d` of a `check` or `clippy` of a crate with a `cdylib`) wrote none: it is
            // not that unit, even when it is newer.
            let units = marked(profile, "lib", &name);
            let writers: Vec<&(String, SystemTime)> = units.iter().filter(|(h, _)| !hashed.contains(h.as_str())).collect();
            let newest = if writers.is_empty() {
                units.iter().max_by_key(|(_, t)| *t)
            } else {
                writers.into_iter().max_by_key(|(_, t)| *t)
            };
            live.hashes.extend(newest.map(|(h, _)| h.clone()));
        }
    }
    Ok(plans)
}

/// A bin or example output without a hash in its name, to be matched to its hashed file.
struct Pending {
    profile: PathBuf,
    path: PathBuf,
    name: String,
    marker: &'static str,
}

/// The profile folder of an output path under `target`: the first folder that holds `.fingerprint`.
fn profile_of(target: &Path, p: &Path) -> Result<PathBuf> {
    let rel = p.strip_prefix(target).with_context(|| format!("output outside the target folder: {}", p.display()))?;
    let n = rel.components().count();
    let mut dir = target.to_path_buf();
    for c in rel.components().take(n.saturating_sub(1)) {
        dir.push(c);
        if dir.join(".fingerprint").is_dir() {
            return Ok(dir);
        }
    }
    bail!("no profile folder for the output {}", p.display())
}

fn file_name(p: &Path) -> Option<&str> {
    p.file_name()?.to_str()
}

/// The unit hash of an output: in its file name, or in the name of its folder (`build/<package>-<hash>/out`).
fn hash_of_output(p: &Path) -> Option<String> {
    let file = file_name(p)?;
    let folder = p.parent().and_then(file_name);
    unit_hash(file).or_else(|| folder.and_then(unit_hash)).map(String::from)
}

/// One hashed output file of a profile folder (`deps`, `examples`).
struct Output {
    name: String,
    hash: String,
    len: u64,
    mtime: (i64, i64),
}

fn outputs(profile: &Path) -> Vec<Output> {
    let mut v = Vec::new();
    for folder in OUTPUT_FOLDERS {
        for e in read_dir(&profile.join(folder)) {
            let name = name_of(&e);
            let (Some(hash), Ok(m)) = (unit_hash(&name), e.metadata()) else { continue };
            if m.is_file() {
                let hash = hash.to_string();
                v.push(Output { name, hash, len: m.len(), mtime: (m.mtime(), m.mtime_nsec()) });
            }
        }
    }
    v
}

/// The unit name before its hash: `lib-a` for `lib-a-<hash>`, `ex` for `ex-<hash>.d`.
fn unit_base(name: &str) -> Option<&str> {
    let stem = name.split('.').next()?;
    stem.rsplit_once('-').filter(|_| unit_hash(name).is_some()).map(|(base, _)| base)
}

/// The units (hash and time) whose `.fingerprint` folder holds the marker `<kind>-<name>`. The time is the
/// start of the unit's last build (`invoked.timestamp`), or the marker's time.
fn marked(profile: &Path, kind: &str, name: &str) -> Vec<(String, SystemTime)> {
    let marker = format!("{kind}-{name}");
    let mut out = Vec::new();
    for e in read_dir(&profile.join(".fingerprint")) {
        let dir = e.path();
        let name = name_of(&e);
        let Some(hash) = unit_hash(&name) else { continue };
        let Ok(m) = dir.join(&marker).metadata() else { continue };
        if !m.is_file() {
            continue;
        }
        let t = dir.join("invoked.timestamp").metadata().and_then(|m| m.modified()).or_else(|_| m.modified());
        if let Ok(t) = t {
            out.push((hash.to_string(), t));
        }
    }
    out
}

/// A cargo object file: `<name>-<hash>.<cgu>.<build>.rcgu.o`, or `<name>-<hash>.<crate>.<id>-cgu.<n>.rcgu.o`.
/// rustc names the codegen units of one compile with one build id. The owner of an unhashed object is its
/// crate (`studio_client_core.<cgu>.<build>.rcgu.o`).
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Owner {
    Unit(String),
    Crate(String),
}

fn object_group(name: &str) -> Option<(Owner, &str)> {
    let parts: Vec<&str> = name.split('.').collect();
    let n = parts.len();
    if n < 2 || parts[n - 1] != "o" || parts[n - 2] != "rcgu" {
        return None;
    }
    let build = match n {
        5 => parts[2],
        6 => parts[2].strip_suffix("-cgu")?,
        _ => return None,
    };
    let first = parts[0];
    let owner = match unit_hash(first) {
        Some(h) => Owner::Unit(h.to_string()),
        None => Owner::Crate(first.to_string()),
    };
    Some((owner, build))
}

fn is_live_owner(owner: &Owner, live: &Live) -> bool {
    match owner {
        Owner::Unit(h) => live.hashes.contains(h),
        Owner::Crate(c) => live.crates.contains(c),
    }
}

/// The object files that stay: of each live owner, the files of its newest build (by modification time).
/// Each build writes a new set of object files for the same owner, and cargo never removes the old set.
fn kept_objects(profile: &Path, live: &Live) -> BTreeSet<PathBuf> {
    let mut objects: Vec<(PathBuf, Owner, String, SystemTime)> = Vec::new();
    for folder in OUTPUT_FOLDERS {
        for e in read_dir(&profile.join(folder)) {
            let name = name_of(&e);
            let Some((owner, build)) = object_group(&name) else { continue };
            if !is_live_owner(&owner, live) {
                continue;
            }
            let Ok(t) = e.metadata().and_then(|m| m.modified()) else { continue };
            objects.push((e.path(), owner, build.to_string(), t));
        }
    }
    let mut newest: BTreeMap<&Owner, (SystemTime, &str)> = BTreeMap::new();
    for (_, owner, build, t) in &objects {
        if newest.get(owner).is_none_or(|(n, _)| t > n) {
            newest.insert(owner, (*t, build.as_str()));
        }
    }
    objects
        .iter()
        .filter(|(_, owner, build, _)| newest.get(owner).is_some_and(|(_, b)| *b == build.as_str()))
        .map(|(p, ..)| p.clone())
        .collect()
}

/// Everything that a sweep of `profile` removes, as whole entries that do not overlap. The live set decides
/// what stays: the listed outputs, cargo's lock, the entries of the live units, the newest object build of
/// each live owner, and the newest finalized incremental session of each crate folder that a kept output is
/// hard-linked to. An empty live set removes nothing: a listing that names no output is not a fact about the build.
fn doomed(profile: &Path, live: &Live) -> Vec<PathBuf> {
    if live.paths.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for e in read_dir(profile) {
        let (name, path) = (name_of(&e), e.path());
        let folder = is_dir(&path) && matches!(name.as_str(), "deps" | "examples" | "build" | ".fingerprint" | INCREMENTAL);
        let dep_info = name.strip_suffix(".d").is_some_and(|b| live.paths.contains(Path::new(b)));
        if folder || is_cargo_lock(&name) || dep_info || live.paths.contains(Path::new(&name)) {
            continue;
        }
        out.push(path);
    }
    for folder in UNIT_FOLDERS {
        for e in read_dir(&profile.join(folder)) {
            if !unit_hash(&name_of(&e)).is_some_and(|h| live.hashes.contains(h)) {
                out.push(e.path());
            }
        }
    }
    let objects = kept_objects(profile, live);
    let mut kept: Vec<PathBuf> = Vec::new();
    for folder in OUTPUT_FOLDERS {
        for e in read_dir(&profile.join(folder)) {
            let (name, path) = (name_of(&e), e.path());
            let listed = live.paths.contains(&Path::new(folder).join(&name));
            let keep = listed
                || match object_group(&name) {
                    Some(_) => objects.contains(&path),
                    None => {
                        unit_hash(&name).is_some_and(|h| live.hashes.contains(h))
                            || live.crates.iter().any(|c| crate_file(&name, c))
                    }
                };
            if keep {
                kept.push(path);
            } else {
                out.push(path);
            }
        }
    }
    let inodes: BTreeSet<(u64, u64)> = kept.iter().filter_map(|p| ino(p)).collect();
    // A folder that no kept output links to is kept only for a live test binary, bin or example of its crate, and then
    // only the newest of them, and only when no linked folder of that crate is a test binary, bin or example.
    // Without this, an unlinked folder of each build (a unit built without debug info) would stay.
    let exes = live_exe_crates(profile, live);
    let invoked = invocations(profile);
    let folders: Vec<(PathBuf, bool)> = read_dir(&profile.join(INCREMENTAL))
        .iter()
        .map(|e| {
            let path = e.path();
            let by_link = files_under(&path).iter().any(|f| ino(f).is_some_and(|i| inodes.contains(&i)));
            let linked = is_dir(&path) && (by_link || session_of_live_unit(&path, &invoked, live));
            (path, linked)
        })
        .collect();
    let exe_linked: BTreeSet<String> = folders
        .iter()
        .filter(|(path, linked)| *linked && newest_session(path).is_some() && !is_library_folder(path))
        .filter_map(|(path, _)| crate_of_folder(path))
        .collect();
    let mut unlinked: BTreeMap<String, Vec<(SystemTime, PathBuf)>> = BTreeMap::new();
    let mut to_prune: Vec<PathBuf> = Vec::new();
    for (path, linked) in folders {
        if linked {
            to_prune.push(path);
            continue;
        }
        match crate_of_folder(&path) {
            Some(c) if is_dir(&path) && exes.contains(&c) && !exe_linked.contains(&c) => {
                let t = path.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
                unlinked.entry(c).or_default().push((t, path));
            }
            _ => out.push(path),
        }
    }
    for (_, mut folders) in unlinked {
        folders.sort();
        let (_, newest) = folders.pop().unwrap();
        out.extend(folders.into_iter().map(|(_, p)| p));
        to_prune.push(newest);
    }
    for path in to_prune {
        prune_folder(path, &mut out);
    }
    out
}

/// Keeps the newest finalized session of an incremental folder, with its lock, and removes the rest.
/// A folder without a finalized session goes whole.
fn prune_folder(path: PathBuf, out: &mut Vec<PathBuf>) {
    let Some(keep) = newest_session(&path) else {
        out.push(path);
        return;
    };
    // rustc names the lock of a finalized session by the session name without its hash (`s-<time>-<id>.lock`).
    for x in read_dir(&path) {
        let n = name_of(&x);
        let lock_of_keep = n.strip_suffix(".lock").is_some_and(|stem| keep.starts_with(stem));
        if n != keep && !lock_of_keep {
            out.push(x.path());
        }
    }
}

/// Every unit of the profile folder: its crate (as its incremental folder names it, `lib_a`), its hash, and its
/// `invoked.timestamp`. Cargo writes that file when it starts a unit's compiler, and rustc starts the unit's
/// incremental session just after, on its own clock.
fn invocations(profile: &Path) -> Vec<(String, String, SystemTime)> {
    let mut out = Vec::new();
    for e in read_dir(&profile.join(".fingerprint")) {
        let name = name_of(&e);
        let (Some(hash), Some(base)) = (unit_hash(&name), unit_base(&name)) else { continue };
        if let Ok(t) = e.path().join("invoked.timestamp").metadata().and_then(|m| m.modified()) {
            out.push((base.replace('-', "_"), hash.to_string(), t));
        }
    }
    out
}

/// The start of an incremental session: rustc names it `s-<base36 microseconds>-<id>-<hash>`.
fn session_start(session: &str) -> Option<SystemTime> {
    let micros = u64::from_str_radix(session.strip_prefix("s-")?.split('-').next()?, 36).ok()?;
    Some(UNIX_EPOCH + Duration::from_micros(micros))
}

/// A session belongs to the unit whose invocation is the latest one before the session start, in the same crate.
/// A later unit of the crate starts its own session after its own invocation, so the nearest one is the owner.
/// Without a hard link, this is how an incremental folder is matched to a live unit. It keeps a folder only when
/// the owner of one of its sessions is live. The bound only stops an unrelated old invocation from owning a session.
const MAX_SESSION_LAG: Duration = Duration::from_secs(60);

fn session_of_live_unit(folder: &Path, invoked: &[(String, String, SystemTime)], live: &Live) -> bool {
    let Some(c) = crate_of_folder(folder) else { return false };
    read_dir(folder).iter().filter_map(|e| session_start(&name_of(e))).any(|s| {
        let owner = invoked.iter().filter(|(k, _, t)| *k == c && *t <= s).max_by_key(|(_, _, t)| *t);
        owner.is_some_and(|(_, h, t)| live.hashes.contains(h) && s.duration_since(*t).is_ok_and(|d| d <= MAX_SESSION_LAG))
    })
}

/// The crate of an incremental folder: its name without the id (`lib_a-<id>` is `lib_a`).
fn crate_of_folder(folder: &Path) -> Option<String> {
    file_name(folder)?.rsplit_once('-').map(|(c, _)| c.to_string())
}

/// Whether the newest session of an incremental folder is a library's: a library session holds `metadata.rmeta`.
fn is_library_folder(folder: &Path) -> bool {
    newest_session(folder).is_some_and(|s| folder.join(s).join("metadata.rmeta").is_file())
}

/// Crate names of the live test binaries, bins and examples: `deps/<name>-<hash>` or `examples/<name>-<hash>` with no
/// extension and a live hash. The crate name of a hashed file uses `_`, like its incremental folder.
fn live_exe_crates(profile: &Path, live: &Live) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for folder in OUTPUT_FOLDERS {
        for e in read_dir(&profile.join(folder)) {
            let name = name_of(&e);
            if name.contains('.') || !unit_hash(&name).is_some_and(|h| live.hashes.contains(h)) {
                continue;
            }
            if let Some((c, _)) = name.rsplit_once('-') {
                out.insert(c.replace('-', "_"));
            }
        }
    }
    out
}

/// An unhashed output of the crate `c` in `deps`: `lib<c>.<ext>` (the file that cargo compares, and its uplifted copy) or
/// `<c>.d` (its dep-info). A crate has one such unit, so these files are that unit's.
fn crate_file(name: &str, c: &str) -> bool {
    name == format!("{c}.d") || name.strip_prefix(&format!("lib{c}.")).is_some_and(|ext| !ext.is_empty() && !ext.contains('-'))
}

/// Cargo's lock files in a profile folder: `.cargo-lock`, `.cargo-build-lock`, `.cargo-artifact-lock`.
fn is_cargo_lock(name: &str) -> bool {
    name.starts_with(".cargo-") && name.ends_with("lock")
}

/// The newest finalized incremental session of a crate folder: `s-*`, not `-working`, a folder.
fn newest_session(folder: &Path) -> Option<String> {
    read_dir(folder)
        .iter()
        .filter(|e| {
            let n = name_of(e);
            n.starts_with("s-") && !n.ends_with(".lock") && !n.ends_with("-working") && is_dir(&e.path())
        })
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(name_of)
}

/// Remove the entries of the profile folder `profile` that `live` does not need, and return the freed bytes
/// (counted, not removed, with `dry`). An inode frees its bytes when every one of its links is removed.
pub fn sweep(profile: &Path, live: &Live, dry: bool) -> u64 {
    let doomed = doomed(profile, live);
    let freed = freed_by(&doomed);
    if !dry {
        for p in &doomed {
            remove(p);
        }
    }
    freed
}

/// Bytes of the files under `p`, each inode counted once. A hard link of a file counts its bytes once.
pub fn tree_bytes(p: &Path) -> u64 {
    let mut out = BTreeMap::new();
    visit(p, &mut out);
    out.values().map(|(_, _, len)| len).sum()
}

/// Bytes that removing `paths` frees: a file frees its bytes when every link to its inode is in `paths`.
fn freed_by(paths: &[PathBuf]) -> u64 {
    // (dev, ino) -> (links removed, links in total, bytes)
    let mut doomed: BTreeMap<(u64, u64), (u64, u64, u64)> = BTreeMap::new();
    for p in paths {
        visit(p, &mut doomed);
    }
    doomed.values().filter(|(removed, links, _)| removed == links).map(|(_, _, len)| len).sum()
}

fn visit(p: &Path, out: &mut BTreeMap<(u64, u64), (u64, u64, u64)>) {
    let Ok(m) = p.symlink_metadata() else { return };
    if m.is_dir() {
        for e in read_dir(p) {
            visit(&e.path(), out);
        }
    } else {
        let e = out.entry((m.dev(), m.ino())).or_insert((0, m.nlink(), m.len()));
        e.0 += 1;
    }
}

fn remove(p: &Path) {
    match p.symlink_metadata() {
        Ok(m) if m.is_dir() => {
            let _ = std::fs::remove_dir_all(p);
        }
        Ok(_) => {
            let _ = std::fs::remove_file(p);
        }
        Err(_) => {}
    }
}

/// Files under `p` (not following symlinks), or `p` itself when it is a file.
fn files_under(p: &Path) -> Vec<PathBuf> {
    if !is_dir(p) {
        return vec![p.to_path_buf()];
    }
    read_dir(p).iter().flat_map(|e| files_under(&e.path())).collect()
}

fn ino(p: &Path) -> Option<(u64, u64)> {
    let m = p.symlink_metadata().ok()?;
    (!m.is_dir()).then(|| (m.dev(), m.ino()))
}

fn is_dir(p: &Path) -> bool {
    p.symlink_metadata().is_ok_and(|m| m.is_dir())
}

fn name_of(e: &std::fs::DirEntry) -> String {
    e.file_name().to_string_lossy().into_owned()
}

fn read_dir(p: &Path) -> Vec<std::fs::DirEntry> {
    std::fs::read_dir(p).into_iter().flatten().flatten().collect()
}

/// Run `f` while this process holds cargo's build lock of the profile folder `debug`. The lock is
/// taken without waiting: `None` when another cargo holds it, or when the folder has no lock file yet.
pub fn locked<T>(debug: &Path, f: impl FnOnce() -> T) -> Result<Option<T>> {
    let Ok(lock) = File::open(debug.join(LOCK)) else { return Ok(None) };
    match lock.try_lock() {
        Ok(()) => {
            let out = f();
            crate::cargo::unlock_all(std::slice::from_ref(&lock));
            Ok(Some(out))
        }
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e).context("cannot lock the cargo build lock"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    const H1: &str = "0123456789abcdef";
    const H2: &str = "fedcba9876543210";
    const H3: &str = "1111111111111111";
    const H4: &str = "2222222222222222";

    fn touch(p: &Path, n: usize) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, vec![b'x'; n]).unwrap();
    }

    fn at(p: &Path, secs: u64) {
        File::open(p).unwrap().set_modified(UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
    }

    fn names(dir: &Path) -> BTreeSet<String> {
        read_dir(dir).iter().map(name_of).collect()
    }

    /// A new folder for each call. `cargo test` runs the tests of this binary in one process, so the
    /// process id alone does not separate two tests that use the same name.
    fn tempdir(n: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let k = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("layr-retest-sweep-{n}-{}-{k}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A profile folder with a live unit (H1) and a unit that is not live (H2) in every folder cargo uses,
    /// and every kind of stale output. Returns the folder, the live set, and the bytes that the sweep frees.
    fn fake_debug(n: &str) -> (PathBuf, Live, u64) {
        let debug = tempdir(n).join("debug");
        for h in [H1, H2] {
            touch(&debug.join(format!(".fingerprint/lib-a-{h}/dep-lib-a")), 10);
            touch(&debug.join(format!("build/lib-a-{h}/out/bindgen.rs")), 10);
        }
        touch(&debug.join(format!("deps/liblib_a-{H1}.rlib")), 10);
        touch(&debug.join(format!("deps/liblib_a-{H1}.rmeta")), 10);
        touch(&debug.join(format!("deps/lib_a-{H1}.d")), 10);
        touch(&debug.join(format!("deps/lib_a-{H1}.dSYM/Contents/x")), 10);
        touch(&debug.join(format!("examples/ex-{H1}")), 10);
        touch(&debug.join(format!("deps/liblib_a-{H2}.rlib")), 10);
        touch(&debug.join(format!("deps/lib_a-{H2}.d")), 10);
        touch(&debug.join(format!("examples/ex-{H2}")), 10);
        // Outputs with no hash: never listed, so removed. A stale top-level bin and example copy too.
        touch(&debug.join("deps/notes.txt"), 10);
        touch(&debug.join("examples/ex.d"), 10);
        touch(&debug.join("libgone.a"), 10);
        touch(&debug.join("stale-bin"), 10);
        touch(&debug.join(".cargo-lock"), 0);
        let live = Live {
            hashes: BTreeSet::from([H1.to_string()]),
            paths: BTreeSet::from([PathBuf::from("deps/liblib_a-0123456789abcdef.rlib")]),
            crates: BTreeSet::new(),
        };
        let freed = 90;
        (debug, live, freed)
    }

    #[test]
    fn unit_hash_reads_cargo_hashes_only() {
        assert_eq!(unit_hash(&format!("libfoo-{H1}.rlib")), Some(H1));
        assert_eq!(unit_hash(&format!("foo-{H1}.cgu.1aaa.rcgu.o")), Some(H1));
        assert_eq!(unit_hash(&format!("lib-a-{H1}")), Some(H1));
        assert_eq!(unit_hash("app-0tw3p7axdwsy8"), None);
        assert_eq!(unit_hash("notes.txt"), None);
        assert_eq!(unit_hash("short-abc.rlib"), None);
    }

    #[test]
    fn object_group_reads_the_owner_and_the_build() {
        assert_eq!(object_group(&format!("foo-{H1}.cgu.1aaa.rcgu.o")), Some((Owner::Unit(H1.into()), "1aaa")));
        assert_eq!(object_group(&format!("foo-{H1}.foo.id1-cgu.02.rcgu.o")), Some((Owner::Unit(H1.into()), "id1")));
        assert_eq!(object_group("studio_core.06h5.057pils.rcgu.o"), Some((Owner::Crate("studio_core".into()), "057pils")));
        assert_eq!(object_group("notes.o"), None);
        assert_eq!(object_group(&format!("foo-{H1}.d")), None);
    }

    #[test]
    fn plan_names_the_outputs_of_the_messages_per_profile() {
        let (debug, _, _) = fake_debug("plan");
        let target = debug.parent().unwrap().to_path_buf();
        let d = debug.display().to_string();
        // A bin copy matched to its hashed file by size and mtime; a dylib with no hashed file; a build script.
        touch(&debug.join(format!("deps/app-{H3}")), 7);
        touch(&debug.join("app"), 7);
        at(&debug.join(format!("deps/app-{H3}")), 1_700_000_000);
        at(&debug.join("app"), 1_700_000_000);
        touch(&debug.join("libdy.dylib"), 9);
        touch(&debug.join(format!(".fingerprint/dy-{H4}/lib-dy")), 1);
        touch(&debug.join(format!(".fingerprint/dy-{H2}/lib-dy")), 1);
        at(&debug.join(format!(".fingerprint/dy-{H4}/lib-dy")), 200);
        at(&debug.join(format!(".fingerprint/dy-{H2}/lib-dy")), 100);
        let msgs = [
            serde_json::json!({"reason": "compiler-artifact", "target": {"name": "app", "kind": ["bin"]}, "filenames": [format!("{d}/app")], "executable": format!("{d}/app")}),
            serde_json::json!({"reason": "compiler-artifact", "target": {"name": "dy", "kind": ["dylib"]}, "filenames": [format!("{d}/libdy.dylib")], "executable": null}),
            serde_json::json!({"reason": "build-script-executed", "out_dir": format!("{d}/build/lib-a-{H1}/out")}),
            serde_json::json!({"reason": "build-finished", "success": true}),
        ];
        let text = msgs.iter().map(|m| m.to_string()).collect::<Vec<_>>().join("\n");
        let plans = plan(&target, text.as_bytes()).unwrap();
        let live = &plans[&debug];
        assert!(live.hashes.contains(H3) && live.hashes.contains(H1), "{live:?}");
        // The newest marked unit of the dylib, not the older one.
        assert!(live.hashes.contains(H4) && !live.hashes.contains(H2), "{live:?}");
        assert!(live.paths.contains(Path::new("app")) && live.paths.contains(Path::new("libdy.dylib")), "{live:?}");
        assert!(live.paths.contains(Path::new("build/lib-a-0123456789abcdef/out")), "{live:?}");
        std::fs::remove_dir_all(debug.parent().unwrap()).unwrap();
    }

    #[test]
    fn plan_takes_the_build_unit_of_an_unhashed_library_and_not_a_newer_check_unit() {
        // A crate with a cdylib: its build writes `libcw.rlib` and `libcw.dylib` without a hash. A later clippy
        // of the same crate is a newer unit of kind lib that writes a hashed `.rmeta` and `.d`.
        let debug = tempdir("cdylib").join("debug");
        let target = debug.parent().unwrap().to_path_buf();
        let d = debug.display().to_string();
        touch(&debug.join("deps/libcw.rlib"), 9);
        touch(&debug.join("deps/libcw.dylib"), 9);
        touch(&debug.join(format!(".fingerprint/cw-{H3}/lib-cw")), 1);
        touch(&debug.join(format!(".fingerprint/cw-{H4}/lib-cw")), 1);
        at(&debug.join(format!(".fingerprint/cw-{H3}/lib-cw")), 100);
        at(&debug.join(format!(".fingerprint/cw-{H4}/lib-cw")), 200);
        touch(&debug.join(format!("deps/libcw-{H4}.rmeta")), 5);
        touch(&debug.join(format!("deps/cw-{H4}.d")), 5);
        let msg = serde_json::json!({"reason": "compiler-artifact", "target": {"name": "cw", "kind": ["cdylib", "rlib"]},
            "filenames": [format!("{d}/deps/libcw.rlib"), format!("{d}/deps/libcw.dylib")], "executable": null});
        let plans = plan(&target, msg.to_string().as_bytes()).unwrap();
        let live = &plans[&debug];
        assert!(live.hashes.contains(H3) && !live.hashes.contains(H4), "{live:?}");
        std::fs::remove_dir_all(target).unwrap();
    }

    #[test]
    fn plan_refuses_an_output_outside_a_profile_folder() {
        let t = tempdir("refuse").join("target");
        std::fs::create_dir_all(t.join("loose")).unwrap();
        let msg = serde_json::json!({"reason": "compiler-artifact", "target": {"name": "a", "kind": ["bin"]}, "filenames": [format!("{}/loose/a", t.display())], "executable": null});
        assert!(plan(&t, msg.to_string().as_bytes()).is_err());
        std::fs::remove_dir_all(t.parent().unwrap()).unwrap();
    }

    #[test]
    fn sweep_keeps_the_live_set_and_removes_every_other_entry() {
        let (debug, live, freed) = fake_debug("allow");
        assert_eq!(sweep(&debug, &live, false), freed);
        let left = |sub: &str| names(&debug.join(sub));
        assert_eq!(left(".fingerprint"), BTreeSet::from([format!("lib-a-{H1}")]));
        assert_eq!(left("build"), BTreeSet::from([format!("lib-a-{H1}")]));
        assert_eq!(left("examples"), BTreeSet::from([format!("ex-{H1}")]));
        assert_eq!(
            left("deps"),
            BTreeSet::from([
                format!("liblib_a-{H1}.rlib"),
                format!("liblib_a-{H1}.rmeta"),
                format!("lib_a-{H1}.d"),
                format!("lib_a-{H1}.dSYM")
            ])
        );
        assert_eq!(
            left(""),
            BTreeSet::from([".cargo-lock".to_string(), "build".into(), "deps".into(), ".fingerprint".into(), "examples".into()])
        );
        std::fs::remove_dir_all(debug.parent().unwrap()).unwrap();
    }

    #[test]
    fn sweep_with_an_empty_live_set_removes_nothing() {
        let (debug, _, _) = fake_debug("empty");
        let before = names(&debug.join("deps"));
        assert_eq!(sweep(&debug, &Live::default(), false), 0);
        assert_eq!(names(&debug.join("deps")), before);
        std::fs::remove_dir_all(debug.parent().unwrap()).unwrap();
    }

    #[test]
    fn sweep_dry_run_removes_nothing_and_counts_the_same() {
        let (debug, live, freed) = fake_debug("dry");
        let before = names(&debug.join("deps"));
        assert_eq!(sweep(&debug, &live, true), freed);
        assert_eq!(names(&debug.join("deps")), before);
        assert!(debug.join(format!(".fingerprint/lib-a-{H2}")).exists());
        std::fs::remove_dir_all(debug.parent().unwrap()).unwrap();
    }

    #[test]
    fn sweep_keeps_the_newest_object_build_of_each_live_owner() {
        let debug = tempdir("obj").join("debug");
        let d = debug.join("deps");
        let objs = [
            (format!("foo-{H1}.cgu1.aaa.rcgu.o"), 100),
            (format!("foo-{H1}.cgu2.aaa.rcgu.o"), 101),
            (format!("foo-{H1}.cgu1.bbb.rcgu.o"), 200),
            (format!("foo-{H1}.cgu2.bbb.rcgu.o"), 199),
            (format!("foo-{H2}.cgu1.aaa.rcgu.o"), 50),
            ("core.cgu1.old.rcgu.o".to_string(), 10),
            ("core.cgu1.new.rcgu.o".to_string(), 20),
        ];
        for (f, t) in &objs {
            touch(&d.join(f), 4);
            at(&d.join(f), *t);
        }
        touch(&d.join("core.d"), 1);
        touch(&d.join(format!("libcore.rlib")), 1);
        let live = Live {
            hashes: BTreeSet::from([H1.to_string()]),
            paths: BTreeSet::from([PathBuf::from("deps/libcore.rlib")]),
            crates: BTreeSet::from(["core".to_string()]),
        };
        sweep(&debug, &live, false);
        let left = names(&d);
        assert_eq!(
            left,
            BTreeSet::from([
                format!("foo-{H1}.cgu1.bbb.rcgu.o"),
                format!("foo-{H1}.cgu2.bbb.rcgu.o"),
                "core.cgu1.new.rcgu.o".into(),
                "core.d".into(),
                "libcore.rlib".into()
            ])
        );
        std::fs::remove_dir_all(debug.parent().unwrap()).unwrap();
    }

    #[test]
    fn incremental_folder_stays_only_when_a_kept_object_is_hard_linked_to_it() {
        let (debug, live, _) = fake_debug("inc");
        let inc = debug.join("incremental");
        // The live object and its link in the session of a folder that stays.
        touch(&debug.join(format!("deps/liblib_a-{H1}.cgu.o")), 5);
        let kept = debug.join(format!("deps/liblib_a-{H1}.cgu.o"));
        let linked = inc.join("lib_a-keep/s-1-aa-bb/x.o");
        std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
        std::fs::hard_link(&kept, &linked).unwrap();
        touch(&inc.join("lib_a-keep/s-1-aa-bb.lock"), 0);
        touch(&inc.join("lib_a-keep/s-0-zz/x.o"), 3); // older finalized session: removed
        at(&inc.join("lib_a-keep/s-0-zz"), 10);
        at(&inc.join("lib_a-keep/s-1-aa-bb"), 20);
        // A folder with no link to a kept output, and a folder with only a working session.
        touch(&inc.join("gone-zz/s-1-aa/x.o"), 3);
        touch(&inc.join("lib_a-work/s-1-aa-working/x.o"), 3);
        touch(&inc.join("lib_a-keep/stray-file"), 1);
        sweep(&debug, &live, false);
        assert_eq!(names(&inc), BTreeSet::from(["lib_a-keep".to_string()]));
        assert_eq!(names(&inc.join("lib_a-keep")), BTreeSet::from(["s-1-aa-bb".to_string(), "s-1-aa-bb.lock".to_string()]));
        std::fs::remove_dir_all(debug.parent().unwrap()).unwrap();
    }

    #[test]
    fn incremental_session_keeps_the_lock_that_rustc_names_by_the_session_prefix() {
        let (debug, live, _) = fake_debug("lock");
        let inc = debug.join("incremental");
        // rustc names the lock of a finalized session `s-<time>-<id>.lock`, and the session `s-<time>-<id>-<hash>`.
        // Without the lock, the next build cannot reuse the object files of the session.
        touch(&inc.join("lib_a-keep/s-1-aa-bb/x.o"), 3);
        touch(&inc.join("lib_a-keep/s-1-aa.lock"), 0);
        touch(&inc.join("lib_a-keep/s-0-zz.lock"), 0);
        touch(&inc.join("lib_a-keep/s-0-zz-cc/x.o"), 3);
        at(&inc.join("lib_a-keep/s-0-zz-cc"), 10);
        at(&inc.join("lib_a-keep/s-1-aa-bb"), 20);
        // The link of the kept folder: the library's metadata is hard-linked to its rmeta in deps.
        std::fs::create_dir_all(debug.join("deps")).unwrap();
        touch(&debug.join("deps/liblib_a-0123456789abcdef.rmeta"), 4);
        std::fs::hard_link(debug.join("deps/liblib_a-0123456789abcdef.rmeta"), inc.join("lib_a-keep/s-1-aa-bb/metadata.rmeta"))
            .unwrap();
        sweep(&debug, &live, false);
        assert_eq!(names(&inc.join("lib_a-keep")), BTreeSet::from(["s-1-aa-bb".to_string(), "s-1-aa.lock".to_string()]));
        std::fs::remove_dir_all(debug.parent().unwrap()).unwrap();
    }

    #[test]
    fn sweep_keeps_only_the_newest_unlinked_folder_of_a_live_exe_crate() {
        let (debug, live, _) = fake_debug("exe");
        let inc = debug.join("incremental");
        // `app` is a live bin: its hashed output has no extension, so it names the crate.
        touch(&debug.join(format!("deps/app-{H1}")), 3);
        touch(&inc.join("app-old/s-1-aa/x.o"), 3);
        touch(&inc.join("app-new/s-1-bb/x.o"), 3);
        touch(&inc.join("other-cc/s-1-cc/x.o"), 3);
        at(&inc.join("app-old"), 10);
        at(&inc.join("app-new"), 20);
        sweep(&debug, &live, false);
        assert_eq!(names(&inc), BTreeSet::from(["app-new".to_string()]));
        std::fs::remove_dir_all(debug.parent().unwrap()).unwrap();
    }

    #[test]
    fn hard_link_frees_bytes_only_when_every_link_goes() {
        let d = tempdir("link");
        touch(&d.join("a"), 10);
        std::fs::hard_link(d.join("a"), d.join("b")).unwrap();
        assert_eq!(freed_by(&[d.join("a")]), 0);
        assert_eq!(freed_by(&[d.join("a"), d.join("b")]), 10);
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// Base 36 digits of `n`, as rustc writes the start of a session name.
    fn b36(mut n: u64) -> String {
        let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let mut out = Vec::new();
        loop {
            out.push(digits[(n % 36) as usize]);
            n /= 36;
            if n == 0 {
                break;
            }
        }
        out.reverse();
        String::from_utf8(out).unwrap()
    }

    /// A session belongs to the latest invocation before it in its crate. Only a live owner keeps the folder.
    #[test]
    fn session_belongs_to_the_latest_invocation_before_it() {
        let t = tempdir("owner");
        let debug = t.join("debug");
        let (live_hash, feature_hash) = (H1, H2);
        for (h, secs) in [(live_hash, 100u64), (feature_hash, 200)] {
            let inv = debug.join(format!(".fingerprint/dy-{h}/invoked.timestamp"));
            touch(&inv, 0);
            at(&inv, secs);
        }
        // Each session starts 0.2 s after the invocation of its unit.
        let session = |secs: u64| format!("s-{}-0-x", b36((secs * 1_000_000) + 200_000));
        touch(&debug.join(format!("incremental/dy-aaaa/{}/dep-graph.bin", session(100))), 1);
        touch(&debug.join(format!("incremental/dy-bbbb/{}/dep-graph.bin", session(200))), 1);
        let live = Live { hashes: BTreeSet::from([live_hash.to_string()]), ..Default::default() };
        let invoked = invocations(&debug);
        assert!(session_of_live_unit(&debug.join("incremental/dy-aaaa"), &invoked, &live));
        assert!(!session_of_live_unit(&debug.join("incremental/dy-bbbb"), &invoked, &live));
        std::fs::remove_dir_all(&t).unwrap();
    }

    #[test]
    fn locked_skips_a_busy_lock_and_a_missing_lock_file() {
        let t = tempdir("lock");
        let debug = t.join("debug");
        assert_eq!(locked(&debug, || 1).unwrap(), None);
        touch(&debug.join(LOCK), 0);
        assert_eq!(locked(&debug, || 1).unwrap(), Some(1));
        let held = File::open(debug.join(LOCK)).unwrap();
        held.lock().unwrap();
        assert_eq!(locked(&debug, || 1).unwrap(), None);
        drop(held);
        assert_eq!(locked(&debug, || 2).unwrap(), Some(2));
        std::fs::remove_dir_all(&t).unwrap();
    }
}
