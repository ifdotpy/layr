//! Commands that change or report the working content of a line: status, add, rm, mv, apply,
//! commit, restore, reset, stash, clean.

use super::{exit, here, matches, Here};
use crate::args::{parse, Spec};
use crate::changes::{self, Change};
use crate::ctx::Ctx;
use crate::diff;
use crate::fsutil;
use crate::ids;
use crate::model::{Conflict, Line, Record, StateKind};
use crate::radar::Overlap;
use crate::repo::{conflict_open, skip_path, snap, Repo, Snap, Status, Temp};
use crate::revs;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

fn xy(c: &Change) -> char {
    match (&c.old, &c.new) {
        (None, Some(_)) => 'A',
        (Some(_), None) => 'D',
        _ if c.old_path.is_some() => 'R',
        (Some(o), Some(n)) if o.ftype != n.ftype => 'T',
        _ => 'M',
    }
}

fn conflict_code(kind: &str) -> &'static str {
    match kind {
        "add/add" => "AA",
        "modify/delete" => "UD",
        "delete/modify" => "DU",
        "rename/delete" => "AU",
        _ => "UU",
    }
}

/// The status that `layr status` shows: the comparison of the working folder, minus the files of
/// quiet overlays. `filtered` is true for a view that shows only the caller's layers.
fn shown_status(ctx: &Ctx, h: &Here, line: &Line, filtered: bool) -> Result<Status> {
    #[cfg(target_os = "macos")]
    let mut st = {
        let _t = crate::trace::span("status compare");
        if filtered {
            h.repo.snapshot_status(ctx, line)?
        } else {
            // The comparison runs without the store lock, as on Linux: it can last many seconds
            // and the saved status is published by an atomic rename. A save, add or reset that
            // swaps the staging folder meanwhile makes it fail; then it runs again under the lock.
            match h.repo.status_report(line) {
                Ok(st) => st,
                Err(_) => {
                    let _lock = h.repo.p.lock()?;
                    let line = h.repo.line(&line.id)?;
                    h.repo.status_report(&line)?
                }
            }
        }
    };
    #[cfg(not(target_os = "macos"))]
    let mut st = if filtered { h.repo.snapshot_status(ctx, line)? } else { h.repo.status(line)?.0 };
    // The files of quiet overlays are not shown (docs/cache-overlays.md, section 4.1).
    let quiet = {
        let _t = crate::trace::span("status quiet paths");
        h.repo.quiet_paths(line)?
    };
    st.drop_paths(&quiet);
    Ok(st)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StatusOverlaps {
    Include,
    Skip,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StatusIgnored {
    Include,
    Omit,
}

struct StatusOptions {
    overlaps: StatusOverlaps,
    ignored: StatusIgnored,
}

#[derive(serde::Serialize)]
struct StatusChange<'a> {
    old_path: &'a Option<String>,
    path: &'a str,
    status: char,
}

impl<'a> From<&'a Change> for StatusChange<'a> {
    fn from(c: &'a Change) -> Self {
        Self { path: &c.path, old_path: &c.old_path, status: xy(c) }
    }
}

#[derive(serde::Serialize)]
struct StatusConflict<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<&'a str>,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    ours: Option<&'a str>,
    path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    theirs: Option<&'a str>,
}

impl<'a> From<&'a Conflict> for StatusConflict<'a> {
    fn from(c: &'a Conflict) -> Self {
        Self { base: c.base.as_deref(), kind: &c.kind, ours: c.ours.as_deref(), path: &c.path, theirs: c.theirs.as_deref() }
    }
}

#[derive(serde::Serialize)]
struct StatusOverlap<'a> {
    changed_at: i64,
    line: &'a str,
    owner: u32,
    path: &'a str,
}

impl<'a> From<&'a Overlap> for StatusOverlap<'a> {
    fn from(o: &'a Overlap) -> Self {
        Self { changed_at: o.changed_at, line: &o.line, owner: o.owner, path: &o.path }
    }
}

// Keep all DTO fields in alphabetical order to preserve the former json! map's byte order.
#[derive(serde::Serialize)]
struct StatusJson<'a> {
    conflicts: Vec<StatusConflict<'a>>,
    head: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    ignored: Option<Vec<&'a String>>,
    line: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    overlaps: Option<Vec<StatusOverlap<'a>>>,
    staged: Vec<StatusChange<'a>>,
    unstaged: Vec<StatusChange<'a>>,
    untracked: Vec<&'a String>,
}

impl<'a> StatusJson<'a> {
    fn new(line: &'a Line, st: &'a Status, specs: &[String], overlaps: &'a [Overlap], options: &StatusOptions) -> Self {
        Self {
            line: &line.name,
            head: &line.head,
            staged: st.staged.iter().filter(|c| matches(specs, &c.path)).map(StatusChange::from).collect(),
            unstaged: st.unstaged.iter().filter(|c| matches(specs, &c.path)).map(StatusChange::from).collect(),
            untracked: st.untracked.iter().filter(|p| matches(specs, p)).collect(),
            // Ignore classification is shared with the other status entries. Keep it intact;
            // the opt-out avoids constructing and serializing its output list.
            ignored: (options.ignored == StatusIgnored::Include)
                .then(|| st.ignored.iter().filter(|p| matches(specs, p)).collect()),
            conflicts: st.conflicts.iter().filter(|c| matches(specs, &c.path)).map(StatusConflict::from).collect(),
            overlaps: (options.overlaps == StatusOverlaps::Include).then(|| overlaps.iter().map(StatusOverlap::from).collect()),
        }
    }
}

