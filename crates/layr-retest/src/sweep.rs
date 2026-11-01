//! Remove the build output of the cargo units that no standard build command uses, from one profile
//! folder (`target/<...>/debug`). See docs/retest.md, "Sweeping stale build output".

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Length of the hash that cargo appends to the name of a unit.
const HASH_LEN: usize = 16;
/// Folders of a profile folder with one entry per unit, named `<package>-<hash>`.
const UNIT_FOLDERS: [&str; 2] = [".fingerprint", "build"];
/// Folders with the output files of the units: `<crate>-<hash>[.ext]` files and folders.
const OUTPUT_FOLDERS: [&str; 2] = ["deps", "examples"];

/// The cargo hash of a file or folder name: the 16 hex digits after the last `-` of the name before
/// its first `.`. `None` for a name without one, such as `incremental/<crate>-<base 36>`.
pub fn unit_hash(name: &str) -> Option<&str> {
    let stem = name.split('.').next()?;
    let (_, hash) = stem.rsplit_once('-')?;
    (hash.len() == HASH_LEN && hash.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hash)
}

/// The name of a unit before its hash: `lib-a` for `lib-a-<hash>`, `ex` for `ex-<hash>.d`.
fn unit_base(name: &str) -> Option<&str> {
    let stem = name.split('.').next()?;
    stem.rsplit_once('-').filter(|_| unit_hash(name).is_some()).map(|(base, _)| base)
}

/// One output file of a unit in `deps` or `examples`.
struct Output {
    name: String,
    hash: String,
    len: u64,
    mtime: (i64, i64),
}

fn outputs(debug: &Path) -> Vec<Output> {
    let mut v = Vec::new();
    for folder in OUTPUT_FOLDERS {
        for e in read_dir(&debug.join(folder)) {
            let name = e.file_name().to_string_lossy().into_owned();
            let (Some(hash), Ok(m)) = (unit_hash(&name), e.metadata()) else { continue };
            if m.is_file() {
                let hash = hash.to_string();
                v.push(Output { name, hash, len: m.len(), mtime: (m.mtime(), m.mtime_nsec()) });
            }
        }
    }
    v
}

/// What a listing of cargo's JSON names: the live unit hashes, and the artifacts that the listing
/// itself compiled (`fresh: false`). A folder with built artifacts is not swept: its live set would
/// have been produced by a build that the sweep did not check.
pub struct Listing {
    pub live: BTreeSet<String>,
    pub built: Vec<String>,
}

/// Unit hashes that cargo names in a `--message-format json` output: every artifact, fresh or not,
/// and every build script run. Cargo names a bin or an example only by its copy in `debug` (`debug/app`);
/// a copy is matched to its `deps` or `examples` file by size and modification time. When no file
/// matches, every output file of that name is live: a unit that may be live is never removed.
pub fn live_units(debug: &Path, messages: &[u8]) -> Listing {
    let mut live = BTreeSet::new();
    let mut built = Vec::new();
    let mut copies: Vec<(PathBuf, String)> = Vec::new();
    for line in String::from_utf8_lossy(messages).lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let paths: Vec<&str> = match v["reason"].as_str() {
            Some("compiler-artifact") => {
                if v["fresh"].as_bool() == Some(false) {
                    built.push(v["target"]["name"].as_str().unwrap_or("?").to_string());
                }
                v["filenames"].as_array().into_iter().flatten().filter_map(Value::as_str).chain(v["executable"].as_str()).collect()
            }
            // `build/<package>-<hash>/out`: the hash is in the folder of the out dir.
            Some("build-script-executed") => v["out_dir"].as_str().into_iter().collect(),
            _ => continue,
        };
        let target = v["target"]["name"].as_str().unwrap_or_default().to_string();
        for p in paths {
            let p = Path::new(p);
            let file = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let folder = p.parent().and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            match unit_hash(&file).or_else(|| unit_hash(&folder)) {
                Some(h) => {
                    live.insert(h.to_string());
                }
                None => copies.push((p.to_path_buf(), target.clone())),
            }
        }
    }
    if copies.is_empty() {
        return Listing { live, built };
    }
    let outputs = outputs(debug);
    for (c, target) in copies {
        let (Some(name), Ok(m)) = (c.file_name().map(|n| n.to_string_lossy().into_owned()), c.metadata()) else { continue };
        let same: Vec<&Output> = outputs.iter().filter(|o| o.len == m.len() && o.mtime == (m.mtime(), m.mtime_nsec())).collect();
        if same.is_empty() {
            live.extend(outputs.iter().filter(|o| unit_base(&o.name) == Some(name.as_str())).map(|o| o.hash.clone()));
            // A dylib has no hashed file in `deps` (cargo only uplifts it): its units are the folders with
            // the marker file of the target. Every one of them is kept.
            live.extend(marked_units(debug, &target));
        } else {
            live.extend(same.iter().map(|o| o.hash.clone()));
        }
    }
    Listing { live, built }
}

