//! Overlays (docs/overlays.md): file trees with an owner, readers and their own history, shown in
//! a line above the shared set.
//!
//! An overlay is the internal project `<project>%<overlay>` below the project's private `meta`
//! folder, with its own states, signed records and access policy (its owner is its admin, its
//! readers have the reader role). History, permissions and fsck work as for any project, and
//! store pushes and peer sync carry it under its own reader rules. Its lines are pointers without
//! working folders: `user-<uid>` (shared mode: one for all lines of a user) and `line-<line id>`
//! (follow mode: one per line of the project, branched and merged with it).
//!
//! A line shows a stack: the shared set (the line's head) at the bottom and overlays above it. The
//! view of the working folder (`view.<line id>`, docs/overlays.md section 1a, slice 5) chooses the
//! glasses and their order; the setting `stack.<line id>` gives the state the folder shows on each
//! (and the glasses too, for a line made before views).
//! The working folder holds the files of all layers, the upper file winning. The base
//! (`meta/base/<line id>@<key>`, read-only) holds the same full tree for the head and the stack,
//! so status and diff still compare two snapshots of one tree in O(changes).
//!
//! A commit splits its change list by layer: a changed path belongs to the highest layer that has
//! it (a moved file stays in the layer of its old path), a new path to the shared set unless
//! `layr overlay move` named it, and a delete goes to every layer that has the path. Each changed
//! layer gets its next state from its previous one: a writable snapshot, the layer's changes as
//! reflink copies, read-only. So the shared set never holds an overlay file. Automatic states of a
//! line with overlays leave the overlay files out.

use crate::access::{Policy, Who};
use crate::changes::{self, Change};
use crate::ctx::Ctx;
use crate::fsutil;
use crate::ids;
use crate::model::{Line, LineChange, LineState, Record, StateKind, StateRec};
use crate::repo::{skip_path, Repo, Temp};
use crate::storage as btrfs;
use crate::store::Project;
use crate::tree::Tree;
use anyhow::{anyhow, bail, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub use layr_model::validation::{is_view_id, Layer, View};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// One overlay line for all lines of a user.
    Shared,
    /// The overlay line branches and merges with the line.
    Follow,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "shared" => Some(Mode::Shared),
            "follow" => Some(Mode::Follow),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Mode::Shared => "shared",
            Mode::Follow => "follow",
        }
    }
}

/// How a merge combines a follow overlay (`overlay.<name>.merge`, docs/cache-overlays.md).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MergeKind {
    /// A three-way merge with conflicts, as in the shared set.
    #[default]
    ThreeWay,
    /// Every file of both sides; the incoming side wins on the same path; no conflicts.
    UnionTheirs,
}

impl MergeKind {
    pub fn parse(s: &str) -> Option<MergeKind> {
        match s {
            "three-way" => Some(MergeKind::ThreeWay),
            "union-theirs" => Some(MergeKind::UnionTheirs),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            MergeKind::ThreeWay => "three-way",
            MergeKind::UnionTheirs => "union-theirs",
        }
    }
}

/// The properties of an overlay (docs/cache-overlays.md, section 2). Each one is the setting
/// `overlay.<name>.<property>`; a missing setting is the default, which keeps the behaviour of
/// overlays without properties.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Props {
    /// The number of states each overlay line keeps on its first-parent chain. None: all.
    pub history: Option<u32>,
    /// The overlay files are not in status, diff and add; a named save takes them from the
    /// working folder.
    pub quiet: bool,
    pub merge: MergeKind,
    /// `gc lru:<size>`: the size limit in bytes of the overlay in each working folder.
    pub gc_lru: Option<u64>,
    /// `prune=<name>|none`: the cache adapter that prunes the folder (issue #167). None: detect it.
    pub prune: Option<String>,
    /// `platform=<key>` (`<os>-<arch>`): the glass is shown and fetched only on that platform
    /// (docs/overlays.md, section 1a, slice 3). None: every platform.
    pub platform: Option<String>,
    /// `trust=protected|readers`: whose parts a line takes (docs/overlays.md, section 1a, slice 6).
    /// `protected`: a part that another user saved only from a protected line. `readers`: any
    /// part that the reader may read. None: `protected` for a build-output glass, else `readers`.
    pub trust: Option<Trust>,
}

/// The `trust` property of a glass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trust {
    Protected,
    Readers,
}

impl Trust {
    pub fn parse(s: &str) -> Option<Trust> {
        match s {
            "protected" => Some(Trust::Protected),
            "readers" => Some(Trust::Readers),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Trust::Protected => "protected",
            Trust::Readers => "readers",
        }
    }
}

/// The property names, in the order that `overlay list` shows them.
pub const PROPS: &[&str] = &["history", "quiet", "merge", "gc", "prune", "platform", "trust"];

/// A platform key as `--platform` takes it: lowercase letters, digits, `-` and `_` (for example
/// `macos-aarch64`, `linux-x86_64`).
pub fn platform_name(s: &str) -> Option<String> {
    let ok = !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    ok.then(|| s.to_string())
}

/// Whether a glass with `platform=<p>` belongs to another platform than this machine's
/// (`cache_platform()`, `<os>-<arch>`). A glass with no platform is on every platform.
pub fn other_platform_glass(platform: Option<&str>) -> bool {
    let here = crate::model::cache_platform();
    platform.is_some_and(|p| p != here && !here.starts_with(&format!("{p}-")))
}

/// The value of `prune=<name>|none`: a lowercase adapter name (letters, digits, `-`), or `none`. An adapter
/// name is not checked against the adapters here: core has no list of them. An unknown name is logged at prune time.
pub fn prune_name(s: &str) -> Option<String> {
    let ok = !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    ok.then(|| s.to_string())
}

/// `all`, `last` or a count of states (at least 1).
pub fn parse_history(s: &str) -> Option<Option<u32>> {
    match s {
        "all" => Some(None),
        "last" => Some(Some(1)),
        n => n.parse::<u32>().ok().filter(|n| *n > 0).map(Some),
    }
}

/// A size with an optional unit: `500M`, `20G`, `1T`, `4096` (bytes). Units are powers of 1024.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult) = match s.char_indices().last()? {
        (i, 'K' | 'k') => (&s[..i], 1u64 << 10),
        (i, 'M' | 'm') => (&s[..i], 1 << 20),
        (i, 'G' | 'g') => (&s[..i], 1 << 30),
        (i, 'T' | 't') => (&s[..i], 1 << 40),
        _ => (s, 1),
    };
    num.parse::<u64>().ok().filter(|n| *n > 0).and_then(|n| n.checked_mul(mult))
}

/// `none` or `lru:<size>`. The outer None is an invalid value.
pub fn parse_gc(s: &str) -> Option<Option<u64>> {
    match s {
        "none" => Some(None),
        _ => s.strip_prefix("lru:").and_then(parse_size).map(Some),
    }
}

impl Props {
    /// The setting value of one property (`PROPS`); null for the default.
    pub fn value(&self, prop: &str) -> serde_json::Value {
        use serde_json::json;
        match prop {
            "history" => self.history.map(|n| json!(n)).unwrap_or(json!(null)),
            "quiet" => {
                if self.quiet {
                    json!(true)
                } else {
                    json!(null)
                }
            }
            "merge" => {
                if self.merge == MergeKind::ThreeWay {
                    json!(null)
                } else {
                    json!(self.merge.name())
                }
            }
            "gc" => self.gc_lru.map(|n| json!(format!("lru:{n}"))).unwrap_or(json!(null)),
            "prune" => self.prune.as_ref().map(|n| json!(n)).unwrap_or(json!(null)),
            "platform" => self.platform.as_ref().map(|n| json!(n)).unwrap_or(json!(null)),
            "trust" => self.trust.map(|t| json!(t.name())).unwrap_or(json!(null)),
            _ => json!(null),
        }
    }

    /// Read the properties from the settings: `get(<property>)` gives `overlay.<name>.<property>`.
    /// An unknown value is the default, so a newer record never breaks an older binary's reads.
    pub fn from_settings(get: impl Fn(&str) -> Option<serde_json::Value>) -> Props {
        let mut p = Props::default();
        if let Some(v) = get("history") {
            p.history = v.as_u64().and_then(|n| u32::try_from(n).ok()).filter(|n| *n > 0);
        }
        p.quiet = get("quiet").and_then(|v| v.as_bool()).unwrap_or(false);
        p.merge = get("merge").and_then(|v| v.as_str().and_then(MergeKind::parse)).unwrap_or_default();
        p.gc_lru = get("gc").and_then(|v| v.as_str().and_then(parse_gc)).flatten();
        p.prune = get("prune").and_then(|v| v.as_str().and_then(prune_name));
        p.platform = get("platform").and_then(|v| v.as_str().and_then(platform_name));
        p.trust = get("trust").and_then(|v| v.as_str().and_then(Trust::parse));
        p
    }

    /// The properties that differ from the default, as `overlay list` shows them.
    pub fn describe(&self) -> Vec<String> {
        let mut out = Vec::new();
        match self.history {
            Some(1) => out.push("history=last".to_string()),
            Some(n) => out.push(format!("history={n}")),
            None => {}
        }
        if self.quiet {
            out.push("quiet".into());
        }
        if self.merge != MergeKind::ThreeWay {
            out.push(format!("merge={}", self.merge.name()));
        }
        if let Some(n) = self.gc_lru {
            out.push(format!("gc=lru:{}", show_size(n)));
        }
        if let Some(n) = &self.prune {
            out.push(format!("prune={n}"));
        }
        if let Some(p) = &self.platform {
            out.push(format!("platform={p}"));
        }
        if let Some(t) = self.trust {
            out.push(format!("trust={}", t.name()));
        }
        out
    }

    pub fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "history": self.history.map(|n| serde_json::json!(n)).unwrap_or(serde_json::json!("all")),
            "quiet": self.quiet,
            "merge": self.merge.name(),
            "gc": self.gc_lru.map(|n| format!("lru:{n}")).unwrap_or_else(|| "none".into()),
            "prune": self.prune.clone().unwrap_or_else(|| "auto".into()),
        });
        // Only when set: older readers of `overlay list --json` see the keys they know.
        if let Some(p) = &self.platform {
            v["platform"] = serde_json::json!(p);
        }
        if let Some(t) = self.trust {
            v["trust"] = serde_json::json!(t.name());
        }
        v
    }
}

/// A byte count with the largest unit that divides it (`20G`, `512M`, `4096`).
pub fn show_size(n: u64) -> String {
    for (unit, shift) in [("T", 40), ("G", 30), ("M", 20), ("K", 10)] {
        if n >= 1 << shift && n % (1 << shift) == 0 {
            return format!("{}{unit}", n >> shift);
        }
    }
    n.to_string()
}

/// The overlay pass of `gc --repair`, read without locks (`Repo::overlay_plan`).
pub struct OverlayPlan {
    lines: Vec<Line>,
    stacks: BTreeMap<String, Vec<Layer>>,
    overlays: Vec<OverlayClean>,
}

/// One overlay of an `OverlayPlan`.
struct OverlayClean {
    name: String,
    /// The records of the overlay project when the plan read it (`Db::records_mark`).
    mark: (i64, i64),
    /// Cache overlay lines of deleted lines.
    orphans: Vec<Line>,
    keep_n: Option<usize>,
    /// States beyond the history limit that no line uses.
    excess: Vec<String>,
}

/// An overlay as the project knows it (setting `overlay.<name>`).
#[derive(Clone, Debug)]
pub struct Info {
    pub hidden: bool,
    /// A build-output glass (`regenerable`): it follows the cache paths of each line. Without a
    /// `platform`, its lines have an overlay line per platform (`cache_line_name`).
    pub cache: bool,
    /// `platform=<key>`: the glass of one platform (docs/overlays.md, section 1a, slice 3).
    pub platform: Option<String>,
    pub owner: u32,
    pub mode: Mode,
    /// The empty first state of the overlay.
    pub root: String,
}

/// Metadata and the local project opened for one overlay listing. Public overlays may have
/// metadata without a local project; hidden overlays need their project to supply the metadata.
pub(crate) struct ListedOverlay<'a> {
    pub name: String,
    pub info: Info,
    pub project: Result<Project<'a>>,
}

impl ListedOverlay<'_> {
    pub(crate) fn visible_to(&self, ctx: &Ctx) -> bool {
        !self.info.hidden
            || ctx.caller.is_root()
            || self
                .project
                .as_ref()
                .is_ok_and(|p| Policy::load(p).check(&Who::of(ctx.caller.uid, ctx.caller.gid), "read").is_ok())
    }
}

/// A glass whose parts the state records name (`StateRec::parts`, docs/overlays.md section 1a): a
/// public follow overlay that is no cache. Its overlay store gets no overlay lines for new lines; a
/// line's part on it lives in the line's stack and in the state records. Shared, hidden and cache
/// overlays keep their overlay lines (slices 5 and 6).
pub fn parts_glass(info: &Info) -> bool {
    info.mode == Mode::Follow && !info.cache && !info.hidden
}

/// The overlay line of the line `line_id` in a build-output glass: `line-<id>` in the glass of one
/// platform (slice 3), else `cache_line_name`.
pub fn glass_line_name(info: &Info, line_id: &str) -> String {
    if info.cache && info.platform.is_none() {
        cache_line_name(line_id)
    } else {
        format!("line-{line_id}")
    }
}

/// The overlay line of the line `line_id` in a cache overlay without a `platform` (made before
/// slice 3): one per platform (`<os>-<arch>`), so a
/// build on another platform does not replace the cache of this one (docs/cache-overlays.md, section 6).
pub fn cache_line_name(line_id: &str) -> String {
    format!("line-{line_id}.{}", crate::model::cache_platform())
}

