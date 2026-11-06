//! Reading commands: diff, show, log, rev-parse, merge-base, blame, grep, ls-files.

use super::{exit, here, matches, Here};
use crate::args::{parse, Spec};
use crate::changes::{self, Change};
use crate::ctx::Ctx;
use crate::diff::{self, FileDiff};
use crate::fsutil;
use crate::ids;
use crate::model::{Line, StateKind, StateRec};
use crate::repo::{skip_path, snap, Repo, Snap, Temp};
use crate::revs;
use anyhow::{bail, Result};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

fn diff_spec() -> Spec {
    Spec::new()
        .flag("--cached|--staged")
        .flag("--stat")
        .flag("--numstat")
        .flag("--shortstat")
        .flag("--name-only")
        .flag("--name-status")
        .flag("--check")
        .flag("--quiet")
        .flag("--exit-code")
        .flag("--no-color")
        .optional("--color")
        .flag("-p|-u|--patch")
        .flag("--raw")
        .optional("-M|--find-renames")
        .flag("--no-renames")
        .flag("-R")
        .flag("-a|--text")
        .flag("--minimal")
        .flag("--patience")
        .flag("--histogram")
        .flag("--no-ext-diff")
        .flag("--summary")
        .flag("-w|--ignore-all-space")
        .flag("-b|--ignore-space-change")
        .flag("--full-index")
        .value("-U|--unified")
        .optional("--diff-filter")
        .value("--stat-width")
        .optional("--relative")
}

/// An empty folder to diff against a root state.
fn empty_root() -> Result<Temp> {
    let p = std::env::temp_dir().join(format!("layr-empty-{}", ids::new_id()));
    std::fs::create_dir(&p)?;
    Ok(Temp { path: p })
}

/// All files of a root as additions.
fn all_files(root: &(impl fsutil::Src + ?Sized)) -> Result<Vec<Change>> {
    let mut v = Vec::new();
    fsutil::walk(root, "", &mut |rel, m| {
        if skip_path(rel) {
            return Ok(false);
        }
        if !m.is_dir() {
            v.push(Change { path: rel.to_string(), old_path: None, old: None, new: Some(m.clone()), ranges: None });
        }
        Ok(true)
    })?;
    Ok(v)
}

pub struct DiffOpts {
    pub stat: bool,
    pub numstat: bool,
    pub shortstat: bool,
    pub name_only: bool,
    pub name_status: bool,
    pub check: bool,
    pub patch: bool,
    pub context: usize,
    pub stat_width: usize,
    pub filter: Option<String>,
}

impl DiffOpts {
    fn from(a: &crate::args::Parsed, default_patch: bool) -> Result<DiffOpts> {
        let stat = a.has("--stat");
        let numstat = a.has("--numstat");
        let shortstat = a.has("--shortstat");
        let name_only = a.has("--name-only");
        let name_status = a.has("--name-status");
        let check = a.has("--check");
        let any_summary = stat || numstat || shortstat || name_only || name_status || check;
        Ok(DiffOpts {
            stat,
            numstat,
            shortstat,
            name_only,
            name_status,
            check,
            patch: a.has("-p") || (!any_summary && default_patch),
            context: a.get("-U").map(|v| v.parse()).transpose()?.unwrap_or(3),
            stat_width: a.get("--stat-width").map(|v| v.parse()).transpose()?.unwrap_or(80),
            filter: a.get("--diff-filter").map(|s| s.to_string()),
        })
    }
}

pub fn print_diffs(ctx: &Ctx, fds: &[FileDiff], o: &DiffOpts) -> bool {
    let analyzed: Vec<_> = match &o.filter {
        Some(f) => fds.iter().filter(|d| f.contains(d.status())).map(diff::AnalyzedDiff::new).collect(),
        None => fds.iter().map(diff::AnalyzedDiff::new).collect(),
    };
    let mut problems = false;
    if o.check {
        let s = diff::check_analyzed(&analyzed);
        problems = !s.is_empty();
        ctx.print(&s);
        return problems;
    }
    if o.numstat {
        ctx.print(&diff::numstat_analyzed(&analyzed));
    }
    if o.stat {
        ctx.print(&diff::stat_analyzed(&analyzed, o.stat_width));
    }
    if o.shortstat {
        ctx.print(&diff::shortstat_analyzed(&analyzed));
    }
    if o.name_only {
        ctx.print(&diff::name_only_analyzed(&analyzed));
    }
    if o.name_status {
        ctx.print(&diff::name_status_analyzed(&analyzed));
    }
    if o.patch {
        if o.stat || o.numstat || o.shortstat {
            outln!(ctx);
        }
        for f in &analyzed {
            ctx.print(&diff::patch_analyzed(f, o.context));
        }
    }
    problems
}

/// Split diff arguments into revisions and paths, as git does without `--`.
fn split_revs(repo: &Repo, line: Option<&Line>, a: &crate::args::Parsed) -> Result<(Vec<String>, Vec<String>)> {
    let mut revs = Vec::new();
    let mut paths = a.paths.clone();
    let mut in_paths = false;
    for p in &a.pos {
        if !in_paths {
            for part in p.split("..").filter(|part| !part.is_empty()) {
                revs::reject_old(part.trim_start_matches('.'))?;
            }
        }
        if !in_paths && is_rev(repo, line, p) {
            revs.push(p.clone());
        } else {
            in_paths = true;
            paths.push(p.clone());
        }
    }
    Ok((revs, paths))
}

fn is_rev(repo: &Repo, line: Option<&Line>, s: &str) -> bool {
    if let Some((a, b)) = s.split_once("...") {
        return (a.is_empty() || revs::resolve(repo, line, a).is_ok()) && (b.is_empty() || revs::resolve(repo, line, b).is_ok());
    }
    if let Some((a, b)) = s.split_once("..") {
        return (a.is_empty() || revs::resolve(repo, line, a).is_ok()) && (b.is_empty() || revs::resolve(repo, line, b).is_ok());
    }
    revs::resolve(repo, line, s).is_ok()
}

/// Pin the saved states that a read names (and the head of its line): they stay on disk for the
/// command, so a free step of another command cannot remove them while the read runs. A name that
/// does not resolve is left to the read's own error.
fn pin_revs(repo: &Repo, line: Option<&Line>, names: &[String]) -> Result<crate::free::Pin> {
    let mut ids: Vec<String> = line.map(|l| vec![l.head.clone()]).unwrap_or_default();
    for name in names {
        let rev = name.split_once(':').map_or(name.as_str(), |(r, _)| r);
        let rev = if rev.is_empty() { "head" } else { rev };
        for part in rev.split("...").flat_map(|s| s.split("..")) {
            let part = if part.is_empty() { "head" } else { part };
            if let Ok(id) = revs::resolve(repo, line, part) {
                ids.push(id);
            }
        }
    }
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    repo.pin(&refs)
}