/// The hashes of the `.fingerprint` folders that hold the marker file of the target `target`
/// (`lib-<name>`, `bin-<name>`, `example-<name>` or `test-<name>`).
fn marked_units(debug: &Path, target: &str) -> Vec<String> {
    if target.is_empty() {
        return Vec::new();
    }
    let markers = ["lib", "bin", "example", "test"].map(|k| format!("{k}-{target}"));
    read_dir(&debug.join(".fingerprint"))
        .iter()
        .filter(|e| markers.iter().any(|m| e.path().join(m).is_file()))
        .filter_map(|e| unit_hash(&e.file_name().to_string_lossy()).map(String::from))
        .collect()
}

/// Size that deleting `p` frees: files with one hard link, recursively, without following symlinks.
fn size(p: &Path) -> u64 {
    let Ok(m) = p.symlink_metadata() else { return 0 };
    if m.is_dir() {
        read_dir(p).iter().map(|e| size(&e.path())).sum()
    } else if m.nlink() == 1 {
        m.len()
    } else {
        0
    }
}

/// Remove `p` (a file, a symlink or a folder) and return the freed bytes. With `dry`, remove nothing
/// and return the bytes that the removal would free.
fn remove(p: &Path, dry: bool) -> u64 {
    let n = size(p);
    if dry {
        return n;
    }
    let done = match p.symlink_metadata() {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p).is_ok(),
        Ok(_) => std::fs::remove_file(p).is_ok(),
        Err(_) => false,
    };
    if done {
        n
    } else {
        0
    }
}

fn read_dir(p: &Path) -> Vec<std::fs::DirEntry> {
    std::fs::read_dir(p).into_iter().flatten().flatten().collect()
}

/// The unit hash and the build id of an object file, from one of two forms:
/// `<name>-<hash>.<cgu>.<build>.rcgu.o`, or `<name>-<hash>.<crate>.<id>-cgu.<n>.rcgu.o` (rustc names
/// the codegen units of a compile with one `<id>`). Every other name returns `None` and is kept.
fn object_build(name: &str) -> Option<(&str, &str)> {
    let parts: Vec<&str> = name.split('.').collect();
    let n = parts.len();
    if n < 2 || parts[n - 1] != "o" || parts[n - 2] != "rcgu" {
        return None;
    }
    let hash = unit_hash(parts[0])?;
    match n {
        5 => Some((hash, parts[2])),
        6 => parts[2].strip_suffix("-cgu").map(|id| (hash, id)),
        _ => None,
    }
}

/// In a folder of outputs, remove the object files of a live unit that come from an older build of
/// that unit (the newest build id of each unit stays, all its files). Each build writes a new set of object files for the same unit hash, and cargo never
/// removes the old set. Only this rule removes a file of a live unit: the newest set stays, and the
/// other rules keep their files (see docs/retest.md, "Object files").
fn sweep_objects(folder: &Path, live: &BTreeSet<String>, dry: bool) -> u64 {
    let entries = read_dir(folder);
    let mut newest: BTreeMap<String, (SystemTime, String)> = BTreeMap::new();
    for e in &entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let (Some((hash, build)), Ok(t)) = (object_build(&name), e.metadata().and_then(|m| m.modified())) else { continue };
        if live.contains(hash) && newest.get(hash).is_none_or(|(n, _)| t > *n) {
            newest.insert(hash.to_string(), (t, build.to_string()));
        }
    }
    let mut freed = 0;
    for e in &entries {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some((hash, build)) = object_build(&name) {
            if newest.get(hash).is_some_and(|(_, b)| b != build) {
                freed += remove(&e.path(), dry);
            }
        }
    }
    freed
}

