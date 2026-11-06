//! Copy a whole project folder to another file system and keep the sharing between its states.
//!
//! This module walks the project and refuses unsafe targets. The backend does the copy
//! (`storage::Copier`): btrfs sends each subvolume, APFS clones the files that share data inside
//! the copy, and OverlayFS has no copier yet. The database is written with `VACUUM INTO`, so the
//! copy is consistent and the source is only read.
//!
//! Interrupted copies: `copy_project` marks the target with `MARKER` until the copy is complete.
//! A later call removes a marked target and copies again (no partial reuse). The source is not
//! changed; a backend may use temporary snapshots inside it and removes them.

use crate::storage;
use anyhow::{bail, ensure, Context, Result};
use rusqlite::types::Value;
use rusqlite::OpenFlags;
use layr_hashers::FastCryptographic;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Marks a target folder that a copy has not finished.
pub const MARKER: &str = ".layr-copy-in-progress";
/// Temporary snapshots of the source (backends only), removed by the copy.
pub(crate) const TMP: &str = ".layr-copy-tmp";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    State,
    Delta,
    Line,
    Overlay,
    Database,
}

/// A unit that the copy starts: a state, a thin layer, a line, a cache overlay or the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub kind: Kind,
    /// The path inside the project, for example `states/<id>`.
    pub name: String,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct CopyReport {
    pub states: usize,
    pub deltas: usize,
    pub lines: usize,
    pub overlays: usize,
    /// Bytes that the copy occupies on the target (shared extents counted once).
    pub need: u64,
    /// Time to copy the project (without the verification).
    pub copy_ms: u128,
    /// Time to verify the copy (`verify_copy`), which reads every file.
    pub verify_ms: u128,
}

#[derive(Default)]
struct Counts {
    states: usize,
    deltas: usize,
    lines: usize,
    overlays: usize,
}

struct Sink<'a> {
    progress: &'a mut dyn FnMut(Step),
    counts: Counts,
}

impl Sink<'_> {
    fn step(&mut self, kind: Kind, rel: &[String]) {
        match kind {
            Kind::State => self.counts.states += 1,
            Kind::Delta => self.counts.deltas += 1,
            Kind::Line => self.counts.lines += 1,
            Kind::Overlay => self.counts.overlays += 1,
            Kind::Database => {}
        }
        (self.progress)(Step { kind, name: rel.join("/") });
    }
}

/// The unit that `rel` (a path inside the project) starts, if any.
fn unit(rel: &[String]) -> Option<Kind> {
    match rel.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["states", _] => Some(Kind::State),
        ["deltas", _] => Some(Kind::Delta),
        ["lines", _] => Some(Kind::Line),
        ["meta", "overlays", _] => Some(Kind::Overlay),
        _ => None,
    }
}

fn is_database(rel: &[String]) -> bool {
    rel.len() >= 2 && rel[rel.len() - 2] == "meta" && rel[rel.len() - 1] == "db"
}

/// The older database file `meta/layr.sqlite` (projects before `meta/db`).
fn is_old_database(rel: &[String]) -> bool {
    rel.len() >= 2 && rel[rel.len() - 2] == "meta" && rel[rel.len() - 1] == "layr.sqlite"
}