pub fn diff(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&diff_spec(), args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = h.line.clone();
    let (revs_v, paths) = split_revs(repo, line.as_ref(), &a)?;
    repo.hold(pin_revs(repo, line.as_ref(), &revs_v)?);
    for rev in &revs_v {
        for part in rev.split("...").flat_map(|s| s.split("..")) {
            let id = revs::resolve(repo, line.as_ref(), if part.is_empty() { "head" } else { part })?;
            repo.source_state(&id)?;
        }
    }
    let specs = h.specs(&paths)?;
    let cached = a.has("--cached");
    let opts = DiffOpts::from(&a, true)?;
    let mut keep: Vec<Temp> = Vec::new();
    let resolve_id = |r: &str| revs::resolve(repo, line.as_ref(), if r.is_empty() { "head" } else { r });
    // (old root, new root, new side is the working content, index for tracked check)
    let mut index_root: Option<Snap> = None;
    let needs_working_view = revs_v.is_empty() || (revs_v.len() == 1 && !revs_v[0].contains(".."));
    let filtered =
        if needs_working_view { line.as_ref().map(|l| repo.filtered_view(ctx, l)).transpose()?.flatten() } else { None };
    let (old, new, working): (Snap, Snap, bool) = match revs_v.len() {
        0 if filtered.is_some() => {
            let root = snap(&filtered.as_ref().unwrap().path);
            (root.clone(), root, false)
        }
        0 => {
            let l = h.line()?;
            repo.check_view(ctx, l)?;
            let (idx, it) = repo.index_view(l)?;
            if let Some(t) = it {
                keep.push(t);
            }
            if cached {
                (repo.head_src(l)?, idx, false)
            } else {
                index_root = Some(idx.clone());
                (idx, snap(&repo.working(l)), true)
            }
        }
        1 => {
            let r = &revs_v[0];
            if let Some((x, y)) = r.split_once("...") {
                let xi = revs::resolve(repo, line.as_ref(), if x.is_empty() { "head" } else { x })?;
                let yi = revs::resolve(repo, line.as_ref(), if y.is_empty() { "head" } else { y })?;
                let b = repo.merge_base(&xi, &yi)?.ok_or_else(|| anyhow::anyhow!("no merge base"))?;
                let (o, n, k) = pair_of(repo, &b, &yi)?;
                keep.extend(k);
                (o, n, false)
            } else if let Some((x, y)) = r.split_once("..") {
                let (o, n, k) = pair_of(repo, &resolve_id(x)?, &resolve_id(y)?)?;
                keep.extend(k);
                (o, n, false)
            } else if let Some(filtered) = &filtered {
                let l = h.line()?;
                let id = revs::resolve(repo, line.as_ref(), r)?;
                let layers = repo.caller_layers(ctx, l)?;
                let old = repo.assemble(l, &id, &layers)?;
                let root = snap(&old.path);
                keep.push(old);
                (root, snap(&filtered.path), false)
            } else {
                let l = h.line()?;
                repo.check_view(ctx, l)?;
                // Against the working content of a line with overlays, a state is compared with
                // the line's overlays in place (its own head: the line's base).
                let id = revs::resolve(repo, line.as_ref(), r)?;
                let layers = repo.layers(l)?;
                let old = if layers.is_empty() {
                    repo.state_src(&id)?
                } else if id == l.head {
                    repo.head_src(l)?
                } else {
                    let v = repo.assemble(l, &id, &layers)?;
                    let p = snap(&v.path);
                    keep.push(v);
                    p
                };
                let (idx, it) = repo.index_view(l)?;
                if let Some(t) = it {
                    keep.push(t);
                }
                if cached {
                    (old, idx, false)
                } else {
                    index_root = Some(idx);
                    (old, snap(&repo.working(l)), true)
                }
            }
        }
        _ => {
            let (o, n, k) = pair_of(repo, &resolve_id(&revs_v[0])?, &resolve_id(&revs_v[1])?)?;
            keep.extend(k);
            (o, n, false)
        }
    };
    // The files of quiet overlays stay out of a diff of the working content or the index.
    let quiet_paths = match (&line, revs_v.len()) {
        (Some(l), n) if n <= 1 && !revs_v.iter().any(|r| r.contains("..")) => repo.quiet_paths(l)?,
        _ => Default::default(),
    };
    let reverse = a.has("-R");
    let renames = !a.has("--no-renames");
    let compute = |old: &Snap, new: &Snap| -> Result<Vec<FileDiff>> {
        let (old, new) = if reverse { (new, old) } else { (old, new) };
        let mut list: Vec<Change> = changes::file_changes(old, new)?
            .into_iter()
            .filter(|c| {
                !skip_path(&c.path)
                    && !quiet_paths.contains(&c.path)
                    && (matches(&specs, &c.path) || c.old_path.as_deref().map(|o| matches(&specs, o)).unwrap_or(false))
            })
            .collect();
        if working {
            // Untracked files are not part of a working tree diff.
            let idx = index_root.as_ref().unwrap();
            list = changes::without_moves(list);
            list.retain(|c| !(c.old.is_none() && fsutil::lmeta(idx, &c.path).is_none()));
        }
        let list = if renames { changes::with_renames(old, new, list) } else { list };
        diff::file_diffs(old, new, &list)
    };
    let fds = if working {
        // The live working folder, validated by kernel change counters as `status` does; a
        // read-only snapshot when validation fails.
        let l = h.line()?;
        #[cfg(target_os = "macos")]
        let fast = crate::storage::stable_read(&[repo.working(l).as_path()], || compute(&old, &new))?;
        #[cfg(not(target_os = "macos"))]
        let fast: Option<Vec<FileDiff>> = None;
        match fast {
            Some(v) => v,
            None => {
                let scan = repo.scan(l)?;
                let v = compute(&old, &snap(&scan.path))?;
                keep.push(scan);
                v
            }
        }
    } else {
        compute(&old, &new)?
    };
    let quiet = a.has("--quiet");
    let problems = if quiet { false } else { print_diffs(ctx, &fds, &opts) };
    drop(keep);
    if opts.check && problems {
        return Ok(2);
    }
    if (a.has("--exit-code") || quiet) && !fds.is_empty() {
        return Ok(1);
    }
    Ok(0)
}

// ----- formatting of states -----

pub fn fmt_date(ms: i64, style: &str) -> String {
    let dt = chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default();
    match style {
        "iso" | "iso8601" => dt.format("%Y-%m-%d %H:%M:%S +0000").to_string(),
        "iso-strict" | "iso8601-strict" => dt.format("%Y-%m-%dT%H:%M:%S+00:00").to_string(),
        "short" => dt.format("%Y-%m-%d").to_string(),
        "unix" => (ms / 1000).to_string(),
        "relative" => relative(ms),
        "rfc" | "rfc2822" => dt.format("%a, %-d %b %Y %H:%M:%S +0000").to_string(),
        _ => dt.format("%a %b %-d %H:%M:%S %Y +0000").to_string(),
    }
}

fn relative(ms: i64) -> String {
    let d = (ids::now_ms() - ms) / 1000;
    let (n, u) = if d < 90 {
        (d, "second")
    } else if d < 90 * 60 {
        (d / 60, "minute")
    } else if d < 36 * 3600 {
        (d / 3600, "hour")
    } else if d < 14 * 86400 {
        (d / 86400, "day")
    } else if d < 70 * 86400 {
        (d / (7 * 86400), "week")
    } else if d < 365 * 86400 {
        (d / (30 * 86400), "month")
    } else {
        (d / (365 * 86400), "year")
    };
    format!("{n} {u}{} ago", if n == 1 { "" } else { "s" })
}

pub struct Decor {
    map: HashMap<String, Vec<String>>,
    /// The git commit of each state that has one (`git_map`), shown next to the state id.
    git: HashMap<String, String>,
}

impl Decor {
    fn load_with_head(repo: &Repo, current: Option<&Line>, head: &str) -> Result<Decor> {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for l in repo.lines()? {
            let name =
                if current.map(|c| c.id == l.id).unwrap_or(false) { format!("{head} -> {}", l.name) } else { l.name.clone() };
            map.entry(l.head.clone()).or_default().push(name);
        }
        for (t, s, _) in repo.p.db.tags()? {
            map.entry(s).or_default().push(format!("tag: {t}"));
        }
        for (r, s, _) in repo.p.db.remote_refs()? {
            map.entry(s).or_default().push(r);
        }
        for v in map.values_mut() {
            v.sort_by_key(|x| (!x.starts_with(head), x.starts_with("tag:"), x.clone()));
        }
        Ok(Decor { map, git: repo.p.db.git_map()? })
    }
    pub fn of(&self, id: &str) -> String {
        match self.map.get(id) {
            Some(v) if !v.is_empty() => format!(" ({})", v.join(", ")),
            _ => String::new(),
        }
    }
    pub fn raw(&self, id: &str) -> String {
        self.map.get(id).map(|v| v.join(", ")).unwrap_or_default()
    }
}

fn subject(s: &StateRec) -> String {
    let m = if s.message.is_empty() && s.kind == StateKind::Auto { "(automatic state)" } else { s.message.as_str() };
    m.lines().next().unwrap_or("").to_string()
}

fn body(s: &StateRec) -> String {
    let mut it = s.message.splitn(2, '\n');
    it.next();
    it.next().unwrap_or("").trim_start_matches('\n').to_string()
}

fn unescape_format(f: &str) -> String {
    f.to_string()
}

