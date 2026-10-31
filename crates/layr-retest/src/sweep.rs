//! Remove build output that the current build did not use, from `target/retest/debug` only.
//! See docs/retest.md, "Sweeping stale build output".

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Length of the hash that cargo appends to a file name in `deps`.
const HASH_LEN: usize = 16;

/// The names of the files the build produced, from `cargo --message-format json`: one entry per
/// `compiler-artifact` file, fresh or not.
pub fn live_files(messages: &[u8]) -> BTreeSet<String> {
    let mut live = BTreeSet::new();
    for l in String::from_utf8_lossy(messages).lines() {
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        if v["reason"] != "compiler-artifact" {
            continue;
        }
        let names = v["filenames"].as_array().into_iter().flatten().chain(v["executable"].as_str().map(|_| &v["executable"]));
        for f in names.filter_map(Value::as_str) {
            if let Some(n) = Path::new(f).file_name() {
                live.insert(n.to_string_lossy().into_owned());
            }
        }
    }
    live
}

/// `name-hash` of a cargo file name (`libfoo-<hash>.rlib`, `foo-<hash>.d`, `foo-<hash>`), without
/// the optional `lib` prefix; or `None` when the name has no cargo hash. Both spellings are
/// returned: a crate named `libc` has a test binary `libc-<hash>`.
fn stems(name: &str) -> Option<Vec<String>> {
    let stem = name.split('.').next()?;
    let (_, hash) = stem.rsplit_once('-')?;
    if hash.len() != HASH_LEN || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut v = vec![stem.to_string()];
    if let Some(s) = stem.strip_prefix("lib") {
        v.push(s.to_string());
    }
    Some(v)
}

/// Size that deleting `p` frees: files with one hard link, recursively, without following symlinks.
fn size(p: &Path) -> u64 {
    let Ok(m) = p.symlink_metadata() else { return 0 };
    if m.is_dir() {
        std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| size(&e.path())).sum()
    } else if m.nlink() == 1 {
        m.len()
    } else {
        0
    }
}

/// Remove `p` (a file, a symlink or a folder) and return the freed bytes.
fn remove(p: &Path) -> u64 {
    let n = size(p);
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

/// Build number of an object file `<name-hash>.<unit>.<build>.rcgu.o` (the macOS default keeps the
/// object files next to the binary, and each incremental build writes a new set): `<name-hash>` and `<build>`.
fn object_build(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_suffix(".rcgu.o")?;
    let (stem, _) = rest.split_once('.')?;
    Some((stem, rest.rsplit_once('.')?.1))
}

/// Remove the files of `deps` that carry a cargo hash and belong to no live file, and the object
/// files of older builds: of each `name-hash` only the build with the newest object file stays.
pub fn sweep_deps(deps: &Path, live: &BTreeSet<String>) -> u64 {
    let live: BTreeSet<String> = live.iter().filter_map(|f| stems(f)).flatten().collect();
    let entries: Vec<_> = std::fs::read_dir(deps).into_iter().flatten().flatten().collect();
    let mut newest: std::collections::BTreeMap<String, (std::time::SystemTime, String)> = Default::default();
    for e in &entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let (Some((stem, build)), Ok(t)) = (object_build(&name), e.metadata().and_then(|m| m.modified())) else { continue };
        if newest.get(stem).is_none_or(|(n, _)| t > *n) {
            newest.insert(stem.to_string(), (t, build.to_string()));
        }
    }
    let mut freed = 0;
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let old_object = object_build(&name).is_some_and(|(stem, build)| stems(stem).is_some() && newest.get(stem).is_some_and(|(_, b)| b != build));
        if old_object || stems(&name).is_some_and(|s| s.iter().all(|k| !live.contains(k))) {
            freed += remove(&e.path());
        }
    }
    freed
}

/// In each crate folder of `incremental` keep the newest finalized session and its lock file; remove
/// the other `s-*` entries. A folder without a finalized session stays as it is.
pub fn sweep_incremental(incremental: &Path) -> u64 {
    let mut freed = 0;
    for krate in std::fs::read_dir(incremental).into_iter().flatten().flatten() {
        let entries: Vec<_> = std::fs::read_dir(krate.path()).into_iter().flatten().flatten().collect();
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
                freed += remove(&e.path());
            }
        }
    }
    freed
}