/// The project line of a follow overlay line: `line-<id>`, or `line-<id>.<platform>` in a cache overlay.
pub fn line_of_overlay_line(name: &str) -> Option<&str> {
    name.strip_prefix("line-").map(|s| s.split('.').next().unwrap_or(s))
}

/// The settings key of the stack of a line.
pub fn stack_key(line_id: &str) -> String {
    format!("stack.{line_id}")
}

/// Record data that sets the stack of a line.
pub fn stack_data(line_id: &str, layers: &[Layer]) -> serde_json::Value {
    serde_json::json!({"line": line_id, "layers": layers})
}

/// The settings key of the view of the working folder of a line (docs/overlays.md, section 1a,
/// slice 5): the glasses that the folder shows, bottom first. It names glasses only; the part on
/// each glass comes from the line (its stack and its state records).
pub fn view_key(line_id: &str) -> String {
    format!("view.{line_id}")
}

/// The settings key of the project order of glasses (`layr overlay order`).
pub const ORDER_KEY: &str = "overlays.order";

/// Record data that sets the view of the working folder of a line: the names of its public
/// glasses, bottom first.
pub fn view_data(line_id: &str, glasses: &[String]) -> serde_json::Value {
    serde_json::json!({"line": line_id, "glasses": glasses})
}

/// A folder path as a folder overlay keeps it: relative, without `.`, `..` or empty parts, not
/// `.git`.
pub fn folder_path(s: &str) -> Result<String> {
    let t = s.trim_end_matches('/');
    let bad = t.is_empty()
        || t.starts_with('/')
        || t.split('/').any(|c| c.is_empty() || c == "." || c == "..")
        || t == ".git"
        || t.starts_with(".git/");
    if bad {
        return Err(crate::cmd::exit(128, format!("fatal: invalid folder '{s}': use a relative path inside the line")));
    }
    Ok(t.to_string())
}

/// The apparent size of the files below `root` (not following links, not entering other layers).
pub fn tree_size(root: &Path) -> u64 {
    let mut total = 0u64;
    let _ = fsutil::walk(root, "", &mut |_, m| {
        if !m.is_dir() {
            total += m.size;
        }
        Ok(true)
    });
    total
}

impl Repo<'_> {
    /// The folder glasses of the project on this platform: name and folder. A glass with
    /// `platform=<key>` of another platform is left out (docs/overlays.md, section 1a, slice 3).
    pub fn folder_overlays(&self) -> Result<Vec<(String, String)>> {
        Ok(self
            .all_folder_overlays()?
            .into_iter()
            .filter(|(_, _, p)| !crate::overlay::other_platform_glass(p.as_deref()))
            .map(|(n, f, _)| (n, f))
            .collect())
    }

    /// The folder glasses of every platform: name, folder and `platform`.
    pub fn all_folder_overlays(&self) -> Result<Vec<(String, String, Option<String>)>> {
        let mut out = Vec::new();
        for name in self.overlay_names()? {
            if let Some(f) = self.overlay_setting(&name, "folder")?.and_then(|v| v.as_str().map(str::to_owned)) {
                let p = self.overlay_setting(&name, "platform")?.and_then(|v| v.as_str().and_then(crate::overlay::platform_name));
                out.push((name, f, p));
            }
        }
        Ok(out)
    }

    /// The folder glass of `folder` on this platform. A glass of this platform wins over an older
    /// glass without a platform (a `cache.<folder>` overlay), which serves every platform.
    pub fn folder_overlay_of(&self, folder: &str) -> Result<Option<String>> {
        let mut found: Option<(String, bool)> = None;
        for (n, f, p) in self.all_folder_overlays()? {
            if f != folder || crate::overlay::other_platform_glass(p.as_deref()) {
                continue;
            }
            if found.as_ref().is_none_or(|(_, own)| !own && p.is_some()) {
                found = Some((n, p.is_some()));
            }
        }
        Ok(found.map(|(n, _)| n))
    }
}

/// The files (not folders) of a tree, without `.git`.
pub fn files_of(root: &(impl fsutil::Src + ?Sized)) -> Result<Vec<String>> {
    let mut v = Vec::new();
    fsutil::walk(root, "", &mut |rel, m| {
        if skip_path(rel) {
            return Ok(false);
        }
        if !m.is_dir() {
            v.push(rel.to_string());
        }
        Ok(true)
    })?;
    Ok(v)
}

fn has_file(root: &Path, p: &str) -> bool {
    fsutil::lmeta(root, p).map(|m| !m.is_dir()).unwrap_or(false)
}

/// Remove parent folders of `rel` that became empty (states hold files, as git trees do).
fn prune(t: &Tree, rel: &str) {
    let mut comps: Vec<&str> = rel.split('/').collect();
    comps.pop();
    while !comps.is_empty() && t.rmdir(&comps.join("/")) {
        comps.pop();
    }
}

/// The changes of one layer in a commit, applied to a writable snapshot of its previous state.
#[derive(Default, Debug)]
pub struct Ops {
    pub moves: Vec<(String, String)>,
    pub dels: BTreeSet<String>,
    pub puts: BTreeSet<String>,
}

impl Ops {
    pub fn is_empty(&self) -> bool {
        self.moves.is_empty() && self.dels.is_empty() && self.puts.is_empty()
    }

    /// Apply to `dst` (writable) with the content of `src`: moves first (the inode stays, so btrfs
    /// reports the move), then deletes, then reflink copies.
    pub fn apply(&self, dst: &Path, src: &Path, own: Option<(u32, u32)>) -> Result<()> {
        let t = Tree::open(dst)?;
        for (o, p) in &self.moves {
            if t.stat(o).is_some() {
                if t.stat(p).is_none() {
                    t.mkdirs_for(p, own)?;
                    t.rename(o, p)?;
                } else {
                    t.remove(o)?;
                }
                prune(&t, o);
            }
        }
        for p in &self.dels {
            if t.stat(p).is_some() {
                t.remove(p)?;
            }
            prune(&t, p);
        }
        for p in &self.puts {
            fsutil::copy_entry(src, p, dst, p, own)?;
        }
        // The times last: an entry made by a copy changes the time of its folder. Cargo compares the times of the
        // cache, so every copied entry gets the source's time, on every file system (union and snapshot alike).
        for p in &self.puts {
            fsutil::keep_times(&src.join(p), &dst.join(p))?;
        }
        // Folders too (a file copy changes the time of the folder it is in, so folders come last).
        let mut folders = Vec::new();
        fsutil::walk(src, "", &mut |rel, m| {
            if skip_path(rel) {
                return Ok(false);
            }
            if m.is_dir() && !rel.is_empty() {
                folders.push(rel.to_string());
            }
            Ok(true)
        })?;
        for f in &folders {
            if dst.join(f).is_dir() {
                fsutil::keep_times(&src.join(f), &dst.join(f))?;
            }
        }
        Ok(())
    }
}

/// Which layer gets a change: the shared set or an overlay of the stack (index, bottom first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Owner {
    Shared,
    Overlay(usize),
}

/// The split of a change list by layer.
pub struct Split {
    pub shared: Ops,
    pub overlays: Vec<Ops>,
    /// The change list of the whole tree (with renames), for the summary.
    pub list: Vec<Change>,
}

/// Split the changes from `base` (the line's full tree) to `new` by layer. `shared` and `roots`
/// are the previous states of the layers; `pending` names paths that `layr overlay move` sends to
/// an overlay.
pub fn split(base: &Path, new: &Path, shared: &Path, roots: &[PathBuf], pending: &BTreeMap<String, usize>) -> Result<Split> {
    let list: Vec<Change> = changes::file_changes(base, new)?.into_iter().filter(|c| !skip_path(&c.path)).collect();
    let list = changes::with_renames(base, new, list);
    let all = |p: &str| -> Vec<Owner> {
        let mut v: Vec<Owner> = Vec::new();
        if has_file(shared, p) {
            v.push(Owner::Shared);
        }
        for (i, r) in roots.iter().enumerate() {
            if has_file(r, p) {
                v.push(Owner::Overlay(i));
            }
        }
        v
    };
    let top = |p: &str| -> Option<Owner> { all(p).into_iter().max() };
    let mut s = Split { shared: Ops::default(), overlays: roots.iter().map(|_| Ops::default()).collect(), list: Vec::new() };
    fn ops(s: &mut Split, o: Owner) -> &mut Ops {
        match o {
            Owner::Shared => &mut s.shared,
            Owner::Overlay(i) => &mut s.overlays[i],
        }
    }
    let mut handled: BTreeSet<String> = BTreeSet::new();
    for c in &list {
        handled.insert(c.path.clone());
        let forced = pending.get(&c.path).map(|i| Owner::Overlay(*i));
        match (&c.old, &c.new, &c.old_path) {
            // A delete goes to every layer that has the path, so no lower file shows again.
            (_, None, _) => {
                for o in all(&c.path) {
                    ops(&mut s, o).dels.insert(c.path.clone());
                }
            }
            // A moved file stays in the layer of its old path: a move never publishes it.
            (Some(_), Some(_), Some(old)) => {
                handled.insert(old.clone());
                let owner = forced.or(top(old)).unwrap_or(Owner::Shared);
                for o in all(old) {
                    if o == owner {
                        ops(&mut s, o).moves.push((old.clone(), c.path.clone()));
                    } else {
                        ops(&mut s, o).dels.insert(old.clone());
                    }
                }
                ops(&mut s, owner).puts.insert(c.path.clone());
            }
            // A new path goes to the shared set, unless `overlay move` named it.
            (None, Some(_), _) => {
                ops(&mut s, forced.unwrap_or(Owner::Shared)).puts.insert(c.path.clone());
            }
            // A change goes to the highest layer that has the path.
            (Some(_), Some(_), None) => {
                let owner = forced.or(top(&c.path)).unwrap_or(Owner::Shared);
                ops(&mut s, owner).puts.insert(c.path.clone());
                if forced.is_some() {
                    for o in all(&c.path) {
                        if o != owner {
                            ops(&mut s, o).dels.insert(c.path.clone());
                        }
                    }
                }
            }
        }
    }
    // Moves of unchanged files: out of their layer, into the overlay.
    for (p, i) in pending {
        if handled.contains(p) || !has_file(new, p) {
            continue;
        }
        let owner = Owner::Overlay(*i);
        ops(&mut s, owner).puts.insert(p.clone());
        for o in all(p) {
            if o != owner {
                ops(&mut s, o).dels.insert(p.clone());
            }
        }
    }
    s.list = list;
    Ok(s)
}

/// The key of a line's base: its head and stack.
fn base_key(head: &str, layers: &[Layer]) -> String {
    use layr_hashers::Digest;
    let text = serde_json::json!({"head": head, "layers": layers}).to_string();
    hex::encode(layr_hashers::ContentAddress::digest(text.as_bytes()))[..16].to_string()
}

impl<'a> Repo<'a> {
    pub fn view_of(&self, line: &Line) -> Result<View> {
        let view = View::new(line.head.clone(), self.layers(line)?);
        view.validate()?;
        Ok(view)
    }

    pub fn saved_view(&self, id: &str) -> Result<View> {
        if !is_view_id(id) {
            bail!("invalid view id '{id}'");
        }
        let mut view = self.p.db.saved_view(id)?;
        if view.is_none() {
            let mut components = BTreeMap::new();
            let mut shared = None;
            let mut public = Vec::new();
            for name in self.overlay_names()? {
                let op = self.overlay_project(&name)?;
                for line in op.export_jsonl()?.lines() {
                    let record = crate::oplog::parse_line(line, None)?.record;
                    if record.op != "view.hidden" || record.data["view_id"] != id {
                        continue;
                    }
                    let layer: Layer = serde_json::from_value(record.data["layer"].clone())?;
                    if layer.overlay != name {
                        bail!("hidden view component belongs to another overlay");
                    }
                    let position = record.data["position"].as_u64().ok_or_else(|| anyhow!("hidden view has no position"))?;
                    components.insert(position, layer);
                    shared = record.data["shared"].as_str().map(str::to_owned);
                    public = serde_json::from_value(record.data["public"].clone())?;
                }
            }
            if let Some(shared) = shared {
                for (position, layer) in components {
                    public.insert((position as usize).min(public.len()), layer);
                }
                view = Some(View::new(shared, public));
            }
        }
        let view = view.ok_or_else(|| anyhow!("unknown view '{id}'"))?;
        view.validate()?;
        if view.id()? != id {
            bail!("view descriptor does not match its id");
        }
        Ok(view)
    }

    /// The caller holds the project lock. Saving a descriptor never captures working files.
    pub fn save_view(&self, ctx: &Ctx, view: &View) -> Result<String> {
        self.check_view_access(ctx, view)?;
        let id = view.id()?;
        let public = self.public_layers(&view.layers)?;
        if public.len() != view.layers.len() {
            for (position, layer) in view.layers.iter().enumerate() {
                if !self.need_overlay(&layer.overlay)?.hidden {
                    continue;
                }
                let orepo = self.overlay_repo(&layer.overlay)?;
                let _lock = orepo.p.lock()?;
                if orepo.p.export_jsonl()?.lines().any(|line| {
                    crate::oplog::parse_line(line, None)
                        .is_ok_and(|s| s.record.op == "view.hidden" && s.record.data["view_id"] == id)
                }) {
                    continue;
                }
                let mut record = Record::new("view.hidden");
                record.data =
                    serde_json::json!({"view_id":id,"shared":view.shared,"public":public,"position":position,"layer":layer});
                orepo.commit_record(ctx, record)?;
            }
        } else if self.p.db.saved_view(&id)?.is_none() {
            let mut record = Record::new("view.save");
            record.data = serde_json::json!({"view_id":id,"view":view});
            self.commit_record(ctx, record)?;
        }
        Ok(id)
    }