/// The git tree hash of a state, for `%T` and `%t`: the hash of its content, the same on every
/// machine and in git.
pub type TreeOf<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Expand a `--format` string for one state.
pub fn format_state(f: &str, s: &StateRec, decor: &Decor, date_style: &str, tree: TreeOf) -> String {
    let f = unescape_format(f);
    let mut out = String::new();
    let b = f.as_bytes();
    let mut i = 0;
    let date = |st: &str| fmt_date(s.time, st);
    while i < b.len() {
        if b[i] != b'%' || i + 1 >= b.len() {
            out.push(b[i] as char);
            i += 1;
            continue;
        }
        let c = b[i + 1] as char;
        i += 2;
        let two = if i < b.len() { Some(b[i] as char) } else { None };
        let take2 = |o: &mut String, v: String| {
            o.push_str(&v);
        };
        match c {
            'H' => out.push_str(&ids::display(&s.id)),
            'h' => out.push_str(&ids::short(&s.id)),
            'T' => out.push_str(&tree(&s.id).unwrap_or_default()),
            't' => out.push_str(&tree(&s.id).map(|t| t[..7.min(t.len())].to_string()).unwrap_or_default()),
            'P' => out.push_str(&s.parents.iter().map(|p| ids::display(p)).collect::<Vec<_>>().join(" ")),
            'p' => out.push_str(&s.parents.iter().map(|p| ids::short(p)).collect::<Vec<_>>().join(" ")),
            's' => out.push_str(&subject(s)),
            'b' => {
                let bd = body(s);
                if !bd.is_empty() {
                    out.push_str(&bd);
                    out.push('\n');
                }
            }
            'B' => {
                out.push_str(&s.message);
                out.push('\n');
            }
            'n' => out.push('\n'),
            '%' => out.push('%'),
            'd' => out.push_str(&decor.of(&s.id)),
            'D' => out.push_str(&decor.raw(&s.id)),
            'x' => {
                if i + 1 < b.len() + 1 && i + 2 <= b.len() {
                    if let Ok(v) = u8::from_str_radix(&f[i..i + 2], 16) {
                        out.push(v as char);
                        i += 2;
                    }
                }
            }
            'a' | 'c' => {
                let k = two.unwrap_or(' ');
                i += 1;
                match k {
                    'n' => take2(&mut out, s.author.name.clone()),
                    'N' => take2(&mut out, s.author.name.clone()),
                    'e' | 'E' => take2(&mut out, s.author.email.clone()),
                    'l' => take2(&mut out, s.author.email.split('@').next().unwrap_or("").to_string()),
                    'd' => take2(&mut out, date(date_style)),
                    'D' => take2(&mut out, date("rfc")),
                    'r' => take2(&mut out, date("relative")),
                    't' => take2(&mut out, date("unix")),
                    'i' => take2(&mut out, date("iso")),
                    'I' => take2(&mut out, date("iso-strict")),
                    's' => take2(&mut out, date("short")),
                    _ => {
                        out.push('%');
                        out.push(c);
                        i -= 1;
                    }
                }
            }
            _ => {
                out.push('%');
                out.push(c);
            }
        }
    }
    out
}

fn medium(s: &StateRec, decor: &Decor, date_style: &str, kind_note: bool, human: bool) -> String {
    let label = if human { "state" } else { "commit" };
    let mut o = format!("{label} {}{}\n", ids::display(&s.id), decor.of(&s.id));
    if s.parents.len() > 1 {
        o.push_str(&format!("Merge: {}\n", s.parents.iter().map(|p| ids::short(p)).collect::<Vec<_>>().join(" ")));
    }
    if human {
        if let Some(c) = decor.git.get(&s.id) {
            o.push_str(&format!("Git:    {c}\n"));
        }
    }
    o.push_str(&format!("Author: {} <{}>\n", s.author.name, s.author.email));
    o.push_str(&format!("Date:   {}\n", fmt_date(s.time, date_style)));
    if kind_note && s.kind != StateKind::Commit {
        let k = match s.kind {
            StateKind::Auto => {
                if s.running {
                    "automatic state (taken while processes ran)"
                } else {
                    "automatic state"
                }
            }
            StateKind::Stash => {
                if human {
                    "park"
                } else {
                    "stash"
                }
            }
            StateKind::Import => "imported from git",
            StateKind::Commit => "",
            StateKind::Cache => "cache",
            StateKind::Derived => "derived",
        };
        o.push_str(&format!("Kind:   {k}\n"));
    }
    o.push('\n');
    let msg = if s.message.is_empty() { "(no message)".to_string() } else { s.message.clone() };
    for l in msg.lines() {
        if l.is_empty() {
            o.push('\n');
        } else {
            o.push_str(&format!("    {l}\n"));
        }
    }
    if !s.conflicts.is_empty() {
        o.push_str("\n    Conflicts:\n");
        for c in &s.conflicts {
            o.push_str(&format!("    \t{} ({})\n", c.path, c.kind));
        }
    }
    o
}

/// Changes of a state against its first parent (all files for a root state).
/// The two sides of a comparison of two states. Imported states that are not on disk are read from
/// git (`gitbridge::sparse_pair`): the sides hold the paths that differ, and no tree is built.
fn pair_of(repo: &Repo, a: &str, b: &str) -> Result<(Snap, Snap, Vec<Temp>)> {
    if !(repo.p.has_state(a) && repo.p.has_state(b)) {
        if let (Some(ca), Some(cb)) = (crate::gitbridge::imported_commit(repo, a)?, crate::gitbridge::imported_commit(repo, b)?) {
            // Without the Git objects (a store remote holds the trees), the trees are read below.
            if let Ok(p) = crate::gitbridge::sparse_pair(repo, Some(&ca), &cb) {
                return Ok((p.old, p.new, p.keep));
            }
        }
    }
    Ok((repo.state_src(a)?, repo.state_src(b)?, Vec::new()))
}

/// One side of a comparison: a tree, or the bytes of an imported state read from git.
pub enum Entries {
    Tree(Snap),
    Git(crate::gitbridge::GitFiles),
}

impl diff::Contents for Entries {
    fn bytes(&self, path: &str) -> Result<Vec<u8>> {
        match self {
            Entries::Tree(root) => fsutil::entry_bytes(root, path),
            Entries::Git(files) => files.bytes(path),
        }
    }
}

/// The sides of a state against its first parent, when the state is an imported state and its parent
/// (if any) is an imported state: read from git objects (`gitbridge::sides_of`). The trees on disk are
/// not needed, so a present state builds nothing either.
fn git_sides(repo: &Repo, s: &StateRec) -> Result<Option<crate::gitbridge::Sides>> {
    let Some(new) = crate::gitbridge::imported_commit(repo, &s.id)? else { return Ok(None) };
    let old = match s.parents.first() {
        None => None,
        Some(p) => match crate::gitbridge::imported_commit(repo, p)? {
            Some(c) => Some(c),
            None => return Ok(None),
        },
    };
    // Without the Git objects, the trees are read below (a store remote may hold them).
    Ok(crate::gitbridge::sides_of(repo, old.as_deref(), &new).ok())
}

/// The changes of a state against its first parent, and the two sides.
fn state_changes(repo: &Repo, s: &StateRec) -> Result<(Entries, Entries, Vec<Change>)> {
    repo.source_state(&s.id)?;
    if let Some(sides) = git_sides(repo, s)? {
        let changes = sides.changes.into_iter().filter(|c| !skip_path(&c.path)).collect();
        return Ok((Entries::Git(sides.old), Entries::Git(sides.new), changes));
    }
    let root = repo.state_src(&s.id)?;
    match s.parents.first() {
        Some(p) => {
            let pr = repo.state_src(p)?;
            let v = changes::file_changes(&pr, &root)?.into_iter().filter(|c| !skip_path(&c.path)).collect();
            let v = changes::with_renames(&pr, &root, v);
            Ok((Entries::Tree(pr), Entries::Tree(root), v))
        }
        None => {
            let e = empty_root()?;
            let v = all_files(&root)?;
            Ok((Entries::Tree(snap(&e.path)), Entries::Tree(root), v))
        }
    }
}

/// `layr show <rev>:<path>` of an imported state that is not on disk: the entry is read from git.
fn show_git_entry(repo: &Repo, ctx: &Ctx, commit: &str, r: &str, rv: &str, p: &str) -> Result<()> {
    use crate::gitbridge::Entry;
    use std::io::Write;
    match crate::gitbridge::entry(repo, commit, p)? {
        Entry::Missing => Err(exit(128, format!("fatal: path '{p}' does not exist in '{rv}'"))),
        Entry::Dir(names) => {
            outln!(ctx, "tree {r}\n");
            for (n, d) in names.iter().filter(|(n, _)| !(p.is_empty() && n == ".git")) {
                outln!(ctx, "{n}{}", if *d { "/" } else { "" });
            }
            Ok(())
        }
        Entry::File(data) | Entry::Link(data) => {
            let _ = ctx.out.borrow_mut().write_all(&data);
            Ok(())
        }
    }
}

fn pretty_format(a: &crate::args::Parsed) -> Option<String> {
    if a.has("--oneline") {
        return Some("oneline".into());
    }
    a.get("--format").or(a.get("--pretty")).map(|s| s.to_string())
}