/// Sweep `<target>/debug` while holding cargo's build lock, so that no cargo of this target builds
/// at the same time. Returns the freed bytes; `None` when the lock is busy or absent, or `live` is empty.
pub fn sweep(target: &Path, live: &BTreeSet<String>) -> Result<Option<u64>> {
    let debug = target.join("debug");
    if live.is_empty() {
        return Ok(None);
    }
    let Ok(lock) = File::open(debug.join(".cargo-lock")) else { return Ok(None) };
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
        Err(std::fs::TryLockError::Error(e)) => return Err(e).context("cannot lock the cargo build lock"),
    }
    Ok(Some(sweep_deps(&debug.join("deps"), live) + sweep_incremental(&debug.join("incremental"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    const H1: &str = "0123456789abcdef";
    const H2: &str = "fedcba9876543210";

    fn touch(p: &Path, n: usize) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, vec![b'x'; n]).unwrap();
    }

    #[test]
    fn live_files_reads_artifacts_only() {
        let m = format!(
            "{{\"reason\":\"compiler-artifact\",\"filenames\":[\"/t/deps/libfoo-{H1}.rlib\"],\"executable\":null}}\n\
             {{\"reason\":\"compiler-artifact\",\"filenames\":[],\"executable\":\"/t/deps/e2e-{H1}\"}}\n\
             {{\"reason\":\"build-finished\",\"success\":true}}\nnot json\n"
        );
        let live = live_files(m.as_bytes());
        assert_eq!(live.into_iter().collect::<Vec<_>>(), vec![format!("e2e-{H1}"), format!("libfoo-{H1}.rlib")]);
    }

    #[test]
    fn deps_keeps_live_stems_and_foreign_files() {
        let d = tempdir("deps");
        for f in [
            format!("libfoo-{H1}.rlib"),
            format!("libfoo-{H1}.rmeta"),
            format!("foo-{H1}.d"),
            format!("libfoo-{H2}.rlib"),
            format!("libfoo-{H2}.rmeta"),
            format!("foo-{H2}.d"),
            format!("t-{H2}"),
            format!("t-{H1}"),
            "notes.txt".into(),
            "short-abc.rlib".into(),
        ] {
            touch(&d.join(f), 10);
        }
        touch(&d.join(format!("t-{H2}.dSYM/Contents/x")), 5);
        let live = BTreeSet::from([format!("libfoo-{H1}.rlib"), format!("t-{H1}")]);
        assert_eq!(sweep_deps(&d, &live), 4 * 10 + 5);
        let mut left: Vec<_> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(
            left,
            vec![format!("foo-{H1}.d"), format!("libfoo-{H1}.rlib"), format!("libfoo-{H1}.rmeta"), "notes.txt".into(), "short-abc.rlib".into(), format!("t-{H1}")]
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn deps_keeps_only_the_newest_object_build() {
        let d = tempdir("obj");
        for (f, t) in [
            (format!("foo-{H1}.cgu1.aaa.rcgu.o"), 100),
            (format!("foo-{H1}.cgu2.aaa.rcgu.o"), 101),
            (format!("foo-{H1}.cgu1.bbb.rcgu.o"), 200),
            (format!("foo-{H1}.cgu2.bbb.rcgu.o"), 199),
            (format!("bar-{H1}.cgu1.aaa.rcgu.o"), 50),
        ] {
            touch(&d.join(&f), 4);
            std::fs::File::open(d.join(&f)).unwrap().set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(t)).unwrap();
        }
        touch(&d.join(format!("foo-{H1}")), 1);
        let live = BTreeSet::from([format!("foo-{H1}"), format!("bar-{H1}")]);
        assert_eq!(sweep_deps(&d, &live), 8);
        let mut left: Vec<_> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, vec![format!("bar-{H1}.cgu1.aaa.rcgu.o"), format!("foo-{H1}"), format!("foo-{H1}.cgu1.bbb.rcgu.o"), format!("foo-{H1}.cgu2.bbb.rcgu.o")]);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn deps_crate_named_lib_prefix_stays() {
        let d = tempdir("libc");
        touch(&d.join(format!("libc-{H1}")), 1);
        touch(&d.join(format!("liblibc-{H1}.rlib")), 1);
        let live = BTreeSet::from([format!("liblibc-{H1}.rlib")]);
        assert_eq!(sweep_deps(&d, &live), 0);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn deps_hard_link_frees_nothing() {
        let d = tempdir("link");
        touch(&d.join(format!("a-{H2}")), 10);
        std::fs::hard_link(d.join(format!("a-{H2}")), d.join("a")).unwrap();
        let live = BTreeSet::from([format!("b-{H1}")]);
        assert_eq!(sweep_deps(&d, &live), 0);
        assert!(!d.join(format!("a-{H2}")).exists());
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
            f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(t)).unwrap();
        }
        // A crate folder without a finalized session stays.
        touch(&d.join("bar-abc/s-1-aa-working/data"), 7);
        assert_eq!(sweep_incremental(&d), 20);
        assert!(c.join("s-2-bb/data").exists() && c.join("s-2-bb.lock").exists());
        assert!(!c.join("s-1-aa").exists() && !c.join("s-3-cc-working").exists() && !c.join("s-1-aa.lock").exists());
        assert!(d.join("bar-abc/s-1-aa-working/data").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn sweep_skips_a_busy_lock_and_an_empty_live_set() {
        let t = tempdir("lock");
        touch(&t.join("debug/.cargo-lock"), 0);
        touch(&t.join(format!("debug/deps/a-{H2}")), 3);
        let live = BTreeSet::from([format!("b-{H1}")]);
        assert_eq!(sweep(&t, &BTreeSet::new()).unwrap(), None);
        let held = File::open(t.join("debug/.cargo-lock")).unwrap();
        held.lock().unwrap();
        assert_eq!(sweep(&t, &live).unwrap(), None);
        assert!(t.join(format!("debug/deps/a-{H2}")).exists());
        drop(held);
        assert_eq!(sweep(&t, &live).unwrap(), Some(3));
        std::fs::remove_dir_all(&t).unwrap();
    }

    fn tempdir(n: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("layr-retest-sweep-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