pub fn status(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .flag("-s|--short")
        .flag("-b|--branch")
        .optional("--porcelain")
        .optional("-u|--untracked-files")
        .optional("--ignored")
        .flag("-z")
        .flag("--long")
        .flag("-v|--verbose")
        .flag("--no-renames")
        .flag("--no-overlaps")
        .flag("--no-ignored")
        .flag("--json");
    let a = parse(&spec, args)?;
    let options = StatusOptions {
        overlaps: if a.has("--no-overlaps") { StatusOverlaps::Skip } else { StatusOverlaps::Include },
        ignored: if a.has("--no-ignored") { StatusIgnored::Omit } else { StatusIgnored::Include },
    };
    let h = here(ctx)?;
    let line = h.line()?.clone();
    #[cfg(target_os = "macos")]
    let line = h.repo.line(&line.id)?;
    // Every virtual-line change is included in the next save.
    let view = crate::views::of(&h.repo, &line);
    let filtered = if view.is_some() { None } else { h.repo.filtered_view(ctx, &line)? };
    let specs = h.specs(&a.rest())?;
    let st = match &view {
        Some(v) => crate::views::status(&h.repo, &line, v)?,
        None => shown_status(ctx, &h, &line, filtered.is_some())?,
    };
    let overlaps = if options.overlaps == StatusOverlaps::Include
        && view.is_none()
        && filtered.is_none()
        && (a.has("--json") || (!a.has("-s") && !a.has("--porcelain")))
    {
        let _t = crate::trace::span("status overlaps");
        h.repo.overlaps(ctx, &line)?.into_iter().filter(|o| matches(&specs, &o.path)).collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if a.has("--json") {
        let output = StatusJson::new(&line, &st, &specs, &overlaps, &options);
        outln!(ctx, "{}", serde_json::to_string(&output)?);
        return Ok(0);
    }
    let porcelain = a.has("--porcelain");
    let short = a.has("-s") || porcelain;
    let untracked_mode = a.get("-u").map(|s| if s.is_empty() { "all" } else { s }).unwrap_or("normal").to_string();
    let show_ignored = options.ignored == StatusIgnored::Include && a.has("--ignored");
    let nul = a.has("-z");
    let end = if nul { "\0" } else { "\n" };
    let path_of = |p: &str| -> String {
        if porcelain || nul {
            p.to_string()
        } else {
            h.rel(p)
        }
    };
    let conflicted: BTreeSet<String> = st.conflicts.iter().map(|c| c.path.clone()).collect();

    if short {
        if a.has("-b") {
            out!(ctx, "## {}{}", line.name, end);
        }
        let mut rows: BTreeMap<String, (char, char, Option<String>)> = BTreeMap::new();
        for c in &st.staged {
            if !matches(&specs, &c.path) {
                continue;
            }
            rows.insert(c.path.clone(), (xy(c), ' ', c.old_path.clone()));
        }
        for c in &st.unstaged {
            if !matches(&specs, &c.path) {
                continue;
            }
            let e = rows.entry(c.path.clone()).or_insert((' ', ' ', None));
            e.1 = xy(c);
        }
        for c in &st.conflicts {
            if !matches(&specs, &c.path) {
                continue;
            }
            let code = conflict_code(&c.kind);
            let b = code.as_bytes();
            rows.insert(c.path.clone(), (b[0] as char, b[1] as char, None));
        }
        for (p, (x, y, from)) in &rows {
            match from {
                Some(f) if nul => out!(ctx, "{x}{y} {}\0{}\0", path_of(p), path_of(f)),
                Some(f) => out!(ctx, "{x}{y} {} -> {}\n", path_of(f), path_of(p)),
                None => out!(ctx, "{x}{y} {}{}", path_of(p), end),
            }
        }
        if untracked_mode != "no" {
            for u in untracked_list(&st, &untracked_mode) {
                if matches(&specs, u.trim_end_matches('/')) {
                    out!(ctx, "?? {}{}", path_of(&u), end);
                }
            }
        }
        if show_ignored {
            for i in &st.ignored {
                if matches(&specs, i) {
                    out!(ctx, "!! {}{}", path_of(i), end);
                }
            }
        }
        return Ok(0);
    }

    outln!(ctx, "On line {}", line.name);
    if let Some(s) = super::followcmd::status_line(&h.repo, &line)? {
        outln!(ctx, "{s}");
    }
    if !overlaps.is_empty() {
        outln!(ctx, "Overlaps with other live lines:");
        for overlap in &overlaps {
            outln!(
                ctx,
                "\t{} ({}, owner {}, changed_at {})",
                h.rel(&overlap.path),
                overlap.line,
                overlap.owner,
                overlap.changed_at
            );
        }
        outln!(ctx);
    }
    let head = h.repo.state(&line.head)?;
    if !head.conflicts.is_empty() && !st.conflicts.is_empty() {
        outln!(ctx, "You have unmerged paths.\n  (fix conflicts and run \"layr save -m <message>\")\n");
    }
    let staged: Vec<&Change> = st.staged.iter().filter(|c| matches(&specs, &c.path) && !conflicted.contains(&c.path)).collect();
    let unstaged: Vec<&Change> =
        st.unstaged.iter().filter(|c| matches(&specs, &c.path) && !conflicted.contains(&c.path)).collect();
    let untracked: Vec<String> = if untracked_mode == "no" {
        vec![]
    } else {
        untracked_list(&st, &untracked_mode).into_iter().filter(|u| matches(&specs, u.trim_end_matches('/'))).collect()
    };
    let label = |c: &Change| -> String {
        match xy(c) {
            'A' => format!("new file:   {}", h.rel(&c.path)),
            'D' => format!("deleted:    {}", h.rel(&c.path)),
            'R' => format!("renamed:    {} -> {}", h.rel(c.old_path.as_deref().unwrap_or("")), h.rel(&c.path)),
            'T' => format!("typechange: {}", h.rel(&c.path)),
            _ => format!("modified:   {}", h.rel(&c.path)),
        }
    };
    if !staged.is_empty() {
        outln!(ctx, "Changes to be saved:\n  (use \"layr restore --staged <file>...\" to unstage)");
        for c in &staged {
            outln!(ctx, "\t{}", label(c));
        }
        outln!(ctx);
    }
    let moves: Vec<(String, String)> = h.repo.pending_moves(&line).into_iter().filter(|(p, _)| matches(&specs, p)).collect();
    if !moves.is_empty() {
        outln!(ctx, "Moves to overlays (the next state):");
        for (p, o) in &moves {
            outln!(ctx, "\t{o}: {}", h.rel(p));
        }
        outln!(ctx);
    }
    if !st.conflicts.is_empty() {
        outln!(ctx, "Unmerged paths:\n  (fix the files, then \"layr add <file>...\" and \"layr save -m <message>\")");
        for c in &st.conflicts {
            if matches(&specs, &c.path) {
                let what = match c.kind.as_str() {
                    "content" => "both modified:",
                    "add/add" => "both added:   ",
                    "modify/delete" => "deleted by them:",
                    "delete/modify" => "deleted by us:",
                    _ => "conflict:     ",
                };
                outln!(ctx, "\t{what}   {} ({})", h.rel(&c.path), c.kind);
            }
        }
        outln!(ctx);
    }
    if !unstaged.is_empty() {
        outln!(ctx, "Changes not staged for a state:\n  (use \"layr add <file>...\" to update what will be saved)\n  (use \"layr restore <file>...\" to discard changes in working directory)");
        for c in &unstaged {
            outln!(ctx, "\t{}", label(c));
        }
        outln!(ctx);
    }
    if !untracked.is_empty() {
        outln!(ctx, "Untracked files:\n  (use \"layr add <file>...\" to include in what will be saved)");
        for u in &untracked {
            outln!(ctx, "\t{}", h.rel(u));
        }
        outln!(ctx);
    }
    if show_ignored && !st.ignored.is_empty() {
        outln!(ctx, "Ignored files:");
        for i in &st.ignored {
            outln!(ctx, "\t{}", h.rel(i));
        }
        outln!(ctx);
    }
    if staged.is_empty() {
        if !unstaged.is_empty() {
            outln!(ctx, "no changes added to a state (use \"layr add\" and/or \"layr save -a -m <message>\")");
        } else if !untracked.is_empty() {
            outln!(ctx, "nothing added to a state but untracked files present (use \"layr add\" to track)");
        } else if st.conflicts.is_empty() {
            outln!(ctx, "nothing to save, working tree clean");
        }
    }
    Ok(0)
}

fn untracked_list(st: &Status, mode: &str) -> Vec<String> {
    if mode == "all" {
        return st.untracked.clone();
    }
    let mut out: BTreeSet<String> = BTreeSet::new();
    for f in &st.untracked {
        match st.untracked_dirs.iter().find(|d| f.starts_with(&format!("{d}/"))) {
            Some(d) => {
                out.insert(format!("{d}/"));
            }
            None => {
                out.insert(f.clone());
            }
        }
    }
    out.into_iter().collect()
}

/// The locks of a command that keeps the project lock for all of its records (`begin_change`).
/// The line lock is taken first.
pub struct ChangeLocks(pub crate::store::LineLock, pub crate::store::Lock);

/// Lock the line and the project and save the working content (snapshot rule 2). For the commands
/// whose records are written under the project lock they hold: stash, clean, rm and mv.
pub fn begin_change(ctx: &Ctx, repo: &Repo, line: &Line) -> Result<(ChangeLocks, Option<String>)> {
    begin(ctx, repo, line, crate::repo::LineOp::Direct)
}

/// `begin_change` for another kind of change.
pub fn begin(ctx: &Ctx, repo: &Repo, line: &Line, op: crate::repo::LineOp) -> Result<(ChangeLocks, Option<String>)> {
    repo.authorize_line(ctx, line, op)?;
    let line_lock = repo.line_lock(line)?;
    let line = repo.line(&line.id)?;
    repo.authorize_line(ctx, &line, op)?;
    // The copy of changed files goes before the project lock; under it the save takes it by a rename.
    repo.prepare_save(&line)?;
    let lock = repo.p.lock()?;
    let w = repo.working_state(ctx, &line, "before a layr command")?;
    Ok((ChangeLocks(line_lock, lock), w))
}

/// `begin` for a command that writes the working folder or moves the head of the line. It holds the
/// line lock for the whole command (the caller keeps the returned guard) and takes the project lock
/// only for the automatic save of the working folder: its copy is made before, under the line lock.
pub fn begin_line(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    op: crate::repo::LineOp,
) -> Result<(crate::store::LineLock, Option<String>)> {
    repo.authorize_line(ctx, line, op)?;
    let line_lock = repo.line_lock(line)?;
    let line = repo.line(&line.id)?;
    repo.authorize_line(ctx, &line, op)?;
    repo.prepare_save(&line)?;
    let w = {
        let _commit = repo.p.lock()?;
        repo.working_state(ctx, &line, "before a layr command")?
    };
    Ok((line_lock, w))
}

/// Stage paths: move them from the working snapshot (a private copy of this command) into the
/// staging line. A file that moved
/// in the working folder (same inode number) is moved in the staging line too, so states keep
/// the inode and btrfs reports the move.
pub fn stage_paths(repo: &Repo, line: &Line, scan: &Path, paths: &BTreeSet<String>) -> Result<()> {
    let stage = repo.ensure_stage(line)?;
    #[cfg(target_os = "macos")]
    if crate::thin::is_thin(&stage) {
        // A file with several names keeps them only in a complete index.
        if !crate::thin::any_linked(&repo.working(line), paths)
            && !paths.iter().any(|p| fsutil::lmeta(scan, p).is_some_and(|m| m.is_dir()))
        {
            let (index, _) = repo.index_source(line)?;
            return crate::thin::stage_entries(&index, scan, paths, repo.own(line));
        }
        let stage = repo.ensure_full_stage(line)?;
        return stage_into(repo, line, &stage, scan, paths);
    }
    stage_into(repo, line, &stage, scan, paths)
}

/// `stage_paths` into a given copy of the index.
fn stage_into(repo: &Repo, line: &Line, stage: &Path, scan: &Path, paths: &BTreeSet<String>) -> Result<()> {
    let stage = stage.to_path_buf();
    let own = repo.own(line);
    let mut gone: std::collections::HashMap<Vec<u8>, String> = std::collections::HashMap::new();
    for p in paths {
        if fsutil::lmeta(scan, p).is_none() {
            if let Some(m) = fsutil::lmeta(&stage, p) {
                if m.is_file() || m.ftype == fsutil::FType::Symlink {
                    gone.insert(crate::storage::file_identity(&stage.join(p))?, p.clone());
                }
            }
        }
    }
    for p in paths {
        if let (Some(_), None) = (fsutil::lmeta(scan, p), fsutil::lmeta(&stage, p)) {
            if let Some(from) = gone.remove(&crate::storage::file_identity(&scan.join(p))?) {
                let t = crate::tree::Tree::open(&stage)?;
                t.mkdirs_for(p, own)?;
                t.rename(&from, p)?;
            }
        }
    }
    let mut first_link: std::collections::HashMap<(u64, u64), String> = std::collections::HashMap::new();
    for p in paths {
        if fsutil::lmeta(scan, p).is_some() {
            use std::os::unix::fs::MetadataExt;
            let metadata = std::fs::symlink_metadata(fsutil::safe_join(scan, p)?)?;
            if metadata.is_file() && metadata.nlink() > 1 {
                let key = (metadata.dev(), metadata.ino());
                if let Some(first) = first_link.get(&key) {
                    let tree = crate::tree::Tree::open(&stage)?;
                    tree.replace_hard_link(first, p)?;
                    continue;
                }
                first_link.insert(key, p.clone());
            }
            fsutil::copy_index_entry(scan, p, &stage, p, own)?;
        } else if fsutil::lmeta(&stage, p).is_some() {
            fsutil::remove_entry(&stage, p)?;
            prune_empty(&stage, p);
        }
    }
    Ok(())
}

/// Include the tracked names of changed hard-linked files in `save -a`.
#[cfg(target_os = "linux")]
fn include_linked_tracked(
    scan: &Path,
    index: &(impl fsutil::Src + ?Sized),
    paths: &mut BTreeSet<String>,
    quiet: &BTreeSet<String>,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let mut keys = std::collections::HashSet::new();
    for p in paths.iter() {
        let path = fsutil::safe_join(scan, p)?;
        if let Ok(m) = std::fs::symlink_metadata(path) {
            if m.is_file() && m.nlink() > 1 {
                keys.insert((m.dev(), m.ino()));
            }
        }
    }
    if keys.is_empty() {
        return Ok(());
    }
    let dev = std::fs::symlink_metadata(scan)?.dev();
    let mut dirs = vec![std::path::PathBuf::new()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(scan.join(&dir))? {
            let entry = entry?;
            let rel = dir.join(entry.file_name());
            if rel.to_str().is_some_and(|p| skip_path(p) || quiet.contains(p)) {
                continue;
            }
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if metadata.dev() != dev {
                continue;
            }
            if metadata.is_dir() {
                #[cfg(feature = "overlayfs")]
                if crate::storage::is_subvolume(&entry.path()) {
                    continue;
                }
                dirs.push(rel);
            } else if metadata.is_file() && keys.contains(&(metadata.dev(), metadata.ino())) {
                let rel = rel.to_str().ok_or_else(|| anyhow::anyhow!("a hard link has a non-UTF-8 path"))?;
                if fsutil::lmeta(index, rel).is_some() {
                    paths.insert(rel.to_string());
                }
            }
        }
    }
    Ok(())
}

/// Remove parent folders that became empty (states hold files, as git trees do).
fn prune_empty(root: &Path, rel: &str) {
    if let Ok(t) = crate::tree::Tree::open(root) {
        prune_empty_in(&t, rel);
    }
}

pub(super) fn prune_empty_in(t: &crate::tree::Tree, rel: &str) {
    let mut comps: Vec<&str> = rel.split('/').collect();
    comps.pop();
    while !comps.is_empty() && t.rmdir(&comps.join("/")) {
        comps.pop();
    }
}

/// Resolve add pathspecs against one consistent view before modifying the index.
fn select_add(
    h: &Here<'_>,
    a: &crate::args::Parsed,
    specs: &[String],
    st: &Status,
    scan: &Path,
    index: &(impl fsutil::Src + ?Sized),
    quiet: &BTreeSet<String>,
) -> Result<(BTreeSet<String>, Vec<String>)> {
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for c in &st.unstaged {
        if matches(specs, &c.path) {
            paths.insert(c.path.clone());
        }
    }
    if !a.has("-u") {
        for u in &st.untracked {
            if matches(specs, u) {
                paths.insert(u.clone());
            }
        }
    }
    // Ignored paths named explicitly.
    let mut ignored_named = Vec::new();
    for s in specs {
        if s.is_empty() {
            continue;
        }
        let in_scan = fsutil::lmeta(scan, s).is_some();
        let in_index = fsutil::lmeta(index, s).is_some();
        let matched = paths.iter().any(|p| p == s || p.starts_with(&format!("{s}/"))) || s.contains(['*', '?']);
        if !matched {
            if st.ignored.iter().any(|i| i == s || i.starts_with(&format!("{s}/"))) {
                if a.has("-f") {
                    for i in st.ignored.iter().filter(|i| *i == s || i.starts_with(&format!("{s}/"))) {
                        paths.insert(i.clone());
                    }
                } else {
                    ignored_named.push(h.rel(s));
                }
            } else if !in_scan && !in_index {
                return Err(exit(128, format!("fatal: pathspec '{}' did not match any files", h.rel(s))));
            }
        }
    }
    // A quiet overlay's files never go into the index: a named save takes them as they are.
    paths.retain(|p| !quiet.contains(p));
    #[cfg(target_os = "linux")]
    {
        include_linked_tracked(scan, index, &mut paths, quiet)?;
        paths.retain(|p| matches(specs, p));
    }
    Ok((paths, ignored_named))
}

/// The paths that `add` with these pathspecs can touch, with the ignore files that decide
/// their state: every parent folder's. `None` (the whole tree) when a spec names the root.
#[cfg(target_os = "macos")]
fn add_scan_limit(specs: &[String]) -> Option<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for s in specs {
        // A pattern is read from its last literal folder.
        let lit = match s.find(['*', '?', '[']) {
            Some(i) => s[..i].rfind('/').map(|j| &s[..j]).unwrap_or(""),
            None => s.as_str(),
        };
        if lit.is_empty() {
            return None;
        }
        out.insert(lit.to_string());
        let mut dir = String::new();
        let mut parts: Vec<&str> = lit.split('/').collect();
        parts.pop();
        for name in std::iter::once("").chain(parts.into_iter()) {
            if !name.is_empty() {
                if !dir.is_empty() {
                    dir.push('/');
                }
                dir.push_str(name);
            }
            for f in [".gitignore", ".layrignore"] {
                out.insert(if dir.is_empty() { f.to_string() } else { format!("{dir}/{f}") });
            }
        }
    }
    Some(out)
}