/// Print one state in a log or show format.
fn print_state(
    ctx: &Ctx,
    s: &StateRec,
    fmt: Option<&str>,
    decor: &Decor,
    date_style: &str,
    first: bool,
    tree: TreeOf,
    human: bool,
) -> Result<bool> {
    match fmt {
        Some("oneline") => {
            outln!(ctx, "{}{} {}", ids::short(&s.id), decor.of(&s.id), subject(s));
            Ok(false)
        }
        Some("short") => {
            if !first {
                outln!(ctx);
            }
            let label = if human { "state" } else { "commit" };
            out!(
                ctx,
                "{label} {}{}\nAuthor: {} <{}>\n\n    {}\n",
                ids::display(&s.id),
                decor.of(&s.id),
                s.author.name,
                s.author.email,
                subject(s)
            );
            Ok(true)
        }
        Some("full") | Some("fuller") | Some("medium") | None => {
            if !first {
                outln!(ctx);
            }
            ctx.print(&medium(s, decor, date_style, true, human));
            Ok(true)
        }
        Some("raw") => {
            if !first {
                outln!(ctx);
            }
            ctx.print(&medium(s, decor, "unix", true, human));
            Ok(true)
        }
        Some(f) => {
            let (body, term) = if let Some(x) = f.strip_prefix("format:") {
                (x, false)
            } else if let Some(x) = f.strip_prefix("tformat:") {
                (x, true)
            } else {
                (f, true)
            };
            if !term && !first {
                outln!(ctx);
            }
            let text = format_state(body, s, decor, date_style, tree);
            out!(ctx, "{}", text);
            if term && !text.is_empty() {
                outln!(ctx);
            }
            Ok(!text.is_empty())
        }
    }
}

fn show_spec() -> Spec {
    diff_spec()
        .optional("--format|--pretty")
        .flag("--oneline")
        .flag("-s|--no-patch")
        .value("--date")
        .flag("--decorate")
        .flag("--no-decorate")
        .flag("--abbrev-commit")
        .flag("--first-parent")
}

pub fn show(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&show_spec(), args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = h.line.clone();
    let mut revs_v: Vec<String> = a.pos.clone();
    if revs_v.is_empty() {
        revs_v.push("head".into());
    }
    repo.hold(pin_revs(repo, line.as_ref(), &revs_v)?);
    let specs = h.specs(&a.paths)?;
    let decor = Decor::load_with_head(repo, line.as_ref(), "HEAD")?;
    let date_style = a.get("--date").unwrap_or("default").to_string();
    let fmt = pretty_format(&a);
    let opts = DiffOpts::from(&a, true)?;
    let mut first = true;
    for r in revs_v {
        // <rev>:<path> and :<path> (index).
        if let Some((rv, path)) = r.split_once(':') {
            // An imported state that is not on disk: its entry is read from git, no tree is built.
            if !rv.is_empty() {
                let id = revs::resolve(repo, line.as_ref(), rv)?;
                if let Some(commit) = crate::gitbridge::unbuilt_commit(repo, &id)? {
                    let p = if path.starts_with("./") || path.starts_with("../") {
                        super::normalize(&h.loc.sub, path)?
                    } else {
                        path.trim_start_matches('/').to_string()
                    };
                    show_git_entry(repo, ctx, &commit, &r, rv, &p)?;
                    first = false;
                    continue;
                }
            }
            let root_path: PathBuf;
            let mut _keep = Vec::new();
            if rv.is_empty() {
                let l = h.line()?;
                repo.check_view(ctx, l)?;
                if let Some(filtered) = repo.filtered_view(ctx, l)? {
                    root_path = filtered.path.clone();
                    _keep.push(filtered);
                } else {
                    let (idx, t) = repo.index_view(l)?;
                    _keep.extend(t);
                    root_path = repo.complete(idx, &mut _keep)?;
                }
            } else {
                let id = revs::resolve(repo, line.as_ref(), rv)?;
                root_path = repo.state_root(&id)?;
            }
            let p = if path.starts_with("./") || path.starts_with("../") {
                super::normalize(&h.loc.sub, path)?
            } else {
                path.trim_start_matches('/').to_string()
            };
            match fsutil::lmeta(&root_path, &p) {
                None => return Err(exit(128, format!("fatal: path '{p}' does not exist in '{rv}'"))),
                Some(m) if m.is_dir() => {
                    outln!(ctx, "tree {r}\n");
                    let d = fsutil::safe_join(&root_path, &p)?;
                    let mut names: Vec<(String, bool)> = std::fs::read_dir(d)?
                        .flatten()
                        .map(|e| {
                            (e.file_name().to_string_lossy().into_owned(), e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                        })
                        .filter(|(n, _)| !(p.is_empty() && n == ".git"))
                        .collect();
                    names.sort();
                    for (n, d) in names {
                        outln!(ctx, "{n}{}", if d { "/" } else { "" });
                    }
                }
                Some(m) if m.ftype == fsutil::FType::File => {
                    // In pieces: a file of any size.
                    fsutil::copy_file_to(&root_path, &p, &mut **ctx.out.borrow_mut())?;
                }
                Some(_) => {
                    let data = fsutil::entry_bytes(&root_path, &p)?;
                    use std::io::Write;
                    let _ = ctx.out.borrow_mut().write_all(&data);
                }
            }
            first = false;
            continue;
        }
        let id = revs::resolve(repo, line.as_ref(), &r)?;
        let s = repo.state(&id)?;
        let header_ends_blank = print_state(
            ctx,
            &s,
            fmt.as_deref(),
            &decor,
            &date_style,
            first,
            &|id| crate::gitbridge::tree_of(repo, id).ok(),
            false,
        )?;
        first = false;
        if a.has("-s") {
            continue;
        }
        let (old, new, list) = state_changes(repo, &s)?;
        let list: Vec<Change> = list.into_iter().filter(|c| matches(&specs, &c.path)).collect();
        if list.is_empty() {
            continue;
        }
        let fds = diff::file_diffs(&old, &new, &list)?;
        if header_ends_blank && (opts.patch || opts.stat || opts.name_only || opts.name_status || opts.numstat || opts.shortstat)
        {
            outln!(ctx);
        }
        print_diffs(ctx, &fds, &opts);
    }
    Ok(0)
}

fn log_spec() -> Spec {
    show_spec()
        .flag("--json")
        .value("-n|--max-count")
        .flag("--all")
        .flag("--reverse")
        .flag("--graph")
        .value("--skip")
        .value("--since|--after")
        .value("--until|--before")
        .value("--author")
        .value("--grep")
        .flag("--no-merges")
        .flag("--merges")
        .flag("--auto")
        .flag("--follow")
        .flag("-i|--regexp-ignore-case")
}

fn parse_since(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis());
    }
    let parts: Vec<&str> = s.split([' ', '.']).filter(|x| !x.is_empty()).collect();
    if parts.len() >= 2 {
        let n: i64 = parts[0].parse().ok()?;
        let unit = parts[1].trim_end_matches('s');
        let secs = match unit {
            "second" => 1,
            "minute" => 60,
            "hour" => 3600,
            "day" => 86400,
            "week" => 7 * 86400,
            "month" => 30 * 86400,
            "year" => 365 * 86400,
            _ => return None,
        };
        return Some(ids::now_ms() - n * secs * 1000);
    }
    None
}