    pub fn check_view_collected(&self, view: &View) -> Result<()> {
        let id = view.id()?;
        let collected = |repo: &Repo, state: &str| -> Result<()> {
            if repo.p.db.state_flags(state)?.is_some_and(|(_, deleted)| deleted) {
                bail!("view {id} refers to a collected state {state}");
            }
            Ok(())
        };
        collected(self, &view.shared)?;
        for layer in &view.layers {
            if let Ok(repo) = self.overlay_repo(&layer.overlay) {
                collected(&repo, &layer.state)?;
            }
        }
        Ok(())
    }

    pub(crate) fn prepare_view_layers(&self, ctx: &Ctx, line: &Line, view: &View) -> Result<()> {
        self.check_view_read(ctx, view)?;
        self.check_stack(line, &view.layers)?;
        for layer in &view.layers {
            let info = self.need_overlay(&layer.overlay)?;
            if parts_glass(&info) {
                continue;
            }
            let repo = self.overlay_repo(&layer.overlay)?;
            let _lock = repo.p.lock()?;
            repo.ensure_overlay_line(ctx, &Self::overlay_line_name(&info, &line.id, line.owner), &layer.state, line.owner)?;
        }
        Ok(())
    }

    pub fn create_line_view(&self, ctx: &Ctx, name: &str, view: &View, owner: u32) -> Result<Line> {
        Ok(self.create_named_lines(ctx, vec![name.to_string()], &view.shared, owner, None, Some(view))?.remove(0))
    }

    pub fn materialize_view(&self, ctx: &Ctx, line: &Line, view: &View, dst: &Path) -> Result<()> {
        self.check_view_read(ctx, view)?;
        let assembled = self.assemble(line, &view.shared, &view.layers)?;
        let work = self.temp_work(&assembled.path)?;
        self.prepare_at(line, &view.shared, &work.path, &[])?;
        crate::fsutil::rename_tree(&work.path, dst)
    }

    /// Descriptor metadata remains usable after its source snapshots are collected.
    pub(crate) fn check_view_access(&self, ctx: &Ctx, view: &View) -> Result<()> {
        self.can_read(ctx)?;
        view.validate()?;
        for layer in &view.layers {
            if !self.reads_overlay(&layer.overlay, ctx.caller.uid, ctx.caller.gid).unwrap_or(false) {
                bail!("permission denied: you do not read overlay '{}'", layer.overlay);
            }
        }
        Ok(())
    }

    /// Materialization also requires every source snapshot to remain available.
    pub fn check_view_read(&self, ctx: &Ctx, view: &View) -> Result<()> {
        self.check_view_access(ctx, view)?;
        for layer in &view.layers {
            if self.layer_root(layer).is_none() {
                bail!("overlay '{}' state {} is not on this machine", layer.overlay, layer.state);
            }
        }
        self.state_root(&view.shared)?;
        Ok(())
    }

    // ----- overlays of the project -----

    pub fn overlay_info(&self, name: &str) -> Result<Option<Info>> {
        self.open_overlay_info(name).map(|(info, _)| info)
    }

    /// Retain the project that supplied hidden metadata, so a read caller can use it again.
    fn open_overlay_info(&self, name: &str) -> Result<(Option<Info>, Option<Project<'a>>)> {
        if crate::store::check_name("overlay", name).is_err() {
            return Ok((None, None));
        }
        // A removed cache overlay (`layr overlay remove`) is gone, also from the history of its own
        // project, which would otherwise name it again.
        if self.p.db.overlay_config(&format!("overlay.{name}.removed"))?.is_some() {
            return Ok((None, None));
        }
        let cache_key = format!("overlay.{name}.cache");
        let platform_key = format!("overlay.{name}.platform");
        let mut value = self.p.db.overlay_config(&format!("overlay.{name}"))?;
        let mut cache = None;
        let mut platform = None;
        let mut project = None;
        if value.is_none() {
            project = self.overlay_project(name).ok();
            if let Some(op) = &project {
                if op.db.overlay_config("overlay")?.is_some_and(|v| v["project"] == self.p.id && v["name"] == name) {
                    value = op.db.overlay_config(&format!("overlay.{name}"))?;
                    cache = op.db.overlay_config(&cache_key)?;
                    platform = op.db.overlay_config(&platform_key)?;
                }
            }
        } else {
            cache = self.p.db.overlay_config(&cache_key)?;
            platform = self.p.db.overlay_config(&platform_key)?;
        }
        let v = match value {
            Some(v) if v.is_object() => v,
            _ => return Ok((None, project)),
        };
        Ok((
            Some(Info {
                hidden: v["hidden"].as_bool().unwrap_or(false),
                cache: cache.and_then(|c| c.as_bool()).unwrap_or(false),
                platform: platform.and_then(|p| p.as_str().and_then(platform_name)),
                owner: v["owner"].as_u64().unwrap_or(0) as u32,
                mode: v["mode"].as_str().and_then(Mode::parse).unwrap_or(Mode::Shared),
                root: v["root"].as_str().unwrap_or("").to_string(),
            }),
            project,
        ))
    }

    pub fn need_overlay(&self, name: &str) -> Result<Info> {
        self.overlay_info(name)?
            .ok_or_else(|| crate::cmd::exit(1, format!("error: no overlay '{name}' in project {}", self.p.name)))
    }

    /// The setting `overlay.<name>.<suffix>`: in the project's records, or for a hidden overlay
    /// in the overlay's own records.
    pub fn overlay_setting(&self, name: &str, suffix: &str) -> Result<Option<serde_json::Value>> {
        let project = self.hidden_settings_project(name)?;
        project.as_ref().unwrap_or(&self.p).db.overlay_config(&format!("overlay.{name}.{suffix}"))
    }

