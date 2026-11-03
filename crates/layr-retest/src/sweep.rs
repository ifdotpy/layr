//! Remove the build output of a profile folder (`target/<...>/<profile>`) that no standard command
//! needs. The rule is an allowlist: an entry stays only when a listed unit needs it. See docs/retest.md,
//! "Sweeping stale build output".

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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
                            live.crates.insert(name.clone());
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
    for c in copies {
        let outputs = outputs_of.entry(c.profile.clone()).or_insert_with(|| outputs(&c.profile));
        let m = c.path.metadata().with_context(|| format!("cannot read the output {}", c.path.display()))?;
        let live = plans.entry(c.profile.clone()).or_default();
        let same: Vec<&Output> = outputs.iter().filter(|o| o.len == m.len() && o.mtime == (m.mtime(), m.mtime_nsec())).collect();
        if same.is_empty() {
            // No file matches: the outputs of that name and every unit of its kind stay.
            let base = c.name.replace('-', "_");
            live.hashes.extend(outputs.iter().filter(|o| unit_base(&o.name).is_some_and(|b| b == base)).map(|o| o.hash.clone()));
            live.hashes.extend(marked(&c.profile, c.marker, &base).into_iter().map(|(h, _)| h));
        } else {
            live.hashes.extend(same.iter().map(|o| o.hash.clone()));
        }
    }
    for (profile, live) in plans.iter_mut() {
        for name in live.crates.clone() {
            // The newest unit of the crate is the one whose build wrote the unhashed output.
            let newest = marked(profile, "lib", &name).into_iter().max_by_key(|(_, t)| *t);
            live.hashes.extend(newest.map(|(h, _)| h));
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
    for e in read_dir(&profile.join(INCREMENTAL)) {
        let path = e.path();
        let linked = is_dir(&path) && files_under(&path).iter().any(|f| ino(f).is_some_and(|i| inodes.contains(&i)));
        let session = if linked { newest_session(&path) } else { None };
        match session {
            None => out.push(path),
            Some(keep) => {
                for x in read_dir(&path) {
                    let n = name_of(&x);
                    if n.strip_suffix(".lock").unwrap_or(&n) != keep {
                        out.push(x.path());
                    }
                }
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
        Ok(()) => Ok(Some(f())),
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

    fn tempdir(n: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("layr-retest-sweep-{n}-{}", std::process::id()));
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
    fn hard_link_frees_bytes_only_when_every_link_goes() {
        let d = tempdir("link");
        touch(&d.join("a"), 10);
        std::fs::hard_link(d.join("a"), d.join("b")).unwrap();
        assert_eq!(freed_by(&[d.join("a")]), 0);
        assert_eq!(freed_by(&[d.join("a"), d.join("b")]), 10);
        std::fs::remove_dir_all(&d).unwrap();
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