pub fn log(ctx: &Ctx, args: &[String]) -> Result<i32> {
    if args.iter().any(|v| v == "--remote" || v.starts_with("--remote=")) {
        return crate::store_remote::command(ctx, "log", args);
    }
    let a = parse(&log_spec(), args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = h.line.clone();
    let (revs_v, paths) = split_revs(repo, line.as_ref(), &a)?;
    repo.hold(pin_revs(repo, line.as_ref(), &revs_v)?);
    let specs = h.specs(&paths)?;
    let mut include: Vec<String> = Vec::new();
    let mut exclude: BTreeSet<String> = BTreeSet::new();
    let mut add_excl = |id: &str| -> Result<()> {
        for x in repo.ancestors(id)? {
            exclude.insert(x);
        }
        Ok(())
    };
    for r in &revs_v {
        if let Some((x, y)) = r.split_once("...") {
            let xi = revs::resolve(repo, line.as_ref(), if x.is_empty() { "head" } else { x })?;
            let yi = revs::resolve(repo, line.as_ref(), if y.is_empty() { "head" } else { y })?;
            include.push(xi.clone());
            include.push(yi.clone());
            if let Some(b) = repo.merge_base(&xi, &yi)? {
                add_excl(&b)?;
            }
        } else if let Some((x, y)) = r.split_once("..") {
            let xi = revs::resolve(repo, line.as_ref(), if x.is_empty() { "head" } else { x })?;
            let yi = revs::resolve(repo, line.as_ref(), if y.is_empty() { "head" } else { y })?;
            add_excl(&xi)?;
            include.push(yi);
        } else if let Some(x) = r.strip_prefix('^') {
            add_excl(&revs::resolve(repo, line.as_ref(), x)?)?;
        } else {
            include.push(revs::resolve(repo, line.as_ref(), r)?);
        }
    }
    if a.has("--all") {
        for l in repo.lines()? {
            include.push(l.head);
        }
        for (_, s, _) in repo.p.db.tags()? {
            include.push(s);
        }
        for (_, s, _) in repo.p.db.remote_refs()? {
            include.push(s);
        }
    }
    if include.is_empty() {
        include.push(h.line()?.head.clone());
    }
    if a.has("--auto") {
        if let Some(l) = &line {
            for s in repo.p.db.line_states(&l.id, StateKind::Auto)? {
                include.push(s.id);
            }
        }
    }
    // File history and browser history share traversal and first-parent change selection.
    let first_parent = a.has("--first-parent");
    if !a.has("--all") {
        for id in &include {
            if !exclude.contains(id) && repo.p.db.state(id)?.is_some_and(|s| s.kind.auxiliary()) {
                repo.source_state(id)?;
            }
        }
    }
    let max: usize = a.get("-n").map(|v| v.parse()).transpose()?.unwrap_or(usize::MAX);
    let skip: usize = a.get("--skip").map(|v| v.parse()).transpose()?.unwrap_or(0);
    let since = a.get("--since").and_then(parse_since);
    let until = a.get("--until").and_then(parse_since);
    let author = a.get("--author").map(|s| regex::RegexBuilder::new(s).case_insensitive(a.has("-i")).build()).transpose()?;
    let grep = a.get("--grep").map(|s| regex::RegexBuilder::new(s).case_insensitive(a.has("-i")).build()).transpose()?;
    if max == 0 {
        return Ok(0);
    }
    let mut picked: Vec<StateRec> = Vec::new();
    let mut skipped = 0;
    layr_model::history::file_history(
        &include,
        &exclude,
        first_parent,
        |id| repo.p.db.state(id),
        |s| {
            if a.has("--no-merges") && s.parents.len() > 1 || a.has("--merges") && s.parents.len() < 2 {
                return Ok(Vec::new());
            }
            if since.map(|t| s.time < t).unwrap_or(false) || until.map(|t| s.time > t).unwrap_or(false) {
                return Ok(Vec::new());
            }
            if let Some(r) = &author {
                if !r.is_match(&format!("{} <{}>", s.author.name, s.author.email)) {
                    return Ok(Vec::new());
                }
            }
            if let Some(r) = &grep {
                if !r.is_match(&s.message) {
                    return Ok(Vec::new());
                }
            }
            if specs.is_empty() {
                return Ok(vec![String::new()]);
            }
            if !crate::gitbridge::readable(repo, &s.id)? {
                return Ok(Vec::new());
            }
            let (_o, _n, list) = state_changes(repo, s)?;
            Ok(list.into_iter().flat_map(|c| std::iter::once(c.path).chain(c.old_path)).collect())
        },
        |path| specs.is_empty() || matches(&specs, path),
        |s| {
            if picked.len() >= max && !a.has("--reverse") {
                return Ok(false);
            }
            if skipped < skip {
                skipped += 1;
                return Ok(true);
            }
            picked.push(s);
            if picked.len() >= max && !a.has("--reverse") {
                return Ok(false);
            }
            Ok(true)
        },
    )?;
    if a.has("--reverse") {
        picked.truncate(max);
        picked.reverse();
    }
    if a.has("--json") {
        for state in &picked {
            outln!(ctx, "{}", serde_json::to_string(state)?);
        }
        return Ok(0);
    }
    let decor = Decor::load_with_head(repo, line.as_ref(), if a.has("--format") { "HEAD" } else { "head" })?;
    let decor = if a.has("--decorate") || a.get("--format").map(|f| f.contains("%d") || f.contains("%D")).unwrap_or(false) {
        decor
    } else {
        Decor { map: HashMap::new(), ..decor }
    };
    let date_style = a.get("--date").unwrap_or("default").to_string();
    let fmt = pretty_format(&a);
    let opts = DiffOpts::from(&a, false)?;
    let want_diff = opts.patch || opts.stat || opts.numstat || opts.name_only || opts.name_status || opts.shortstat;
    let mut first = true;
    for s in &picked {
        let blank = print_state(
            ctx,
            s,
            fmt.as_deref(),
            &decor,
            &date_style,
            first,
            &|id| crate::gitbridge::tree_of(repo, id).ok(),
            !a.has("--format"),
        )?;
        first = false;
        if want_diff && crate::gitbridge::readable(repo, &s.id)? {
            let (old, new, list) = state_changes(repo, s)?;
            let list: Vec<Change> = list.into_iter().filter(|c| matches(&specs, &c.path)).collect();
            if !list.is_empty() {
                if blank {
                    outln!(ctx);
                }
                let fds = diff::file_diffs(&old, &new, &list)?;
                print_diffs(ctx, &fds, &opts);
            }
        }
    }
    Ok(0)
}

pub fn rev_parse(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .optional("--short")
        .flag("--show-toplevel")
        .optional("--abbrev-ref")
        .flag("--git-dir")
        .flag("--absolute-git-dir")
        .flag("--is-inside-work-tree")
        .flag("--is-inside-git-dir")
        .flag("--is-bare-repository")
        .flag("--verify")
        .flag("-q|--quiet")
        .flag("--show-prefix")
        .flag("--show-cdup")
        .flag("--symbolic-full-name")
        .flag("--git")
        .flag("--git-common-dir");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = h.line.clone();
    if a.has("--show-toplevel") {
        outln!(ctx, "{}", repo.working(h.line()?).display());
    }
    if a.has("--is-inside-work-tree") {
        outln!(ctx, "{}", if line.is_some() { "true" } else { "false" });
    }
    if a.has("--is-inside-git-dir") || a.has("--is-bare-repository") {
        outln!(ctx, "false");
    }
    if a.has("--git-dir") || a.has("--absolute-git-dir") || a.has("--git-common-dir") {
        let g = repo.working(h.line()?).join(".git");
        if !g.exists() {
            return Err(exit(128, "fatal: this line has no .git folder"));
        }
        outln!(ctx, "{}", g.display());
    }
    if a.has("--show-prefix") {
        outln!(ctx, "{}", if h.loc.sub.is_empty() { String::new() } else { format!("{}/", h.loc.sub) });
    }
    if a.has("--show-cdup") {
        let n = if h.loc.sub.is_empty() { 0 } else { h.loc.sub.split('/').count() };
        outln!(ctx, "{}", "../".repeat(n));
    }
    let short: Option<usize> = match a.get("--short") {
        Some("") => Some(7),
        Some(n) => Some(n.parse()?),
        None => None,
    };
    for r in &a.pos {
        revs::reject_old(r)?;
        if a.has("--abbrev-ref") || a.has("--symbolic-full-name") {
            if r == "head" || r == "@" {
                let n = h.line()?.name.clone();
                outln!(ctx, "{}", if a.has("--symbolic-full-name") { format!("refs/heads/{n}") } else { n });
                continue;
            }
            if r.ends_with("@{upstream}") || r.ends_with("@{u}") {
                let n = h.line()?.name.clone();
                if repo.p.db.remote_ref(&format!("origin/{n}"))?.is_some() {
                    outln!(ctx, "origin/{n}");
                    continue;
                }
                return Err(exit(128, format!("fatal: no upstream configured for line '{n}'")));
            }
            outln!(ctx, "{r}");
            continue;
        }
        if let Some((rv, path)) = r.split_once(':').filter(|(rv, _)| !rv.is_empty()) {
            // <rev>:<path>: the git object id of the path in the commit a push exports for the
            // state (a tree for a folder, a blob for a file). Relative paths as in `show`.
            let rv_state = rv.trim_end_matches("^{tree}").trim_end_matches("^{commit}").trim_end_matches("^{}");
            let obj = revs::resolve(repo, line.as_ref(), rv_state).and_then(|id| {
                let p = if path.starts_with("./") || path.starts_with("../") {
                    super::normalize(&h.loc.sub, path)?
                } else {
                    path.trim_start_matches('/').to_string()
                };
                Ok((crate::gitbridge::object_of(repo, &id, &p)?, p))
            });
            match obj {
                Ok((Some(o), _)) => outln!(ctx, "{}", &o[..short.unwrap_or(o.len()).clamp(4, o.len())]),
                Ok((None, _)) | Err(_) if a.has("-q") => return Ok(1),
                Ok((None, p)) => return Err(exit(128, format!("fatal: path '{p}' does not exist in '{rv}'"))),
                Err(e) => return Err(exit(128, format!("fatal: {e}"))),
            }
            continue;
        }
        if let Some(rr) = r.strip_suffix("^{tree}") {
            // The git tree hash of the state's content: the same on every machine and in git.
            let tree = revs::resolve(repo, line.as_ref(), rr).and_then(|id| crate::gitbridge::tree_of(repo, &id));
            match tree {
                Ok(t) => outln!(ctx, "{}", &t[..short.unwrap_or(t.len()).clamp(4, t.len())]),
                Err(e) => {
                    if a.has("-q") {
                        return Ok(1);
                    }
                    return Err(exit(128, format!("fatal: {e}")));
                }
            }
            continue;
        }
        let rr = r.trim_end_matches("^{commit}").trim_end_matches("^{}");
        match revs::resolve(repo, line.as_ref(), rr) {
            // --git: the git commit of the state, not its layr id.
            Ok(id) if a.has("--git") => match crate::gitbridge::commit_of(repo, &id)? {
                Some(c) => outln!(ctx, "{}", &c[..short.unwrap_or(c.len()).clamp(4, c.len())]),
                None if a.has("-q") => return Ok(1),
                None => {
                    return Err(exit(
                        128,
                        format!("fatal: state {} has no git commit yet; 'layr git push' makes one", ids::short(&id)),
                    ))
                }
            },
            Ok(id) => match short {
                Some(n) => outln!(ctx, "{}", &ids::display(&id)[..n.clamp(4, 32)]),
                None => outln!(ctx, "{}", ids::display(&id)),
            },
            Err(e) => {
                if a.has("-q") {
                    return Ok(1);
                }
                if a.has("--verify") {
                    return Err(exit(128, "fatal: Needed a single revision"));
                }
                return Err(exit(128, format!("fatal: {e}")));
            }
        }
    }
    Ok(0)
}

pub fn merge_base(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("--is-ancestor").flag("-a|--all").flag("--fork-point");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let line = h.line.clone();
    if a.pos.len() < 2 {
        return Err(exit(128, "usage: layr merge-base [--is-ancestor] <a> <b>"));
    }
    let x = revs::resolve(&h.repo, line.as_ref(), &a.pos[0])?;
    let y = revs::resolve(&h.repo, line.as_ref(), &a.pos[1])?;
    if a.has("--is-ancestor") {
        return Ok(if h.repo.is_ancestor(&x, &y)? { 0 } else { 1 });
    }
    match h.repo.merge_base(&x, &y)? {
        Some(b) => {
            outln!(ctx, "{}", ids::display(&b));
            Ok(0)
        }
        None => Ok(1),
    }
}

pub fn blame(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .value("-L")
        .flag("-s")
        .flag("-e|--show-email")
        .flag("-l")
        .flag("--porcelain")
        .flag("-w")
        .flag("-M")
        .flag("-C");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = h.line.clone();
    let (rev, path) = match (a.pos.len(), a.paths.len()) {
        (_, 1) => (a.pos.first().cloned(), a.paths[0].clone()),
        (1, 0) => (None, a.pos[0].clone()),
        (2, 0) => (Some(a.pos[0].clone()), a.pos[1].clone()),
        _ => return Err(exit(128, "usage: layr blame [-L <a>,<b>] [<rev>] [--] <file>")),
    };
    let path = h.specs(&[path])?.remove(0);
    // Versions of the file along first parents; the working content when no revision is given.
    let start = revs::resolve(repo, line.as_ref(), rev.as_deref().unwrap_or("head"))?;
    let mut versions: Vec<(Option<StateRec>, Vec<u8>)> = Vec::new();
    // Versions are held in memory: files of at most MAX_TEXT bytes, at most 4 * MAX_TEXT bytes in
    // all. Older history is cut there, and its lines belong to the oldest version read.
    let mut held = 0u64;
    let too_large = || exit(128, format!("fatal: {path} is larger than {} MiB", fsutil::MAX_TEXT >> 20));
    let mut _scan = None;
    if rev.is_none() {
        if let Some(l) = &line {
            repo.check_view(ctx, l)?;
            let scan = match repo.filtered_view(ctx, l)? {
                Some(view) => view,
                None => repo.scan(l)?,
            };
            if fsutil::lmeta(&scan.path, &path).map(|m| m.size > fsutil::MAX_TEXT).unwrap_or(false) {
                return Err(too_large());
            }
            if let Ok(d) = fsutil::read_file(&scan.path, &path) {
                held += d.len() as u64;
                versions.push((None, d));
            }
            _scan = Some(scan);
        }
    }
    let mut cur = Some(start.clone());
    while let Some(id) = cur {
        let s = repo.state(&id)?;
        // A state that is not on disk: its file is read from git. Otherwise the file is read through
        // the layers of a thin state, with no view of each state of the history.
        let (size, data) = match crate::gitbridge::unbuilt_commit(repo, &id)? {
            Some(commit) => match crate::gitbridge::entry(repo, &commit, &path)? {
                crate::gitbridge::Entry::File(d) | crate::gitbridge::Entry::Link(d) => (Some(d.len() as u64), Some(d)),
                _ => (None, None),
            },
            None => {
                let root = match repo.state_src(&id) {
                    Ok(r) => r,
                    Err(_) => break,
                };
                (fsutil::lmeta(&root, &path).map(|m| m.size), fsutil::read_file(&root, &path).ok())
            }
        };
        if versions.is_empty() && size.map(|n| n > fsutil::MAX_TEXT).unwrap_or(false) {
            return Err(too_large());
        }
        match data.ok_or(()) {
            Ok(d) => {
                if versions.last().map(|v| v.1 != d).unwrap_or(true) {
                    held += d.len() as u64;
                    if held > 4 * fsutil::MAX_TEXT && !versions.is_empty() {
                        break;
                    }
                    versions.push((Some(s.clone()), d));
                } else if let Some(last) = versions.last_mut() {
                    if last.0.is_some() {
                        last.0 = Some(s.clone());
                    }
                }
            }
            Err(()) => break,
        }
        cur = s.parents.first().cloned();
    }
    if versions.is_empty() {
        return Err(exit(128, format!("fatal: no such path '{path}'")));
    }
    let versions: Vec<_> = versions.into_iter().map(|(state, bytes)| layr_model::history::FileVersion { state, bytes }).collect();
    let attributed = layr_model::history::blame(&versions)?;
    let lines: Vec<_> = attributed.iter().map(|l| l.text.as_str()).collect();
    let owner: Vec<_> = attributed.iter().map(|l| l.version).collect();
    let (from, to) = match a.get("-L") {
        Some(r) => {
            let (x, y) = r.split_once(',').unwrap_or((r, r));
            let x: usize = x.parse().unwrap_or(1);
            let y: usize = if let Some(n) = y.strip_prefix('+') {
                x + n.parse::<usize>().unwrap_or(1) - 1
            } else {
                y.parse().unwrap_or(lines.len())
            };
            (x.max(1), y.min(lines.len()))
        }
        None => (1, lines.len()),
    };
    let width = lines.len().to_string().len();
    let name_w =
        versions.iter().filter_map(|v| v.state.as_ref().map(|s| s.author.name.chars().count())).max().unwrap_or(10).max(10);
    for i in from.saturating_sub(1)..to {
        let v = &versions[owner[i]];
        let text = lines[i].trim_end_matches('\n');
        match &v.state {
            None => {
                if a.has("-s") {
                    outln!(ctx, "00000000 {:>w$}) {}", i + 1, text, w = width);
                } else {
                    outln!(
                        ctx,
                        "00000000 ({:<nw$} {} {:>w$}) {}",
                        "Not Committed Yet",
                        fmt_date(ids::now_ms(), "iso"),
                        i + 1,
                        text,
                        nw = name_w,
                        w = width
                    );
                }
            }
            Some(s) => {
                let boundary = owner[i] == versions.len() - 1 && s.parents.is_empty();
                let idn = &ids::display(&s.id)[..if boundary { 7 } else { 8 }];
                let pre = if boundary { "^" } else { "" };
                if a.has("-s") {
                    outln!(ctx, "{pre}{idn} {:>w$}) {}", i + 1, text, w = width);
                } else {
                    let who = if a.has("-e") { format!("<{}>", s.author.email) } else { s.author.name.clone() };
                    outln!(
                        ctx,
                        "{pre}{idn} ({:<nw$} {} {:>w$}) {}",
                        who,
                        fmt_date(s.time, "iso"),
                        i + 1,
                        text,
                        nw = name_w,
                        w = width
                    );
                }
            }
        }
    }
    Ok(0)
}

fn search_root(repo: &Repo, line: Option<&Line>, rev: Option<&str>) -> Result<(PathBuf, Vec<Temp>, Option<PathBuf>)> {
    match rev {
        Some(r) => {
            let id = revs::resolve(repo, line, r)?;
            Ok((repo.state_root(&id)?, Vec::new(), None))
        }
        None => {
            // A read-only snapshot of the working content: the live folder belongs to its owner
            // and can change while the service (root) reads it.
            let l = match line {
                Some(l) => l,
                None => bail!("not in a line"),
            };
            let (idx, it) = repo.index_view(l)?;
            let scan = repo.scan(l)?;
            let root = scan.path.clone();
            let mut keep = vec![scan];
            keep.extend(it);
            let idx = repo.complete(idx, &mut keep)?;
            Ok((root, keep, Some(idx)))
        }
    }
}

/// A temporary folder with the ignore files (`.gitignore`, `.layrignore`) of a git commit, where the
/// ignore rules of `grep` read them. Removed when the returned guard drops.
fn ignore_folder(repo: &Repo, commit: &str) -> Result<Temp> {
    let dir = repo.p.scan_dir().join(crate::store::temp_name());
    std::fs::create_dir_all(&dir)?;
    let guard = Temp { path: dir.clone() };
    let ignores: Vec<(String, String)> = crate::gitbridge::regular_files(repo, commit)?
        .into_iter()
        .filter(|(p, _)| p.ends_with(".gitignore") || p.ends_with(".layrignore"))
        .collect();
    let oids: Vec<String> = ignores.iter().map(|(_, o)| o.clone()).collect();
    for ((path, _), data) in ignores.iter().zip(crate::gitbridge::blobs(repo, &oids)?) {
        let target = dir.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, data)?;
    }
    Ok(guard)
}