pub fn add(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .flag("-A|--all")
        .flag("-u|--update")
        .flag("-f|--force")
        .flag("-n|--dry-run")
        .flag("-v|--verbose")
        .flag("--no-all")
        .flag("-N|--intent-to-add")
        .flag("-p|--patch")
        .flag("-i|--interactive");
    let a = parse(&spec, args)?;
    if a.has("-p") || a.has("-i") {
        return Err(exit(
            1,
            "interactive add is not supported: write the hunks to a patch file and run 'layr apply --cached <file>'",
        ));
    }
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let mut specs = h.specs(&a.rest())?;
    if specs.is_empty() && !(a.has("-A") || a.has("-u")) {
        return Err(exit(0, "Nothing specified, nothing added.\nhint: Maybe you wanted to say 'layr add .'?"));
    }
    if specs.is_empty() {
        specs = vec![String::new()];
    }
    h.repo.authorize_line(ctx, &line, crate::repo::LineOp::Direct)?;
    // The stage and the working folder of the line: no other command of the line writes them meanwhile.
    let _line_lock = h.repo.line_lock(&line)?;
    // The status and the copy of the named paths read the whole working folder: seconds in a
    // large line. They run before the store lock; under it the copy is used when the head and
    // the staging content did not change, else the read runs again under the lock.
    #[cfg(target_os = "macos")]
    let limit = add_scan_limit(&specs);
    #[cfg(target_os = "macos")]
    let read = |line: &Line| -> Result<Option<(crate::repo::Temp, BTreeSet<String>, Vec<String>)>> {
        let quiet = h.repo.quiet_paths(line)?;
        let work = h.repo.working(line);
        let (index, stage) = h.repo.index_source(line)?;
        let mut roots = vec![work.as_path()];
        if let Some(s) = &stage {
            roots.push(s.as_path());
        }
        let read = || {
            let st = h.repo.status_for_add(line, &work, &index, limit.as_ref())?;
            let (paths, ignored) = select_add(&h, &a, &specs, &st, &work, &index, &quiet)?;
            let scan = crate::repo::Temp { path: h.repo.p.scan_dir().join(crate::store::temp_name()) };
            crate::storage::create_subvolume(&scan.path)?;
            crate::storage::copy_selected(&work, &scan.path, &paths)?;
            Ok((scan, paths, ignored))
        };
        match &limit {
            Some(l) => crate::storage::stable_read_paths(&roots, l, read),
            None => crate::storage::stable_read(&roots, read),
        }
    };
    #[cfg(target_os = "macos")]
    let early = {
        let token = h.repo.index_token(&line)?;
        read(&line)?.map(|r| (token, r))
    };
    let _lock = h.repo.p.lock()?;
    let line = h.repo.line(&line.id)?;
    let quiet = h.repo.quiet_paths(&line)?;
    #[cfg(target_os = "macos")]
    let prepared = match early {
        Some((token, r)) if token == h.repo.index_token(&line)? => Some(r),
        _ => read(&line)?,
    };
    #[cfg(not(target_os = "macos"))]
    let prepared: Option<(crate::repo::Temp, BTreeSet<String>, Vec<String>)> = None;
    let (scan, paths, ignored_named) = match prepared {
        Some(v) => v,
        None => {
            let (st, scan, _it, index) = h.repo.status_light(&line)?;
            let (paths, ignored) = select_add(&h, &a, &specs, &st, &scan.path, &index, &quiet)?;
            (scan, paths, ignored)
        }
    };
    if a.has("-n") {
        for p in &paths {
            let what = if fsutil::lmeta(&scan.path, p).is_some() { "add" } else { "remove" };
            outln!(ctx, "{what} '{}'", h.rel(p));
        }
        return Ok(0);
    }
    // Staging moves the entries out of the copy: name them first.
    let verbose: Vec<(&str, &String)> = if a.has("-v") {
        paths.iter().map(|p| (if fsutil::lmeta(&scan.path, p).is_some() { "add" } else { "remove" }, p)).collect()
    } else {
        Vec::new()
    };
    stage_paths(&h.repo, &line, &scan.path, &paths)?;
    for (what, p) in verbose {
        outln!(ctx, "{what} '{}'", h.rel(p));
    }
    if !ignored_named.is_empty() {
        let mut m = String::from("The following paths are ignored by one of your .gitignore files:\n");
        for i in ignored_named {
            m.push_str(&format!("{i}\n"));
        }
        m.push_str("hint: Use -f if you really want to add them.");
        return Err(exit(1, m));
    }
    Ok(0)
}

pub fn rm(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("--cached").flag("-r").flag("-f|--force").flag("-q|--quiet").flag("-n|--dry-run");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let specs = h.specs(&a.rest())?;
    if specs.is_empty() {
        return Err(exit(128, "usage: layr rm [--cached] [-r] [-f] <path>..."));
    }
    let (_lock, _w) = begin_change(ctx, &h.repo, &line)?;
    let line = h.repo.line(&line.id)?;
    let w = h.repo.working(&line);
    // A thin index takes the removals as one change of its top layer; no complete index.
    #[cfg(target_os = "macos")]
    let thin = {
        let st = h.repo.ensure_stage(&line)?;
        if crate::thin::is_thin(&st) {
            Some(h.repo.index_source(&line)?.0)
        } else {
            None
        }
    };
    #[cfg(not(target_os = "macos"))]
    let thin: Option<Snap> = None;
    let full = if thin.is_none() { Some(h.repo.ensure_full_stage(&line)?) } else { None };
    let index: &dyn fsutil::Src = match (&thin, &full) {
        (Some(t), _) => t,
        (None, Some(f)) => f,
        (None, None) => unreachable!("one index exists"),
    };
    // Check every path first, then change the index, then the working folder.
    let mut plan: Vec<(String, Vec<String>)> = Vec::new();
    for s in &specs {
        match fsutil::lmeta(index, s) {
            None => return Err(exit(128, format!("fatal: pathspec '{}' did not match any files", h.rel(s)))),
            Some(m) if m.is_dir() && !a.has("-r") => {
                return Err(exit(128, format!("fatal: not removing '{}' recursively without -r", h.rel(s))))
            }
            _ => {}
        }
        let mut files = Vec::new();
        if a.has("-n") {
            files.push(s.clone());
        } else if fsutil::lmeta(index, s).map(|m| m.is_dir()).unwrap_or(false) {
            fsutil::walk(index, s, &mut |rel, m| {
                if !m.is_dir() {
                    files.push(rel.to_string());
                }
                Ok(true)
            })?;
        } else {
            files.push(s.clone());
        }
        plan.push((s.clone(), files));
    }
    // Git's rule: a staged version that differs from both the head and the working file exists
    // nowhere else, so removing it needs -f.
    if !a.has("-f") {
        let head = h.repo.head_src(&line)?;
        let mut lost = Vec::new();
        for (_, files) in &plan {
            for f in files {
                if fsutil::lmeta(index, f).is_some()
                    && !fsutil::same_entry(index, f, &head, f, None)?
                    && !fsutil::same_entry(index, f, &w, f, None)?
                {
                    lost.push(h.rel(f));
                }
            }
        }
        if !lost.is_empty() {
            let list: String = lost.iter().map(|f| format!("    {f}\n")).collect();
            let many = if lost.len() == 1 { "file has" } else { "files have" };
            return Err(exit(
                1,
                format!("error: the following {many} staged content different from both the\nfile and the HEAD:\n{list}(use -f to force removal)"),
            ));
        }
    }
    let mut removed = Vec::new();
    if a.has("-n") {
        removed.extend(plan.into_iter().flat_map(|(_, files)| files));
    } else {
        match (&thin, &full) {
            #[cfg(target_os = "macos")]
            (Some(t), _) => {
                let none = crate::repo::Temp { path: h.repo.p.scan_dir().join(crate::store::temp_name()) };
                std::fs::create_dir_all(&none.path)?;
                let paths: BTreeSet<String> = plan.iter().map(|(s, _)| s.clone()).collect();
                crate::thin::stage_entries(t, &none.path, &paths, h.repo.own(&line))?;
            }
            (_, Some(f)) => {
                for (s, _) in &plan {
                    fsutil::remove_entry(f, s)?;
                }
            }
            _ => {}
        }
        for (s, files) in plan {
            if !a.has("--cached") {
                for f in &files {
                    fsutil::remove_entry(&w, f)?;
                }
                if crate::tree::Tree::open(&w)?.stat(&s).map(|m| m.is_dir()).unwrap_or(false) {
                    crate::tree::Tree::open(&w)?.rmdir(&s);
                }
            }
            removed.extend(files);
        }
    }
    if !a.has("-q") {
        for r in removed {
            outln!(ctx, "rm '{}'", h.rel(&r));
        }
    }
    Ok(0)
}

pub fn mv(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("-f|--force").flag("-n|--dry-run").flag("-k").flag("-v|--verbose");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let rest = h.specs(&a.rest())?;
    if rest.len() < 2 {
        return Err(exit(128, "usage: layr mv <source>... <destination>"));
    }
    let (_lock, _w) = begin_change(ctx, &h.repo, &line)?;
    let line = h.repo.line(&line.id)?;
    let w = h.repo.working(&line);
    let dst = rest.last().unwrap().clone();
    let dst_is_dir = fsutil::lmeta(&w, &dst).map(|m| m.is_dir()).unwrap_or(false);
    let mut moves = Vec::new();
    for src in &rest[..rest.len() - 1] {
        if fsutil::lmeta(&w, src).is_none() {
            return Err(exit(128, format!("fatal: bad source, source={}, destination={}", h.rel(src), h.rel(&dst))));
        }
        let name = src.rsplit('/').next().unwrap_or(src);
        let target = if dst_is_dir {
            if dst.is_empty() {
                name.to_string()
            } else {
                format!("{dst}/{name}")
            }
        } else {
            dst.clone()
        };
        if fsutil::lmeta(&w, &target).is_some() && !a.has("-f") {
            return Err(exit(128, format!("fatal: destination exists, source={}, destination={}", h.rel(src), h.rel(&target))));
        }
        moves.push((src.clone(), target));
    }
    if a.has("-n") {
        for (s, t) in &moves {
            outln!(ctx, "Renaming {} to {}", h.rel(s), h.rel(t));
        }
        return Ok(0);
    }
    let tree = crate::tree::Tree::open(&w)?;
    for (s, t) in &moves {
        tree.mkdirs_for(t, h.repo.own(&line))?;
        tree.rename(s, t)?;
    }
    // Stage both sides of each move. On APFS with a thin index, a private copy of the moved
    // entries is enough, and the index removes the old folder itself.
    #[cfg(target_os = "macos")]
    let thin = crate::thin::is_thin(&h.repo.ensure_stage(&line)?);
    #[cfg(not(target_os = "macos"))]
    let thin = false;
    let mut paths = BTreeSet::new();
    for (s, t) in &moves {
        paths.insert(s.clone());
        paths.insert(t.clone());
        if fsutil::lmeta(&w, t).map(|m| m.is_dir()).unwrap_or(false) {
            fsutil::walk(&w, t, &mut |rel, _| {
                paths.insert(rel.to_string());
                Ok(true)
            })?;
        }
    }
    let scan = if thin {
        let copy = crate::repo::Temp { path: h.repo.p.scan_dir().join(crate::store::temp_name()) };
        crate::storage::create_subvolume(&copy.path)?;
        #[cfg(target_os = "macos")]
        crate::storage::copy_selected(&w, &copy.path, &paths)?;
        copy
    } else {
        let scan = h.repo.scan(&line)?;
        for (s, _) in &moves {
            let stage = h.repo.ensure_full_stage(&line)?;
            if fsutil::lmeta(&stage, s).map(|m| m.is_dir()).unwrap_or(false) {
                fsutil::remove_entry(&stage, s)?;
            }
        }
        scan
    };
    stage_paths(&h.repo, &line, &scan.path, &paths)?;
    if a.has("-v") {
        for (s, t) in &moves {
            outln!(ctx, "Renaming {} to {}", h.rel(s), h.rel(t));
        }
    }
    Ok(0)
}