/// Remove the units of the profile folder `debug` that `live` does not name, and return the freed
/// bytes (counted, not removed, with `dry`). A unit goes whole: its entry in `.fingerprint` and `build`,
/// and its files and folders in `deps` and `examples`. The one exception is an object file of a live
/// unit from an older build (`sweep_objects`). An empty `live` removes nothing: a listing that names
/// no unit is not a fact about the build.
pub fn sweep_units(debug: &Path, live: &BTreeSet<String>, dry: bool) -> u64 {
    if live.is_empty() {
        return 0;
    }
    let mut freed = 0;
    for folder in UNIT_FOLDERS.iter().chain(&OUTPUT_FOLDERS) {
        for e in read_dir(&debug.join(folder)) {
            let name = e.file_name().to_string_lossy().into_owned();
            if unit_hash(&name).is_some_and(|h| !live.contains(h)) {
                freed += remove(&e.path(), dry);
            }
        }
    }
    for folder in OUTPUT_FOLDERS {
        freed += sweep_objects(&debug.join(folder), live, dry);
    }
    freed + sweep_incremental(&debug.join("incremental"), dry)
}

/// In each crate folder of `incremental` keep the newest finalized session and its lock file; remove
/// the other `s-*` entries. A folder without a finalized session stays as it is. The folder name is
/// not the cargo hash, so the rule is per folder.
fn sweep_incremental(incremental: &Path, dry: bool) -> u64 {
    let mut freed = 0;
    for krate in read_dir(incremental) {
        let entries = read_dir(&krate.path());
        let session = |n: &str| n.starts_with("s-") && !n.ends_with(".lock");
        let newest = entries
            .iter()
            .filter(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                session(&n) && !n.ends_with("-working") && e.path().is_dir()
            })
            .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
            .map(|e| e.file_name().to_string_lossy().into_owned());
        let Some(keep) = newest else { continue };
        for e in entries {
            let n = e.file_name().to_string_lossy().into_owned();
            let base = n.strip_suffix(".lock").unwrap_or(&n);
            if base.starts_with("s-") && base != keep {
                freed += remove(&e.path(), dry);
            }
        }
    }
    freed
}