    /// Keep the project that supplied hidden metadata while reading its settings.
    fn hidden_settings_project(&self, name: &str) -> Result<Option<Project<'a>>> {
        let (info, project) = self.open_overlay_info(name)?;
        let info = info.ok_or_else(|| crate::cmd::exit(1, format!("error: no overlay '{name}' in project {}", self.p.name)))?;
        if info.hidden {
            return project.map(Ok).unwrap_or_else(|| self.overlay_project(name)).map(Some);
        }
        Ok(None)
    }

    /// The properties of an overlay (docs/cache-overlays.md, section 2).
    pub fn overlay_props(&self, name: &str) -> Result<Props> {
        let project = self.hidden_settings_project(name)?;
        let db = &project.as_ref().unwrap_or(&self.p).db;
        let mut values = BTreeMap::new();
        for prop in PROPS {
            if let Some(v) = db.overlay_config(&format!("overlay.{name}.{prop}"))? {
                values.insert(*prop, v);
            }
        }
        Ok(Props::from_settings(|k| values.get(k).cloned()))
    }

    /// The files of the quiet overlays of a line's stack and the paths that `overlay move` sends
    /// to them: `status`, `diff` and `add` leave them out, and a named save takes them from the
    /// working folder.
    pub fn quiet_paths(&self, line: &Line) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        if self.p.is_overlay() {
            return Ok(out);
        }
        let layers = self.layers(line)?;
        let mut quiet = BTreeSet::new();
        for l in &layers {
            if self.overlay_info(&l.overlay)?.is_some() && self.overlay_props(&l.overlay)?.quiet {
                quiet.insert(l.overlay.clone());
                if let Some(root) = self.layer_root(l) {
                    out.extend(files_of(&root)?);
                }
            }
        }
        for (p, o) in self.pending_moves(line) {
            if quiet.contains(&o) {
                out.insert(p);
            }
        }
        Ok(out)
    }

    /// Names of the overlays of this project on this machine.
    pub fn overlay_names(&self) -> Result<Vec<String>> {
        Ok(self.overlay_candidates()?.into_iter().filter(|n| self.overlay_info(n).ok().flatten().is_some()).collect())
    }

    fn overlay_candidates(&self) -> Result<Vec<String>> {
        if self.p.is_overlay() {
            return Ok(Vec::new());
        }
        let mut names: BTreeSet<String> = self
            .p
            .db
            .config_all()?
            .into_iter()
            .filter_map(|(k, _)| k.strip_prefix("overlay.").map(|s| s.split('@').next().unwrap_or("").to_string()))
            .collect();
        for entry in std::fs::read_dir(self.p.dir.join("meta/overlays")).into_iter().flatten().flatten() {
            names.insert(entry.file_name().to_string_lossy().into_owned());
        }
        Ok(names.into_iter().collect())
    }

    /// Open each local overlay once for `overlay list`, before reading its metadata and policy.
    /// No handle survives the command, and write paths continue to open at their own boundaries.
    pub(crate) fn listed_overlays(&self) -> Result<Vec<ListedOverlay<'a>>> {
        let mut overlays = Vec::new();
        for name in self.overlay_candidates()? {
            // As in overlay_names, unavailable or invalid metadata is left out.
            let Ok((Some(info), project)) = self.open_overlay_info(&name) else { continue };
            let project = project.map(Ok).unwrap_or_else(|| self.overlay_project(&name));
            overlays.push(ListedOverlay { name, info, project });
        }
        Ok(overlays)
    }

    pub(crate) fn listed_layers(&self, line: &Line, overlays: &[ListedOverlay<'a>]) -> Result<Vec<Layer>> {
        let hidden = overlays
            .iter()
            .filter(|o| o.info.hidden)
            .map(|o| {
                let project = o.project.as_ref().map_err(|error| anyhow!("{error:#}"))?;
                Ok((o.name.as_str(), &o.info, project))
            })
            .collect::<Result<Vec<_>>>()?;
        self.layers_with(line, hidden)
    }

    pub fn visible_overlay_names(&self, ctx: &Ctx) -> Result<Vec<String>> {
        Ok(self
            .overlay_names()?
            .into_iter()
            .filter(|name| {
                !self.overlay_info(name).ok().flatten().is_some_and(|i| i.hidden)
                    || self.reads_overlay(name, ctx.caller.uid, ctx.caller.gid).unwrap_or(false)
            })
            .collect())
    }

    /// Only public layers may appear in a parent record.
    pub fn public_layers(&self, layers: &[Layer]) -> Result<Vec<Layer>> {
        let mut public = Vec::new();
        for layer in layers {
            if !self.need_overlay(&layer.overlay)?.hidden {
                public.push(layer.clone());
            }
        }
        Ok(public)
    }

    pub fn public_stack_data(&self, line: &str, layers: &[Layer]) -> Result<serde_json::Value> {
        Ok(stack_data(line, &self.public_layers(layers)?))
    }

    /// Record data that sets the view of the working folder of `line` to the public glasses of
    /// `layers` (`view_key`). Hidden glasses keep their place in their own project
    /// (`record_hidden_selection`).
    pub fn public_view_data(&self, line: &str, layers: &[Layer]) -> Result<serde_json::Value> {
        let names: Vec<String> = self.public_layers(layers)?.into_iter().map(|l| l.overlay).collect();
        Ok(view_data(line, &names))
    }

    /// Hidden selection and order travel only with that overlay.
    pub fn record_hidden_selection(&self, ctx: &Ctx, line: &str, layers: &[Layer]) -> Result<()> {
        for name in self.overlay_names()? {
            if !self.need_overlay(&name)?.hidden || !self.reads_overlay(&name, ctx.caller.uid, ctx.caller.gid)? {
                continue;
            }
            let orepo = self.overlay_repo(&name)?;
            let position = layers.iter().position(|l| l.overlay == name);
            let value = serde_json::json!({"enabled":position.is_some(),"position":position});
            let key = format!("hidden.stack.{line}");
            if orepo.p.db.overlay_config(&key)?.as_ref() == Some(&value) {
                continue;
            }
            let _lock = orepo.p.lock()?;
            let mut record = Record::new("overlay.use");
            record.data = serde_json::json!({"config":{key:value}});
            orepo.commit_record(ctx, record)?;
        }
        Ok(())
    }

    /// The parts of a state on the hidden glasses that the caller reads (`layers` at that state): each goes to the
    /// records of its own glass as `hidden.part.<state>`, so the public state record never names a hidden glass, and
    /// the remote copy of the glass encrypts it for the glass's readers (docs/overlays.md section 1a).
    pub fn record_hidden_parts(&self, ctx: &Ctx, state: &str, layers: &[Layer]) -> Result<()> {
        for l in layers {
            if !self.overlay_info(&l.overlay)?.is_some_and(|i| i.hidden) || !self.reads_overlay(&l.overlay, ctx.caller.uid, ctx.caller.gid)? {
                continue;
            }
            let orepo = self.overlay_repo(&l.overlay)?;
            let key = format!("hidden.part.{state}");
            if orepo.p.db.overlay_config(&key)?.and_then(|v| v.as_str().map(str::to_owned)).as_deref() == Some(l.state.as_str()) {
                continue;
            }
            let _lock = orepo.p.lock()?;
            let mut record = Record::new("overlay.use");
            record.data = serde_json::json!({"config":{key:l.state}});
            orepo.commit_record(ctx, record)?;
        }
        Ok(())
    }

    /// The new state `to` names the same parts of the hidden glasses as `from` (a replayed state).
    pub fn copy_hidden_parts(&self, ctx: &Ctx, from: &str, to: &str) -> Result<()> {
        let mut layers = Vec::new();
        for name in self.overlay_names()? {
            if let Some(part) = self.hidden_part(&name, from)? {
                layers.push(Layer { overlay: name, state: part });
            }
        }
        self.record_hidden_parts(ctx, to, &layers)
    }

    /// The part of the hidden glass `name` that the state `state` named when it was saved, if this machine reads it.
    pub fn hidden_part(&self, name: &str, state: &str) -> Result<Option<String>> {
        let Ok(op) = self.overlay_project(name) else { return Ok(None) };
        Ok(op.db.overlay_config(&format!("hidden.part.{state}"))?.and_then(|v| v.as_str().map(str::to_owned)))
    }

    /// The internal project of an overlay.
    pub fn overlay_project(&self, name: &str) -> Result<Project<'a>> {
        Project::open(self.p.store, &format!("{}%{name}", self.p.name))
    }

    pub fn overlay_repo(&self, name: &str) -> Result<Repo<'a>> {
        Ok(Repo { p: self.overlay_project(name)?, ctx: self.ctx })
    }

    /// The user reads the overlay: the reader role in its policy (its owner is its admin).
    pub fn reads_overlay(&self, name: &str, uid: u32, gid: u32) -> Result<bool> {
        if uid == 0 {
            return Ok(true);
        }
        let op = self.overlay_project(name)?;
        Ok(Policy::load(&op).check(&Who::of(uid, gid), "read").is_ok())
    }

    fn owner_reads(&self, name: &str, owner: u32) -> Result<bool> {
        let gid = crate::ctx::Caller::from_uid(owner).gid;
        self.reads_overlay(name, owner, gid)
    }

    /// The owner of a line reads the overlay project `op` (`reads_overlay` for an open project).
    fn project_reads(op: &Project, owner: u32) -> bool {
        let gid = crate::ctx::Caller::from_uid(owner).gid;
        owner == 0 || Policy::load(op).check(&Who::of(owner, gid), "read").is_ok()
    }

    /// The name of the overlay line that a line of `owner` uses.
    pub fn overlay_line_name(info: &Info, line_id: &str, owner: u32) -> String {
        match info.mode {
            Mode::Shared => format!("user-{owner}"),
            Mode::Follow if info.cache && info.platform.is_none() => cache_line_name(line_id),
            Mode::Follow => format!("line-{line_id}"),
        }
    }

    // ----- the stack of a line -----

    /// The overlays a line shows, bottom first.
    pub fn layers(&self, line: &Line) -> Result<Vec<Layer>> {
        let hidden = self.hidden_overlays()?;
        self.layers_with(line, hidden.iter().map(|(name, info, project)| (name.as_str(), info, project)))
    }

    /// The stacks of many lines (line id to layers). The overlays are read once, not once per line.
    pub fn stacks(&self, lines: &[Line]) -> Result<BTreeMap<String, Vec<Layer>>> {
        let hidden = self.hidden_overlays()?;
        lines
            .iter()
            .map(|l| {
                Ok((
                    l.id.clone(),
                    self.layers_with(l, hidden.iter().map(|(name, info, project)| (name.as_str(), info, project)))?,
                ))
            })
            .collect()
    }

    /// The hidden overlays of the project with their projects. Empty in an overlay project.
    fn hidden_overlays(&self) -> Result<Vec<(String, Info, Project<'a>)>> {
        if self.p.is_overlay() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for name in self.overlay_candidates()? {
            // Match overlay_names: unavailable or invalid metadata is left out.
            let Ok((Some(info), project)) = self.open_overlay_info(&name) else { continue };
            if info.hidden {
                let op = project.map(Ok).unwrap_or_else(|| self.overlay_project(&name))?;
                out.push((name, info, op));
            }
        }
        Ok(out)
    }

    fn layers_with<'b>(
        &self,
        line: &Line,
        hidden_overlays: impl IntoIterator<Item = (&'b str, &'b Info, &'b Project<'a>)>,
    ) -> Result<Vec<Layer>>
    where
        'a: 'b,
    {
        let key = stack_key(&line.id);
        let value = if line.machine == self.p.store.machine.id {
            self.p.config_value(&key)?
        } else {
            self.p.config_value(&format!("{key}@{}", line.machine))?
        };
        let value = match value {
            Some(value) => Some(value),
            None => self.p.db.overlay_config(&key)?,
        };
        let mut layers: Vec<Layer> = value.map(serde_json::from_value).transpose()?.unwrap_or_default();
        // The view of the working folder chooses the glasses and their order (slice 5). A line
        // without a view (made before views) shows its stack as before.
        if !self.p.is_overlay() {
            if let Some(glasses) = self.folder_view(line)? {
                let mut shown = Vec::new();
                for name in glasses {
                    match layers.iter().find(|l| l.overlay == name) {
                        Some(l) => shown.push(l.clone()),
                        None => {
                            if let Some(l) = self.glass_layer(Some(line), &line.head, &name)? {
                                shown.push(l);
                            }
                        }
                    }
                }
                layers = shown;
            }
        }
        if !self.p.is_overlay() {
            let mut hidden = Vec::new();
            for (name, info, op) in hidden_overlays {
                if !Self::project_reads(op, line.owner) {
                    continue;
                }
                let selection = op.db.overlay_config(&format!("hidden.stack.{}", line.id))?;
                if selection.as_ref().is_some_and(|s| s["enabled"] == false) {
                    continue;
                }
                if let Some(ol) = op.db.line_by_name(&Self::overlay_line_name(info, &line.id, line.owner))? {
                    let position = selection.as_ref().and_then(|s| s["position"].as_u64()).unwrap_or(u64::MAX);
                    hidden.push((position, Layer { overlay: name.to_string(), state: ol.head }));
                }
            }
            hidden.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.overlay.cmp(&b.1.overlay)));
            for (position, layer) in hidden {
                layers.insert((position as usize).min(layers.len()), layer);
            }
        }
        Ok(layers)
    }

    /// The view of the working folder of `line`: the names of the public glasses it shows,
    /// bottom first. None for a line without a view (it shows its stack).
    pub fn folder_view(&self, line: &Line) -> Result<Option<Vec<String>>> {
        let key = view_key(&line.id);
        let value = if line.machine == self.p.store.machine.id {
            self.p.config_value(&key)?
        } else {
            self.p.config_value(&format!("{key}@{}", line.machine))?
        };
        let value = match value {
            Some(value) => Some(value),
            None => self.p.db.overlay_config(&key)?,
        };
        Ok(value.filter(|v| !v.is_null()).map(serde_json::from_value).transpose()?)
    }

    /// The layer of glass `name` over `state` when the stack of the line names no state for it:
    /// the part that `state` names on a glass that keeps parts, else the overlay line of `line`,
    /// else the empty first state of the glass. None for a glass that is not on this machine.
    pub fn glass_layer(&self, line: Option<&Line>, state: &str, name: &str) -> Result<Option<Layer>> {
        let Some(info) = self.overlay_info(name)? else { return Ok(None) };
        if parts_glass(&info) {
            if let Some(part) = self.p.db.state(state)?.and_then(|s| s.parts.get(name).cloned()) {
                return Ok(Some(Layer { overlay: name.to_string(), state: part }));
            }
        }
        let mut layer = Layer { overlay: name.to_string(), state: info.root.clone() };
        if let Some(line) = line {
            if let Ok(op) = self.overlay_project(name) {
                if let Some(ol) = op.db.line_by_name(&Self::overlay_line_name(&info, &line.id, line.owner))? {
                    layer.state = ol.head;
                }
            }
        }
        Ok(Some(layer))
    }

    /// The project order of glasses: the names that `layr overlay order` set (setting
    /// `overlays.order`), then the others in the order of their creation (the time of the empty
    /// first state), then by name.
    pub fn project_order(&self) -> Result<Vec<String>> {
        let names = self.overlay_names()?;
        let set: Vec<String> =
            self.p.config_value(ORDER_KEY)?.and_then(|v| serde_json::from_value::<Vec<String>>(v).ok()).unwrap_or_default();
        let mut out: Vec<String> = set.into_iter().filter(|n| names.contains(n)).collect();
        let mut rest = Vec::new();
        for name in names {
            if out.contains(&name) {
                continue;
            }
            let time = match self.overlay_info(&name)? {
                Some(info) => self
                    .overlay_project(&name)
                    .ok()
                    .and_then(|op| op.db.state(&info.root).ok().flatten())
                    .map(|s| s.time)
                    .unwrap_or(i64::MAX),
                None => i64::MAX,
            };
            rest.push((time, name));
        }
        rest.sort();
        out.extend(rest.into_iter().map(|(_, n)| n));
        Ok(out)
    }

    /// The glasses of the default view of a new line folder of `owner`: every glass that the owner
    /// reads and whose `platform` (if set) is this machine's, in the project order. Folder
    /// overlays are no glass of the file stack (a line takes their parts as folders). A cache glass
    /// is an ordinary glass (owner decision 2026-11-11): it comes like any other.
    pub fn default_glasses(&self, owner: u32, _shown: &[String]) -> Result<Vec<String>> {
        let folders: Vec<String> = self.folder_overlays()?.into_iter().map(|(n, _)| n).collect();
        let mut out = Vec::new();
        for name in self.project_order()? {
            let Some(info) = self.overlay_info(&name)? else { continue };
            if folders.contains(&name) {
                continue;
            }
            if other_platform_glass(info.platform.as_deref()) {
                continue;
            }
            if self.layer_root(&Layer { overlay: name.clone(), state: info.root.clone() }).is_none() {
                continue;
            }
            if !self.owner_reads(&name, owner)? {
                continue;
            }
            out.push(name);
        }
        Ok(out)
    }

    /// The parts that a new state of a line with `layers` names: the state of each glass that
    /// keeps parts (`parts_glass`).
    pub fn parts_of(&self, layers: &[Layer]) -> Result<BTreeMap<String, String>> {
        let mut out = BTreeMap::new();
        for l in layers {
            if self.overlay_info(&l.overlay)?.is_some_and(|i| parts_glass(&i)) {
                out.insert(l.overlay.clone(), l.state.clone());
            }
        }
        Ok(out)
    }

    /// A new state of a line keeps the parts of `head` on the glasses that its view does not show
    /// (`layers`): a folder that shows fewer glasses does not clear the others (slice 5).
    pub fn carry_parts(&self, parts: &mut BTreeMap<String, String>, head: &str, layers: &[Layer]) -> Result<()> {
        if self.p.is_overlay() {
            return Ok(());
        }
        for (glass, part) in self.p.db.state(head)?.map(|s| s.parts).unwrap_or_default() {
            if !layers.iter().any(|l| l.overlay == glass) && self.overlay_info(&glass)?.is_some_and(|i| parts_glass(&i)) {
                parts.entry(glass).or_insert(part);
            }
        }
        Ok(())
    }

    /// The layers of `line` when it shows its state `state`: the line's stack, each glass that
    /// keeps parts at the part that `state` names. A state with parts that names no part on such a
    /// glass is clear on it (the empty first state). A state without parts (saved before them, or
    /// without overlays) and the head itself keep the stack as it is.
    pub fn layers_at(&self, line: &Line, state: &str) -> Result<Vec<Layer>> {
        let mut layers = self.layers(line)?;
        if self.p.is_overlay() || state == line.head || layers.is_empty() {
            return Ok(layers);
        }
        // A hidden glass: the part that the state recorded in the glass's own records.
        for l in layers.iter_mut() {
            if self.overlay_info(&l.overlay)?.is_some_and(|i| i.hidden) {
                if let Some(part) = self.hidden_part(&l.overlay, state)? {
                    l.state = part;
                }
            }
        }
        let parts = match self.p.db.state(state)? {
            Some(s) if !s.parts.is_empty() => s.parts,
            _ => return Ok(layers),
        };
        for l in layers.iter_mut() {
            let Some(info) = self.overlay_info(&l.overlay)? else { continue };
            if parts_glass(&info) {
                l.state = parts.get(&l.overlay).cloned().unwrap_or(info.root);
            }
        }
        Ok(layers)
    }

    /// The glasses of the view of a state (docs/overlays.md, section 1a): the parts that the state
    /// names, in the order of the stack of `line` and then of the project's overlays. Empty for a
    /// state without parts.
    pub fn part_layers(&self, line: Option<&Line>, state: &str) -> Result<Vec<Layer>> {
        if self.p.is_overlay() {
            return Ok(Vec::new());
        }
        let mut parts = self.p.db.state(state)?.map(|s| s.parts).unwrap_or_default();
        // The hidden glasses that this machine reads: the part that the state recorded in each glass's own records.
        let mut hidden = BTreeSet::new();
        for name in self.overlay_names()? {
            if self.overlay_info(&name)?.is_some_and(|i| i.hidden) {
                if let Some(part) = self.hidden_part(&name, state)? {
                    parts.insert(name.clone(), part);
                    hidden.insert(name);
                }
            }
        }
        if parts.is_empty() {
            return Ok(Vec::new());
        }
        let mut order: Vec<String> = match line {
            Some(l) => self.layers(l)?.into_iter().map(|l| l.overlay).collect(),
            None => Vec::new(),
        };
        for name in self.overlay_names()? {
            if !order.contains(&name) {
                order.push(name);
            }
        }
        // A build-output glass is a folder, not a glass of the file stack: `line new` places its part.
        let mut out = Vec::new();
        for o in order {
            if let Some(s) = parts.get(&o) {
                if hidden.contains(&o) || self.overlay_info(&o)?.is_some_and(|i| parts_glass(&i)) {
                    out.push(Layer { overlay: o.clone(), state: s.clone() });
                }
            }
        }
        Ok(out)
    }

    pub fn readable_layers(&self, line: &Line) -> Result<Vec<Layer>> {
        if self.p.is_overlay() {
            return Ok(Vec::new());
        }
        let gid = crate::ctx::Caller::from_uid(line.owner).gid;
        Ok(self
            .layers(line)?
            .into_iter()
            .filter(|layer| {
                self.layer_root(layer).is_some() && self.reads_overlay(&layer.overlay, line.owner, gid).unwrap_or(false)
            })
            .collect())
    }

    pub fn is_stacked(&self, line: &Line) -> Result<bool> {
        Ok(!self.p.is_overlay() && !self.layers(line)?.is_empty())
    }

    /// Every layer of the stack must still be readable by the line owner before the line changes
    /// it: a file of an overlay that is not in the stack would go to the shared set.
    pub fn check_stack(&self, line: &Line, layers: &[Layer]) -> Result<()> {
        for l in layers {
            if self.overlay_info(&l.overlay)?.is_none() || self.layer_root(l).is_none() {
                return Err(crate::cmd::exit(
                    1,
                    format!(
                        "error: line '{}' shows overlay '{}', which is not on this machine; run 'layr overlay use --remove {}'",
                        line.name, l.overlay, l.overlay
                    ),
                ));
            }
            if !self.owner_reads(&l.overlay, line.owner)? {
                return Err(crate::cmd::exit(
                    1,
                    format!(
                        "error: line '{}' shows overlay '{}', which its owner no longer reads; run 'layr overlay use' without it",
                        line.name, l.overlay
                    ),
                ));
            }
        }
        Ok(())
    }

    /// The caller owns the line and the line's home is this machine (an owner of another machine's line is a
    /// user of that machine).
    fn owns_here(&self, ctx: &Ctx, line: &Line) -> bool {
        ctx.caller.uid == line.owner && line.machine == self.p.store.machine.id
    }

    /// Reading the working content of a line with overlays (status, diff, grep, ls-files, the
    /// index) needs the read right of each of its overlays, unless the caller owns the line.
    pub fn check_view(&self, ctx: &Ctx, line: &Line) -> Result<()> {
        if ctx.caller.is_root() || self.owns_here(ctx, line) || self.p.is_overlay() {
            return Ok(());
        }
        for l in self.layers(line)? {
            if !self.need_overlay(&l.overlay)?.hidden && !self.reads_overlay(&l.overlay, ctx.caller.uid, ctx.caller.gid)? {
                return Err(crate::cmd::exit(
                    1,
                    format!("permission denied: line '{}' shows overlays that you do not read", line.name),
                ));
            }
        }
        Ok(())
    }

    /// A non-reader sees a saved view without any unread hidden layers.
    /// Visible private overlays keep their existing access refusal.
    pub fn caller_layers(&self, ctx: &Ctx, line: &Line) -> Result<Vec<Layer>> {
        self.check_view(ctx, line)?;
        let mut layers = Vec::new();
        for layer in self.layers(line)? {
            if !self.need_overlay(&layer.overlay)?.hidden || self.reads_overlay(&layer.overlay, ctx.caller.uid, ctx.caller.gid)? {
                layers.push(layer);
            }
        }
        Ok(layers)
    }

    pub fn filtered_view(&self, ctx: &Ctx, line: &Line) -> Result<Option<Temp>> {
        self.check_view(ctx, line)?;
        if ctx.caller.is_root() || self.owns_here(ctx, line) || self.p.is_overlay() {
            return Ok(None);
        }
        let layers = self.caller_layers(ctx, line)?;
        if layers.len() == self.layers(line)?.len() {
            return Ok(None);
        }
        Ok(Some(self.assemble(line, &line.head, &layers)?))
    }

    pub fn snapshot_status(&self, ctx: &Ctx, line: &Line) -> Result<crate::repo::Status> {
        let mut status = crate::repo::Status::default();
        status.conflicts = self.state(&line.head)?.conflicts;
        for layer in self.caller_layers(ctx, line)? {
            if let Some(state) = self.overlay_project(&layer.overlay)?.db.state(&layer.state)? {
                status.conflicts.extend(state.conflicts);
            }
        }
        Ok(status)
    }

    /// The paths that `layr overlay move` sends to an overlay at the next commit. They stay until
    /// that commit or `overlay move --cancel`, whatever happens to the index: until then the
    /// automatic states leave them out.
    pub fn pending_moves(&self, line: &Line) -> BTreeMap<String, String> {
        let f = self.p.meta_dir().join("stage").join(format!("{}.moves", line.id));
        std::fs::read(&f).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default()
    }

    pub fn set_pending_moves(&self, line: &Line, m: &BTreeMap<String, String>) -> Result<()> {
        let f = self.p.meta_dir().join("stage").join(format!("{}.moves", line.id));
        if m.is_empty() {
            let _ = std::fs::remove_file(&f);
            return Ok(());
        }
        fsutil::atomic_write(&f, serde_json::to_string(m)?.as_bytes(), 0o600)
    }

    /// The key of the staging line: it is current while the head and the stack are.
    pub fn stage_key(&self, line: &Line) -> Result<String> {
        let layers = self.layers(line)?;
        Ok(if layers.is_empty() { line.head.clone() } else { format!("{}@{}", line.head, base_key(&line.head, &layers)) })
    }

    /// Files that automatic states of the line leave out: the files of its overlays (the states it
    /// shows and the newest states of its overlay lines) and the paths of pending moves.
    pub fn private_paths(&self, line: &Line) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        if self.p.is_overlay() {
            return Ok(out);
        }
        let hidden = self.hidden_overlays()?;
        let layers = self.layers_with(line, hidden.iter().map(|(name, info, project)| (name.as_str(), info, project)))?;
        for l in layers {
            let project;
            let public_info;
            let (op, info) = match hidden.iter().find(|(name, _, _)| name == &l.overlay) {
                Some((_, info, op)) => (op, Some(info)),
                None => {
                    project = match self.overlay_project(&l.overlay) {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    public_info = self.overlay_info(&l.overlay)?;
                    (&project, public_info.as_ref())
                }
            };
            let mut states = vec![l.state.clone()];
            if let Some(info) = info {
                if let Some(ol) = op.db.line_by_name(&Self::overlay_line_name(info, &line.id, line.owner))? {
                    states.push(ol.head);
                }
            }
            for s in states {
                let root = op.state_path(&s);
                if root.exists() {
                    out.extend(files_of(&root)?);
                }
            }
        }
        out.extend(self.pending_moves(line).into_keys());
        Ok(out)
    }

    // ----- the base: the full tree of a line with overlays -----

    fn base_path(&self, line: &Line, head: &str, layers: &[Layer]) -> PathBuf {
        self.p.base_dir().join(format!("{}@{}", line.id, base_key(head, layers)))
    }

    /// The full tree of the line's head: the head state, or for a line with overlays its base
    /// (made when missing).
    pub fn head_tree(&self, line: &Line) -> Result<PathBuf> {
        let layers = if self.p.is_overlay() { Vec::new() } else { self.layers(line)? };
        if layers.is_empty() {
            return self.state_root(&line.head);
        }
        let path = self.base_path(line, &line.head, &layers);
        if path.exists() {
            // A base that another request just placed may still be writable for a moment.
            if !btrfs::subvol_info(&path).map(|i| i.readonly).unwrap_or(true) {
                btrfs::set_readonly(&path)?;
            }
            return Ok(path);
        }
        let view = self.assemble(line, &line.head, &layers)?;
        self.place_base(&view, &path)?;
        Ok(path)
    }

    fn place_base(&self, view: &Temp, path: &Path) -> Result<()> {
        let dir = self.p.base_dir();
        if !dir.exists() {
            std::fs::create_dir_all(&dir)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        // btrfs renames only a writable subvolume: the view is complete, so it is writable only
        // for the rename.
        btrfs::set_writable(&view.path)?;
        match crate::fsutil::rename_tree(&view.path, path) {
            Ok(()) => btrfs::set_readonly(path),
            // Another request made the same base at the same time.
            Err(_) if path.exists() => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// The full tree for `head` and `layers`, as a temporary read-only snapshot: a writable
    /// snapshot of the head state, the files of each overlay state from the bottom (an upper file
    /// wins), read-only.
    pub fn assemble(&self, line: &Line, head: &str, layers: &[Layer]) -> Result<Temp> {
        let _t = crate::trace::span("assemble");
        let t = Temp { path: self.p.work_dir().join(crate::store::temp_name()) };
        self.snapshot_state(head, &t.path, false)?;
        self.put_layers(&t.path, layers, self.own(line))?;
        btrfs::set_readonly(&t.path)?;
        Ok(t)
    }

    /// The folder of the state a layer shows, if the overlay and the state are on this machine
    /// (a restored or replicated project has the stacks of its lines but not their overlays).
    pub fn layer_root(&self, l: &Layer) -> Option<PathBuf> {
        let op = self.overlay_project(&l.overlay).ok()?;
        let root = op.state_path(&l.state);
        root.exists().then_some(root)
    }

    /// Reflink copies of the files of each overlay state into `dst`, from the bottom. A layer
    /// whose overlay is not on this machine adds nothing (`check_stack` refuses a commit then).
    pub fn put_layers(&self, dst: &Path, layers: &[Layer], own: Option<(u32, u32)>) -> Result<()> {
        for l in layers {
            if let Some(root) = self.layer_root(l) {
                for rel in files_of(&root)? {
                    fsutil::copy_entry(&root, &rel, dst, &rel, own)?;
                }
            }
        }
        Ok(())
    }

    /// Keep `view` as the base of the line for `head` and `layers`. Older bases of the line stay a
    /// minute for requests that still read them.
    pub fn install_base(&self, line: &Line, view: Temp, head: &str, layers: &[Layer]) -> Result<()> {
        let path = self.base_path(line, head, layers);
        if layers.is_empty() {
            drop(view);
        } else if !path.exists() {
            self.place_base(&view, &path)?;
        }
        self.prune_bases(line, Some(&path), 60)
    }

    /// Delete the bases of every line that are not its current one (older than a minute), and
    /// those of deleted lines (`gc`).
    pub fn prune_all_bases(&self) -> Result<()> {
        if self.p.is_overlay() || !self.p.base_dir().exists() {
            return Ok(());
        }
        let lines = self.lines()?;
        let live: BTreeSet<String> = lines.iter().map(|l| l.id.clone()).collect();
        for l in &lines {
            let layers = self.layers(l)?;
            let keep = (!layers.is_empty()).then(|| self.base_path(l, &l.head, &layers));
            self.prune_bases(l, keep.as_deref(), 60)?;
        }
        for e in std::fs::read_dir(self.p.base_dir())?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let id = name.split('@').next().unwrap_or("").to_string();
            if !live.contains(&id) {
                let _ = btrfs::delete_tree(&e.path());
            }
        }
        Ok(())
    }

    /// Delete the bases of a line except `keep`, those older than `age` seconds.
    pub fn prune_bases(&self, line: &Line, keep: Option<&Path>, age: u64) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let dir = self.p.base_dir();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let pre = format!("{}@", line.id);
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&pre) || Some(e.path().as_path()) == keep {
                continue;
            }
            let ctime = e.metadata().map(|m| m.ctime()).unwrap_or(0);
            if now - ctime >= age as i64 {
                let _ = btrfs::delete_tree(&e.path());
            }
        }
        Ok(())
    }

    /// Records in an overlay: a new line `name` of `owner` at `start`.
    fn overlay_line_create(&self, ctx: &Ctx, name: &str, start: &str, owner: u32) -> Result<Line> {
        let line = Line {
            id: ids::new_id(),
            name: name.to_string(),
            head: start.to_string(),
            owner,
            machine: self.p.store.machine.id.clone(),
        };
        let mut rec = Record::new("line.create");
        rec.lines.insert(
            line.id.clone(),
            LineChange { before: None, after: Some(self.line_state(&line, start)), working_before: None, working_after: None },
        );
        rec.data = serde_json::json!({"name": name, "start": start});
        self.p.append(rec, &ctx.actor())?;
        Ok(line)
    }

    /// The overlay line that a line uses for overlay `info`, made at `start` when missing (only
    /// in an overlay's own repository).
    pub fn ensure_overlay_line(&self, ctx: &Ctx, name: &str, start: &str, owner: u32) -> Result<Line> {
        if let Some(l) = self.p.db.line_by_name(name)? {
            return Ok(l);
        }
        match self.unnamed_platform_line(name)? {
            Some(l) => self.overlay_line_rename(ctx, l, name),
            None => self.overlay_line_create(ctx, name, start, owner),
        }
    }

    /// The overlay line `line-<id>` of a cache overlay that layr made before the overlay lines had a
    /// platform, when its head was saved on the platform that `name` (`line-<id>.<platform>`) names.
    /// `ensure_overlay_line` renames it: the migration of each line happens at its first write.
    pub fn unnamed_platform_line(&self, name: &str) -> Result<Option<Line>> {
        let Some((old, platform)) = name.split_once('.') else { return Ok(None) };
        let Some(l) = self.p.db.line_by_name(old)? else { return Ok(None) };
        let made_on = self.p.db.state(&l.head)?.and_then(|s| s.cache_platform);
        let same = made_on.is_none_or(|p| p == platform || p.starts_with(&format!("{platform}-")));
        Ok(same.then_some(l))
    }

    fn overlay_line_rename(&self, ctx: &Ctx, l: Line, name: &str) -> Result<Line> {
        let mut after = self.line_state(&l, &l.head);
        after.name = name.to_string();
        let mut rec = Record::new("line.rename");
        rec.lines.insert(
            l.id.clone(),
            LineChange {
                before: Some(self.line_state(&l, &l.head)),
                after: Some(after),
                working_before: None,
                working_after: None,
            },
        );
        rec.data = serde_json::json!({"from": l.name, "to": name});
        self.p.append(rec, &ctx.actor())?;
        Ok(Line { name: name.to_string(), ..l })
    }

    /// Apply `history=<n>` to overlay `name`: keep the newest n states on the first-parent chain
    /// of each overlay line, the states that lines of the project show, and the empty first
    /// state; delete the others. Their records stay (a `gc` record in the overlay names them).
    /// The caller holds the project lock and no lock of the overlay. Returns the deleted states.
    pub fn apply_history(&self, ctx: &Ctx, name: &str) -> Result<Vec<String>> {
        if self.overlay_info(name)?.is_none() {
            return Ok(Vec::new());
        }
        let Some(keep_n) = self.overlay_props(name)?.history.map(|n| n as usize) else {
            return Ok(Vec::new());
        };
        let orepo = self.overlay_repo(name)?;
        let _lock = orepo.p.lock()?;
        let lines = self.lines()?;
        let stacks = self.stacks(&lines)?;
        let excess = self.history_excess(&orepo, name, keep_n, &lines, &stacks)?;
        Self::delete_excess(ctx, &orepo, keep_n, excess)
    }

    /// The present states of cache overlay `name` that nothing keeps: beyond its history limit (all
    /// states when it has none), not shown by a line, or of a deleted line. `fsck --glasses` reports
    /// them; the operations that make them dead delete them (`apply_history`). It only reads.
    pub fn cache_unreachable(&self, name: &str) -> Result<Vec<String>> {
        let keep_n = self.overlay_props(name)?.history.map_or(usize::MAX, |n| n as usize);
        let orepo = self.overlay_repo(name)?;
        let lines = self.lines()?;
        let stacks = self.stacks(&lines)?;
        self.history_excess(&orepo, name, keep_n, &lines, &stacks)
    }

    /// The states of overlay `name` (project `orepo`) beyond its history limit `keep_n` that no
    /// line uses. It only reads.
    fn history_excess(
        &self,
        orepo: &Repo,
        name: &str,
        keep_n: usize,
        lines: &[Line],
        stacks: &BTreeMap<String, Vec<Layer>>,
    ) -> Result<Vec<String>> {
        let Some(keep) = self.history_kept(orepo, name, keep_n, lines, stacks)? else {
            return Ok(Vec::new());
        };
        // A part that is here only as objects (its native copy retired) counts too, so history=last drops it.
        let mut all: Vec<String> = orepo.p.db.present_states()?.into_iter().map(|(id, _)| id).collect();
        all.extend(orepo.p.db.object_only_states()?);
        Ok(all.into_iter().filter(|id| !keep.contains(id)).collect())
    }

    /// The states of cache overlay `name` that its records keep, on every machine and platform (the
    /// states need not be on this machine): the newest `history` states of the overlay line of each
    /// live line, the states that lines show, and the empty first state. A store remote keeps the
    /// trees of these states and of states it does not know yet (`store_remote::sweep`). None: no
    /// such overlay.
    pub fn cache_kept(&self, name: &str) -> Result<Option<BTreeSet<String>>> {
        let keep_n = self.overlay_props(name)?.history.map_or(usize::MAX, |n| n as usize);
        let orepo = self.overlay_repo(name)?;
        let lines = self.lines()?;
        let stacks = self.stacks(&lines)?;
        self.history_kept(&orepo, name, keep_n, &lines, &stacks)
    }

    fn history_kept(
        &self,
        orepo: &Repo,
        name: &str,
        keep_n: usize,
        lines: &[Line],
        stacks: &BTreeMap<String, Vec<Layer>>,
    ) -> Result<Option<BTreeSet<String>>> {
        let Some(info) = self.overlay_info(name)? else {
            return Ok(None);
        };
        let mut keep: BTreeSet<String> = BTreeSet::from([info.root.clone()]);
        // A cache overlay keeps nothing for a line that is gone (docs/cache-overlays.md).
        let cache = self.overlay_setting(name, "cache")?.and_then(|v| v.as_bool()).unwrap_or(false);
        let live: BTreeSet<&str> = lines.iter().map(|l| l.id.as_str()).collect();
        for ol in orepo.p.db.lines()? {
            if cache && !line_of_overlay_line(&ol.name).is_some_and(|id| live.contains(id)) {
                continue;
            }
            let mut at = Some(ol.head.clone());
            for _ in 0..keep_n {
                let Some(id) = at.take() else { break };
                at = orepo.p.db.state(&id)?.and_then(|s| s.parents.first().cloned());
                keep.insert(id);
            }
        }
        for l in stacks.values().flatten() {
            if l.overlay == name {
                keep.insert(l.state.clone());
            }
        }
        // A glass that keeps parts: the parts of the newest `history` states of each line.
        if !cache {
            for line in lines {
                let mut at = Some(line.head.clone());
                for _ in 0..keep_n {
                    let Some(id) = at.take() else { break };
                    let Some(st) = self.p.db.state(&id)? else { break };
                    if let Some(part) = st.parts.get(name) {
                        keep.insert(part.clone());
                    }
                    at = st.parents.first().cloned();
                }
            }
        }
        Ok(Some(keep))
    }

    /// Delete the states `excess` of the overlay project `orepo` that are still present, and
    /// record it. The caller holds the overlay project's lock.
    fn delete_excess(ctx: &Ctx, orepo: &Repo, keep_n: usize, excess: Vec<String>) -> Result<Vec<String>> {
        let present: BTreeSet<String> = orepo.p.db.present_states()?.into_iter().map(|(id, _)| id).collect();
        let objects_only: BTreeSet<String> = orepo.p.db.object_only_states()?.into_iter().collect();
        let mut deleted = Vec::new();
        for id in excess {
            // A part that is here only as objects has no tree to delete: the gc record marks it deleted, and the next
            // object store prune gives its objects back.
            if objects_only.contains(&id) || (present.contains(&id) && orepo.p.delete_state(&id)?) {
                deleted.push(id);
            }
        }
        if !deleted.is_empty() {
            let mut rec = Record::new("gc");
            rec.data = serde_json::json!({"deleted": deleted, "history": keep_n});
            orepo.commit_record(ctx, rec)?;
        }
        Ok(deleted)
    }

    /// `apply_history` after an overlay line moved. A failure leaves old states on the disk for
    /// the next `gc`; it never fails the operation that made the new state.
    pub fn after_overlay_move(&self, ctx: &Ctx, name: &str) {
        if let Err(e) = self.apply_history(ctx, name) {
            eprintln!("warning: overlay '{name}': old states stay until the next gc: {e:#}");
        }
    }

    /// What the overlay pass of `gc --repair` changes: the cache overlay lines of deleted lines
    /// and the states beyond the history limits. It only reads and takes no lock, so `gc --repair`
    /// makes it before the project lock (#194).
    pub fn overlay_plan(&self) -> Result<OverlayPlan> {
        let lines = self.lines()?;
        let stacks = self.stacks(&lines)?;
        let mut overlays = Vec::new();
        for name in self.overlay_names()? {
            if let Some(c) = self.overlay_clean(&name, &lines, &stacks)? {
                overlays.push(c);
            }
        }
        Ok(OverlayPlan { lines, stacks, overlays })
    }

    fn overlay_clean(&self, name: &str, lines: &[Line], stacks: &BTreeMap<String, Vec<Layer>>) -> Result<Option<OverlayClean>> {
        // An overlay that is not on this machine has nothing to clean here.
        if !self.p.store.project_dir(&format!("{}%{name}", self.p.name))?.exists() {
            return Ok(None);
        }
        let orepo = self.overlay_repo(name)?;
        let mark = orepo.p.db.records_mark()?;
        let cache = self.overlay_setting(name, "cache")?.and_then(|v| v.as_bool()).unwrap_or(false);
        let orphans = if cache { self.orphan_cache_lines(&orepo)? } else { Vec::new() };
        let keep_n = self.overlay_props(name)?.history.map(|n| n as usize);
        let excess = match keep_n {
            Some(n) => self.history_excess(&orepo, name, n, lines, stacks)?,
            None => Vec::new(),
        };
        Ok(Some(OverlayClean { name: name.to_string(), mark, orphans, keep_n, excess }))
    }

    /// Apply an overlay plan. The caller holds the project lock, and no record of the project came
    /// in since the plan was made (the lines and stacks are the same). An overlay whose own records
    /// changed is read again under its lock. A failure leaves old states for the next gc.
    pub fn commit_overlay_plan(&self, ctx: &Ctx, plan: OverlayPlan) {
        for c in plan.overlays {
            let name = c.name.clone();
            if let Err(e) = self.commit_overlay_clean(ctx, c, &plan.lines, &plan.stacks) {
                eprintln!("warning: overlay '{name}': old states stay until the next gc: {e:#}");
            }
        }
    }

    fn commit_overlay_clean(
        &self,
        ctx: &Ctx,
        c: OverlayClean,
        lines: &[Line],
        stacks: &BTreeMap<String, Vec<Layer>>,
    ) -> Result<()> {
        let orepo = self.overlay_repo(&c.name)?;
        let _lock = orepo.p.lock()?;
        let c = if orepo.p.db.records_mark()? == c.mark {
            c
        } else {
            match self.overlay_clean(&c.name, lines, stacks)? {
                Some(c) => c,
                None => return Ok(()),
            }
        };
        self.drop_overlay_lines(ctx, &orepo, &c.name, &c.orphans)?;
        // The states of the dropped lines are dead now; `delete_excess` skips them.
        if let Some(keep_n) = c.keep_n {
            Self::delete_excess(ctx, &orepo, keep_n, c.excess)?;
        }
        Ok(())
    }

    /// The layers of the working folder of a new line of `owner`: its default view (slice 5,
    /// `default_glasses`), every glass that the owner reads in the project order.
    ///
    /// The part on each glass: a glass that keeps parts (`parts_glass`) shows the part that the
    /// start state names, or the source's state of the glass when the start is the source's head
    /// (an unsaved part of an overlay-only save) or names no part; else the empty first state. It
    /// gets no overlay line. Another glass gets an overlay line: a follow glass at the source's
    /// state (else its empty first state), a shared one the owner's line (made at the source's
    /// state, or the head of the glass owner's line, when missing).
    pub fn inherit_layers(&self, ctx: &Ctx, source: Option<&Line>, start: &str, line_id: &str, owner: u32) -> Result<Vec<Layer>> {
        let start_parts = self.p.db.state(start)?.map(|s| s.parts).unwrap_or_default();
        let at_head = source.is_some_and(|s| s.head == start);
        let source_layers = match source {
            Some(s) => self.layers(s)?,
            None => Vec::new(),
        };
        let shown: Vec<String> = source_layers.iter().map(|l| l.overlay.clone()).collect();
        let mut out = Vec::new();
        for name in self.default_glasses(owner, &shown)? {
            let Some(info) = self.overlay_info(&name)? else { continue };
            let from_source = source_layers.iter().find(|l| l.overlay == name).map(|l| l.state.clone());
            if parts_glass(&info) {
                let named = start_parts.get(&name).cloned();
                let state = if at_head { from_source.or(named) } else { named.or(from_source) };
                let l = Layer { overlay: name, state: state.unwrap_or(info.root) };
                if self.layer_root(&l).is_some() {
                    out.push(l);
                }
                continue;
            }
            let orepo = self.overlay_repo(&name)?;
            let _lock = orepo.p.lock()?;
            let begin = match (from_source, info.mode) {
                (Some(s), _) => s,
                (None, Mode::Follow) => info.root.clone(),
                (None, Mode::Shared) => {
                    orepo.p.db.line_by_name(&format!("user-{}", info.owner))?.map(|l| l.head).unwrap_or(info.root.clone())
                }
            };
            if self.layer_root(&Layer { overlay: name.clone(), state: begin.clone() }).is_none() {
                continue;
            }
            let ol = orepo.ensure_overlay_line(ctx, &Self::overlay_line_name(&info, line_id, owner), &begin, owner)?;
            out.push(Layer { overlay: name, state: ol.head });
        }
        self.record_hidden_selection(ctx, line_id, &out)?;
        Ok(out)
    }
}

