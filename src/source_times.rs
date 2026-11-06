//! The source file times that a cache saw (docs/cache-overlays.md, section 20).
//!
//! Build tools such as cargo compare file times: a source file newer than its build output is built again. A line
//! from a state, a merge and a follow pass write source files with a new time, so a warm cache looks stale. A cache
//! save writes `.layr-times` into each cache folder: path, time, size and content hash of each tracked file of the
//! working copy. The file goes with the cache (snapshots, copies, overlay layers) and goes away with it. A line that
//! takes the cache gives each file whose bytes equal the recorded bytes the recorded time back. Equal bytes make the
//! build output current, whatever the time. A file with other bytes must look newer than the build output, so the
//! tool builds it again: one whose time is not after the save of the cache (the time of `.layr-times`) gets the
//! current time. A line's files can carry an old time (a clone keeps the time of the first write), and a cache
//! built later from other bytes would look current otherwise.

use crate::fsutil;
use crate::ignore_rules::Rules;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// The name of the file in a cache folder.
pub const FILE: &str = ".layr-times";

/// One tracked file: modification time (seconds, nanoseconds), size and content hash.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    sec: i64,
    nsec: i64,
    size: u64,
    hash: String,
}

/// The tracked files of the working folder `w` (the ignore rules keep them) and their entries. Symbolic links and
/// `.git` are left out.
fn collect(w: &Path) -> Result<BTreeMap<String, Entry>> {
    let rules = Rules::new(w);
    let mut files = Vec::new();
    walk(w, w, &rules, &mut files)?;
    let hashes = hash_all(w, &files.iter().map(|(rel, _)| rel.as_str()).collect::<Vec<_>>())?;
    let mut out = BTreeMap::new();
    for ((rel, m), hash) in files.into_iter().zip(hashes) {
        // A file that went away during the walk is left out.
        if let Some(hash) = hash {
            out.insert(rel, Entry { sec: m.mtime(), nsec: m.mtime_nsec(), size: m.len(), hash });
        }
    }
    Ok(out)
}

fn walk(root: &Path, dir: &Path, rules: &Rules, out: &mut Vec<(String, std::fs::Metadata)>) -> Result<()> {
    for e in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let path = e?.path();
        let rel = path.strip_prefix(root)?.to_string_lossy().into_owned();
        let Ok(m) = std::fs::symlink_metadata(&path) else { continue };
        if m.file_type().is_symlink() || rel == ".git" || rules.ignored(&rel, m.is_dir()) {
            continue;
        }
        if m.is_dir() {
            walk(root, &path, rules, out)?;
        } else if m.is_file() {
            out.push((rel, m));
        }
    }
    Ok(())
}

/// The content hash (git blob id) of each file, on a few threads. None for a file that cannot be read.
fn hash_all(w: &Path, rels: &[&str]) -> Result<Vec<Option<String>>> {
    let hash = |rel: &&str| hash_file(&w.join(rel));
    if rels.len() < 2 {
        return Ok(rels.iter().map(hash).collect());
    }
    crate::pools::run(crate::pools::Priority::Foreground, crate::pools::Width::items(rels.len()), || {
        use rayon::prelude::*;
        rels.par_iter().map(hash).collect()
    }).map_err(Into::into)
}

/// The git blob id of one file. Source files are small, so the file is read whole.
fn hash_file(p: &Path) -> Option<String> {
    let m = std::fs::symlink_metadata(p).ok()?;
    if !m.is_file() {
        return None;
    }
    Some(fsutil::git_blob_id(&std::fs::read(p).ok()?))
}

fn encode(files: &BTreeMap<String, Entry>) -> Vec<u8> {
    let mut out = String::from("layr-times 1\n");
    for (rel, e) in files {
        // The path is last: it may hold spaces. A path with a newline is left out.
        if !rel.contains('\n') {
            out.push_str(&format!("{} {} {} {} {rel}\n", e.sec, e.nsec, e.size, e.hash));
        }
    }
    out.into_bytes()
}

fn decode(text: &str) -> BTreeMap<String, Entry> {
    let mut out = BTreeMap::new();
    let mut lines = text.lines();
    if lines.next() != Some("layr-times 1") {
        return out;
    }
    for l in lines {
        let mut p = l.splitn(5, ' ');
        let (Some(sec), Some(nsec), Some(size), Some(hash), Some(rel)) = (p.next(), p.next(), p.next(), p.next(), p.next())
        else {
            continue;
        };
        let (Ok(sec), Ok(nsec), Ok(size)) = (sec.parse(), nsec.parse(), size.parse()) else { continue };
        out.insert(rel.to_string(), Entry { sec, nsec, size, hash: hash.to_string() });
    }
    out
}