pub fn grep(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .flag("-n|--line-number")
        .flag("-i|--ignore-case")
        .flag("-w|--word-regexp")
        .flag("-l|--files-with-matches|--name-only")
        .flag("-L|--files-without-match")
        .flag("-c|--count")
        .flag("-F|--fixed-strings")
        .flag("-E|--extended-regexp")
        .flag("-G|--basic-regexp")
        .flag("-P|--perl-regexp")
        .flag("-v|--invert-match")
        .flag("-h")
        .flag("-H")
        .flag("-q|--quiet")
        .flag("--untracked")
        .flag("-I")
        .flag("--cached")
        .flag("-z|--null")
        .value("-e")
        .value("-A|--after-context")
        .value("-B|--before-context")
        .value("-C|--context")
        .value("-m|--max-count");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = h.line.clone();
    let mut pos = a.pos.clone();
    let mut pats = a.all("-e");
    if pats.is_empty() {
        if pos.is_empty() {
            return Err(exit(128, "usage: layr grep [<options>] <pattern> [<rev>] [-- <path>...]"));
        }
        pats.push(pos.remove(0));
    }
    let mut rev: Option<String> = None;
    let mut paths = a.paths.clone();
    for p in pos {
        if rev.is_none() && paths.is_empty() && revs::resolve(repo, line.as_ref(), &p).is_ok() {
            rev = Some(p);
        } else {
            paths.push(p);
        }
    }
    let specs = h.specs(&paths)?;
    let joined = pats
        .iter()
        .map(|p| {
            let p = if a.has("-F") { regex::escape(p) } else { p.clone() };
            if a.has("-w") {
                format!(r"\b(?:{p})\b")
            } else {
                format!("(?:{p})")
            }
        })
        .collect::<Vec<_>>()
        .join("|");
    let re = regex::bytes::RegexBuilder::new(&joined).case_insensitive(a.has("-i")).build()?;
    if a.has("--cached") || rev.is_none() {
        if let Some(l) = &line {
            repo.check_view(ctx, l)?;
        }
    }
    let filtered = if a.has("--cached") || rev.is_none() {
        line.as_ref().map(|l| repo.filtered_view(ctx, l)).transpose()?.flatten()
    } else {
        None
    };
    // An imported state that is not on disk: its files are read from git, and only its ignore files
    // go to a temporary folder (the rules read them from a folder). No tree is built.
    let git_commit = match (&rev, filtered.is_some(), a.has("--cached")) {
        (Some(r), false, false) => {
            let id = revs::resolve(repo, line.as_ref(), r)?;
            crate::gitbridge::unbuilt_commit(repo, &id)?
        }
        _ => None,
    };
    let (root, _t, index, from_git) = if let Some(commit) = &git_commit {
        let ignore = ignore_folder(repo, commit)?;
        (ignore.path.clone(), vec![ignore], None, Some(commit.clone()))
    } else if let Some(filtered) = filtered {
        let root = filtered.path.clone();
        (root.clone(), vec![filtered], Some(root), None)
    } else if a.has("--cached") {
        let l = h.line()?;
        let (idx, t) = repo.index_view(l)?;
        let mut keep: Vec<Temp> = t.into_iter().collect();
        let idx = repo.complete(idx, &mut keep)?;
        (idx, keep, None, None)
    } else {
        let (r, t, i) = search_root(repo, line.as_ref(), rev.as_deref())?;
        (r, t, i, None)
    };
    let rules = crate::ignore_rules::Rules::new(&root);
    let prefix = rev.as_ref().map(|r| format!("{r}:")).unwrap_or_default();
    let maxc: usize = a.get("-m").map(|v| v.parse()).transpose()?.unwrap_or(usize::MAX);
    let mut found = false;
    let mut files = Vec::new();
    // Git blobs of the files of a state that is not on disk, by path.
    let mut blob_of: HashMap<String, String> = HashMap::new();
    if let Some(commit) = &from_git {
        for (path, oid) in crate::gitbridge::regular_files(repo, commit)? {
            if !skip_path(&path) && !rules.ignored(&path, false) && matches(&specs, &path) {
                blob_of.insert(path.clone(), oid);
                files.push(path);
            }
        }
    }
    if from_git.is_none() {
    fsutil::walk(&root, "", &mut |rel, m| {
        if skip_path(rel) {
            return Ok(false);
        }
        if rules.ignored(rel, m.is_dir()) {
            if let Some(idx) = &index {
                if fsutil::lmeta(idx, rel).is_none() {
                    return Ok(false);
                }
            } else {
                return Ok(false);
            }
        }
        if m.is_file() {
            if let Some(idx) = &index {
                if !a.has("--untracked") && fsutil::lmeta(idx, rel).is_none() {
                    return Ok(true);
                }
            }
            if matches(&specs, rel) {
                files.push(rel.to_string());
            }
        }
        Ok(true)
    })?;
    }
    let sep = if a.has("-z") { "\0" } else { ":" };
    // Files of a git state come in batches of blobs, not one process per file.
    let listed = files.clone();
    let mut cache: HashMap<String, Vec<u8>> = HashMap::new();
    for (idx, f) in files.into_iter().enumerate() {
        let data = match &from_git {
            Some(_) => {
                if !cache.contains_key(&f) {
                    let end = (idx + 256).min(listed.len());
                    let batch = &listed[idx..end];
                    let oids: Vec<String> = batch.iter().map(|p| blob_of[p].clone()).collect();
                    for (p, d) in batch.iter().zip(crate::gitbridge::blobs(repo, &oids)?) {
                        cache.insert(p.clone(), d);
                    }
                }
                match cache.remove(&f) {
                    Some(d) => d,
                    None => continue,
                }
            }
            None => match fsutil::read_file(&root, &f) {
                Ok(d) => d,
                Err(_) => continue,
            },
        };
        let shown = format!("{prefix}{}", if rev.is_some() { f.clone() } else { h.rel(&f) });
        if fsutil::is_binary(&data) {
            if a.has("-I") {
                continue;
            }
            let hit = re.is_match(&data) != a.has("-v");
            if hit {
                found = true;
                if a.has("-q") {
                    return Ok(0);
                }
                if a.has("-l") {
                    outln!(ctx, "{shown}");
                } else if !a.has("-L") {
                    outln!(ctx, "Binary file {shown} matches");
                }
            }
            continue;
        }
        let mut count = 0;
        let mut out_lines = Vec::new();
        for (i, l) in grep_lines(&data) {
            if re.is_match(l) != a.has("-v") {
                count += 1;
                if count <= maxc {
                    out_lines.push((i + 1, String::from_utf8_lossy(l).into_owned()));
                }
            }
        }
        if count > 0 {
            found = true;
        }
        if a.has("-q") {
            if found {
                return Ok(0);
            }
            continue;
        }
        if a.has("-L") {
            if count == 0 {
                outln!(ctx, "{shown}");
            }
            continue;
        }
        if count == 0 {
            continue;
        }
        if a.has("-l") {
            outln!(ctx, "{shown}");
        } else if a.has("-c") {
            outln!(ctx, "{shown}{sep}{count}");
        } else {
            for (n, l) in out_lines {
                let name = if a.has("-h") { String::new() } else { format!("{shown}{sep}") };
                if a.has("-n") {
                    outln!(ctx, "{name}{n}{sep}{l}");
                } else {
                    outln!(ctx, "{name}{l}");
                }
            }
        }
    }
    Ok(if found { 0 } else { 1 })
}