/// A new state of a layer from its previous one: a writable snapshot, the layer's changes from
/// `src`, read-only. No record is written.
#[allow(clippy::too_many_arguments)]
pub fn next_state(
    repo: &Repo,
    ctx: &Ctx,
    prev: &str,
    src: &Path,
    ops: &Ops,
    own: Option<(u32, u32)>,
    parents: Vec<String>,
    message: &str,
    line: Option<&str>,
) -> Result<StateRec> {
    let _t = crate::trace::span("layer state");
    let id = ids::new_id();
    let dst = repo.p.state_path(&id);
    repo.snapshot_state(prev, &dst, false)?;
    let made = ops.apply(&dst, src, own).and_then(|_| btrfs::set_readonly(&dst)).and_then(|_| btrfs::subvol_info(&dst));
    let info = match made {
        Ok(i) => i,
        Err(e) => {
            let _ = btrfs::delete_tree(&dst);
            return Err(e);
        }
    };
    Ok(StateRec {
        id,
        kind: StateKind::Commit,
        change_id: Some(ids::new_id()),
        derived_from: None,
        context: None,
        parents,
        line: line.map(|s| s.to_string()),
        subvol: info.uuid,
        author: ctx.author(),
        message: message.to_string(),
        time: ids::now_ms(),
        running: false,
        conflicts: Vec::new(),
        caches: BTreeMap::new(),
        cache_by: None,
        cache_shared: false,
        cache_platform: None,
        cache_of: None,
        tree: None,
        times: None,
        cache_key_parts: Default::default(),
        parts: Default::default(),
    })
}