/// Read a file named by the caller with the caller's permissions.
/// Read at most `limit` bytes; more is an error (the service holds the input in memory).
pub fn read_limited(r: &mut dyn std::io::Read, limit: u64, what: &str) -> Result<Vec<u8>> {
    let mut v = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(r, limit + 1), &mut v)?;
    if v.len() as u64 > limit {
        anyhow::bail!("{what} is larger than {} MiB", limit >> 20);
    }
    Ok(v)
}

/// A file named by the caller (a patch, a message, records), read with the caller's
/// permissions, or stdin for `-`.
pub fn read_caller_file(ctx: &Ctx, path: &str, limit: u64) -> Result<Vec<u8>> {
    if path == "-" {
        return read_limited(&mut std::io::stdin(), limit, "the input");
    }
    let p = if path.starts_with('/') { std::path::PathBuf::from(path) } else { ctx.cwd.join(path) };
    let mut f = crate::privs::as_caller_fs(ctx, || Ok(std::fs::File::open(&p)?))?;
    read_limited(&mut f, limit, path)
}

pub fn apply(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .flag("--cached")
        .flag("--index")
        .flag("--check")
        .flag("-R|--reverse")
        .flag("-v|--verbose")
        .flag("--stat")
        .flag("--numstat")
        .flag("--3way|-3")
        .value("-p")
        .flag("--recount")
        .flag("--unidiff-zero");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let files = a.rest();
    let patch: Vec<u8> = if files.is_empty() || files[0] == "-" {
        read_caller_file(ctx, "-", fsutil::MAX_TEXT)?
    } else {
        let mut v = Vec::new();
        for f in &files {
            v.extend(read_caller_file(ctx, f, fsutil::MAX_TEXT)?);
        }
        v
    };
    let mut flags: Vec<String> = Vec::new();
    for f in ["-R", "--recount", "--unidiff-zero", "-v"] {
        if a.has(f) {
            flags.push(f.into());
        }
    }
    if let Some(p) = a.get("-p") {
        flags.push(format!("-p{p}"));
    }
    if a.has("--stat") || a.has("--numstat") {
        let mode = if a.has("--stat") { "--stat" } else { "--numstat" };
        let out = run_git_apply(ctx, Path::new("/"), &patch, &[mode.to_string()], ctx.caller.uid)?;
        ctx.print(&out);
        return Ok(0);
    }
    let to_stage = a.has("--cached") || a.has("--index");
    let to_work = !a.has("--cached");
    let (_line_lock, _w) = begin_line(ctx, &h.repo, &line, crate::repo::LineOp::Direct)?;
    let line = h.repo.line(&line.id)?;
    let own = h.repo.own(&line);
    // The staging line is private to the service (applied as root, then given to the owner);
    // the working folder belongs to the owner, so the patch is applied there as the owner.
    let mut targets: Vec<(std::path::PathBuf, u32)> = Vec::new();
    if to_stage {
        targets.push((h.repo.ensure_full_stage(&line)?, rustix::process::geteuid().as_raw()));
    }
    if to_work {
        targets.push((h.repo.working(&line), line.owner));
    }
    // Check all targets first, then apply.
    for (t, uid) in &targets {
        let mut f = flags.clone();
        f.push("--check".into());
        run_git_apply(ctx, t, &patch, &f, *uid)?;
    }
    if a.has("--check") {
        return Ok(0);
    }
    let touched = patch_paths(&patch);
    for (t, uid) in &targets {
        run_git_apply(ctx, t, &patch, &flags, *uid)?;
        if *uid == 0 {
            if let Some((u, g)) = own {
                for p in &touched {
                    if let Ok(full) = fsutil::safe_join(t, p) {
                        if full.symlink_metadata().is_ok() {
                            fsutil::chown_nofollow(&full, u, g)?;
                        }
                    }
                }
            }
        }
    }
    Ok(0)
}

fn patch_paths(patch: &[u8]) -> Vec<String> {
    let mut v = Vec::new();
    for l in String::from_utf8_lossy(patch).lines() {
        for pre in ["+++ b/", "--- a/"] {
            if let Some(p) = l.strip_prefix(pre) {
                v.push(p.trim().to_string());
            }
        }
    }
    v.sort();
    v.dedup();
    v
}