/// Copy the project folder `src` to the new folder `dst`, which must be on another file system.
///
/// `progress` gets one `Step` per unit before the unit is copied. The copy refuses a target that
/// is not empty (a target with `MARKER` is a partial copy, which is removed first), a target on
/// the same file system, and a target with less free space than the copy needs.
pub fn copy_project(src: &Path, dst: &Path, mut progress: impl FnMut(Step)) -> Result<CopyReport> {
    let started = Instant::now();
    let src = fs::canonicalize(src).with_context(|| format!("open {}", src.display()))?;
    ensure!(src.join("meta").is_dir() || src.join("states").is_dir(), "not a layr project: {}", src.display());
    let dst = std::path::absolute(dst)?;
    ensure!(!dst.starts_with(&src), "the target is inside the source: {}", dst.display());
    let parent = dst.parent().context("the target has no parent folder")?;
    ensure!(parent.is_dir(), "the parent of the target does not exist: {}", parent.display());
    let ancestor = existing_ancestor(&dst)?;
    ensure!(
        fs::metadata(&src)?.dev() != fs::metadata(&ancestor)?.dev(),
        "the source and the target are on the same file system; rename the project instead"
    );
    prepare_target(&dst)?;
    let need = storage::copy_need(&src)?;
    // After prepare_target: a removed partial copy no longer exists.
    let ancestor = existing_ancestor(&dst)?;
    let free = free_bytes(&ancestor)?;
    ensure!(need <= free, "the copy needs {need} bytes, {free} bytes are free on {}", ancestor.display());

    if !dst.exists() {
        fs::create_dir(&dst).with_context(|| format!("create {}", dst.display()))?;
    }
    File::create(dst.join(MARKER)).context("mark the copy as unfinished")?.sync_all()?;
    let mut copier = storage::Copier::new(&src, &dst)?;
    let mut sink = Sink { progress: &mut progress, counts: Counts::default() };
    walk(&mut copier, &src, &dst, &mut Vec::new(), &mut sink)?;
    copier.finish()?;
    let copy_ms = started.elapsed().as_millis();
    // A failed verification leaves the target marked: the next call removes and copies again.
    let verified = Instant::now();
    compare_copy(&src, &dst).context("the copy does not match the source")?;
    let verify_ms = verified.elapsed().as_millis();
    fs::remove_file(dst.join(MARKER)).context("finish the copy")?;
    File::open(&dst)?.sync_all()?;
    Ok(CopyReport {
        states: sink.counts.states,
        deltas: sink.counts.deltas,
        lines: sink.counts.lines,
        overlays: sink.counts.overlays,
        need,
        copy_ms,
        verify_ms,
    })
}

fn existing_ancestor(path: &Path) -> Result<PathBuf> {
    let mut at = path;
    loop {
        if at.exists() {
            return Ok(at.to_path_buf());
        }
        at = at.parent().context("no existing ancestor of the target")?;
    }
}

fn free_bytes(path: &Path) -> Result<u64> {
    let st = rustix::fs::statvfs(path).with_context(|| format!("statvfs {}", path.display()))?;
    Ok(st.f_bavail.saturating_mul(st.f_frsize))
}

/// Refuse a target that holds data. A partial copy (with `MARKER`) is removed.
fn prepare_target(dst: &Path) -> Result<()> {
    match fs::symlink_metadata(dst) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
        Ok(m) if !m.is_dir() => bail!("the target exists and is not a folder: {}", dst.display()),
        Ok(_) if dst.join(MARKER).exists() => storage::delete_tree(dst),
        Ok(_) => {
            ensure!(fs::read_dir(dst)?.next().is_none(), "the target is not empty: {}", dst.display());
            Ok(())
        }
    }
}

/// The plain extents of a file as (logical offset, physical address, length), for comparing its
/// content. None when the file has none (empty, inline or encoded data) or the file system does
/// not report extents.
#[cfg(target_os = "linux")]
pub(crate) fn plain_extents(file: &File) -> Option<Vec<(u64, u64, u64)>> {
    const LAST: u32 = 0x1;
    const SHARED: u32 = 0x2000;
    let extents = crate::sys::fiemap_full(file).ok()?;
    if extents.is_empty() || extents.iter().any(|e| e.3 & !(LAST | SHARED) != 0) {
        return None;
    }
    Some(extents.iter().map(|&(logical, physical, len, _)| (logical, physical, len)).collect())
}