/// The `union-theirs` merge of an overlay line (docs/cache-overlays.md, section 5): a writable
/// snapshot of `ours`, every file of `theirs` copied over it (the incoming file wins), read-only.
/// A file of `ours` where `theirs` has a folder, and the files of `ours` below a path where
/// `theirs` has a file, go. No conflicts. No record is written.
pub fn union_state(orepo: &Repo, ctx: &Ctx, ol: &Line, ours: &str, theirs: &str, message: &str) -> Result<StateRec> {
    let theirs_root = orepo.state_root(theirs)?;
    let ours_files: BTreeSet<String> = files_of(&orepo.state_root(ours)?)?.into_iter().collect();
    let ops = union_ops_by_unit(&ours_files, files_of(&theirs_root)?, &crate::cache_hook::unit_of);
    next_state(
        orepo,
        ctx,
        ours,
        &theirs_root,
        &ops,
        orepo.own(ol),
        vec![ours.to_string(), theirs.to_string()],
        message,
        Some(&ol.id),
    )
}

/// `union_ops`, and each build unit whole from one side: a file of `ours` goes when `theirs` brings files of its
/// unit (`unit`). Files of one unit from two builds (an rmeta of one, a fingerprint of the other) make a stale unit
/// look fresh, or name a file that is not there.
pub(crate) fn union_ops_by_unit(ours: &BTreeSet<String>, theirs: Vec<String>, unit: &dyn Fn(&str) -> Option<String>) -> Ops {
    let incoming: BTreeSet<String> = theirs.iter().cloned().collect();
    let units: BTreeSet<String> = theirs.iter().filter_map(|p| unit(p)).collect();
    let mut ops = union_ops(ours, theirs);
    if !units.is_empty() {
        for q in ours.iter().filter(|q| !incoming.contains(*q)) {
            if unit(q).is_some_and(|u| units.contains(&u)) {
                ops.dels.insert(q.clone());
            }
        }
    }
    ops
}