fn run_git_apply(ctx: &Ctx, dir: &Path, patch: &[u8], flags: &[String], uid: u32) -> Result<String> {
    use std::io::Write;
    let mut c = std::process::Command::new("git");
    c.arg("apply").args(flags).arg("-").current_dir(dir);
    // A service may apply the staging tree as root. Never take executable or
    // config paths from a different caller when doing that privileged work.
    let mut env = if uid == ctx.caller.uid && uid == rustix::process::geteuid().as_raw() {
        crate::privs::pick_env(ctx, &["PATH", "GIT_CONFIG_GLOBAL"])
    } else {
        Vec::new()
    };
    env.extend([
        ("GIT_CEILING_DIRECTORIES".to_string(), dir.parent().unwrap_or(Path::new("/")).to_string_lossy().into_owned()),
        ("GIT_DIR".to_string(), "/nonexistent-layr".to_string()),
    ]);
    crate::privs::command_as(&mut c, uid, &env)?;
    c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut ch = c.spawn()?;
    ch.stdin.take().unwrap().write_all(patch)?;
    let out = ch.wait_with_output()?;
    if !out.status.success() {
        return Err(exit(1, String::from_utf8_lossy(&out.stderr).trim().replace("/nonexistent-layr", "").to_string()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A named save (`save -m`, with or without `-a`). Once it made a state, it names the files that
/// the save left out. The warning runs after the store lock is released.
pub fn commit(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let code = commit_state(ctx, args);
    // Copies of build-output folders made before the lock that no part took (nothing to save, or
    // an error) go to the trash.
    #[cfg(target_os = "macos")]
    {
        let left = crate::storage::take_prepared();
        if !left.is_empty() {
            if let Ok(h) = here(ctx) {
                for p in left {
                    let _ = h.repo.p.trash(&p);
                }
            }
        }
    }
    let code = code?;
    if code == 0 {
        if let Err(e) = warn_left_out(ctx) {
            eprintln!("warning: cannot list the files this save left out: {e:#}");
        }
    }
    Ok(code)
}

/// The options of a named save that its checks use.
#[derive(Clone, Copy, Default)]
pub(crate) struct NamedOpts {
    allow_empty: bool,
    allow_markers: bool,
    amend: bool,
}

fn named_opts(a: &crate::args::Parsed) -> NamedOpts {
    NamedOpts { allow_empty: a.has("--allow-empty"), allow_markers: a.has("--allow-conflict-markers"), amend: a.has("--amend") }
}

/// The checks of a named save that read only the changes and the trees: nothing to save (None), the
/// conflict markers, the conflicts that stay open (into `state`), and the secret check. Returns the
/// staged changes and the conflicts that the save clears. On APFS they run before the project lock,
/// on the clone, which does not change.
pub(crate) fn named_checks(
    ctx: &Ctx,
    repo: &Repo,
    head: &crate::model::StateRec,
    state: &mut crate::model::StateRec,
    head_root: &crate::repo::Snap,
    new_root: &crate::repo::Snap,
    line: &Line,
    all_changes: &[Change],
    opts: NamedOpts,
) -> Result<Option<(Vec<Change>, Vec<crate::model::Conflict>)>> {
    let staged: Vec<Change> = all_changes.iter().filter(|c| !skip_path(&c.path)).cloned().collect();
    if staged.is_empty() && !opts.allow_empty && !opts.amend {
        return Ok(None);
    }
    // A file with a full set of conflict markers is no result of a merge: refuse it unless asked.
    if !opts.allow_markers {
        let _t = crate::trace::span("conflict markers");
        let mut marked = Vec::new();
        for c in &staged {
            if !c.new.as_ref().is_some_and(|m| m.is_file() && m.size <= fsutil::MAX_TEXT) {
                continue;
            }
            if crate::repo::has_markers(&fsutil::read_file(new_root, &c.path)?) {
                marked.push(c.path.clone());
            }
        }
        if !marked.is_empty() {
            return Err(exit(
                1,
                format!("error: conflict markers in {}: resolve them, or save with --allow-conflict-markers.", marked.join(", ")),
            ));
        }
    }
    // Content conflicts stay listed while the committed file has markers; the commit clears the other kinds.
    let mut cleared = Vec::new();
    for c in &head.conflicts {
        if (c.kind == "content" || c.kind == "add/add") && conflict_open(new_root, c)? {
            state.conflicts.push(c.clone());
        } else {
            cleared.push(c.clone());
        }
    }
    let secrets = crate::trace::span("secret check");
    crate::secrets::check_changes(ctx, repo, head_root, new_root, &line.head, all_changes)?;
    drop(secrets);
    Ok(Some((staged, cleared)))
}

/// The part of a named save that runs before the project lock on APFS: the clone of the index, the
/// state that describes it, and the changes against the head.
#[cfg(target_os = "macos")]
struct NamedPrepared<'p, 's> {
    head_id: String,
    head_root: crate::repo::Snap,
    pending: Option<crate::apfs_commit::PendingCommit<'p, 's>>,
    state: crate::model::StateRec,
    new_root: crate::repo::Snap,
    all_changes: Vec<Change>,
    /// The staged changes and the conflicts that the save clears; None: nothing to save.
    checks: Option<(Vec<Change>, Vec<crate::model::Conflict>)>,
}

/// Clone the index of `line` and compare the clone with the head. Call with the line lock held and
/// without the project lock. A failure drops the clone (`PendingCommit`) and changes nothing.
#[cfg(target_os = "macos")]
fn prepare_named_save<'p, 's>(
    ctx: &Ctx,
    repo: &'p Repo<'s>,
    line: &Line,
    head: &crate::model::StateRec,
    message: &str,
    opts: NamedOpts,
) -> Result<NamedPrepared<'p, 's>> {
    let amend = opts.amend;
    let head_root = repo.state_src(&line.head)?;
    let parents = if amend { head.parents.clone() } else { vec![line.head.clone()] };
    let stage = repo.current_stage(line)?;
    let pending = match &stage {
        Some(_) => Some(crate::apfs_commit::PendingCommit::prepare(&repo.p, &line.id)?),
        None => None,
    };
    if pending.is_some() {
        crate::store::failpoint("pending-cloned");
        crate::store::test_pause();
    }
    let mut state = match &pending {
        Some(pc) => repo.state_record(ctx, pc.id.clone(), StateKind::Commit, parents, message, Some(line), pc.subvol.clone()),
        None => repo.same_state(ctx, &line.head, StateKind::Commit, parents, message, Some(line))?,
    };
    if amend {
        state.change_id = head.change_id.clone();
    }
    let new_root = match &pending {
        // A thin clone is a layer over the head, as the stage was (`stage_tree`).
        Some(pc) if pc.thin => crate::thin::Layered::over(&pc.tree, repo.head_src(line)?)?,
        Some(pc) => crate::repo::snap(&pc.tree),
        None => repo.state_src(&state.id)?,
    };
    // An attribute-only change that `add` staged is a change of the state.
    let all_changes = crate::storage::with_stored_compare(|| changes::file_changes(&head_root, &new_root))?;
    let checks = named_checks(ctx, repo, head, &mut state, &head_root, &new_root, line, &all_changes, opts)?;
    // The clone is durable before the project lock: under the lock only the rename and the record.
    #[cfg(target_os = "macos")]
    if let (Some(pc), Some(_)) = (&pending, &checks) {
        pc.sync_tree()?;
        crate::store::failpoint("pending-synced");
    }
    Ok(NamedPrepared { head_id: line.head.clone(), head_root, pending, state, new_root, all_changes, checks })
}

fn commit_state(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .value("-m|--message")
        .value("-F|--file")
        .flag("-a|--all")
        .flag("--amend")
        .flag("--allow-empty")
        .flag("--no-verify|-n")
        .flag("-q|--quiet")
        .flag("-v|--verbose")
        .flag("--no-edit")
        .flag("--allow-conflict-markers")
        .value("--author")
        .flag("-s|--signoff");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let mut message: String = a.all("-m").join("\n\n");
    if let Some(f) = a.get("-F") {
        message = String::from_utf8_lossy(&read_caller_file(ctx, f, fsutil::MAX_TEXT)?).into_owned();
    }
    let head = h.repo.state(&line.head)?;
    let stacked = h.repo.is_stacked(&line)?;
    if a.has("--amend") {
        if stacked {
            return Err(exit(1, "error: --amend is not available in a line with overlays yet; make a new state"));
        }
        // The amended state replaces the head: the line's history changes.
        h.repo.authorize_line(ctx, &line, crate::repo::LineOp::Rewrite)?;
    }
    if a.has("--amend") && message.is_empty() {
        message = head.message.clone();
    }
    let message = message.trim_end().to_string();
    if message.is_empty() {
        return Err(exit(1, "Cannot save a state with an empty message (use -m <message>)."));
    }
    // A virtual line saves every change of its view (docs/views.md).
    #[cfg(target_os = "macos")]
    if let Some(v) = crate::views::of(&h.repo, &line) {
        if ["--amend", "--allow-empty", "--author", "-s", "--allow-conflict-markers"].iter().any(|f| a.has(f)) {
            return Err(exit(2, "a virtual line takes only 'layr save -m <message>' for now (docs/views.md)"));
        }
        return crate::views::save(ctx, &h, &line, &v, &message, a.has("-q"));
    }
    #[cfg(target_os = "linux")]
    let virtual_save = {
        crate::views::ensure_no_pending_move(&h.repo, &line)?;
        crate::views::of(&h.repo, &line).is_some()
    };
    #[cfg(not(target_os = "linux"))]
    let virtual_save = false;
    if virtual_save && ["--amend", "--allow-empty", "--author", "-s", "--allow-conflict-markers"].iter().any(|f| a.has(f)) {
        return Err(exit(2, "a virtual line takes only 'layr save -m <message>' for now (docs/views.md)"));
    }
    // In a line with overlays the owner commits; a change of the shared set also needs the line's
    // rule (commit_layers checks it), so a protected line takes commits of its overlays only.
    let op = if stacked { crate::repo::LineOp::Save } else { crate::repo::LineOp::Direct };
    h.repo.authorize_line(ctx, &line, op)?;
    // The head of the line and its working folder: the line lock holds from here to the end of the
    // save. A refused save fails before this wait.
    let _line_lock = h.repo.line_lock(&line)?;
    // -a stages the changed tracked files like `add -u`, which reads the working folder before
    // the store lock (a status of a large line took tens of seconds under it).
    #[cfg(target_os = "macos")]
    if a.has("-a") && !stacked {
        add(ctx, &["-u".to_string()])?;
    }
    // Cache folders become layers, and their copies are made, before the lock (rule 90).
    if !stacked {
        h.repo.restore_caches(&line)?;
        h.repo.prepare_named_cache_parts(&line)?;
    }
    let nothing = |h: &Here, line: &Line| -> Result<i32> {
        let (mut st2, _s2, _i2, _x2) = h.repo.status(line)?;
        st2.drop_paths(&h.repo.quiet_paths(line)?);
        outln!(ctx, "On line {}", line.name);
        if !st2.unstaged.is_empty() {
            outln!(ctx, "Changes not staged for a state:");
            for c in &st2.unstaged {
                outln!(ctx, "\tmodified:   {}", h.rel(&c.path));
            }
            outln!(ctx, "\nno changes added to a state (use \"layr add\" and/or \"layr save -a -m <message>\")");
        } else if !st2.untracked.is_empty() {
            outln!(ctx, "nothing added to a state but untracked files present (use \"layr add\" to track)");
        } else {
            outln!(ctx, "nothing to save, working tree clean");
        }
        Ok(1)
    };
    // `save -m` on APFS: the index is cloned and the new tree compared before the project lock
    // (issue #194). The line lock, taken above, keeps the stage and the head unchanged to the record.
    #[cfg(target_os = "macos")]
    let mut prepared =
        if stacked { None } else { Some(prepare_named_save(ctx, &h.repo, &line, &head, &message, named_opts(&a))?) };
    #[cfg(target_os = "macos")]
    if prepared.as_ref().is_some_and(|np| np.checks.is_none()) {
        prepared = None;
        return nothing(&h, &line);
    }
    let lock = h.repo.p.lock()?;
    let line = h.repo.line(&line.id)?;
    #[cfg(target_os = "macos")]
    if let Some(np) = &prepared {
        if line.head != np.head_id {
            return Err(exit(
                1,
                format!(
                    "line {} moved during the command (head {} -> {}); run it again",
                    line.name,
                    ids::short(&np.head_id),
                    ids::short(&line.head)
                ),
            ));
        }
    }
    crate::gitbridge::check_git_head(&h.repo, &line)?;
    crate::repo::recover_cache_replacements(&h.repo.working(&line))?;
    if (a.has("-a") || virtual_save) && !stacked && !cfg!(target_os = "macos") {
        let (st, scan, _it, index) = h.repo.status_light(&line)?;
        let mut paths: BTreeSet<String> = st.unstaged.iter().map(|c| c.path.clone()).collect();
        if virtual_save {
            paths.extend(st.untracked.iter().cloned());
        }
        #[cfg(target_os = "linux")]
        include_linked_tracked(&scan.path, &index, &mut paths, &h.repo.quiet_paths(&line)?)?;
        if !paths.is_empty() {
            stage_paths(&h.repo, &line, &scan.path, &paths)?;
        }
    }
    let author = a
        .get("--author")
        .and_then(|au| au.split_once('<').map(|(n, e)| (n.trim().to_string(), e.trim_end_matches('>').trim().to_string())));
    if stacked {
        return commit_stacked(ctx, &h, &line, &message, a.has("-a"), a.has("--allow-empty"), a.has("-q"), author, nothing);
    }
    // The new state is a snapshot of the staging line (or the head when nothing is staged);
    // its changes against the head say whether there is anything to commit.
    #[cfg(not(target_os = "macos"))]
    let (head_root, mut state, new_root, all_changes) = {
        let head_root = h.repo.state_src(&line.head)?;
        let parents = if a.has("--amend") { head.parents.clone() } else { vec![line.head.clone()] };
        let stage = h.repo.current_stage(&line)?;
        let mut keep = Vec::new();
        let src = match stage { Some(path) => path, None => h.repo.complete(head_root.clone(), &mut keep)? };
        let mut state = h.repo.new_state(ctx, &src, StateKind::Commit, parents, &message, Some(&line))?;
        if a.has("--amend") {
            state.change_id = head.change_id.clone();
        }
        let new_root = match h.repo.state_src(&state.id) {
            Ok(r) => r,
            Err(e) => {
                h.repo.discard_states(std::slice::from_ref(&state));
                return Err(e);
            }
        };
        let all_changes: Vec<Change> = match changes::file_changes(&head_root, &new_root) {
            Ok(v) => v,
            Err(e) => {
                h.repo.discard_states(std::slice::from_ref(&state));
                return Err(e);
            }
        };
        (head_root, state, new_root, all_changes)
    };
    // On APFS the state, its tree and the changes were made before the lock.
    #[cfg(target_os = "macos")]
    let NamedPrepared { head_root, mut pending, mut state, checks, .. } =
        prepared.take().ok_or_else(|| anyhow::anyhow!("internal error: named save was not prepared"))?;
    #[cfg(target_os = "macos")]
    let (staged, cleared) = checks.ok_or_else(|| anyhow::anyhow!("internal error: named save has no checks"))?;
    #[cfg(not(target_os = "macos"))]
    let (staged, cleared) =
        match named_checks(ctx, &h.repo, &head, &mut state, &head_root, &new_root, &line, &all_changes, named_opts(&a))? {
            Some(checks) => checks,
            None => {
                h.repo.discard_states(std::slice::from_ref(&state));
                return nothing(&h, &line);
            }
        };
    if let Some((n, e)) = author {
        state.author.name = n;
        state.author.email = e;
    }
    // The part of each build-output overlay (docs/overlays.md, section 1a, slice 6): a folder that
    // changed gets a new part saved with this state; the state names the parts.
    for (overlay, part) in h.repo.save_named_cache_parts(ctx, &line, &state.id) {
        state.parts.entry(overlay).or_insert(part);
    }
    // `named_checks` checked the conflicts and the secrets of this content.
    let mut data = serde_json::json!({"line": line.id});
    if a.has("--amend") {
        data["version"] = serde_json::to_value(crate::model::VersionOperation {
            state: state.id.clone(),
            kind: "amend".into(),
            source: head.id.clone(),
            target_line: Some(line.id.clone()),
            target_head: line.head.clone(),
        })?;
    }
    #[cfg(target_os = "macos")]
    if let Some(pc) = pending.as_mut() {
        let _t = crate::trace::span("publish clone");
        if !pc.publish()? {
            return Err(exit(
                1,
                format!("error: the staging content of line {} changed during the save; run it again", line.name),
            ));
        }
    }
    crate::store::failpoint("pending-renamed");
    crate::store::failpoint("before-record");
    let rec = {
        let _t = crate::trace::span("move head");
        h.repo.move_head(ctx, "commit", &line, &state.id, vec![state.clone()], None, data)
    };
    rec?;
    // The history limit of each overlay counts the parts of the new head: a part of the old head
    // that no state keeps now goes.
    let named: std::collections::BTreeSet<&String> = state.parts.keys().chain(head.parts.keys()).collect();
    for name in named {
        h.repo.after_overlay_move(ctx, name);
    }
    crate::store::failpoint("after-record");
    {
        let _t = crate::trace::span("drop stage");
        h.repo.drop_stage(&line)?;
    }
    crate::store::failpoint("after-drop");
    // The output reads the trees: it runs after the project lock is released. The journal of the
    // pending save goes after it too: the record is the commit, and the recovery of another
    // command leaves the journal of a live process alone (and finishes it after a crash).
    drop(lock);
    #[cfg(target_os = "macos")]
    if let Some(pc) = pending.take() {
        let _t = crate::trace::span("finish save");
        pc.finish()?;
    }
    if !a.has("-q") {
        let root = if parents_empty(&state) { " (root-state)" } else { "" };
        outln!(ctx, "[{}{} {}] {}", line.name, root, ids::short(&state.id), message.lines().next().unwrap_or(""));
        // `publish` moved the clone into the state folder: read the saved state.
        #[cfg(target_os = "macos")]
        let new_root = h.repo.state_src(&state.id)?;
        let staged = changes::with_renames(&head_root, &new_root, staged);
        print_summary(ctx, &head_root, &new_root, &staged)?;
        for c in &cleared {
            outln!(ctx, " resolved conflict: {} ({})", c.path, c.kind);
        }
        if !state.conflicts.is_empty() {
            outln!(ctx, "warning: {} file(s) still have conflict markers; see 'layr status'", state.conflicts.len());
        }
    }
    Ok(0)
}

/// Prints on stderr the files that a named save left out: untracked files, and tracked files whose
/// working content differs from the new state. Prints nothing when none are left out.
fn warn_left_out(ctx: &Ctx) -> Result<()> {
    let h = here(ctx)?;
    let line = h.line()?.clone();
    #[cfg(target_os = "macos")]
    let line = h.repo.line(&line.id)?;
    // A virtual line saves every change: nothing is left out.
    #[cfg(target_os = "macos")]
    if crate::views::of(&h.repo, &line).is_some() {
        return Ok(());
    }
    let filtered = h.repo.filtered_view(ctx, &line)?.is_some();
    let st = shown_status(ctx, &h, &line, filtered)?;
    let conflicted: BTreeSet<String> = st.conflicts.iter().map(|c| c.path.clone()).collect();
    let untracked: Vec<String> = untracked_list(&st, "normal").iter().map(|p| h.rel(p)).collect();
    let mut modified: Vec<String> =
        st.unstaged.iter().filter(|c| !conflicted.contains(&c.path)).map(|c| h.rel(&c.path)).collect();
    modified.sort();
    for msg in [left_out_line("untracked", &untracked), left_out_line("modified, not added", &modified)].into_iter().flatten() {
        eprintln!("{msg}");
    }
    Ok(())
}

/// One warning line: the first 20 paths, then the number of the others.
fn left_out_line(kind: &str, paths: &[String]) -> Option<String> {
    const SHOWN: usize = 20;
    if paths.is_empty() {
        return None;
    }
    let mut text = paths.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", ");
    if paths.len() > SHOWN {
        text.push_str(&format!(", and {} more", paths.len() - SHOWN));
    }
    Some(format!("not saved ({kind}): {text}"))
}

/// The summary of a commit: the short statistics and the created, deleted and moved files.
fn print_summary(ctx: &Ctx, old: &Snap, new: &Snap, list: &[Change]) -> Result<()> {
    print_fds(ctx, &diff::file_diffs(old, new, list)?);
    Ok(())
}

fn print_fds(ctx: &Ctx, fds: &[diff::FileDiff]) {
    ctx.print(&diff::shortstat(fds));
    for f in fds {
        match (&f.old, &f.new) {
            (None, Some(n)) => outln!(ctx, " create mode {:06o} {}", n.mode, n.path),
            (Some(o), None) => outln!(ctx, " delete mode {:06o} {}", o.mode, o.path),
            (Some(o), Some(n)) if o.path != n.path => outln!(ctx, " rename {} => {} ({}%)", o.path, n.path, f.similarity()),
            _ => {}
        }
    }
}

/// A commit in a line with overlays: the change list split by layer, a new state for each
/// changed layer (docs/overlays.md, section 3.2).
#[allow(clippy::too_many_arguments)]
fn commit_stacked(
    ctx: &Ctx,
    h: &Here,
    line: &Line,
    message: &str,
    all: bool,
    allow_empty: bool,
    quiet: bool,
    author: Option<(String, String)>,
    nothing: impl Fn(&Here, &Line) -> Result<i32>,
) -> Result<i32> {
    let repo = &h.repo;
    let layers = repo.layers(line)?;
    repo.check_stack(line, &layers)?;
    let base = repo.head_tree(line)?;
    let src = match repo.current_stage(line)? {
        Some(s) => s,
        None => base.clone(),
    };
    // The committed tree: the index, with `-a` the changed files too (staged into a copy, so a
    // refused commit leaves the index as it was). It becomes the line's base.
    // The files of `overlay move` come from the working folder as they are now.
    let pending = repo.pending_moves(line);
    // A quiet overlay's files come from the working folder too (they never enter the index).
    let quiet_files = repo.quiet_paths(line)?;
    let new_tree = if all || !pending.is_empty() || !quiet_files.is_empty() {
        let (st, scan, _it, _index) = repo.status(line)?;
        let mut paths: BTreeSet<String> = BTreeSet::new();
        if all {
            paths.extend(st.unstaged.iter().map(|c| c.path.clone()));
        }
        paths.extend(st.unstaged.iter().filter(|c| quiet_files.contains(&c.path)).map(|c| c.path.clone()));
        paths.extend(pending.keys().cloned());
        let work = repo.temp_work(&src)?;
        stage_into(repo, line, &work.path, &scan.path, &paths)?;
        repo.temp_snapshot(&work.path, true)?
    } else {
        repo.temp_snapshot(&src, true)?
    };
    let done = match crate::overlay::commit_layers(ctx, repo, line, &layers, new_tree, message, allow_empty, author, !quiet)? {
        Some(d) => d,
        None => return nothing(h, line),
    };
    repo.drop_stage(line)?;
    repo.set_pending_moves(line, &BTreeMap::new())?;
    if !quiet {
        let subject = message.lines().next().unwrap_or("");
        match &done.shared {
            Some(s) => outln!(ctx, "[{} {}] {subject}", line.name, ids::short(&s.id)),
            None => {
                let (n, s) = &done.overlays[0];
                outln!(ctx, "[{} overlay {n} {}] {subject}", line.name, ids::short(&s.id));
            }
        }
        for (i, (n, s)) in done.overlays.iter().enumerate() {
            if done.shared.is_some() || i > 0 {
                outln!(ctx, " overlay {n}: {}", ids::short(&s.id));
            }
        }
        print_fds(ctx, &done.fds);
    }
    Ok(0)
}

fn parents_empty(s: &crate::model::StateRec) -> bool {
    s.parents.is_empty()
}

/// Make `target` match `source` for paths matching `specs`, without touching other paths.
/// `keep_untracked`: paths absent from `known` (the previous index) are left alone.
#[allow(clippy::too_many_arguments)]
fn restore_paths(
    repo: &Repo,
    line: &Line,
    source: &Snap,
    target_view: &Path,
    target: &Path,
    specs: &[String],
    known: Option<&Snap>,
) -> Result<Vec<String>> {
    // The index keeps the times of the state; the working folder gets the current time.
    let to_work = known.is_some();
    let own = repo.own(line);
    let mut done = Vec::new();
    for c in changes::without_moves(changes::file_changes(target_view, source)?) {
        if skip_path(&c.path) || !matches(specs, &c.path) {
            continue;
        }
        if c.new.is_none() {
            if let Some(k) = known {
                if fsutil::lmeta(k, &c.path).is_none() {
                    continue;
                }
            }
            fsutil::remove_entry(target, &c.path)?;
        } else {
            if to_work {
                fsutil::copy_work_entry(source, &c.path, target, &c.path, own)?;
            } else {
                fsutil::copy_entry(source, &c.path, target, &c.path, own)?;
            }
        }
        done.push(c.path.clone());
    }
    Ok(done)
}

pub fn restore(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .flag("-S|--staged")
        .flag("-W|--worktree")
        .value("-s|--source")
        .flag("--ours")
        .flag("--theirs")
        .flag("-q|--quiet")
        .flag("--overlay")
        .flag("--no-overlay")
        .flag("--clean");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let specs = h.specs(&a.rest())?;
    if specs.is_empty() {
        return Err(exit(128, "fatal: you must specify path(s) to restore"));
    }
    let worktree = a.has("-W") || !a.has("-S");
    if a.has("--clean") && !worktree {
        return Err(exit(128, "fatal: --clean restores the working folder: use it without --staged only, or with --worktree"));
    }
    restore_impl(ctx, &h, &line, &specs, a.has("-S"), worktree, a.get("-s"), a.has("--ours"), a.has("--theirs"), a.has("--clean"))
}

#[allow(clippy::too_many_arguments)]
pub fn restore_impl(
    ctx: &Ctx,
    h: &Here,
    line: &Line,
    specs: &[String],
    staged: bool,
    worktree: bool,
    source: Option<&str>,
    ours: bool,
    theirs: bool,
    clean: bool,
) -> Result<i32> {
    // Restore changes the working folder and the stage, never the head: a protected line allows it.
    let (_line_lock, w) = begin_line(ctx, &h.repo, line, crate::repo::LineOp::Working)?;
    let line = h.repo.line(&line.id)?;
    let repo = &h.repo;
    if ours || theirs {
        let head = repo.state(&line.head)?;
        let wdir = repo.working(&line);
        let mut n = 0;
        for c in &head.conflicts {
            if !matches(&specs, &c.path) {
                continue;
            }
            let sid = if ours { c.ours.clone() } else { c.theirs.clone() };
            let sid = match sid {
                Some(s) => s,
                None => continue,
            };
            let root = repo.state_src(&sid)?;
            if fsutil::lmeta(&root, &c.path).is_some() {
                fsutil::copy_work_entry(&root, &c.path, &wdir, &c.path, repo.own(&line))?;
            } else {
                fsutil::remove_entry(&wdir, &c.path)?;
            }
            n += 1;
        }
        if n == 0 {
            return Err(exit(1, "error: no conflict matches the given paths"));
        }
        if let Err(e) = record_working_checked(ctx, repo, &line, "restore", w.clone()) {
            put_back(repo, &line, w.as_deref());
            return Err(e);
        }
        return Ok(0);
    }
    let (index, _it) = repo.index_view(&line)?;
    let head_root = repo.head_src(&line)?;
    let src_id = match source {
        Some(r) => Some(revs::resolve(repo, Some(&line), r)?),
        None => None,
    };
    // In a line with overlays a source state comes with the line's overlays in place.
    let layers = repo.layers(&line)?;
    let src_view = match &src_id {
        Some(id) if !layers.is_empty() => Some(repo.assemble(&line, id, &layers)?),
        _ => None,
    };
    let src_root = match &src_id {
        Some(_) if src_view.is_some() => snap(&src_view.as_ref().unwrap().path),
        Some(id) => repo.state_src(id)?,
        None => {
            if staged {
                head_root.clone()
            } else {
                index.clone()
            }
        }
    };
    let mut matched = false;
    if staged {
        let stage = repo.ensure_full_stage(&line)?;
        let view = repo.temp_snapshot(&stage, true)?;
        let d = restore_paths(repo, &line, &src_root, &view.path, &stage, specs, None)?;
        matched |= !d.is_empty();
    }
    if worktree {
        #[cfg(target_os = "macos")]
        let scan = repo.scan_matching(&line, specs)?;
        #[cfg(not(target_os = "macos"))]
        let scan = repo.scan(&line)?;
        let wdir = repo.working(&line);
        let d = restore_paths(repo, &line, &src_root, &scan.path, &wdir, specs, Some(&index))?;
        matched |= !d.is_empty();
        if clean {
            // `clean -fd` in the same operation: each new file the source does not have goes (ignored
            // ones stay), file by file, since a new folder may hold files of the source; then the
            // folders that this left empty and the source lacks.
            let (st, _scan, _it, _index) = repo.status(&line)?;
            let mut folders = BTreeSet::new();
            for f in st.untracked.iter().filter(|f| matches(specs, f) && fsutil::lmeta(&src_root, f).is_none()) {
                fsutil::remove_entry(&wdir, f)?;
                matched = true;
                let mut at = f.as_str();
                while let Some((parent, _)) = at.rsplit_once('/') {
                    folders.insert(parent.to_string());
                    at = parent;
                }
            }
            // Deepest first; a folder that still holds anything stays.
            for d in folders.iter().rev() {
                if fsutil::lmeta(&src_root, d).is_none() {
                    let _ = std::fs::remove_dir(wdir.join(d));
                }
            }
        }
    }
    if !matched {
        let exists =
            specs.iter().any(|s| fsutil::lmeta(&src_root, s).is_some() || fsutil::lmeta(&repo.working(&line), s).is_some());
        if !exists {
            return Err(exit(1, format!("error: pathspec '{}' did not match any file(s) known to layr", h.rel(&specs[0]))));
        }
    }
    if worktree {
        if let Err(e) = record_working_checked(ctx, repo, &line, "restore", w.clone()) {
            put_back(repo, &line, w.as_deref());
            return Err(e);
        }
    }
    Ok(0)
}

/// Record a change of the working content only (for undo). The caller holds the project lock.
pub fn record_working(ctx: &Ctx, repo: &Repo, line: &Line, op: &str, working_before: Option<String>) -> Result<()> {
    let (rec, _) = working_record(ctx, repo, line, op, working_before)?;
    repo.commit_record(ctx, rec)?;
    Ok(())
}

/// `record_working` for a command that holds the line lock only (restore): the snapshot of the
/// folder is made first, and the record takes the project lock alone.
pub fn record_working_checked(ctx: &Ctx, repo: &Repo, line: &Line, op: &str, working_before: Option<String>) -> Result<()> {
    let (rec, states) = working_record(ctx, repo, line, op, working_before)?;
    let head = line.head.clone();
    repo.commit_line(line, &head, |_| repo.commit_record(ctx, rec).map(|_| ())).inspect_err(|_| {
        crate::repo::discard_unrecorded(repo, &states);
    })
}

/// The record of a working content change, with the snapshot of the folder after it.
fn working_record(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    op: &str,
    working_before: Option<String>,
) -> Result<(Record, Vec<crate::model::StateRec>)> {
    let mut rec = Record::new(op);
    let working_after = repo.after_state(ctx, line, op, &mut rec)?;
    rec.lines.insert(
        line.id.clone(),
        crate::model::LineChange {
            before: Some(repo.line_state(line, &line.head)),
            after: Some(repo.line_state(line, &line.head)),
            working_before,
            working_after,
        },
    );
    let states = rec.states.clone();
    Ok((rec, states))
}

/// `reset --hard <state>` when the tree of the state is the tree of the head and the working
/// folder matches the head (APFS): only the head moves, no file is touched. Returns false, with
/// nothing changed, in every other case.
#[cfg(target_os = "macos")]
fn reset_same_tree(ctx: &Ctx, repo: &Repo, line: &Line, target: &str) -> Result<bool> {
    let _t = crate::trace::span("reset same tree");
    let _line_lock = repo.line_lock(line)?;
    let line = repo.line(&line.id)?;
    repo.authorize_line(ctx, &line, reset_op(&line, target))?;
    if repo.current_stage(&line)?.is_some() {
        return Ok(false);
    }
    let w = repo.working(&line);
    let head = repo.head_src(&line)?;
    let tree = repo.state_src(target)?;
    // Equal trees: no entry differs (the root folder is not state content).
    if changes::candidates(&head, &tree)?.iter().any(|x| !x.path.is_empty() && !skip_path(&x.path)) {
        return Ok(false);
    }
    // The working folder matches the head: a local change needs the full reset. The walk for
    // overlay folders (`cache_paths`) runs only when the working folder has a change.
    let local = changes::file_changes(&head, &w)?;
    if !local.is_empty() {
        let regen = repo.cache_paths(&w)?;
        let keep = |p: &str| !skip_path(p) && !regen.iter().any(|r| p == r || p.starts_with(&format!("{r}/")));
        if local.iter().any(|x| keep(&x.path) || x.old_path.as_deref().is_some_and(keep)) {
            return Ok(false);
        }
    }
    let _c = crate::trace::span("reset: check move");
    repo.check_move(ctx, &line, target)?;
    drop(_c);
    let head = line.head.clone();
    let rec = repo.head_record(ctx, "reset", &line, target, vec![], None, serde_json::json!({"mode": "hard"}))?;
    repo.commit_line(&line, &head, |_| repo.commit_record(ctx, rec).map(|_| ()))?;
    repo.after_head_moved(&line, target);
    Ok(true)
}

/// The kind of change a reset to `target` is. A reset to the head moves no head: it changes only
/// the working folder and the stage, which a protected line allows. Any other target moves the
/// head, which a protected line refuses.
fn reset_op(line: &Line, target: &str) -> crate::repo::LineOp<'static> {
    if target == line.head {
        crate::repo::LineOp::Working
    } else {
        crate::repo::LineOp::Direct
    }
}

pub fn reset(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("--soft").flag("--mixed").flag("--hard").flag("--keep").flag("--merge").flag("-q|--quiet");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let line = h.line()?.clone();
    // `reset [<rev>] -- <paths>` unstages.
    let mut pos = a.pos.clone();
    let mut paths = a.paths.clone();
    if !a.dashdash && pos.len() > 1 {
        paths = pos.split_off(1);
    } else if !a.dashdash && pos.len() == 1 && revs::resolve(&h.repo, Some(&line), &pos[0]).is_err() {
        paths = pos.clone();
        pos.clear();
    }
    if !paths.is_empty() {
        let specs = h.specs(&paths)?;
        let src = pos.first().map(|s| s.as_str()).unwrap_or("head");
        return restore_impl(ctx, &h, &line, &specs, true, false, Some(src), false, false, false);
    }
    let target = revs::resolve(&h.repo, Some(&line), pos.first().map(|s| s.as_str()).unwrap_or("head"))?;
    // APFS: an equal tree with a clean working folder moves only the head (`reset_same_tree`).
    #[cfg(target_os = "macos")]
    if a.has("--hard") && !a.has("--soft") && h.repo.layers(&line)?.is_empty() && reset_same_tree(ctx, &h.repo, &line, &target)? {
        if !a.has("-q") {
            let s = h.repo.state(&target)?;
            outln!(ctx, "head is now at {} {}", ids::short(&target), s.message.lines().next().unwrap_or(""));
        }
        return Ok(0);
    }
    // The line lock holds for the whole reset: the working folder is written under it, and the
    // project lock only for the record of the head (`commit_line`).
    let _line_lock = h.repo.line_lock(&line)?;
    // APFS: the copy of the working folder and the tree of the target go before the project lock
    // (minutes under it in a large project). The working folder is never under the project lock.
    #[cfg(target_os = "macos")]
    let prepared = if a.has("--hard") && h.repo.layers(&line)?.is_empty() {
        hard_reset_changes(&h.repo, &line, &h.repo.state_root(&target)?).ok()
    } else {
        None
    };
    #[cfg(not(target_os = "macos"))]
    let prepared: Option<(Temp, Vec<Change>)> = None;
    let (_, w) = begin_line(ctx, &h.repo, &line, reset_op(&line, &target))?;
    let line = h.repo.line(&line.id)?;
    let repo = &h.repo;
    // The head may have moved before the lock: check again with the head read under it.
    repo.authorize_line(ctx, &line, reset_op(&line, &target))?;
    repo.check_move(ctx, &line, &target)?;
    // A line with overlays moves to the target with its overlays in place. A hard reset to a state
    // with parts shows its parts (docs/overlays.md, section 1a); the other modes keep the stack.
    let current = repo.layers(&line)?;
    let layers = if a.has("--hard") && !a.has("--soft") { repo.layers_at(&line, &target)? } else { current.clone() };
    let target_view = if layers.is_empty() { None } else { Some(repo.assemble(&line, &target, &layers)?) };
    let target_root = match &target_view {
        Some(v) => v.path.clone(),
        None => repo.state_root(&target)?,
    };
    let mode = if a.has("--soft") {
        "soft"
    } else if a.has("--hard") {
        "hard"
    } else if a.has("--keep") || a.has("--merge") {
        "keep"
    } else {
        "mixed"
    };
    let (index, _it) = repo.index_view(&line)?;
    // The working folder is written under the line lock. The stage changes follow the commit.
    let wrote = match mode {
        "hard" => {
            if !repo.reset_snapshot(ctx, &line, &target)? {
                let (_scan, list) = match prepared {
                    Some(p) => p,
                    None => hard_reset_changes(repo, &line, &target_root)?,
                };
                let wdir = repo.working(&line);
                let own = repo.own(&line);
                let gone: Vec<&str> = list
                    .iter()
                    .filter(|c| c.new.is_none() && fsutil::lmeta(&index, &c.path).is_some())
                    .map(|c| c.path.as_str())
                    .collect();
                let write: Vec<&str> = list.iter().filter(|c| c.new.is_some()).map(|c| c.path.as_str()).collect();
                fsutil::replace_work_entries(&target_root, &wdir, &gone, &write, own)?;
            }
            true
        }
        "keep" => {
            let (st, _scan, _it2, _idx2) = repo.status(&line)?;
            let wdir = repo.working(&line);
            repo.apply_delta(&repo.head_tree(&line)?, &target_root, &wdir, repo.own(&line), &st.local_paths(), false)?;
            true
        }
        _ => false,
    };
    let head = line.head.clone();
    let mut data = serde_json::json!({"mode": mode});
    if layers != current {
        data["line"] = serde_json::json!(line.id);
        let changed: Vec<&str> =
            layers.iter().zip(&current).filter(|(n, o)| n.state != o.state).map(|(n, _)| n.overlay.as_str()).collect();
        data["overlays"] = serde_json::json!(changed);
    }
    let rec = repo.head_record(ctx, "reset", &line, &target, vec![], w.clone(), data);
    let committed = rec.and_then(|rec| {
        let states = rec.states.clone();
        repo.commit_line(&line, &head, |_| repo.commit_record(ctx, rec).map(|_| ())).inspect_err(|_| {
            crate::repo::discard_unrecorded(repo, &states);
        })
    });
    if let Err(e) = committed {
        if wrote {
            put_back(repo, &line, w.as_deref());
        }
        return Err(e);
    }
    // The line on each overlay follows: it shows the parts of the target from now on.
    if layers != current {
        repo.point_overlay_lines(ctx, &line, &layers, "reset")?;
    }
    repo.after_head_moved(&line, &target);
    // The stage follows the head. Its states are kept until the head moved.
    match mode {
        "soft" => {
            // Keep the index content, now based on the new head.
            let mut temps: Vec<Temp> = Vec::new();
            let index = repo.complete(index, &mut temps)?;
            let keep = repo.temp_snapshot(&index, true)?;
            repo.drop_stage(&line)?;
            let mut l2 = line.clone();
            l2.head = target.clone();
            let stage = repo.p.stage_path(&line.id);
            crate::storage::snapshot(&keep.path, &stage, false)?;
            repo.mark_stage(&l2)?;
        }
        _ => repo.drop_stage(&line)?,
    }
    if let Some(v) = target_view {
        repo.install_base(&line, v, &target, &layers)?;
    }
    if mode == "hard" && !a.has("-q") {
        let s = repo.state(&target)?;
        outln!(ctx, "head is now at {} {}", ids::short(&target), s.message.lines().next().unwrap_or(""));
    }
    // A mixed reset keeps the files: name the ones that now differ from the new head, as git
    // does, and the command that changes them.
    if mode == "mixed" && !a.has("-q") && target != line.head {
        let (st, _scan, _it, _idx) = repo.status(&repo.line(&line.id)?)?;
        if !st.unstaged.is_empty() {
            outln!(ctx, "Unstaged changes after reset:");
            for c in st.unstaged.iter().take(20) {
                outln!(ctx, "{}\t{}", if c.new.is_none() { 'D' } else { 'M' }, c.path);
            }
            if st.unstaged.len() > 20 {
                outln!(ctx, "... and {} more", st.unstaged.len() - 20);
            }
            let rev = pos.first().map(String::as_str).unwrap_or("head");
            eprintln!("hint: reset without --hard keeps the files; 'layr reset --hard {rev}' changes them");
        }
    }
    Ok(0)
}

/// Put the working folder back to the automatic state `w` that a refused command saved first. A
/// failure only warns: the command has failed already, and the folder check is in the message.
pub(crate) fn put_back(repo: &Repo, line: &Line, w: Option<&str>) {
    let Some(w) = w else { return };
    if let Err(e) = super::admin::restore_working(repo, line, w) {
        eprintln!("error: the working folder of {} could not be put back: {e:#}; check it with 'layr status'", line.name);
    }
}

/// The changes from the working folder to `target_root` that `reset --hard` applies: every
/// tracked path; untracked and ignored files stay. The scan stays alive with the list.
fn hard_reset_changes(repo: &Repo, line: &Line, target_root: &std::path::Path) -> Result<(Temp, Vec<Change>)> {
    let scan = repo.scan(line)?;
    let list = changes::without_moves(changes::file_changes(&scan.path, target_root)?)
        .into_iter()
        .filter(|c| !skip_path(&c.path))
        .collect();
    Ok((scan, list))
}

pub fn clean(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .flag("-n|--dry-run")
        .flag("-f|--force")
        .flag("-d")
        .flag("-x")
        .flag("-X")
        .flag("-q|--quiet")
        .flag("-i|--interactive");
    let a = parse(&spec, args)?;
    if !a.has("-n") && !a.has("-f") {
        return Err(exit(128, "fatal: clean requires -f (force) or -n (dry run)"));
    }
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let specs = h.specs(&a.rest())?;
    let targets = |st: &Status, scan: &Path, index: &Snap| -> Vec<String> {
        let t = clean_targets(st, scan, index, a.has("-d"), a.has("-x"), a.has("-X"));
        let mut t: Vec<String> = t.into_iter().filter(|t| matches(&specs, t.trim_end_matches('/'))).collect();
        t.sort();
        t
    };
    // A dry run only reads the folder: it takes no store lock.
    if a.has("-n") {
        let (st, scan, _it, index) = h.repo.status(&line)?;
        for t in targets(&st, &scan.path, &index) {
            outln!(ctx, "Would remove {}", h.rel(&t));
        }
        return Ok(0);
    }
    h.repo.prepare_status(&line)?;
    let r = (|| -> Result<i32> {
        // Clean changes only the working folder: a protected line allows it.
        let (_lock, w) = begin(ctx, &h.repo, &line, crate::repo::LineOp::Working)?;
        let line = h.repo.line(&line.id)?;
        let (st, scan, _it, index) = h.repo.locked_status(&line)?;
        let targets = targets(&st, &scan.path, &index);
        let wdir = h.repo.working(&line);
        for t in &targets {
            let rel = t.trim_end_matches('/');
            // A folder (build output) goes to the trash, which is emptied after the lock.
            match fsutil::safe_join(&wdir, rel) {
                Ok(full) if full.symlink_metadata().is_ok_and(|m| m.is_dir()) && h.repo.p.trash(&full).is_ok() => {}
                _ => fsutil::remove_entry(&wdir, rel)?,
            }
            if !a.has("-q") {
                outln!(ctx, "Removing {}", h.rel(t));
            }
        }
        if !targets.is_empty() {
            record_working(ctx, &h.repo, &line, "clean", w)?;
        }
        Ok(0)
    })();
    h.repo.discard_prepared();
    r
}

/// What `clean` removes, as git does. A folder goes whole only when nothing in it is in the
/// index and everything in it is of the kinds being removed: with `-X` an ignored folder, with
/// `-x` any new folder, without them a new folder that holds no ignored files. Otherwise the
/// files go one by one. Without `-d`, files in new folders stay.
fn clean_targets(st: &Status, scan: &Path, index: &Snap, dirs: bool, with_ignored: bool, only_ignored: bool) -> BTreeSet<String> {
    let rules = crate::ignore_rules::Rules::new(scan);
    let in_index = |d: &str| fsutil::lmeta(index, d).is_some();
    let folders = |p: &str| -> Vec<String> {
        let comps: Vec<&str> = p.split('/').collect();
        (1..comps.len()).map(|n| comps[..n].join("/")).collect()
    };
    // The topmost folder of `p` that is not in the index.
    let new_folder = |p: &str| folders(p).into_iter().find(|d| !in_index(d));
    let has_ignored = |d: &str| st.ignored.iter().any(|i| i.starts_with(&format!("{d}/")));
    let mut out = BTreeSet::new();
    if !only_ignored {
        for f in &st.untracked {
            match new_folder(f) {
                None => {
                    out.insert(f.clone());
                }
                Some(d) if dirs => {
                    out.insert(if with_ignored || !has_ignored(&d) { format!("{d}/") } else { f.clone() });
                }
                Some(_) => {}
            }
        }
    }
    if with_ignored || only_ignored {
        for i in &st.ignored {
            match new_folder(i) {
                None => {
                    out.insert(i.clone());
                }
                Some(d) if dirs => {
                    let whole = if only_ignored {
                        folders(i).into_iter().find(|f| !in_index(f) && rules.ignored(f, true))
                    } else {
                        Some(d)
                    };
                    out.insert(whole.map(|f| format!("{f}/")).unwrap_or_else(|| i.clone()));
                }
                Some(_) => {}
            }
        }
    }
    // A path inside a folder that goes whole is not listed again.
    let all: Vec<String> = out.iter().cloned().collect();
    out.retain(|p| !all.iter().any(|d| d.ends_with('/') && d != p && p.starts_with(d.as_str())));
    out
}

#[cfg(test)]
mod status_tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn saving_a_link_includes_only_tracked_nonquiet_names() {
        let root = std::env::temp_dir().join(format!("layr-save-linked-{}", crate::ids::new_id()));
        let (scan, index) = (root.join("scan"), root.join("index"));
        std::fs::create_dir_all(&scan).unwrap();
        std::fs::create_dir_all(&index).unwrap();
        std::fs::write(scan.join("first"), "data").unwrap();
        for name in ["tracked", "quiet", "untracked"] {
            std::fs::hard_link(scan.join("first"), scan.join(name)).unwrap();
        }
        for name in ["first", "tracked", "quiet"] {
            std::fs::write(index.join(name), "old").unwrap();
        }
        let mut paths = BTreeSet::from(["first".to_string()]);
        let quiet = BTreeSet::from(["quiet".to_string()]);
        include_linked_tracked(&scan, &index, &mut paths, &quiet).unwrap();
        assert_eq!(paths, BTreeSet::from(["first".to_string(), "tracked".to_string()]));
        std::fs::remove_dir_all(root).unwrap();
    }

    fn line() -> Line {
        Line { id: "id".into(), name: "topic".into(), head: "state".into(), owner: 0, machine: "machine".into() }
    }

    #[test]
    fn default_json_keeps_empty_sections() {
        let line = line();
        let st = Status::default();
        let options = StatusOptions { overlaps: StatusOverlaps::Include, ignored: StatusIgnored::Include };
        let output = StatusJson::new(&line, &st, &[], &[], &options);
        assert_eq!(
            serde_json::to_string(&output).unwrap(),
            r#"{"conflicts":[],"head":"state","ignored":[],"line":"topic","overlaps":[],"staged":[],"unstaged":[],"untracked":[]}"#
        );
    }

    #[test]
    fn default_json_keeps_nonempty_nested_key_order() {
        let line = line();
        let st = Status {
            conflicts: vec![
                Conflict { path: "shared.txt".into(), kind: "content".into(), base: None, ours: None, theirs: None },
                Conflict {
                    path: "other.txt".into(),
                    kind: "content".into(),
                    base: Some("base".into()),
                    ours: Some("ours".into()),
                    theirs: Some("theirs".into()),
                },
            ],
            ..Status::default()
        };
        let overlaps = [Overlap { path: "shared.txt".into(), line: "other".into(), owner: 0, changed_at: 0 }];
        let options = StatusOptions { overlaps: StatusOverlaps::Include, ignored: StatusIgnored::Include };
        let output = StatusJson::new(&line, &st, &[], &overlaps, &options);
        assert_eq!(
            serde_json::to_string(&output).unwrap(),
            r#"{"conflicts":[{"kind":"content","path":"shared.txt"},{"base":"base","kind":"content","ours":"ours","path":"other.txt","theirs":"theirs"}],"head":"state","ignored":[],"line":"topic","overlaps":[{"changed_at":0,"line":"other","owner":0,"path":"shared.txt"}],"staged":[],"unstaged":[],"untracked":[]}"#
        );
    }

    #[test]
    fn status_help_lists_opt_outs() {
        assert!(crate::cmd::HELP.contains("status [--json] [--no-overlaps] [--no-ignored]"));
    }

    #[test]
    fn json_opt_outs_omit_only_requested_sections() {
        let line = line();
        let st = Status {
            ignored: vec!["debug.log".into()],
            untracked: vec!["new.txt".into()],
            conflicts: vec![Conflict { path: "shared.txt".into(), kind: "content".into(), base: None, ours: None, theirs: None }],
            ..Status::default()
        };
        let overlaps = [Overlap { path: "shared.txt".into(), line: "other".into(), owner: 0, changed_at: 0 }];
        for radar in [StatusOverlaps::Include, StatusOverlaps::Skip] {
            for ignored in [StatusIgnored::Include, StatusIgnored::Omit] {
                let options = StatusOptions { overlaps: radar, ignored };
                let output = StatusJson::new(&line, &st, &[], &overlaps, &options);
                let json = serde_json::to_string(&output).unwrap();
                assert_eq!(json.contains(r#""overlaps":"#), radar == StatusOverlaps::Include);
                assert_eq!(json.contains(r#""ignored":"#), ignored == StatusIgnored::Include);
                assert!(json.contains(r#""conflicts":[{"kind":"content","path":"shared.txt"}]"#));
                assert!(json.contains(r#""untracked":["new.txt"]"#));
                assert!(!json.contains(r#""ignored":null"#));
                assert!(!json.contains(r#""overlaps":null"#));
            }
        }
        assert_eq!(st.ignored, ["debug.log"]);
        assert_eq!(st.untracked, ["new.txt"]);
    }

    #[test]
    fn json_path_filters_apply_with_opt_outs() {
        let line = line();
        let st = Status { ignored: vec!["debug.log".into()], untracked: vec!["new.txt".into()], ..Status::default() };
        let options = StatusOptions { overlaps: StatusOverlaps::Skip, ignored: StatusIgnored::Include };
        let output = StatusJson::new(&line, &st, &["new.txt".into()], &[], &options);
        assert_eq!(
            serde_json::to_string(&output).unwrap(),
            r#"{"conflicts":[],"head":"state","ignored":[],"line":"topic","staged":[],"unstaged":[],"untracked":["new.txt"]}"#
        );
    }
}