/// Run `f` while this process holds cargo's build lock of the profile folder `debug`. The lock is
/// taken without waiting: `None` when another cargo holds it, or when the folder has no lock file yet.
pub fn locked<T>(debug: &Path, f: impl FnOnce() -> T) -> Result<Option<T>> {
    let Ok(lock) = File::open(debug.join(".cargo-lock")) else { return Ok(None) };
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
    const H5: &str = "3333333333333333";
    const H6: &str = "4444444444444444";
    const H7: &str = "5555555555555555";

    fn touch(p: &Path, n: usize) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, vec![b'x'; n]).unwrap();
    }

    fn names(dir: &Path) -> BTreeSet<String> {
        read_dir(dir).iter().map(|e| e.file_name().to_string_lossy().into_owned()).collect()
    }

    fn tempdir(n: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("layr-retest-sweep-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A profile folder with a live unit (H1) and a unit that is not live (H2), in every folder cargo uses.
    /// Returns the folder and the bytes that removing the units of H2 frees (65).
    fn fake_debug(n: &str) -> (PathBuf, u64) {
        let debug = tempdir(n).join("debug");
        for h in [H1, H2] {
            touch(&debug.join(format!(".fingerprint/lib-a-{h}/dep-lib-a")), 10);
            touch(&debug.join(format!("build/lib-a-{h}/out/bindgen.rs")), 10);
        }
        // Live: the rlib, the metadata, the dep info, an old object file of the live unit, a dSYM.
        touch(&debug.join(format!("deps/liblib_a-{H1}.rlib")), 10);
        touch(&debug.join(format!("deps/liblib_a-{H1}.rmeta")), 10);
        touch(&debug.join(format!("deps/lib_a-{H1}.d")), 10);
        touch(&debug.join(format!("deps/lib_a-{H1}.cgu.1aaa.rcgu.o")), 10);
        touch(&debug.join(format!("deps/lib_a-{H1}.dSYM/Contents/x")), 10);
        touch(&debug.join(format!("examples/ex-{H1}")), 10);
        // Not live: the same kinds of files, and a folder.
        touch(&debug.join(format!("deps/liblib_a-{H2}.rlib")), 10);
        touch(&debug.join(format!("deps/lib_a-{H2}.d")), 10);
        touch(&debug.join(format!("deps/lib_a-{H2}.dSYM/x")), 5);
        touch(&debug.join(format!("examples/ex-{H2}")), 10);
        touch(&debug.join(format!("examples/ex-{H2}.d")), 10);
        // Names without a cargo hash stay.
        touch(&debug.join("deps/notes.txt"), 10);
        touch(&debug.join("examples/ex.d"), 10);
        touch(&debug.join(".cargo-lock"), 0);
        (debug, 10 * 6 + 5)
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
    fn live_units_names_the_artifacts_and_build_scripts_of_the_messages() {
        let (debug, _) = fake_debug("messages");
        // A bin copy and an example copy are matched to their hashed file by size and mtime.
        let when = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        touch(&debug.join(format!("deps/app-{H3}")), 7);
        touch(&debug.join("app"), 7);
        touch(&debug.join(format!("examples/ex-{H4}")), 9);
        touch(&debug.join("examples/ex"), 9);
        for p in [debug.join(format!("deps/app-{H3}")), debug.join("app"), debug.join(format!("examples/ex-{H4}")), debug.join("examples/ex")] {
            std::fs::File::open(&p).unwrap().set_modified(when).unwrap();
        }
        let d = debug.display().to_string();
        let lines = [
            serde_json::json!({"reason": "compiler-artifact", "filenames": [format!("{d}/deps/liblib_a-{H5}.rlib")], "executable": null}),
            serde_json::json!({"reason": "compiler-artifact", "filenames": [format!("{d}/build/lib-a-{H6}/build-script-build")], "executable": null}),
            serde_json::json!({"reason": "compiler-artifact", "filenames": [format!("{d}/app")], "executable": format!("{d}/app")}),
            serde_json::json!({"reason": "compiler-artifact", "filenames": [format!("{d}/examples/ex")], "executable": format!("{d}/examples/ex")}),
            serde_json::json!({"reason": "build-script-executed", "out_dir": format!("{d}/build/lib-a-{H7}/out")}),
            serde_json::json!({"reason": "build-finished", "success": true}),
        ];
        let mut msgs: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        msgs.push("not json".into());
        let live = live_units(&debug, msgs.join("\n").as_bytes()).live;
        assert_eq!(live.into_iter().collect::<Vec<_>>(), vec![H3, H4, H5, H6, H7].into_iter().map(String::from).collect::<Vec<_>>());
    }

    #[test]
    fn live_units_keeps_the_marked_units_of_an_uplifted_dylib() {
        let debug = tempdir("dylib").join("debug");
        touch(&debug.join("libdy_lib.dylib"), 9);
        touch(&debug.join(format!(".fingerprint/dy-lib-{H1}/lib-dy_lib")), 1);
        touch(&debug.join(format!(".fingerprint/dy-lib-{H2}/lib-dy_lib")), 1);
        touch(&debug.join(format!(".fingerprint/other-{H3}/lib-other")), 1);
        let d = debug.display().to_string();
        let msg = serde_json::json!({"reason": "compiler-artifact", "target": {"name": "dy_lib"}, "filenames": [format!("{d}/libdy_lib.dylib")], "executable": null});
        let live = live_units(&debug, msg.to_string().as_bytes()).live;
        assert!(live.contains(H1) && live.contains(H2) && !live.contains(H3), "{live:?}");
        std::fs::remove_dir_all(debug.parent().unwrap()).unwrap();
    }

    #[test]
    fn live_units_keeps_every_output_of_a_copy_that_matches_no_file() {
        let (debug, _) = fake_debug("fallback");
        touch(&debug.join("app"), 7);
        touch(&debug.join(format!("deps/app-{H3}")), 3);
        touch(&debug.join(format!("deps/app-{H4}")), 5);
        let d = debug.display().to_string();
        let msg = serde_json::json!({"reason": "compiler-artifact", "filenames": [format!("{d}/app")], "executable": format!("{d}/app")});
        let live = live_units(&debug, msg.to_string().as_bytes()).live;
        assert!(live.contains(H3) && live.contains(H4), "{live:?}");
    }

    #[test]
    fn live_units_reports_what_the_listing_built() {
        let debug = PathBuf::from("/t/debug");
        let fresh = serde_json::json!({"reason": "compiler-artifact", "target": {"name": "a"}, "fresh": true, "filenames": [format!("/t/debug/deps/liba-{H1}.rlib")]});
        assert!(live_units(&debug, fresh.to_string().as_bytes()).built.is_empty());
        let built = serde_json::json!({"reason": "compiler-artifact", "target": {"name": "b"}, "fresh": false, "filenames": [format!("/t/debug/deps/libb-{H2}.rlib")]});
        let l = live_units(&debug, built.to_string().as_bytes());
        assert_eq!(l.built, vec!["b".to_string()]);
        assert!(l.live.contains(H2));
    }

    #[test]
    fn deps_keeps_object_names_without_a_build_id() {
        let debug = tempdir("objform").join("debug");
        let d = debug.join("deps");
        // A name of another shape has no build id: it stays, whatever its age.
        touch(&d.join(format!("odd-{H1}.x.y.z.rcgu.o")), 4);
        std::fs::File::open(d.join(format!("odd-{H1}.x.y.z.rcgu.o"))).unwrap().set_modified(UNIX_EPOCH + Duration::from_secs(10)).unwrap();
        // Two sets of the `<id>-cgu.<n>` form: the newest set stays, all of it.
        for (n, t) in [("aaaa-cgu.00", 100), ("aaaa-cgu.01", 100), ("bbbb-cgu.00", 200), ("bbbb-cgu.01", 201)] {
            let f = d.join(format!("integration-{H1}.integration.{n}.rcgu.o"));
            touch(&f, 4);
            std::fs::File::open(&f).unwrap().set_modified(UNIX_EPOCH + Duration::from_secs(t)).unwrap();
        }
        sweep_units(&debug, &BTreeSet::from([H1.to_string()]), false);
        let left = names(&d);
        assert!(left.contains(&format!("integration-{H1}.integration.bbbb-cgu.00.rcgu.o")) && left.contains(&format!("integration-{H1}.integration.bbbb-cgu.01.rcgu.o")), "{left:?}");
        assert!(!left.contains(&format!("integration-{H1}.integration.aaaa-cgu.00.rcgu.o")), "{left:?}");
        assert!(left.contains(&format!("odd-{H1}.x.y.z.rcgu.o")), "{left:?}");
        std::fs::remove_dir_all(debug.parent().unwrap()).unwrap();
    }

    #[test]
    fn deps_keeps_only_the_newest_object_build_of_a_live_unit() {
        let debug = tempdir("obj").join("debug");
        let d = debug.join("deps");
        for (f, t) in [
            (format!("foo-{H1}.cgu1.aaa.rcgu.o"), 100),
            (format!("foo-{H1}.cgu2.aaa.rcgu.o"), 101),
            (format!("foo-{H1}.cgu1.bbb.rcgu.o"), 200),
            (format!("foo-{H1}.cgu2.bbb.rcgu.o"), 199),
            (format!("foo-{H2}.cgu1.aaa.rcgu.o"), 50),
        ] {
            touch(&d.join(&f), 4);
            std::fs::File::open(d.join(&f)).unwrap().set_modified(UNIX_EPOCH + Duration::from_secs(t)).unwrap();
        }
        // H1 is live: the build `bbb` stays, the build `aaa` goes. H2 is not live: it goes whole.
        let live = BTreeSet::from([H1.to_string()]);
        sweep_units(&debug, &live, false);
        let left: BTreeSet<String> = names(&d).into_iter().filter(|n| n.contains(H1) && n.ends_with(".rcgu.o")).collect();
        assert_eq!(left, BTreeSet::from([format!("foo-{H1}.cgu1.bbb.rcgu.o"), format!("foo-{H1}.cgu2.bbb.rcgu.o")]));
        assert!(!names(&d).iter().any(|n| n.contains(H2)));
        std::fs::remove_dir_all(debug.parent().unwrap()).unwrap();
    }

    #[test]
    fn sweep_units_removes_a_non_live_unit_whole_and_no_file_of_a_live_unit() {
        let (debug, freed) = fake_debug("whole");
        let live = BTreeSet::from([H1.to_string()]);
        assert_eq!(sweep_units(&debug, &live, false), freed);
        for gone in [
            format!(".fingerprint/lib-a-{H2}"),
            format!("build/lib-a-{H2}"),
            format!("deps/liblib_a-{H2}.rlib"),
            format!("deps/lib_a-{H2}.d"),
            format!("deps/lib_a-{H2}.dSYM"),
            format!("examples/ex-{H2}"),
            format!("examples/ex-{H2}.d"),
        ] {
            assert!(!debug.join(&gone).exists(), "{gone} stays");
        }
        for kept in [
            format!(".fingerprint/lib-a-{H1}/dep-lib-a"),
            format!("build/lib-a-{H1}/out/bindgen.rs"),
            format!("deps/liblib_a-{H1}.rlib"),
            format!("deps/liblib_a-{H1}.rmeta"),
            format!("deps/lib_a-{H1}.d"),
            format!("deps/lib_a-{H1}.cgu.1aaa.rcgu.o"),
            format!("deps/lib_a-{H1}.dSYM/Contents/x"),
            format!("examples/ex-{H1}"),
            "deps/notes.txt".into(),
            "examples/ex.d".into(),
            ".cargo-lock".into(),
        ] {
            assert!(debug.join(&kept).exists(), "{kept} removed");
        }
        std::fs::remove_dir_all(debug.parent().unwrap()).unwrap();
    }

    #[test]
    fn sweep_units_dry_run_removes_nothing_and_counts_the_same() {
        let (debug, freed) = fake_debug("dry");
        let before = names(&debug.join("deps"));
        let live = BTreeSet::from([H1.to_string()]);
        assert_eq!(sweep_units(&debug, &live, true), freed);
        assert_eq!(names(&debug.join("deps")), before);
        assert!(debug.join(format!(".fingerprint/lib-a-{H2}")).exists());
        std::fs::remove_dir_all(debug.parent().unwrap()).unwrap();
    }

    #[test]
    fn sweep_units_removes_nothing_for_an_empty_live_set() {
        let (debug, _) = fake_debug("empty");
        let before = names(&debug.join("deps"));
        assert_eq!(sweep_units(&debug, &BTreeSet::new(), false), 0);
        assert_eq!(names(&debug.join("deps")), before);
        std::fs::remove_dir_all(debug.parent().unwrap()).unwrap();
    }

    #[test]
    fn hard_linked_file_frees_nothing() {
        let d = tempdir("link");
        touch(&d.join(format!("deps/a-{H2}")), 10);
        std::fs::hard_link(d.join(format!("deps/a-{H2}")), d.join("deps/a")).unwrap();
        let debug = d.clone();
        assert_eq!(sweep_units(&debug, &BTreeSet::from([H1.to_string()]), false), 0);
        assert!(!d.join(format!("deps/a-{H2}")).exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn incremental_keeps_newest_finalized_session() {
        let d = tempdir("inc");
        let c = d.join("foo-abc");
        for (s, t) in [("s-1-aa", 100), ("s-2-bb", 200), ("s-3-cc-working", 300)] {
            touch(&c.join(s).join("data"), 10);
            touch(&c.join(format!("{s}.lock")), 0);
            let f = File::open(c.join(s)).unwrap();
            f.set_modified(UNIX_EPOCH + Duration::from_secs(t)).unwrap();
        }
        // A crate folder without a finalized session stays as it is.
        touch(&d.join("bar-abc/s-1-aa-working/data"), 7);
        assert_eq!(sweep_incremental(&d, false), 20);
        assert!(c.join("s-2-bb/data").exists() && c.join("s-2-bb.lock").exists());
        assert!(!c.join("s-1-aa").exists() && !c.join("s-3-cc-working").exists() && !c.join("s-1-aa.lock").exists());
        assert!(d.join("bar-abc/s-1-aa-working/data").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn locked_skips_a_busy_lock_and_a_missing_lock_file() {
        let t = tempdir("lock");
        let debug = t.join("debug");
        assert_eq!(locked(&debug, || 1).unwrap(), None);
        touch(&debug.join(".cargo-lock"), 0);
        assert_eq!(locked(&debug, || 1).unwrap(), Some(1));
        let held = File::open(debug.join(".cargo-lock")).unwrap();
        held.lock().unwrap();
        assert_eq!(locked(&debug, || 1).unwrap(), None);
        drop(held);
        assert_eq!(locked(&debug, || 2).unwrap(), Some(2));
        std::fs::remove_dir_all(&t).unwrap();
    }
}