/// The changes that put every file of `theirs` over a tree with the files `ours`.
pub(crate) fn union_ops(ours: &BTreeSet<String>, theirs: Vec<String>) -> Ops {
    let mut ops = Ops::default();
    for p in theirs {
        let mut at = 0;
        while let Some(i) = p[at..].find('/') {
            let parent = &p[..at + i];
            if ours.contains(parent) {
                ops.dels.insert(parent.to_string());
            }
            at += i + 1;
        }
        let below = format!("{p}/");
        for q in ours.range(below.clone()..).take_while(|q| q.starts_with(&below)) {
            ops.dels.insert(q.clone());
        }
        ops.puts.insert(p);
    }
    ops
}

/// A record in an overlay that moves an overlay line to `head` (with the new states it names).
pub fn move_overlay_line(orepo: &Repo, ctx: &Ctx, op: &str, ol: &Line, head: &str, states: Vec<StateRec>) -> Result<()> {
    let mut rec = Record::new(op);
    rec.states = states;
    let st = |h: &str| LineState { name: ol.name.clone(), head: h.to_string(), owner: ol.owner, machine: ol.machine.clone() };
    rec.lines.insert(
        ol.id.clone(),
        LineChange { before: Some(st(&ol.head)), after: Some(st(head)), working_before: None, working_after: None },
    );
    rec.data = serde_json::json!({"line": ol.id});
    orepo.commit_record(ctx, rec)?;
    Ok(())
}

/// A record in an overlay that keeps new parts (`parts_glass`) of the project line `line_id`. It
/// moves no overlay line: the state records of the line and its stack name the parts.
pub fn record_part(orepo: &Repo, ctx: &Ctx, op: &str, line_id: &str, states: Vec<StateRec>) -> Result<()> {
    let mut rec = Record::new(op);
    rec.states = states;
    rec.data = serde_json::json!({"project_line": line_id});
    orepo.commit_record(ctx, rec)?;
    Ok(())
}

/// What a commit in a line with overlays made.
pub struct Committed {
    pub shared: Option<StateRec>,
    pub overlays: Vec<(String, StateRec)>,
    /// The changes of the whole tree, for the summary (when asked for).
    pub fds: Vec<crate::diff::FileDiff>,
}

/// Commit the index of a line with overlays (docs/overlays.md, section 3.2). The caller holds the
/// project lock and has checked that the caller owns the line. `new_tree` is a read-only snapshot
/// of the index; it becomes the line's base. Returns None when there is nothing to commit.
#[allow(clippy::too_many_arguments)]
pub fn commit_layers(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    layers: &[Layer],
    new_tree: Temp,
    message: &str,
    allow_empty: bool,
    author: Option<(String, String)>,
    summary: bool,
) -> Result<Option<Committed>> {
    let base = repo.head_tree(line)?;
    let shared_root = repo.state_root(&line.head)?;
    let mut orepos = Vec::new();
    let mut roots = Vec::new();
    for l in layers {
        let r = repo.overlay_repo(&l.overlay)?;
        roots.push(r.state_root(&l.state)?);
        orepos.push(r);
    }
    let by_name: BTreeMap<&str, usize> = layers.iter().enumerate().map(|(i, l)| (l.overlay.as_str(), i)).collect();
    let mut pending = BTreeMap::new();
    for (p, o) in repo.pending_moves(line) {
        match by_name.get(o.as_str()) {
            Some(i) => {
                pending.insert(p, *i);
            }
            None => bail!("'{p}' is to move to overlay '{o}', which line '{}' does not show", line.name),
        }
    }
    let s = split(&base, &new_tree.path, &shared_root, &roots, &pending)?;
    if s.shared.is_empty() && s.overlays.iter().all(|o| o.is_empty()) && !allow_empty {
        return Ok(None);
    }
    // A protected line takes changes of the shared set only through merges; changes of its
    // overlays belong to its owner.
    if !s.shared.is_empty() {
        repo.authorize_line(ctx, line, crate::repo::LineOp::Direct)?;
    }
    let own = repo.own(line);
    let mut made: Vec<(usize, StateRec, Option<Line>)> = Vec::new();
    let discard = |made: &[(usize, StateRec, Option<Line>)], orepos: &[Repo]| {
        for (i, st, _) in made {
            orepos[*i].discard_states(std::slice::from_ref(st));
        }
    };
    for (i, ops) in s.overlays.iter().enumerate() {
        if ops.is_empty() {
            continue;
        }
        let l = &layers[i];
        let info = repo.need_overlay(&l.overlay)?;
        let orepo = &orepos[i];
        // A glass that keeps parts moves an overlay line only where an older layr made one.
        let ol = match orepo.p.db.line_by_name(&Repo::overlay_line_name(&info, &line.id, line.owner))? {
            Some(ol) => Some(ol),
            None if parts_glass(&info) => None,
            None => bail!("line '{}' has no line in overlay '{}'", line.name, l.overlay),
        };
        if ol.as_ref().is_some_and(|ol| ol.head != l.state) {
            discard(&made, &orepos);
            return Err(crate::cmd::exit(
                1,
                format!(
                    "error: overlay '{}' changed in another line since this line took it; run 'layr overlay use {}' to take its newest state, then save",
                    l.overlay,
                    layers.iter().map(|x| x.overlay.as_str()).collect::<Vec<_>>().join(" ")
                ),
            ));
        }
        let ol_id = ol.as_ref().map(|ol| ol.id.clone());
        let mut st =
            match next_state(orepo, ctx, &l.state, &new_tree.path, ops, own, vec![l.state.clone()], message, ol_id.as_deref()) {
                Ok(st) => st,
                Err(e) => {
                    discard(&made, &orepos);
                    return Err(e);
                }
            };
        if let Some((n, e)) = &author {
            st.author.name = n.clone();
            st.author.email = e.clone();
        }
        // Conflicts of a merge of the overlay stay listed while the file has markers.
        if let Some(prev) = orepo.p.db.state(&l.state)? {
            let root = orepo.p.state_path(&st.id);
            for c in &prev.conflicts {
                if (c.kind == "content" || c.kind == "add/add") && crate::repo::conflict_open(&root, c)? {
                    st.conflicts.push(c.clone());
                }
            }
        }
        made.push((i, st, ol));
    }
    // The stack after the save; a new shared state names its parts from it.
    let mut new_layers = layers.to_vec();
    for (i, st, _) in &made {
        new_layers[*i].state = st.id.clone();
    }
    let parts = match repo.parts_of(&new_layers).and_then(|mut p| repo.carry_parts(&mut p, &line.head, &new_layers).map(|()| p)) {
        Ok(p) => p,
        Err(e) => {
            discard(&made, &orepos);
            return Err(e);
        }
    };
    let head = repo.state(&line.head)?;
    // A line has its own history (owner decision 2026-11-11): a save that changes only glasses
    // that keep parts makes a state of the line with the same base tree and the new parts. A
    // protected line moves only through merges, so there the parts wait in the stack as before.
    let parts_only = s.shared.is_empty()
        && made.iter().any(|(i, _, _)| repo.overlay_info(&layers[*i].overlay).ok().flatten().is_some_and(|info| parts_glass(&info)))
        && parts != head.parts
        && repo.authorize_line(ctx, line, crate::repo::LineOp::Direct).is_ok();
    let shared = if s.shared.is_empty() && !(allow_empty && made.is_empty()) && !parts_only {
        None
    } else {
        match next_state(repo, ctx, &line.head, &new_tree.path, &s.shared, own, vec![line.head.clone()], message, Some(&line.id))
        {
            Ok(mut st) => {
                if let Some((n, e)) = &author {
                    st.author.name = n.clone();
                    st.author.email = e.clone();
                }
                st.parts = parts;
                // The parts of the build-output glasses (slice 6), as in a save without overlays.
                for (glass, part) in repo.save_named_cache_parts(ctx, line, &st.id) {
                    st.parts.entry(glass).or_insert(part);
                }
                // Content conflicts stay listed while the committed file has markers.
                let root = repo.p.state_path(&st.id);
                for c in &head.conflicts {
                    if (c.kind == "content" || c.kind == "add/add") && crate::repo::conflict_open(&root, c)? {
                        st.conflicts.push(c.clone());
                    }
                }
                Some(st)
            }
            Err(e) => {
                discard(&made, &orepos);
                return Err(e);
            }
        }
    };
    if let Some(state) = &shared {
        if let Err(e) =
            crate::secrets::check_between(ctx, repo, &crate::repo::snap(&shared_root), &repo.state_src(&state.id)?, &line.head)
        {
            repo.discard_states(std::slice::from_ref(state));
            discard(&made, &orepos);
            return Err(e);
        }
    }
    // The overlays first: a lost main record leaves an overlay line ahead of the stack, which the
    // next commit reports, never a stack that names a missing state.
    let mut overlays = Vec::new();
    for (i, st, ol) in made {
        let orepo = &orepos[i];
        let _lock = orepo.p.lock()?;
        match &ol {
            Some(ol) => move_overlay_line(orepo, ctx, "commit", ol, &st.id, vec![st.clone()])?,
            None => record_part(orepo, ctx, "commit", &line.id, vec![st.clone()])?,
        }
        overlays.push((layers[i].overlay.clone(), st));
    }
    let new_head = shared.as_ref().map(|s| s.id.clone()).unwrap_or(line.head.clone());
    let public = repo.public_layers(&new_layers)?;
    let mut data = serde_json::json!({"line":line.id});
    if !public.is_empty() {
        data["stack"] = stack_data(&line.id, &public);
        data["overlays"] = serde_json::json!(overlays
            .iter()
            .filter(|(n, _)| !repo.need_overlay(n).map(|i| i.hidden).unwrap_or(true))
            .map(|(n, _)| n.clone())
            .collect::<Vec<_>>());
    }
    let states: Vec<StateRec> = shared.iter().cloned().collect();
    if shared.is_some() || overlays.iter().any(|(n, _)| !repo.need_overlay(n).map(|i| i.hidden).unwrap_or(true)) {
        repo.move_head(ctx, "commit", line, &new_head, states, None, data)?;
    }
    if let Some(state) = &shared {
        repo.record_hidden_parts(ctx, &state.id, &new_layers)?;
    }
    let fds = if summary { crate::diff::file_diffs(&base, &new_tree.path, &s.list)? } else { Vec::new() };
    repo.install_base(line, new_tree, &new_head, &new_layers)?;
    for (name, _) in &overlays {
        repo.after_overlay_move(ctx, name);
    }
    Ok(Some(Committed { shared, overlays, fds }))
}