/// The first `ls-files` argument that names a revision, if no path was printed.
fn rev_argument<'a>(args: &'a [String], printed: usize, is_rev: impl Fn(&str) -> bool) -> Option<&'a String> {
    if printed > 0 {
        return None;
    }
    args.iter().find(|a| is_rev(a))
}

pub fn ls_files(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .flag("-c|--cached")
        .flag("-o|--others")
        .flag("-m|--modified")
        .flag("-d|--deleted")
        .flag("-i|--ignored")
        .flag("--exclude-standard")
        .flag("-z")
        .flag("-s|--stage")
        .flag("--full-name")
        .flag("--directory");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let line = h.line()?.clone();
    let filtered = h.repo.filtered_view(ctx, &line)?;
    let rest = a.rest();
    let specs = h.specs(&rest)?;
    let end = if a.has("-z") { "\0" } else { "\n" };
    let mut printed = 0usize;
    let show = |p: &str| if a.has("--full-name") { p.to_string() } else { h.rel(p) };
    let want_cached = a.has("-c") || !(a.has("-o") || a.has("-m") || a.has("-d") || a.has("-i"));
    if want_cached || a.has("-s") {
        let (idx, _t) = match &filtered {
            Some(view) => (snap(&view.path), None),
            None => h.repo.index_view(&line)?,
        };
        let mut v = Vec::new();
        fsutil::walk(&idx, "", &mut |rel, m| {
            if skip_path(rel) {
                return Ok(false);
            }
            if !m.is_dir() && matches(&specs, rel) {
                v.push((rel.to_string(), m.clone()));
            }
            Ok(true)
        })?;
        for (p, m) in v {
            if a.has("-s") {
                let id = match m.ftype {
                    fsutil::FType::File => fsutil::git_blob_id_file(&idx, &p)?,
                    _ => fsutil::git_blob_id(&fsutil::entry_bytes(&idx, &p)?),
                };
                printed += 1;
                out!(ctx, "{:06o} {} 0\t{}{}", m.git_mode(), id, show(&p), end);
            } else {
                printed += 1;
                out!(ctx, "{}{}", show(&p), end);
            }
        }
    }
    if a.has("-o") || a.has("-m") || a.has("-d") || a.has("-i") {
        let st = if filtered.is_some() { h.repo.snapshot_status(ctx, &line)? } else { h.repo.status(&line)?.0 };
        if a.has("-m") || a.has("-d") {
            for c in &st.unstaged {
                if (a.has("-m") || c.new.is_none()) && matches(&specs, &c.path) {
                    printed += 1;
                    out!(ctx, "{}{}", show(&c.path), end);
                }
            }
        }
        if a.has("-o") && !a.has("-i") {
            let list = if a.has("--directory") {
                let mut v: BTreeSet<String> = BTreeSet::new();
                for f in &st.untracked {
                    match st.untracked_dirs.iter().find(|d| f.starts_with(&format!("{d}/"))) {
                        Some(d) => v.insert(format!("{d}/")),
                        None => v.insert(f.clone()),
                    };
                }
                v.into_iter().collect()
            } else {
                st.untracked.clone()
            };
            for u in list {
                if matches(&specs, u.trim_end_matches('/')) {
                    printed += 1;
                    out!(ctx, "{}{}", show(&u), end);
                }
            }
        }
        if a.has("-i") {
            for i in &st.ignored {
                if matches(&specs, i) {
                    printed += 1;
                    out!(ctx, "{}{}", show(i), end);
                }
            }
        }
    }
    if let Some(arg) = rev_argument(&rest, printed, |r| revs::resolve(&h.repo, Some(&line), r).is_ok()) {
        return Err(exit(
            128,
            format!("ls-files takes paths of the line's index, not states: '{arg}' is a revision. To list a state, use `layr show {arg}:<folder>`."),
        ));
    }
    Ok(0)
}