/// Writes `.layr-times` for the tracked files of `w` into each cache folder `folders` (relative to `w`) that is a
/// folder. Called before the snapshot of a cache save, outside the project lock.
pub fn record(w: &Path, folders: &[String]) -> Result<()> {
    let dirs: Vec<_> = folders.iter().map(|f| fsutil::safe_join(w, f)).collect::<Result<_>>()?;
    // An empty folder holds no build output, so it needs no times (and stays empty).
    let holds = |d: &std::path::Path| std::fs::read_dir(d).is_ok_and(|mut r| r.any(|e| e.is_ok_and(|e| e.file_name() != FILE)));
    let dirs: Vec<_> = dirs.into_iter().filter(|d| d.is_dir() && holds(d)).collect();
    if dirs.is_empty() {
        return Ok(());
    }
    let _t = crate::trace::span("source times: record");
    let data = encode(&collect(w)?);
    for d in dirs {
        fsutil::atomic_write(&d.join(FILE), &data, 0o644)?;
    }
    Ok(())
}

/// What one cache recorded of a file, and the time of that cache's save.
type Saw = (Entry, (i64, i64));

/// Gives the tracked files of `w` the times that the caches in `folders` saw (their `.layr-times`). A file gets a
/// time only when its size and content hash equal the recorded ones in every cache that records it; with two
/// caches, it gets the older time. When the caches saw other bytes, the file gets the time of the caches that saw
/// its bytes if that time is after the save of each cache that saw other bytes: those still build it again. A line
/// that saved one cache after a change and keeps older saves of the others (a line from a line) then stays warm.
/// Returns the number of files that got a time.
pub fn apply(w: &Path, folders: &[String]) -> Result<usize> {
    // What each cache saw of a file, with the time of that cache's save.
    let mut seen: BTreeMap<String, Vec<Saw>> = BTreeMap::new();
    // The newest save of the caches: build output is not newer than it.
    let mut saved = (i64::MIN, 0);
    for f in folders {
        let path = fsutil::safe_join(w, f)?.join(FILE);
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        // A save of unknown time is after every file.
        let at = std::fs::metadata(&path).map_or((i64::MAX, 0), |m| (m.mtime(), m.mtime_nsec()));
        saved = saved.max(at);
        for (rel, e) in decode(&text) {
            seen.entry(rel).or_default().push((e, at));
        }
    }
    if seen.is_empty() {
        return Ok(0);
    }
    let _t = crate::trace::span("source times: apply");
    // Only a file of the recorded size and another time is read; a file that the caches saw with other bytes is
    // read too.
    let mut cand: Vec<(&str, &[Saw])> = Vec::new();
    // Files with other bytes whose time is not after the save.
    let mut older: Vec<&str> = Vec::new();
    for (rel, saw) in &seen {
        let Ok(m) = std::fs::symlink_metadata(w.join(rel)) else { continue };
        if !m.is_file() {
            continue;
        }
        let time = (m.mtime(), m.mtime_nsec());
        let agree = saw.iter().all(|(e, _)| e.hash == saw[0].0.hash && e.size == saw[0].0.size);
        let size = saw.iter().any(|(e, _)| e.size == m.len());
        let first = saw.iter().map(|(e, _)| (e.sec, e.nsec)).min();
        if size && (!agree || first != Some(time)) {
            cand.push((rel.as_str(), saw.as_slice()));
        } else if !size && time <= saved {
            older.push(rel.as_str());
        }
    }
    let hashes = hash_all(w, &cand.iter().map(|(rel, _)| *rel).collect::<Vec<_>>())?;
    let mut n = 0;
    for ((rel, saw), hash) in cand.into_iter().zip(hashes) {
        let Ok(m) = std::fs::symlink_metadata(w.join(rel)) else { continue };
        let same = |e: &Entry| e.size == m.len() && hash.as_deref() == Some(e.hash.as_str());
        // The oldest time of the caches that saw these bytes, and the newest save of those that saw others.
        let first = saw.iter().filter(|(e, _)| same(e)).map(|(e, _)| (e.sec, e.nsec)).min();
        let others = saw.iter().filter(|(e, _)| !same(e)).map(|(_, at)| *at).max();
        let time = (m.mtime(), m.mtime_nsec());
        match first {
            Some(t) if others.is_none_or(|o| t > o) => {
                if t == time {
                    continue;
                }
                let f = std::fs::OpenOptions::new().write(true).open(w.join(rel));
                // A file the caller cannot write keeps its time.
                if let Ok(f) = f {
                    if fsutil::set_mtime(&f, t.0, t.1).is_ok() {
                        n += 1;
                    }
                }
            }
            _ if time <= saved => older.push(rel),
            _ => {}
        }
    }
    if !older.is_empty() {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
        for rel in older {
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open(w.join(rel)) {
                let _ = fsutil::set_mtime(&f, now.as_secs() as i64, now.subsec_nanos() as i64);
            }
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("layr-source-times-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::create_dir_all(d.join("target")).unwrap();
        std::fs::write(d.join(".gitignore"), "/target\n").unwrap();
        d
    }

    fn mtime(p: &Path) -> (i64, i64) {
        let m = std::fs::metadata(p).unwrap();
        (m.mtime(), m.mtime_nsec())
    }

    fn set(p: &Path, sec: i64) {
        fsutil::set_mtime(&std::fs::OpenOptions::new().write(true).open(p).unwrap(), sec, 0).unwrap();
    }

    #[test]
    fn equal_bytes_get_the_recorded_time_and_other_bytes_keep_theirs() {
        let w = tmp("apply");
        std::fs::write(w.join("src/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(w.join("src/b.rs"), "fn b() {}\n").unwrap();
        set(&w.join("src/a.rs"), 1_000_000_000);
        set(&w.join("src/b.rs"), 1_000_000_000);
        std::fs::write(w.join("target/out"), "built").unwrap();
        record(&w, &["target".to_string()]).unwrap();
        let text = std::fs::read_to_string(w.join("target").join(FILE)).unwrap();
        assert!(text.contains(" src/a.rs\n") && !text.contains("target/"), "{text}");

        // A checkout writes both files with a new time; b gets other bytes of the same size.
        std::fs::write(w.join("src/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(w.join("src/b.rs"), "fn c() {}\n").unwrap();
        // A coarse clock can give the write the time of the save; the write comes later.
        set(&w.join("src/b.rs"), mtime(&w.join("target").join(FILE)).0 + 3600);
        let b_time = mtime(&w.join("src/b.rs"));
        assert_eq!(apply(&w, &["target".to_string()]).unwrap(), 1);
        assert_eq!(mtime(&w.join("src/a.rs")), (1_000_000_000, 0));
        assert_eq!(mtime(&w.join("src/b.rs")), b_time, "other bytes keep the time of the write");
        assert_eq!(apply(&w, &["target".to_string()]).unwrap(), 0, "a second apply changes nothing");
        let _ = std::fs::remove_dir_all(&w);
    }

    #[test]
    fn two_caches_that_saw_other_bytes_leave_the_time() {
        let w = tmp("two");
        std::fs::create_dir_all(w.join("node_modules")).unwrap();
        std::fs::write(w.join(".gitignore"), "/target\n/node_modules\n").unwrap();
        std::fs::write(w.join("target/out"), "built").unwrap();
        std::fs::write(w.join("node_modules/out"), "built").unwrap();
        std::fs::write(w.join("src/a.rs"), "one\n").unwrap();
        set(&w.join("src/a.rs"), 1_000_000_000);
        record(&w, &["target".to_string()]).unwrap();
        std::fs::write(w.join("src/a.rs"), "two\n").unwrap();
        set(&w.join("src/a.rs"), 1_100_000_000);
        record(&w, &["node_modules".to_string()]).unwrap();
        let after = mtime(&w.join("node_modules").join(FILE));
        set(&w.join("src/a.rs"), after.0 + 3600);
        assert_eq!(apply(&w, &["target".to_string(), "node_modules".to_string()]).unwrap(), 0);
        assert_eq!(mtime(&w.join("src/a.rs")), (after.0 + 3600, 0), "a time after both saves stays");
        set(&w.join("src/a.rs"), 1_000_000_000);
        apply(&w, &["target".to_string(), "node_modules".to_string()]).unwrap();
        assert!(mtime(&w.join("src/a.rs")) > after, "an older time goes past the saves");
        let _ = std::fs::remove_dir_all(&w);
    }

    #[test]
    fn a_cache_saved_after_a_change_gives_its_time_when_older_caches_saw_other_bytes() {
        // node_modules was saved before the change, target after it (a line saves one cache, then a line from it
        // takes both).
        let w = tmp("later");
        std::fs::create_dir_all(w.join("node_modules")).unwrap();
        std::fs::write(w.join(".gitignore"), "/target\n/node_modules\n").unwrap();
        std::fs::write(w.join("target/out"), "built").unwrap();
        std::fs::write(w.join("node_modules/out"), "built").unwrap();
        std::fs::write(w.join("src/a.rs"), "one\n").unwrap();
        record(&w, &["node_modules".to_string()]).unwrap();
        let before = mtime(&w.join("node_modules").join(FILE));
        std::fs::write(w.join("src/a.rs"), "two\n").unwrap();
        set(&w.join("src/a.rs"), before.0 + 10);
        record(&w, &["target".to_string()]).unwrap();
        // A checkout writes the file with a new time.
        set(&w.join("src/a.rs"), before.0 + 7200);
        let both = ["target".to_string(), "node_modules".to_string()];
        assert_eq!(apply(&w, &both).unwrap(), 1);
        assert_eq!(mtime(&w.join("src/a.rs")), (before.0 + 10, 0), "target sees its own time");
        assert!(mtime(&w.join("src/a.rs")) > before, "node_modules sees a change after its save");
        assert_eq!(apply(&w, &both).unwrap(), 0, "a second apply changes nothing");
        // Bytes that no cache saw get a time after the saves.
        std::fs::write(w.join("src/a.rs"), "six\n").unwrap();
        set(&w.join("src/a.rs"), 1_000_000_000);
        assert_eq!(apply(&w, &both).unwrap(), 0);
        assert!(mtime(&w.join("src/a.rs")) > mtime(&w.join("target").join(FILE)));
        let _ = std::fs::remove_dir_all(&w);
    }

    #[test]
    fn other_bytes_with_an_old_time_get_a_time_after_the_save() {
        // The cache is built from older bytes after the file got its time (a clone keeps it), so
        // the time alone would make the build output look current.
        let w = tmp("older");
        std::fs::write(w.join("src/a.rs"), "old\n").unwrap();
        std::fs::write(w.join("src/b.rs"), "one\n").unwrap();
        std::fs::write(w.join("target/out"), "built").unwrap();
        record(&w, &["target".to_string()]).unwrap();
        let saved = mtime(&w.join("target").join(FILE));
        std::fs::write(w.join("src/a.rs"), "new\n").unwrap();
        std::fs::write(w.join("src/b.rs"), "longer\n").unwrap();
        set(&w.join("src/a.rs"), 1_000_000_000);
        set(&w.join("src/b.rs"), 1_000_000_000);
        assert_eq!(apply(&w, &["target".to_string()]).unwrap(), 0);
        assert!(mtime(&w.join("src/a.rs")) > saved, "same size, other bytes");
        assert!(mtime(&w.join("src/b.rs")) > saved, "other size");
        let now = mtime(&w.join("src/a.rs"));
        apply(&w, &["target".to_string()]).unwrap();
        assert_eq!(mtime(&w.join("src/a.rs")), now, "a second apply leaves the new time");
        let _ = std::fs::remove_dir_all(&w);
    }

    #[test]
    fn an_empty_cache_folder_gets_no_times() {
        let w = tmp("empty");
        std::fs::write(w.join("src/a.rs"), "a\n").unwrap();
        record(&w, &["target".to_string()]).unwrap();
        assert!(!w.join("target").join(FILE).exists());
        let _ = std::fs::remove_dir_all(&w);
    }

    #[test]
    fn a_path_with_spaces_round_trips() {
        let mut m = BTreeMap::new();
        m.insert("a b/c d.rs".to_string(), Entry { sec: 1, nsec: 2, size: 3, hash: "h".into() });
        assert_eq!(decode(std::str::from_utf8(&encode(&m)).unwrap()), m);
    }
}

/// The cost on a real working folder: `LAYR_TIMES_BENCH=<folder> cargo test --bin layr times_bench -- --ignored
/// --nocapture`. It records into a temp folder and applies to a copy of nothing (reads only).
#[cfg(test)]
#[test]
#[ignore]
fn times_bench() {
    let Some(w) = std::env::var_os("LAYR_TIMES_BENCH") else { return };
    let w = std::path::PathBuf::from(w);
    let t = std::time::Instant::now();
    let files = collect(&w).unwrap();
    let rec = t.elapsed();
    let data = encode(&files);
    let t = std::time::Instant::now();
    let want = decode(std::str::from_utf8(&data).unwrap());
    // The apply reads: one stat per file; all times differ (the worst case: a checkout), so all are hashed.
    let rels: Vec<&str> = want.keys().map(|k| k.as_str()).collect();
    for r in &rels {
        let _ = std::fs::symlink_metadata(w.join(r));
    }
    let _ = hash_all(&w, &rels);
    let app = t.elapsed();
    let bytes: u64 = files.values().map(|e| e.size).sum();
    eprintln!("files {} bytes {} record {:?} apply-worst {:?} manifest {} bytes", files.len(), bytes, rec, app, data.len());
}