/// Change the stack of a line to `layers` and its working folder with it: overlay files come and
/// go, local changes stay (a change in the way refuses, unless `force`). The caller holds the
/// project lock and owns the line.
pub fn set_layers(ctx: &Ctx, repo: &Repo, line: &Line, layers: &[Layer], force: bool, op: &str) -> Result<Vec<Change>> {
    let old = repo.head_tree(line)?;
    let (st, _scan, _it, _idx) = repo.status(line)?;
    let view = repo.assemble(line, &line.head, layers)?;
    let wdir = repo.working(line);
    // An ignored file of the user (a key of its own) is a local file too: never overwritten.
    let mut local = st.local_paths();
    local.extend(st.ignored.iter().cloned());
    let applied = repo.apply_delta(&old, &view.path, &wdir, repo.own(line), &local, force)?;
    let mut rec = Record::new(op);
    rec.data = serde_json::json!({
        "line": line.id,
        "stack": repo.public_stack_data(&line.id, layers)?,
        "folder_view": repo.public_view_data(&line.id, layers)?,
    });
    repo.record_hidden_selection(ctx, &line.id, layers)?;
    if repo.public_layers(&repo.layers(line)?)? != repo.public_layers(layers)? {
        repo.commit_record(ctx, rec)?;
    }
    repo.drop_stage(line)?;
    repo.install_base(line, view, &line.head, layers)?;
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_base_key_names_the_head_and_the_stack() {
        let l = |s: &str| vec![Layer { overlay: "keys".into(), state: s.into() }];
        assert_eq!(base_key("h", &l("a")), base_key("h", &l("a")));
        assert_ne!(base_key("h", &l("a")), base_key("h", &l("b")));
        assert_ne!(base_key("h", &l("a")), base_key("g", &l("a")));
        assert_eq!(base_key("h", &l("a")).len(), 16);
        assert_eq!(Mode::parse("follow"), Some(Mode::Follow));
        assert_eq!(Mode::parse("shared").map(|m| m.name()), Some("shared"));
        assert_eq!(Mode::parse("ephemeral"), None);
    }

    #[test]
    fn union_takes_each_unit_whole_from_theirs() {
        let ours: BTreeSet<String> = ["t/d/deps/libx-1.rlib", "t/d/deps/libx-1.rmeta", "t/d/deps/liby-2.rlib", "t/a.txt"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let theirs = vec!["t/d/deps/libx-1.rmeta".to_string(), "t/b.txt".to_string()];
        let unit = |p: &str| p.split("deps/lib").nth(1).and_then(|r| r.split('.').next()).map(str::to_string);
        let ops = union_ops_by_unit(&ours, theirs, &unit);
        assert!(ops.dels.contains("t/d/deps/libx-1.rlib"), "the old rlib of a unit that theirs rebuilt goes");
        assert!(!ops.dels.contains("t/d/deps/liby-2.rlib"), "a unit theirs did not touch stays");
        assert!(!ops.dels.contains("t/a.txt"));
        assert!(ops.puts.contains("t/d/deps/libx-1.rmeta") && ops.puts.contains("t/b.txt"));
    }

    #[test]
    fn union_takes_every_incoming_file_and_clears_type_changes() {
        let ours: BTreeSet<String> = ["a.txt", "keep.txt", "dir", "tree/x", "tree/y"].iter().map(|s| s.to_string()).collect();
        let ops = union_ops(&ours, vec!["a.txt".into(), "dir/inner".into(), "tree".into(), "new/z".into()]);
        assert!(ops.moves.is_empty());
        let puts: Vec<&str> = ops.puts.iter().map(|s| s.as_str()).collect();
        assert_eq!(puts, ["a.txt", "dir/inner", "new/z", "tree"]);
        let dels: Vec<&str> = ops.dels.iter().map(|s| s.as_str()).collect();
        assert_eq!(dels, ["dir", "tree/x", "tree/y"], "keep.txt stays: a union never deletes a file of ours");
    }

    #[test]
    fn properties_parse_store_and_read_back() {
        assert_eq!(parse_history("all"), Some(None));
        assert_eq!(parse_history("last"), Some(Some(1)));
        assert_eq!(parse_history("3"), Some(Some(3)));
        assert_eq!(parse_history("0"), None);
        assert_eq!(parse_history("x"), None);
        assert_eq!(parse_size("20G"), Some(20 << 30));
        assert_eq!(parse_size("512m"), Some(512 << 20));
        assert_eq!(parse_size("4096"), Some(4096));
        assert_eq!(parse_size("0"), None);
        assert_eq!(parse_size("G"), None);
        assert_eq!(parse_gc("none"), Some(None));
        assert_eq!(parse_gc("lru:1G"), Some(Some(1 << 30)));
        assert_eq!(parse_gc("lru"), None);
        assert_eq!(MergeKind::parse("union-theirs"), Some(MergeKind::UnionTheirs));
        assert_eq!(MergeKind::parse("ours"), None);
        let p = Props {
            history: Some(1),
            quiet: true,
            merge: MergeKind::UnionTheirs,
            gc_lru: Some(20 << 30),
            prune: Some("pnpm".into()),
            platform: Some("linux-x86_64".into()),
            trust: Some(Trust::Protected),
        };
        let values: BTreeMap<&str, serde_json::Value> = PROPS.iter().map(|k| (*k, p.value(k))).collect();
        assert_eq!(Props::from_settings(|k| values.get(k).cloned().filter(|v| !v.is_null())), p);
        assert_eq!(
            p.describe(),
            [
                "history=last",
                "quiet",
                "merge=union-theirs",
                "gc=lru:20G",
                "prune=pnpm",
                "platform=linux-x86_64",
                "trust=protected"
            ]
        );
        // Defaults are no settings at all, and unknown values read as defaults.
        assert!(PROPS.iter().all(|k| Props::default().value(k).is_null()));
        let odd = Props::from_settings(|k| Some(serde_json::json!(if k == "history" { "many" } else { "?" })));
        assert_eq!(odd, Props::default());
        assert_eq!(show_size(1536), "1536");
        assert_eq!(show_size(3 << 20), "3M");
    }

    #[test]
    fn ops_move_delete_and_copy() {
        let root = std::env::temp_dir().join(format!("layr-ops-{}", std::process::id()));
        let (src, dst) = (root.join("src"), root.join("dst"));
        let _ = std::fs::remove_dir_all(&root);
        for d in [&src, &dst] {
            std::fs::create_dir_all(d.join("a/b")).unwrap();
        }
        std::fs::write(dst.join("a/b/old.txt"), "moved").unwrap();
        std::fs::write(dst.join("a/b/gone.txt"), "x").unwrap();
        std::fs::write(dst.join("keep.txt"), "keep").unwrap();
        std::fs::write(src.join("new.txt"), "new").unwrap();
        std::fs::create_dir_all(src.join("c")).unwrap();
        std::fs::write(src.join("c/moved.txt"), "moved and changed").unwrap();
        let ops = Ops {
            moves: vec![("a/b/old.txt".into(), "c/moved.txt".into())],
            dels: ["a/b/gone.txt".to_string()].into(),
            puts: ["new.txt".to_string(), "c/moved.txt".to_string()].into(),
        };
        ops.apply(&dst, &src, None).unwrap();
        assert_eq!(std::fs::read_to_string(dst.join("c/moved.txt")).unwrap(), "moved and changed");
        assert_eq!(std::fs::read_to_string(dst.join("new.txt")).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(dst.join("keep.txt")).unwrap(), "keep");
        assert!(!dst.join("a").exists(), "empty folders go, as in git trees");
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod view_tests {
    use super::*;
    const SHARED: &str = "01a12290-0000-7000-8000-000000000003";
    const OVERLAY: &str = "01a12290-0000-7000-8000-000000000004";

    #[test]
    fn view_identity_uses_exact_canonical_bytes_and_stack_order() {
        use layr_hashers::{ContentAddress, Digest};
        let view = View::new(SHARED.into(), vec![Layer { overlay: "keys".into(), state: OVERLAY.into() }]);
        let expected =
            format!("{{\"format\":1,\"shared\":\"{SHARED}\",\"layers\":[{{\"overlay\":\"keys\",\"state\":\"{OVERLAY}\"}}]}}");
        assert_eq!(serde_json::to_string(&view).unwrap(), expected);
        assert_eq!(view.id().unwrap(), hex::encode(ContentAddress::digest(expected.as_bytes())));
        assert_eq!(view.id().unwrap().len(), 64);
        let mut two = view.clone();
        two.layers.push(Layer { overlay: "config".into(), state: SHARED.into() });
        let mut reversed = two.clone();
        reversed.layers.reverse();
        assert_ne!(two.id().unwrap(), reversed.id().unwrap());
        assert_ne!(view.id().unwrap(), View::new(SHARED.into(), Vec::new()).id().unwrap());
    }

    #[test]
    fn malformed_descriptors_and_ids_fail() {
        let mut view = View::new(SHARED.into(), Vec::new());
        assert!(view.validate().is_ok());
        view.format = 2;
        assert!(view.validate().is_err());
        view.format = 1;
        view.shared = "../escape".into();
        assert!(view.validate().is_err());
        view.shared = SHARED.into();
        view.layers.push(Layer { overlay: "../private".into(), state: OVERLAY.into() });
        assert!(view.validate().is_err());
        assert!(!is_view_id(&"A".repeat(64)));
        assert!(!is_view_id("abc"));
        assert!(is_view_id(&"a".repeat(64)));
    }
}

#[cfg(test)]
mod read_open_tests {
    use super::*;
    use crate::ctx::Caller;
    use crate::store::Store;
    use serde_json::json;

    struct Fixture {
        ctx: Ctx,
        head: String,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            let root = std::env::temp_dir().join(format!("layr-read-opens-{}", ids::new_id()));
            let store = Store::open(&root.join("store"))?;
            let caller = Caller { uid: 1000, gid: 1000, user: "reader".into(), home: root.join("home") };
            let actor = crate::model::Actor { uid: caller.uid, name: caller.user.clone(), session: None };
            let create = |name: &str, config: serde_json::Value| -> Result<Project<'_>> {
                let dir = store.project_dir(name)?;
                for folder in ["meta/db", "meta/scan", "states", "lines"] {
                    std::fs::create_dir_all(dir.join(folder))?;
                }
                let p = Project {
                    store: &store,
                    name: name.into(),
                    db: crate::db::Db::open(&crate::store::db_file(&dir))?,
                    dir,
                    id: ids::new_id(),
                };
                let mut record = Record::new("project");
                record.data = json!({"id":p.id,"name":name,"config":config});
                p.append(record, &actor)?;
                Ok(p)
            };
            let access = json!({format!("user:{}", caller.uid):"admin"});
            let parent = create("p", json!({"access":access}))?;
            let head = ids::new_id();
            let line_id = ids::new_id();
            parent.db.conn.execute(
                "INSERT INTO lines (id,name,owner,machine,head,time,ord) VALUES (?1,'main',?2,?3,?4,0,'1')",
                (&line_id, caller.uid, &store.machine.id, &head),
            )?;
            let cwd = parent.lines_dir().join("main");
            std::fs::create_dir_all(&cwd)?;
            std::fs::create_dir(parent.state_path(&head))?;
            std::fs::write(parent.state_path(&head).join("tracked"), "content")?;
            std::fs::write(cwd.join("tracked"), "content")?;
            create(
                "p%private",
                json!({
                    "access":access,"overlay":{"project":parent.id,"name":"private"},
                    "overlay.private":{"hidden":true,"owner":caller.uid,"mode":"follow","root":head},
                    "overlay.private.quiet":true,"overlay.private.history":3
                }),
            )?;
            drop(parent);
            Ok(Self {
                ctx: Ctx {
                    store,
                    caller,
                    cwd,
                    env: Default::default(),
                    author: Default::default(),
                    out: std::cell::RefCell::new(Box::new(std::io::sink())),
                },
                head,
            })
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.ctx.store.root.parent().unwrap());
        }
    }

    #[test]
    fn ls_files_opens_hidden_overlay_once() -> Result<()> {
        let fixture = Fixture::new()?;
        for _ in 0..2 {
            let before = crate::store::project_open_count();
            assert_eq!(crate::cmd::run(&fixture.ctx, &["ls-files".into()])?, 0);
            assert_eq!(crate::store::project_open_count() - before, 2, "parent plus one hidden overlay");
        }
        Ok(())
    }

    #[test]
    fn hidden_settings_and_private_paths_reuse_their_project() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = Repo::open(&fixture.ctx, "p")?;
        let line = repo.line("main")?;
        let overlay = repo.overlay_project("private")?;
        overlay.db.conn.execute(
            "INSERT INTO lines (id,name,owner,machine,head,time,ord) VALUES (?1,?2,?3,?4,?5,0,'1')",
            (ids::new_id(), format!("line-{}", line.id), fixture.ctx.caller.uid, &fixture.ctx.store.machine.id, &fixture.head),
        )?;
        std::fs::create_dir(overlay.state_path(&fixture.head))?;
        std::fs::write(overlay.state_path(&fixture.head).join("private"), "private content")?;
        for _ in 0..2 {
            let before = crate::store::project_open_count();
            let layers = repo.layers(&line)?;
            assert_eq!(layers.len(), 1);
            assert_eq!(layers[0].overlay, "private");
            assert_eq!(layers[0].state, fixture.head);
            assert_eq!(crate::store::project_open_count() - before, 1);
            let before = crate::store::project_open_count();
            assert_eq!(repo.overlay_setting("private", "history")?, Some(json!(3)));
            assert_eq!(crate::store::project_open_count() - before, 1);
            let before = crate::store::project_open_count();
            let props = repo.overlay_props("private")?;
            assert!(props.quiet);
            assert_eq!(props.history, Some(3));
            assert_eq!(crate::store::project_open_count() - before, 1);
            let before = crate::store::project_open_count();
            assert_eq!(repo.private_paths(&line)?, BTreeSet::from(["private".into()]));
            assert_eq!(crate::store::project_open_count() - before, 1);
        }
        // Public metadata remains readable without a local overlay project; hidden settings
        // and stack discovery still propagate an explicitly declared hidden project's open error.
        let mut record = Record::new("config");
        record.data = json!({"config":{
            "overlay.remote":{"hidden":false},"overlay.remote.quiet":true,
            "overlay.unavailable":{"hidden":true}
        }});
        repo.p.append(record, &fixture.ctx.actor())?;
        assert!(repo.overlay_props("remote")?.quiet);
        for error in [
            repo.overlay_props("unavailable").unwrap_err(),
            repo.layers(&line).unwrap_err(),
            repo.private_paths(&line).unwrap_err(),
        ] {
            assert!(format!("{error:#}").contains("project 'p%unavailable' not found"));
        }
        Ok(())
    }
}