/// Preserve grep's empty-file segment while omitting a final empty newline segment.
fn grep_lines(data: &[u8]) -> impl Iterator<Item = (usize, &[u8])> {
    let final_segment = data.split(|b| *b == b'\n').count() - 1;
    data.split(|b| *b == b'\n').enumerate().take_while(move |(i, l)| !(*i > 0 && *i == final_segment && l.is_empty()))
}

#[allow(dead_code)]
fn unused(_: &Here) {}

#[cfg(test)]
mod tests {
    use super::rev_argument;

    #[test]
    fn revision_argument_is_named_only_when_nothing_printed() {
        let is_rev = |a: &str| a == "0123abcd";
        let args = vec!["src/a.txt".to_string(), "0123abcd".to_string()];
        assert_eq!(rev_argument(&args, 0, is_rev).map(String::as_str), Some("0123abcd"));
        assert_eq!(rev_argument(&args, 1, is_rev), None);
        assert_eq!(rev_argument(&args[..1], 0, is_rev), None);
    }
}

#[cfg(test)]
mod grep_regression {
    use super::grep_lines;

    fn previous(data: &[u8]) -> Vec<(usize, &[u8])> {
        data.split(|b| *b == b'\n')
            .enumerate()
            .take_while(|(i, l)| !(*i > 0 && *i == data.split(|b| *b == b'\n').count() - 1 && l.is_empty()))
            .collect()
    }

    #[test]
    fn generated_grep_segments_match_previous_output() {
        for lines in 0..64 {
            for ending in [b"".as_slice(), b"\n", b"\n\n", b"tail", b"\xff"] {
                let mut data = (0..lines).flat_map(|i| format!("row {i}\n").into_bytes()).collect::<Vec<_>>();
                data.extend_from_slice(ending);
                assert_eq!(grep_lines(&data).collect::<Vec<_>>(), previous(&data));
            }
        }
    }

    #[test]
    #[ignore = "generated scaling measurement"]
    fn measure_grep_scaling() {
        for lines in [1000, 2000, 4000] {
            let data = b"generated line\n".repeat(lines);
            let start = std::time::Instant::now();
            std::hint::black_box(previous(std::hint::black_box(&data)));
            let before = start.elapsed();
            let start = std::time::Instant::now();
            std::hint::black_box(grep_lines(std::hint::black_box(&data)).collect::<Vec<_>>());
            eprintln!("grep lines={lines} previous={before:?} optimized={:?}", start.elapsed());
        }
    }
}

#[cfg(test)]
mod renderer_regression {
    use super::*;
    use std::{
        cell::RefCell,
        io::{self, Write},
        rc::Rc,
    };

    struct Capture(Rc<RefCell<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn generated_print_diffs_combines_flags_without_changing_bytes() {
        let root = std::env::temp_dir().join(format!("layr-render-regression-{}", ids::new_id()));
        let capture = Rc::new(RefCell::new(Vec::new()));
        let ctx = Ctx {
            store: crate::store::Store::open(&root).unwrap(),
            caller: crate::ctx::Caller::from_uid(rustix::process::geteuid().as_raw()),
            cwd: root.clone(),
            env: Default::default(),
            out: RefCell::new(Box::new(Capture(capture.clone()))),
        };
        let files = diff::regression::generated_files();
        let expected: serde_json::Value = serde_json::from_str(include_str!("../diff-regression.json")).unwrap();
        let patches: String = expected[6].as_array().unwrap().iter().skip(3).step_by(5).map(|v| v.as_str().unwrap()).collect();
        for combined in [false, true] {
            let mut options = DiffOpts {
                stat: true,
                numstat: combined,
                shortstat: combined,
                name_only: combined,
                name_status: combined,
                check: false,
                patch: true,
                context: 3,
                stat_width: 80,
                filter: None,
            };
            let mut output = String::new();
            if combined {
                output.push_str(expected[1].as_str().unwrap());
            }
            output.push_str(expected[0].as_str().unwrap());
            if combined {
                for i in [2, 3, 4] {
                    output.push_str(expected[i].as_str().unwrap());
                }
            }
            output.push('\n');
            output.push_str(&patches);
            assert!(!print_diffs(&ctx, &files, &options));
            assert_eq!(*capture.borrow(), output.as_bytes());
            capture.borrow_mut().clear();
            options.filter = Some("Z".into());
            assert!(!print_diffs(&ctx, &files, &options));
            assert_eq!(*capture.borrow(), b"\n");
            capture.borrow_mut().clear();
        }
        drop(ctx);
        std::fs::remove_dir_all(root).unwrap();
    }
}