/// Copy the folder `src` (a project or a part of one) into `dst`, entry by entry.
fn walk(c: &mut storage::Copier, src: &Path, dst: &Path, rel: &mut Vec<String>, out: &mut Sink) -> Result<()> {
    let mut entries = fs::read_dir(src)
        .with_context(|| format!("read {}", src.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        if rel.as_slice() == ["meta"] && name == TMP {
            continue;
        }
        let (s, d) = (src.join(e.file_name()), dst.join(e.file_name()));
        let m = fs::symlink_metadata(&s).with_context(|| format!("stat {}", s.display()))?;
        rel.push(name);
        let result = entry(c, &s, &d, &m, rel, out);
        rel.pop();
        result?;
    }
    Ok(())
}

fn entry(c: &mut storage::Copier, s: &Path, d: &Path, m: &fs::Metadata, rel: &mut Vec<String>, out: &mut Sink) -> Result<()> {
    if let Some(kind) = unit(rel) {
        out.step(kind, rel);
    }
    if m.is_dir() {
        if is_database(rel) {
            out.step(Kind::Database, rel);
            c.shell_dir(s, d)?;
            c.dir_close(s, d, m)?;
            return vacuum_into(&s.join("layr.sqlite"), &d.join("layr.sqlite"));
        }
        if c.native_subvolume(s) {
            return c.subvolume(s, d);
        }
        c.shell_dir(s, d)?;
        walk(c, s, d, rel, out)?;
        c.dir_close(s, d, m)
    } else if m.is_file() {
        if is_old_database(rel) {
            return vacuum_into(s, d);
        }
        // Write-ahead log files belong to the database copy above.
        if rel.len() >= 2 && rel[rel.len() - 2] == "meta" && rel.last().is_some_and(|n| n.starts_with("layr.sqlite-")) {
            return Ok(());
        }
        c.file(s, d, m)
    } else if m.file_type().is_symlink() {
        c.symlink(s, d, m)
    } else if m.file_type().is_fifo() || m.file_type().is_socket() || m.file_type().is_block_device() || m.file_type().is_char_device() {
        bail!("cannot copy special file {}", s.display())
    } else {
        bail!("cannot copy {}", s.display())
    }
}

/// Write a consistent copy of a database. The source is opened read-only; its log files stay.
fn vacuum_into(src: &Path, dst: &Path) -> Result<()> {
    let conn = rusqlite::Connection::open_with_flags(src, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("open {}", src.display()))?;
    conn.execute("VACUUM INTO ?1", [dst.to_string_lossy().as_ref()])
        .with_context(|| format!("copy database {}", src.display()))?;
    Ok(())
}

/// Check that `dst` is a complete copy of `src`: the same entries, the same content in every
/// folder (states, thin layers, lines, Git store, cache overlays), and the same database rows
/// (records count, newest record, line heads, states). The source is only read.
pub fn verify_copy(src: &Path, dst: &Path) -> Result<()> {
    ensure!(!dst.join(MARKER).exists(), "the copy is not finished: {} exists", dst.join(MARKER).display());
    compare_copy(src, dst)
}

/// The comparison of `verify_copy`, also run by `copy_project` before the marker is removed.
fn compare_copy(src: &Path, dst: &Path) -> Result<()> {
    let mut rel = Vec::new();
    let mut stats = Stats::default();
    compare_dir(src, dst, &mut rel, &mut stats).with_context(|| format!("verify {}", dst.display()))?;
    ensure!(stats.databases > 0, "the source has no database");
    compare_contents(&stats.files).with_context(|| format!("verify {}", dst.display()))?;
    storage::verify_registry(src, dst).with_context(|| format!("verify the registry of {}", dst.display()))?;
    Ok(())
}

#[derive(Default)]
struct Stats {
    /// The regular files of the source and the copy (with their path in the project), whose content the walk
    /// leaves to `compare_contents`.
    files: Vec<(PathBuf, PathBuf, String)>,
    databases: usize,
}

fn compare_dir(s: &Path, d: &Path, rel: &mut Vec<String>, stats: &mut Stats) -> Result<()> {
    let names = |p: &Path| -> Result<BTreeSet<String>> {
        let mut set = BTreeSet::new();
        for e in fs::read_dir(p).with_context(|| format!("read {}", p.display()))? {
            let name = e?.file_name().to_string_lossy().into_owned();
            let skipped = (rel.is_empty() && name == MARKER)
                || (rel.as_slice() == ["meta"] && (name == TMP || name.starts_with("layr.sqlite-")));
            if !skipped {
                set.insert(name);
            }
        }
        Ok(set)
    };
    let (sn, dn) = (names(s)?, names(d)?);
    ensure!(sn == dn, "different entries in {}: only in source {:?}, only in copy {:?}", rel.join("/"), sn.difference(&dn).collect::<Vec<_>>(), dn.difference(&sn).collect::<Vec<_>>());
    for name in sn {
        let (sp, dp) = (s.join(&name), d.join(&name));
        rel.push(name.clone());
        let r = compare_entry(&sp, &dp, rel, stats);
        rel.pop();
        r?;
    }
    Ok(())
}

fn compare_entry(s: &Path, d: &Path, rel: &mut Vec<String>, stats: &mut Stats) -> Result<()> {
    let sm = fs::symlink_metadata(s)?;
    let dm = fs::symlink_metadata(d)?;
    if is_database(rel) {
        ensure!(dm.is_dir(), "{} is not a folder in the copy", rel.join("/"));
        ensure!(sm.uid() == dm.uid() && sm.gid() == dm.gid(), "{}: owner differs", rel.join("/"));
        stats.databases += 1;
        return compare_db(&s.join("layr.sqlite"), &d.join("layr.sqlite"), &rel.join("/"));
    }
    if is_old_database(rel) {
        stats.databases += 1;
        return compare_db(s, d, &rel.join("/"));
    }
    ensure!(sm.file_type() == dm.file_type(), "{}: different type", rel.join("/"));
    ensure!(
        sm.uid() == dm.uid() && sm.gid() == dm.gid(),
        "{}: owner {}:{} differs from {}:{}",
        rel.join("/"),
        dm.uid(),
        dm.gid(),
        sm.uid(),
        sm.gid()
    );
    ensure!(
        sm.file_type().is_symlink() || sm.mode() & 0o7777 == dm.mode() & 0o7777,
        "{}: mode {:o} differs from {:o}",
        rel.join("/"),
        dm.mode() & 0o7777,
        sm.mode() & 0o7777
    );
    if sm.is_dir() {
        compare_dir(s, d, rel, stats)
    } else if sm.file_type().is_symlink() {
        ensure!(fs::read_link(s)? == fs::read_link(d)?, "{}: different link", rel.join("/"));
        Ok(())
    } else {
        ensure!(sm.is_file(), "{}: special file", rel.join("/"));
        ensure!(sm.len() == dm.len(), "{}: size {} differs from {}", rel.join("/"), sm.len(), dm.len());
        ensure!(sm.mode() & 0o7777 == dm.mode() & 0o7777, "{}: mode differs", rel.join("/"));
        stats.files.push((s.to_path_buf(), d.to_path_buf(), rel.join("/")));
        Ok(())
    }
}

/// Compare file contents using reusable buffers for each parallel job.
fn compare_contents(files: &[(PathBuf, PathBuf, String)]) -> Result<()> {
    crate::pools::run(crate::pools::Priority::Foreground, || {
        use rayon::prelude::*;
        files.par_iter().map_init(
            || (vec![0u8; 1 << 20], vec![0u8; 1 << 20]),
            |(a, b), (s, d, rel)| {
                let same = same_content(s, d, a, b)?;
                if same { Ok(()) } else { Err(anyhow::anyhow!("{rel}: content differs")) }
            },
        ).collect::<Result<()>>()
    })?
}

/// Whether the files `s` and `d` hold the same bytes. `a` and `b` are the read buffers (of one size).
fn same_content(s: &Path, d: &Path, a: &mut [u8], b: &mut [u8]) -> Result<bool> {
    let mut fs_ = File::open(s).with_context(|| format!("open {}", s.display()))?;
    let mut fd = File::open(d).with_context(|| format!("open {}", d.display()))?;
    loop {
        let n = read_full(&mut fs_, a).with_context(|| format!("read {}", s.display()))?;
        let m = read_full(&mut fd, b).with_context(|| format!("read {}", d.display()))?;
        if n != m || a[..n] != b[..m] {
            return Ok(false);
        }
        if n < a.len() {
            return Ok(true);
        }
    }
}

/// Reads until `buf` is full or the file ends. Returns the bytes read.
fn read_full(f: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

fn compare_db(s: &Path, d: &Path, name: &str) -> Result<()> {
    let open = |p: &Path| {
        rusqlite::Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .with_context(|| format!("open database {}", p.display()))
    };
    let (sc, dc) = (open(s)?, open(d)?);
    let tables = |c: &rusqlite::Connection| -> Result<Vec<String>> {
        let mut stmt = c.prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    };
    let (st, dt) = (tables(&sc)?, tables(&dc)?);
    ensure!(st == dt, "{name}: different tables");
    for table in &st {
        let (sd, sn) = table_digest(&sc, table)?;
        let (dd, dn) = table_digest(&dc, table)?;
        ensure!(sn == dn, "{name}: table {table} has {sn} rows in the source and {dn} in the copy");
        ensure!(sd == dd, "{name}: table {table} differs");
    }
    let newest = |c: &rusqlite::Connection| -> Result<Option<String>> {
        Ok(c.query_row("SELECT id FROM records ORDER BY rowid DESC LIMIT 1", [], |r| r.get::<_, String>(0)).ok())
    };
    ensure!(newest(&sc)? == newest(&dc)?, "{name}: the newest record differs");
    Ok(())
}

/// A digest of all rows of a table, independent of row order, and the row count.
fn table_digest(c: &rusqlite::Connection, table: &str) -> Result<([u8; 32], usize)> {
    let mut stmt = c.prepare(&format!("SELECT * FROM \"{}\"", table.replace('"', "\"\"")))?;
    let columns = stmt.column_count();
    let mut rows = stmt
        .query_map([], |r| {
            let mut line = String::new();
            for i in 0..columns {
                let v: Value = r.get(i)?;
                line.push_str(&format!("{v:?}\u{1f}"));
            }
            Ok(line)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.sort();
    let mut hasher = FastCryptographic::new();
    for row in &rows {
        hasher.update(row.as_bytes());
        hasher.update(b"\n");
    }
    Ok((*hasher.finalize().as_bytes(), rows.len()))
}

/// Hidden test entry points: `__project-copy <src> <dst>`, `__project-verify <src> <dst>` and
/// `__project-space <folder>`. The copy hook `LAYR_TEST_COPY_EXIT_AFTER=N` exits after N units,
/// to test an interrupted copy (a unit is a state, a thin layer, a line, an overlay or the database). Returns the exit code when `argv` names one of them.
pub fn hidden(argv: &[String]) -> Option<i32> {
    let name = argv.first()?.as_str();
    if !matches!(name, "__project-copy" | "__project-verify" | "__project-space") {
        return None;
    }
    let result: Result<()> = (|| match (name, argv.len()) {
        ("__project-copy", 3) => {
            let exit_after: Option<usize> = std::env::var("LAYR_TEST_COPY_EXIT_AFTER").ok().and_then(|v| v.parse().ok());
            let mut seen = 0usize;
            let report = copy_project(Path::new(&argv[1]), Path::new(&argv[2]), |step| {
                seen += 1;
                if exit_after == Some(seen) {
                    eprintln!("test hook: exit after {seen} units (last: {})", step.name);
                    std::process::exit(3);
                }
            })?;
            println!("{}", serde_json::to_string(&report)?);
            Ok(())
        }
        ("__project-verify", 3) => {
            verify_copy(Path::new(&argv[1]), Path::new(&argv[2]))?;
            println!("verified");
            Ok(())
        }
        ("__project-space", 2) => {
            let need = storage::copy_need(Path::new(&argv[1]))?;
            let total = storage::copy_total(Path::new(&argv[1]))?;
            println!("{}", serde_json::json!({ "need": need, "total": total }));
            Ok(())
        }
        _ => bail!("invalid arguments for {name}"),
    })();
    Some(match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("layr: {e:#}");
            1
        }
    })
}
