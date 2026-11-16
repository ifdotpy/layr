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
//! overlays and their order; the setting `stack.<line id>` gives the state the folder shows on each
//! (and the overlays too, for a line made before views).
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

pub(crate) mod create;

use crate::access::{Policy, Who};
use crate::changes::{self, EntryChange};
use crate::ctx::Ctx;
use crate::fsutil;
use crate::ids;
use crate::model::{Line, LineChange, LineState, StateKind, StateRec};
use crate::repo::{skip_path, Repo, Temp};
use crate::store::Project;
use crate::tree::Tree;
use anyhow::{anyhow, bail, Context, Result};
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
    /// `prune=<name>|none`: the cache adapter that prunes the folder (issue #167). None: detect it.
    pub prune: Option<String>,
    /// `platform=<key>` (`<os>-<arch>`): the overlay is shown and fetched only on that platform
    /// (docs/overlays.md, section 1a, slice 3). None: every platform.
    pub platform: Option<String>,
    /// `trust=protected|readers`: whose parts a line takes (docs/overlays.md, section 1a, slice 6).
    /// `protected`: a part that another user saved only from a protected line. `readers`: any
    /// part that the reader may read. None: `protected` for a build-output overlay, else `readers`.
    pub trust: Option<Trust>,
}

/// The `trust` property of an overlay.
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
pub const PROPS: &[&str] = &["history", "quiet", "merge", "prune", "platform", "trust"];

/// A platform key as `--platform` takes it: lowercase letters, digits, `-` and `_` (for example
/// `macos-aarch64`, `linux-x86_64`).
pub fn platform_name(s: &str) -> Option<String> {
    let ok = !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    ok.then(|| s.to_string())
}

/// Whether an overlay with `platform=<p>` belongs to another platform than this machine's
/// (`cache_platform()`, `<os>-<arch>`). An overlay with no platform is on every platform.
pub fn other_platform_overlay(platform: Option<&str>) -> bool {
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
    /// `platform=<key>`: the overlay of one platform (docs/overlays.md, section 1a, slice 3).
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
            || self.project.as_ref().is_ok_and(|p| Policy::load(p).check(&Who::caller(&ctx.caller), "read").is_ok())
    }
}

/// An overlay whose parts the state records name (`StateRec::parts`, docs/overlays.md section 1a): every
/// public follow overlay. A hidden overlay records the parts in its own records (`hidden.part.<state>`).
pub fn parts_overlay(info: &Info) -> bool {
    info.mode == Mode::Follow && !info.hidden
}

/// The name of a line on an overlay: the line's own name (docs/overlays.md section 1). An overlay of
/// the old shared mode keeps one line per user (`user-<uid>`).
pub fn overlay_line_name(info: &Info, line: &Line) -> String {
    match info.mode {
        Mode::Shared => format!("user-{}", line.owner),
        Mode::Follow => line.name.clone(),
    }
}

/// The name that layr before 0.44 gave the line `line_id` in a cache overlay of one folder for all
/// platforms: one overlay line per platform (`<os>-<arch>`).
pub fn cache_line_name(line_id: &str) -> String {
    format!("line-{line_id}.{}", crate::model::cache_platform())
}

/// The line of `line` on an overlay whose project database is `db`: the line of the same name, else
/// the line of the same id (a line that took a new name), else a line with a name of layr before
/// 0.44 (`line-<id>.<platform>`, then `line-<id>` when its head was made on this platform). The
/// caller gives such a line the name of `line` at its next write (`ensure_overlay_line`).
pub fn find_overlay_line(db: &crate::db::Db, info: &Info, line: &Line) -> Result<Option<Line>> {
    if let Some(l) = db.line_by_name(&overlay_line_name(info, line))? {
        return Ok(Some(l));
    }
    if info.mode == Mode::Shared {
        return Ok(None);
    }
    if let Some(l) = db.line_by_id(&line.id)?.filter(|l| !db.line_deleted(&l.id).unwrap_or(true)) {
        return Ok(Some(l));
    }
    let platform_name = cache_line_name(&line.id);
    if let Some(l) = db.line_by_name(&platform_name)? {
        return Ok(Some(l));
    }
    let Some(l) = db.line_by_name(&format!("line-{}", line.id))? else { return Ok(None) };
    let platform = crate::model::cache_platform();
    let made_on = db.state(&l.head)?.and_then(|s| s.cache_platform);
    Ok(made_on.is_none_or(|p| p == platform || p.starts_with(&format!("{platform}-"))).then_some(l))
}

/// The line of `line` on a follow overlay whose project database is `db` (`find_overlay_line`).
pub fn find_follow_line(db: &crate::db::Db, line: &Line) -> Result<Option<Line>> {
    let info = Info { hidden: false, platform: None, owner: line.owner, mode: Mode::Follow, root: String::new() };
    find_overlay_line(db, &info, line)
}

/// Whether the overlay line `ol` belongs to a line of the project that is deleted (`db` is the
/// database of the project). A line that the project does not know (a line of another machine that
/// did not come here) is no proof of a delete: it stays. A line of the old shared mode
/// (`user-<uid>`) always stays.
pub fn orphan_overlay_line(ol: &Line, db: &crate::db::Db) -> Result<bool> {
    let name = ol.name.as_str();
    if name.starts_with("user-") {
        return Ok(false);
    }
    if db.line_by_id(&ol.id)?.is_some() {
        return db.line_deleted(&ol.id);
    }
    if let Some(id) = name.strip_prefix("line-").map(|s| s.split('.').next().unwrap_or(s)) {
        return db.line_deleted(id);
    }
    // A line of the overlay made with a fresh id: its line of the project by name.
    if db.line_by_name(name)?.is_some() {
        return Ok(false);
    }
    Ok(db.lines_deleted_named(name)?)
}

/// The place of a hidden overlay in the view of the line `line_id` (`{"enabled", "position"}`), in the
/// records of the overlay. layr before 0.44 named the setting `hidden.stack.<line id>`.
pub fn hidden_view(op: &Project, line_id: &str) -> Result<Option<serde_json::Value>> {
    match op.db.overlay_config(&format!("hidden.view.{line_id}"))? {
        Some(v) => Ok(Some(v)),
        None => op.db.overlay_config(&format!("hidden.stack.{line_id}")),
    }
}

/// The settings key of the view of the working folder of a line (docs/overlays.md, section 1a,
/// slice 5): the overlays that the folder shows, bottom first. It names overlays only; the part on
/// each overlay comes from the line (its line on the overlay and its state records).
pub fn view_key(line_id: &str) -> String {
    format!("view.{line_id}")
}

/// The setting in the records of an overlay that names the automatic state of its store that holds the working
/// content of the project's automatic state `state` (a folder in the view of the overlay).
pub fn auto_key(state: &str) -> String {
    format!("auto.{state}")
}

/// The settings key of the project order of overlays of older layr. `layr overlay order` is gone
/// (owner decision 2026-11-15); the order only gives old overlays their parent (`derived_parent`).
pub const ORDER_KEY: &str = "overlays.order";

/// The name of the bottom overlay (docs/overlays.md section 1, owner decision 2026-11-15): the
/// project's own files, which `layr git push` publishes. No overlay can take this name.
pub const MAIN: &str = "main";

/// The setting suffix of a cache of the view of an overlay (`overlay.<name>.view`): the overlay of that view.
pub const VIEW: &str = "view";

/// The property suffix of the parent of a patches overlay (`overlay.<name>.parent`).
pub const PARENT: &str = "parent";

/// The kind of a conflict of a patch with its parent that no text merge can show (a binary file, a parent
/// that deleted or renamed the path): the overlay keeps its version, and the next save of the path takes
/// the version of the parent as its base (`split`).
pub const PARENT_CONFLICT: &str = "parent";

/// The longest path of the tree that a view takes; a longer one is a loop.
const MAX_DEPTH: usize = 64;

/// Record data that sets the view of the working folder of a line: the names of its public
/// overlays, bottom first.
pub fn view_data(line_id: &str, overlays: &[String]) -> serde_json::Value {
    serde_json::json!({"line": line_id, "overlays": overlays})
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
        return Err(crate::errors::error(
            crate::errors::Kind::Invalid,
            format!("fatal: invalid folder '{s}': use a relative path inside the line"),
        ));
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
    /// The folder overlays of the project on this platform: name and folder. An overlay with
    /// `platform=<key>` of another platform is left out (docs/overlays.md, section 1a, slice 3).
    pub fn folder_overlays(&self) -> Result<Vec<(String, String)>> {
        Ok(self
            .all_folder_overlays()?
            .into_iter()
            .filter(|(_, _, p)| !crate::overlay::other_platform_overlay(p.as_deref()))
            .map(|(n, f, _)| (n, f))
            .collect())
    }

    /// The folder overlays of every platform that serve the view of `main`: name, folder and `platform`.
    /// The cache of the view of an overlay (`view_cache_overlays`) is not one of them.
    pub fn all_folder_overlays(&self) -> Result<Vec<(String, String, Option<String>)>> {
        let mut out = Vec::new();
        for name in self.overlay_names()? {
            if self.overlay_setting(&name, VIEW)?.is_some_and(|v| !v.is_null()) {
                continue;
            }
            if let Some(f) = self.overlay_setting(&name, "folder")?.and_then(|v| v.as_str().map(str::to_owned)) {
                let p = self.overlay_setting(&name, "platform")?.and_then(|v| v.as_str().and_then(crate::overlay::platform_name));
                out.push((name, f, p));
            }
        }
        Ok(out)
    }

    /// The caches of the views of overlays on this platform (docs/overlays.md section 1, caches per view):
    /// name, folder and the overlay of the view. Only a view whose path holds that overlay takes them.
    pub fn view_cache_overlays(&self) -> Result<Vec<(String, String, String)>> {
        let mut out = Vec::new();
        for name in self.overlay_names()? {
            let Some(view) = self.overlay_setting(&name, VIEW)?.and_then(|v| v.as_str().map(str::to_owned)) else { continue };
            let Some(f) = self.overlay_setting(&name, "folder")?.and_then(|v| v.as_str().map(str::to_owned)) else { continue };
            let p = self.overlay_setting(&name, "platform")?.and_then(|v| v.as_str().and_then(platform_name));
            if !other_platform_overlay(p.as_deref()) {
                out.push((name, f, view));
            }
        }
        Ok(out)
    }

    /// The cache overlays that serve `folder` in the folder of `line`, whose view has the overlays `path`
    /// (bottom first), best first: the cache of the view of each overlay of the path that its owner reads (from the top), then the cache of `main`. The view of
    /// `main` has only the cache of `main`.
    pub fn cache_chain(&self, line: &Line, path: &[String], folder: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        if !path.is_empty() {
            let views = self.view_cache_overlays()?;
            for x in path.iter().rev() {
                for (name, f, v) in &views {
                    if f == folder && v == x && self.owner_reads(name, line.owner).unwrap_or(false) {
                        out.push(name.clone());
                    }
                }
            }
        }
        out.extend(self.folder_overlay_of(folder)?);
        Ok(out)
    }

    /// The folder overlay of `folder` on this platform. An overlay of this platform wins over an older
    /// overlay without a platform (a `cache.<folder>` overlay), which serves every platform.
    pub fn folder_overlay_of(&self, folder: &str) -> Result<Option<String>> {
        let mut found: Option<(String, bool)> = None;
        for (n, f, p) in self.all_folder_overlays()? {
            if f != folder || crate::overlay::other_platform_overlay(p.as_deref()) {
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

/// The metadata of an overlay state (docs/overlays.md section 1, owner decision 2026-11-15): a folder at
/// the root of the state that no view shows. `meta.json` holds the delete markers and the patch bases;
/// `bases/<sha256>` holds the content of each base. It is part of the overlay's tree, so peer sync,
/// store remotes and the overlay's git remote carry it; a merge of the overlay merges it by structure
/// (`merge_meta`), never as text.
pub const META_DIR: &str = ".layrmeta";
const META_FILE: &str = ".layrmeta/meta.json";
const META_BASES: &str = ".layrmeta/bases";
/// The version of `meta.json`. A reader refuses a newer one: it cannot know what such a state hides.
const META_VERSION: u64 = 1;

/// Whether `p` is a path of the metadata of an overlay state.
pub fn is_meta_path(p: &str) -> bool {
    p == META_DIR || p.starts_with(".layrmeta/")
}

/// The metadata of one overlay state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Meta {
    /// The paths that the overlay hides from the layers below it ("deleted here").
    pub removed: BTreeSet<String>,
    /// For each patch: the SHA-256 (hex) of the version of the parent view that it was made from.
    pub bases: BTreeMap<String, String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct MetaFile {
    v: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    removed: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    bases: BTreeMap<String, String>,
}

/// A path that metadata may name: relative, inside the tree (`layr_store::paths::check_rel`), not a path
/// of the metadata itself. A newline is allowed: JSON keeps it.
pub fn meta_path_ok(p: &str) -> bool {
    !p.is_empty() && layr_store::paths::check_rel(p).is_ok() && !is_meta_path(p) && !skip_path(p)
}

fn hash_ok(h: &str) -> bool {
    h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Meta {
    pub fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.bases.is_empty()
    }

    /// Parse `meta.json`. A newer version is an error. A path that is not a relative path inside the tree
    /// (absolute, with `.` or `..`, or a metadata path) and a malformed hash are left out: an overlay state
    /// can come from another machine, and assembly must never remove a path outside the tree.
    pub fn parse(data: &[u8]) -> Result<Meta> {
        let f: MetaFile = serde_json::from_slice(data).map_err(|e| anyhow!("invalid overlay metadata: {e}"))?;
        if f.v != META_VERSION {
            bail!("overlay metadata of version {} needs a newer layr (this layr reads version {META_VERSION})", f.v);
        }
        Ok(Meta {
            removed: f.removed.into_iter().filter(|p| meta_path_ok(p)).collect(),
            bases: f.bases.into_iter().filter(|(p, h)| meta_path_ok(p) && hash_ok(h)).collect(),
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let f = MetaFile { v: META_VERSION, removed: self.removed.iter().cloned().collect(), bases: self.bases.clone() };
        let mut v = serde_json::to_vec_pretty(&f).expect("metadata serializes");
        v.push(b'\n');
        v
    }
}

/// The metadata of the overlay state `root`; a state without it has none. The folder and the file are read
/// without following links.
pub fn read_meta(root: &(impl fsutil::Src + ?Sized)) -> Result<Meta> {
    if !fsutil::lmeta(root, META_DIR).is_some_and(|m| m.is_dir()) || fsutil::lmeta(root, META_FILE).is_none() {
        return Ok(Meta::default());
    }
    Meta::parse(&fsutil::read_file(root, META_FILE)?)
}

/// The paths that the overlay state `root` hides (`Meta::removed`).
pub fn removed_paths(root: &Path) -> Result<BTreeSet<String>> {
    Ok(read_meta(root)?.removed)
}

/// The files of an overlay state that a view shows: its files without its metadata.
pub fn overlay_files(root: &Path) -> Result<Vec<String>> {
    Ok(files_of(root)?.into_iter().filter(|p| !is_meta_path(p)).collect())
}

/// The content address of a version of a file: SHA-256 of its bytes (of `link:<target>` for a link).
pub fn version_hash(root: &(impl fsutil::Src + ?Sized), p: &str) -> Result<Option<String>> {
    use layr_hashers::Digest;
    let Some(m) = fsutil::lmeta(root, p) else { return Ok(None) };
    if m.is_dir() {
        return Ok(None);
    }
    let data = if m.ftype == fsutil::FType::Symlink {
        format!("link:{}", fsutil::read_link(root, p)?).into_bytes()
    } else {
        fsutil::read_file(root, p)?
    };
    Ok(Some(hex::encode(layr_hashers::ContentAddress::digest(&data))))
}

/// Whether `p` is the same entry in two trees: the same kind, content (or link target) and execute bit.
fn same_entry(a: &Path, b: &Path, p: &str) -> Result<bool> {
    let (Some(ma), Some(mb)) = (fsutil::lmeta(a, p), fsutil::lmeta(b, p)) else { return Ok(false) };
    let link = |m: &fsutil::Meta| m.ftype == fsutil::FType::Symlink;
    if ma.is_dir() || mb.is_dir() || link(&ma) != link(&mb) {
        return Ok(false);
    }
    if link(&ma) {
        return Ok(fsutil::read_link(a, p)? == fsutil::read_link(b, p)?);
    }
    if ma.size != mb.size || ma.executable() != mb.executable() || ma.size > fsutil::MAX_TEXT {
        return Ok(false);
    }
    Ok(fsutil::read_file(a, p)? == fsutil::read_file(b, p)?)
}

/// Write the metadata `meta` into the writable state `dst`: `meta.json`, the base blobs that it names
/// (from `blobs`, else kept from the state), and no blob that it does not name. No metadata: no folder.
fn write_meta(dst: &Path, meta: &Meta, blobs: &BTreeMap<String, (PathBuf, String)>, own: Option<(u32, u32)>) -> Result<()> {
    let t = Tree::open(dst)?;
    if meta.is_empty() {
        if fsutil::lmeta(dst, META_DIR).is_some() {
            t.remove(META_DIR)?;
        }
        return Ok(());
    }
    // The state before this one can come from another machine, so any entry of the metadata folder can
    // be a link. Every write goes through `Tree`, which never follows a link and replaces an entry that
    // is in the way. Only `meta.json` and `bases` stay in the folder.
    t.mkdirs(META_BASES, own)?;
    for name in t.list(META_DIR)? {
        if name != "meta.json" && name != "bases" {
            t.remove(&format!("{META_DIR}/{name}"))?;
        }
    }
    {
        use std::io::Write;
        let mut f = t.create(META_FILE, 0o644)?;
        f.write_all(&meta.to_bytes())?;
    }
    let wanted: BTreeSet<&String> = meta.bases.values().collect();
    for h in &wanted {
        let rel = format!("{META_BASES}/{h}");
        if t.stat(&rel).is_some() {
            continue;
        }
        let (root, p) = blobs.get(*h).ok_or_else(|| anyhow!("the base {h} of an overlay patch is missing"))?;
        fsutil::copy_entry(root.as_path(), p, dst, &rel, own)?;
    }
    for name in t.list(META_BASES)? {
        if !wanted.contains(&name) {
            t.remove(&format!("{META_BASES}/{name}"))?;
        }
    }
    if let Some((uid, gid)) = own {
        for rel in [META_DIR, META_BASES, META_FILE] {
            t.chown(rel, uid, gid)?;
        }
    }
    Ok(())
}

/// Write merged metadata into the writable merge result `dst`; each base blob comes from the first of
/// `roots` (the merged states) that holds it.
pub fn write_merged_meta(dst: &Path, meta: &Meta, roots: &[PathBuf], own: Option<(u32, u32)>) -> Result<()> {
    let mut blobs = BTreeMap::new();
    for h in meta.bases.values() {
        let rel = format!("{META_BASES}/{h}");
        if let Some(r) = roots.iter().find(|r| fsutil::lmeta(r.as_path(), &rel).is_some_and(|m| m.is_file())) {
            blobs.insert(h.clone(), (r.clone(), rel));
        }
    }
    // A merged folder may hold a text merge of `meta.json`: it is written again whole.
    let mut meta = meta.clone();
    meta.bases.retain(|_, h| blobs.contains_key(h) || fsutil::lmeta(dst, &format!("{META_BASES}/{h}")).is_some_and(|m| m.is_file()));
    write_meta(dst, &meta, &blobs, own)
}

/// The root of the version of `p` that the view below layer `i` shows: the nearest layer below that holds
/// it, unless a layer between hides it; else `main` when it holds it.
pub fn below_root(shared: &Path, roots: &[PathBuf], removed: &[BTreeSet<String>], i: usize, p: &str) -> Option<PathBuf> {
    for j in (0..i).rev() {
        if removed[j].contains(p) {
            return None;
        }
        if has_file(&roots[j], p) {
            return Some(roots[j].clone());
        }
    }
    has_file(shared, p).then(|| shared.to_path_buf())
}

/// What `patch_report` says of one overlay of a view.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct PatchReport {
    pub overlay: String,
    /// Versions of files of the parent view.
    pub patches: Vec<String>,
    /// Files that the parent view does not have (or that have no recorded base).
    pub own: Vec<String>,
    /// Paths of the parent view that this overlay deletes.
    pub hidden: Vec<String>,
    /// Patches whose parent version changed since the patch was made.
    pub waiting: Vec<String>,
}

impl PatchReport {
    /// The text of `layr status` and `layr save`: one line per kind that has paths.
    pub fn lines(&self) -> Vec<String> {
        let list = |v: &[String]| {
            let mut s: Vec<String> = v.iter().take(10).cloned().collect();
            if v.len() > 10 {
                s.push(format!("and {} more", v.len() - 10));
            }
            s.join(", ")
        };
        let mut out = Vec::new();
        if !self.patches.is_empty() {
            out.push(format!("{}: patches of the parent: {}", self.overlay, list(&self.patches)));
        }
        if !self.hidden.is_empty() {
            out.push(format!("{}: hidden from the parent: {}", self.overlay, list(&self.hidden)));
        }
        if !self.waiting.is_empty() {
            out.push(format!("{}: waits for a merge with the parent: {}", self.overlay, list(&self.waiting)));
        }
        out
    }
}

/// Merge the metadata of three overlay states by structure (a merge of the overlay between lines): per
/// path, the side that changed it wins; when both changed it differently, ours. A path that the merged
/// tree holds is never hidden by its own marker.
pub fn merge_meta(base: &Meta, ours: &Meta, theirs: &Meta, merged: &Path) -> Meta {
    fn pick<T: Clone + PartialEq>(b: Option<T>, o: Option<T>, t: Option<T>) -> Option<T> {
        if o == t || t == b {
            o
        } else {
            t
        }
    }
    let mut out = Meta::default();
    let paths: BTreeSet<&String> = base.removed.iter().chain(&ours.removed).chain(&theirs.removed).collect();
    for p in paths {
        let on = pick(base.removed.contains(p).then_some(()), ours.removed.contains(p).then_some(()), theirs.removed.contains(p).then_some(()));
        if on.is_some() && !has_file(merged, p) {
            out.removed.insert(p.clone());
        }
    }
    let keys: BTreeSet<&String> = base.bases.keys().chain(ours.bases.keys()).chain(theirs.bases.keys()).collect();
    for p in keys {
        if let Some(h) = pick(base.bases.get(p).cloned(), ours.bases.get(p).cloned(), theirs.bases.get(p).cloned()) {
            if has_file(merged, p) {
                out.bases.insert(p.clone(), h);
            }
        }
    }
    out
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
    /// Delete markers to add to an overlay state: the overlay hides these paths of the layers below.
    pub marks: BTreeSet<String>,
    /// Delete markers to take out: the path shows again (from this layer or a lower one).
    pub unmarks: BTreeSet<String>,
    /// Patch bases to set: path to the hash of the version of the parent view (`Meta::bases`).
    pub bases: BTreeMap<String, String>,
    /// Patch bases to drop (the path is no patch any more). A deleted or moved path drops its base too.
    pub unbases: BTreeSet<String>,
    /// Where the content of each new base is: hash to the root and path of that version.
    pub blobs: BTreeMap<String, (PathBuf, String)>,
    /// Paths of `puts` whose content is not in the saved tree: path to the root that holds it (the
    /// version that `save --to <ancestor>` made for the ancestor, `Split::down`).
    pub srcs: BTreeMap<String, PathBuf>,
}

impl Ops {
    pub fn is_empty(&self) -> bool {
        self.moves.is_empty()
            && self.dels.is_empty()
            && self.puts.is_empty()
            && self.marks.is_empty()
            && self.unmarks.is_empty()
            && self.bases.is_empty()
            && self.unbases.is_empty()
    }

    /// Apply to `dst` (writable) with the content of `src`: moves first (the inode stays, so btrfs
    /// reports the move), then deletes, then reflink copies.
    pub fn apply(&self, dst: &Path, src: &Path, own: Option<(u32, u32)>) -> Result<()> {
        let t = Tree::open(dst)?;
        let old = read_meta(dst)?;
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
        let from = |p: &String| self.srcs.get(p).map(|r| r.as_path()).unwrap_or(src);
        for p in &self.puts {
            fsutil::copy_entry(from(p), p, dst, p, own)?;
        }
        let mut meta = old.clone();
        meta.removed.extend(self.marks.iter().cloned());
        meta.removed.retain(|p| !self.unmarks.contains(p));
        for (o, _) in &self.moves {
            meta.bases.remove(o);
        }
        meta.bases.retain(|p, _| !self.dels.contains(p) && !self.unbases.contains(p));
        meta.bases.extend(self.bases.iter().map(|(p, h)| (p.clone(), h.clone())));
        if meta != old {
            write_meta(dst, &meta, &self.blobs, own)?;
        }
        // The times last: an entry made by a copy changes the time of its folder. Cargo compares the times of the
        // cache, so every copied entry gets the source's time, on every file system (union and snapshot alike).
        for p in &self.puts {
            fsutil::keep_times(&from(p).join(p), &dst.join(p))?;
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

/// A layer of the path of a view, for `split`: its name and whether it is a patches overlay.
pub struct LayerKind {
    pub name: String,
    pub patches: bool,
}

/// The version of a path that `save --to <ancestor>` gives the ancestor when an overlay above it keeps
/// a patch of the path: the version of the ancestor with only the change of this save.
pub struct Down {
    pub data: Vec<u8>,
    pub mode: u32,
    /// The overlay (index) whose patch takes this version as its new base, with the hash of the version.
    pub base_of: Option<(usize, String)>,
}

/// The split of a change list by layer.
pub struct Split {
    pub shared: Ops,
    pub overlays: Vec<Ops>,
    /// The change list of the whole tree (with renames), for the summary.
    pub list: Vec<EntryChange>,
    /// The target's versions that are not in the saved tree, by path. The caller writes them to a tree
    /// and names that tree in `Ops::srcs` and `Ops::blobs` (`commit_layers`).
    pub down: BTreeMap<String, Down>,
}

/// Split the changes from `base` (the line's full tree) to `new` by layer (docs/overlays.md section 1,
/// write rule, owner decision 2026-11-15). `shared` and `roots` are the previous states of the
/// layers, the path of the view bottom first; `pending` names paths that `layr overlay move` sends to
/// an overlay, and `sticky` the files of quiet overlays, which stay in their overlay. Every other change
/// goes to `target`: the top of the view, or the ancestor of
/// `save --to`. A new or changed file goes to the target, and the layers above it drop the path (they
/// would hide the saved file). A delete takes the path out of the target and the layers above it,
/// and the target gets a delete marker when a layer below it still shows the path. A rename is a
/// delete of the old path and a new file; inside the target it stays a move (btrfs keeps the inode).
/// `main` changes only when it is the target.
///
/// `save --to <ancestor>` of a file that a patches overlay above the ancestor holds works as a cherry-pick
/// in Git (owner decision 2026-11-15): the ancestor gets only the change of this save (a three-way merge;
/// base: the version that the folder showed, ours: the version of the view of the ancestor, theirs: the
/// saved version), and the overlay keeps its patch and shows the saved version. A change that overlaps
/// the patch, a file that is not text, a file that only the overlay has, and a rename or a delete of such a
/// file are refused: the private part of the file never goes down, and never goes away, as a side effect. `layr overlay move main` moves
/// a whole file to `main` (`publish`): `main` takes the version that the view shows, and every overlay
/// drops its entry and its marker.
///
/// An overlay keeps a path only when it differs from the view of its parent: a file saved equal to the
/// version below drops the overlay's entry (and its marker). A file that the overlay takes from the view
/// below (a new patch) records that version as its base; a patch keeps its base, except a path in
/// `resolving` (a conflict with the parent that this save resolves), which takes the version below now.
pub fn split(
    base: &Path,
    new: &Path,
    shared: &Path,
    roots: &[PathBuf],
    pending: &BTreeMap<String, usize>,
    sticky: &BTreeMap<String, usize>,
    target: Owner,
    resolving: &BTreeSet<(usize, String)>,
    kinds: &[LayerKind],
    publish: &BTreeSet<String>,
) -> Result<Split> {
    let list: Vec<EntryChange> = changes::file_changes(base, new)?.into_iter().filter(|c| !skip_path(&c.path())).collect();
    let list = changes::with_renames(base, new, list);
    let removed: Vec<BTreeSet<String>> = roots.iter().map(|r| removed_paths(r)).collect::<Result<_>>()?;
    let metas: Vec<Meta> = roots.iter().map(|r| read_meta(r.as_path())).collect::<Result<_>>()?;
    let holds = |o: Owner, p: &str| match o {
        Owner::Shared => has_file(shared, p),
        Owner::Overlay(i) => has_file(&roots[i], p),
    };
    let all = |p: &str| -> Vec<Owner> {
        std::iter::once(Owner::Shared).chain((0..roots.len()).map(Owner::Overlay)).filter(|o| holds(*o, p)).collect()
    };
    // The layers at and above an owner: the target and the layers that would hide its file.
    let at_or_above = |o: Owner| -> Vec<Owner> {
        let from = match o {
            Owner::Shared => 0,
            Owner::Overlay(i) => i,
        };
        let mut v = Vec::new();
        if o == Owner::Shared {
            v.push(Owner::Shared);
        }
        v.extend((from..roots.len()).map(Owner::Overlay));
        v
    };
    // The root of the version of `p` that the view below overlay `i` shows: the nearest layer below that
    // holds it, unless a layer between hides it.
    let below = |i: usize, p: &str| -> Option<PathBuf> {
        for j in (0..i).rev() {
            if removed[j].contains(p) {
                return None;
            }
            if has_file(&roots[j], p) {
                return Some(roots[j].clone());
            }
        }
        has_file(shared, p).then(|| shared.to_path_buf())
    };
    let below_shows = |i: usize, p: &str| below(i, p).is_some();
    let mut s = Split {
        shared: Ops::default(),
        overlays: roots.iter().map(|_| Ops::default()).collect(),
        list: Vec::new(),
        down: BTreeMap::new(),
    };
    let target_name = match target {
        Owner::Shared => MAIN.to_string(),
        Owner::Overlay(i) => kinds.get(i).map(|k| k.name.clone()).unwrap_or_default(),
    };
    // The patches overlays above the target that hold `p`, bottom first.
    let patch_holders = |p: &str| -> Vec<usize> {
        let from = match target {
            Owner::Shared => 0,
            Owner::Overlay(i) => i + 1,
        };
        (from..roots.len()).filter(|j| kinds.get(*j).is_some_and(|k| k.patches) && has_file(&roots[*j], p)).collect()
    };
    // The root of the version of `p` that the view of the target shows.
    let target_view = |p: &str| -> Option<PathBuf> {
        match target {
            Owner::Shared => has_file(shared, p).then(|| shared.to_path_buf()),
            Owner::Overlay(t) if removed[t].contains(p) => None,
            Owner::Overlay(t) if has_file(&roots[t], p) => Some(roots[t].clone()),
            Owner::Overlay(t) => below(t, p),
        }
    };
    let refuse = |text: String| -> anyhow::Error { crate::cmd::exit(1, format!("error: --to {target_name}: {text}")) };
    let whole = |p: &str| format!("to move the whole file, run 'layr overlay move {target_name} {p}', then save");
    fn ops(s: &mut Split, o: Owner) -> &mut Ops {
        match o {
            Owner::Shared => &mut s.shared,
            Owner::Overlay(i) => &mut s.overlays[i],
        }
    }
    // A path that a layer gets shows again: the delete markers of that layer and of the layers above go.
    let unmark = |s: &mut Split, o: Owner, p: &str| {
        let from = match o {
            Owner::Shared => 0,
            Owner::Overlay(i) => i,
        };
        for (j, set) in removed.iter().enumerate().skip(from) {
            if set.contains(p) {
                s.overlays[j].unmarks.insert(p.to_string());
            }
        }
    };
    // The metadata of an overlay is no file of the view: no layer gets it from a folder.
    let check_name = |o: Owner, p: &str| -> Result<()> {
        if matches!(o, Owner::Overlay(_)) && (is_meta_path(p) || !meta_path_ok(p)) {
            bail!("'{p}' cannot be a file of an overlay ('{META_DIR}' holds the metadata of an overlay); rename the file");
        }
        Ok(())
    };
    // The path leaves the owner `o` and every layer above it; a lower layer that still shows it is hidden.
    let take_out = |s: &mut Split, o: Owner, p: &str, keep: Option<Owner>| {
        for x in at_or_above(o) {
            if Some(x) != keep && holds(x, p) {
                ops(s, x).dels.insert(p.to_string());
            }
        }
        if let Owner::Overlay(i) = o {
            if below_shows(i, p) {
                s.overlays[i].marks.insert(p.to_string());
            }
        }
    };
    // `layr overlay move` of `p` to overlay `i` (docs/overlays.md section 1, move): `i` holds the version that
    // the view shows, the overlays above it drop their entry, and the layers below keep theirs; `i` records
    // a patch (with its base) when a layer below shows the path. A file that only `main` holds (no overlay
    // has it) moves out of `main`: `main` deletes it.
    let move_into = |s: &mut Split, i: usize, p: &str| -> Result<()> {
        let owner = Owner::Overlay(i);
        check_name(owner, p)?;
        for j in (i + 1)..roots.len() {
            if holds(Owner::Overlay(j), p) {
                s.overlays[j].dels.insert(p.to_string());
            }
        }
        let only_main = !(0..roots.len()).any(|j| holds(Owner::Overlay(j), p));
        if only_main && has_file(shared, p) {
            s.shared.dels.insert(p.to_string());
        }
        let under = if only_main { None } else { below(i, p) };
        if under.as_ref().is_some_and(|r| same_entry(r, new, p).unwrap_or(false)) {
            // The version below is the version that the view shows: `i` keeps no entry.
            if holds(owner, p) {
                s.overlays[i].dels.insert(p.to_string());
            }
            unmark(s, owner, p);
            return Ok(());
        }
        match under {
            Some(r) => {
                if let Some(h) = version_hash(r.as_path(), p)? {
                    s.overlays[i].blobs.insert(h.clone(), (r.clone(), p.to_string()));
                    s.overlays[i].bases.insert(p.to_string(), h);
                }
            }
            None => {
                s.overlays[i].unbases.insert(p.to_string());
            }
        }
        s.overlays[i].puts.insert(p.to_string());
        unmark(s, owner, p);
        Ok(())
    };
    // `layr overlay move main` of `p`: `main` holds the version that the view shows, and no overlay keeps
    // an entry or a marker for the path.
    let publish_into = |s: &mut Split, p: &str| {
        for j in 0..roots.len() {
            if has_file(&roots[j], p) {
                s.overlays[j].dels.insert(p.to_string());
            }
        }
        if !same_entry(shared, new, p).unwrap_or(false) {
            s.shared.puts.insert(p.to_string());
        }
        unmark(s, Owner::Shared, p);
    };
    let mut handled: BTreeSet<String> = BTreeSet::new();
    for c in &list {
        handled.insert(c.path().clone());
        let p = c.path().clone();
        if publish.contains(&p) && c.new().is_some() && !sticky.contains_key(&p) {
            if let Some(old) = c.old_path() {
                handled.insert(old.clone());
                take_out(&mut s, Owner::Shared, old, None);
            }
            publish_into(&mut s, &p);
            continue;
        }
        if let Some(i) = pending.get(&p).or_else(|| sticky.get(&p)).copied() {
            // `layr overlay move`: the file goes to its overlay (`move_into`). A file of a quiet overlay
            // (`sticky`) stays in it whatever the target is: `save --to main` never takes it.
            if let Some(old) = c.old_path() {
                handled.insert(old.clone());
                take_out(&mut s, Owner::Overlay(i), old, None);
            }
            if c.new().is_some() {
                move_into(&mut s, i, &p)?;
            } else {
                take_out(&mut s, Owner::Overlay(i), &p, None);
            }
            continue;
        }
        match (c.new(), c.old_path()) {
            (None, _) => {
                // `save --to <ancestor>` of a delete of a file that a patches overlay above the ancestor holds:
                // the delete would take the patch too. Git stops such a cherry-pick with a modify/delete
                // conflict, and `git rm` refuses a file with changes that the commit does not have. So the
                // save is refused and the patch stays. A file that `layr overlay move main` named goes whole.
                if let (Some(a), false) = (patch_holders(&p).last().copied(), publish.contains(&p)) {
                    let name = &kinds[a].name;
                    return Err(refuse(match target_view(&p) {
                        Some(_) => format!(
                            "'{p}' has a patch of overlay '{name}', and its delete in {target_name} would delete the patch too (in Git: a modify/delete conflict); save it without --to (the view hides the file, and {target_name} keeps it), or drop the patch first (in the view of overlay '{name}': 'layr restore --parent {p}', then save) and delete the file after that"
                        ),
                        None => format!(
                            "'{p}' is a file of overlay '{name}' only ({target_name} does not have it), so {target_name} has nothing to delete; save it without --to"
                        ),
                    }));
                }
                take_out(&mut s, target, &p, None)
            }
            (Some(_), old) => {
                check_name(target, &p)?;
                // `save --to <ancestor>` of a file with a patch above the ancestor: only the change goes down.
                let holders = patch_holders(&p);
                let mut kept: Option<(usize, Vec<u8>, u32)> = None;
                if let Some(old) = old {
                    if let Some(a) = patch_holders(old).last() {
                        return Err(refuse(format!(
                            "'{old}' has a patch of overlay '{}', so its rename to '{p}' cannot go to {target_name}; save it without --to, or {}",
                            kinds[*a].name,
                            whole(old)
                        )));
                    }
                } else if let (Some(a), true) = (holders.last().copied(), has_file(base, &p)) {
                    let name = &kinds[a].name;
                    let Some(view) = target_view(&p) else {
                        return Err(refuse(format!(
                            "'{p}' is a file of overlay '{name}' only ({target_name} does not have it), so the save would publish the whole file; save it without --to, or {}",
                            whole(&p)
                        )));
                    };
                    let file = |r: &Path| fsutil::lmeta(r, &p).is_some_and(|m| !m.is_dir() && m.ftype != fsutil::FType::Symlink);
                    let merged = if file(base) && file(view.as_path()) && file(new) {
                        merge_text(&fsutil::read_file(base, &p)?, &fsutil::read_file(view.as_path(), &p)?, &fsutil::read_file(new, &p)?, "save")?
                    } else {
                        None
                    };
                    let data = match merged {
                        Some((data, true)) => data,
                        Some((_, false)) => {
                            return Err(refuse(format!(
                                "the change of '{p}' touches the patch of overlay '{name}' (the same lines or the lines next to them), so it cannot go to {target_name} alone; save it without --to, or {}",
                                whole(&p)
                            )))
                        }
                        None => {
                            return Err(refuse(format!(
                                "'{p}' has a patch of overlay '{name}' and is not a text file, so layr cannot take only the change of this save; save it without --to, or {}",
                                whole(&p)
                            )))
                        }
                    };
                    let mode = if fsutil::lmeta(new, &p).is_some_and(|m| m.executable()) { 0o755 } else { 0o644 };
                    kept = Some((a, data, mode));
                }
                // The version that the target gets equals `r`'s version of the path.
                let same_as_target = |r: &Path| -> bool {
                    match &kept {
                        None => same_entry(r, new, &p).unwrap_or(false),
                        Some((_, data, mode)) => fsutil::lmeta(r, &p).is_some_and(|m| {
                            !m.is_dir()
                                && m.ftype != fsutil::FType::Symlink
                                && m.executable() == (*mode == 0o755)
                                && fsutil::read_file(r, &p).is_ok_and(|d| &d == data)
                        }),
                    }
                };
                let mut moved = false;
                if let Some(old) = old {
                    handled.insert(old.clone());
                    // A file of the target keeps its inode: a move inside the target.
                    if holds(target, old) && !pending.contains_key(old) {
                        ops(&mut s, target).moves.push((old.clone(), p.clone()));
                        moved = true;
                    }
                    take_out(&mut s, target, old, moved.then_some(target));
                }
                for x in at_or_above(target) {
                    // The patches overlays above the target keep their entry when only the change goes down.
                    let keeps = kept.is_some() && matches!(x, Owner::Overlay(j) if holders.contains(&j));
                    if x != target && holds(x, &p) && !keeps {
                        ops(&mut s, x).dels.insert(p.clone());
                    }
                }
                if let Some((a, data, mode)) = &kept {
                    use layr_hashers::Digest;
                    // The top holder shows the saved version. When the target is its parent view of the path and
                    // its patch is not waiting, the target's new version is its new base; else the patch waits
                    // for the merge with the parent at the end of the save (`rebase_patches`).
                    let first = holders.first() == Some(a);
                    let hidden_between = match target {
                        Owner::Shared => (0..*a).any(|j| removed[j].contains(&p)),
                        Owner::Overlay(t) => ((t + 1)..*a).any(|j| removed[j].contains(&p)),
                    };
                    let parent_is_target = first && !hidden_between;
                    let equal = fsutil::lmeta(new, &p).is_some_and(|m| m.executable() == (*mode == 0o755))
                        && fsutil::read_file(new, &p).is_ok_and(|d| &d == data);
                    let mut base_of = None;
                    if parent_is_target && equal {
                        // The saved version is the target's new version: the overlay keeps no entry.
                        s.overlays[*a].dels.insert(p.clone());
                    } else {
                        s.overlays[*a].puts.insert(p.clone());
                        let view_hash = match target {
                            Owner::Shared => version_hash(shared, &p)?,
                            Owner::Overlay(t) if has_file(&roots[t], &p) => version_hash(roots[t].as_path(), &p)?,
                            Owner::Overlay(t) => match below(t, &p) {
                                Some(r) => version_hash(r.as_path(), &p)?,
                                None => None,
                            },
                        };
                        if parent_is_target && view_hash.is_some() && metas[*a].bases.get(&p) == view_hash.as_ref() {
                            let h = hex::encode(layr_hashers::ContentAddress::digest(data));
                            s.overlays[*a].bases.insert(p.clone(), h.clone());
                            base_of = Some((*a, h));
                        }
                    }
                    s.down.insert(p.clone(), Down { data: data.clone(), mode: *mode, base_of });
                }
                if let Owner::Overlay(i) = target {
                    let under = below(i, &p);
                    // Equal to the view of the parent: the overlay keeps no entry for the path.
                    if !moved && under.as_ref().is_some_and(|r| same_as_target(r.as_path())) {
                        if holds(target, &p) {
                            ops(&mut s, target).dels.insert(p.clone());
                        }
                        unmark(&mut s, target, &p);
                        continue;
                    }
                    let had = holds(target, &p) && metas[i].bases.contains_key(&p);
                    match &under {
                        // A new patch, or a conflict with the parent that this save resolves: the version below
                        // is its base.
                        Some(r) if !had || resolving.contains(&(i, p.clone())) => {
                            if let Some(h) = version_hash(r.as_path(), &p)? {
                                s.overlays[i].blobs.insert(h.clone(), (r.clone(), p.clone()));
                                s.overlays[i].bases.insert(p.clone(), h);
                            }
                        }
                        Some(_) => {}
                        // The parent does not show the path: an own file, no patch.
                        None => {
                            s.overlays[i].unbases.insert(p.clone());
                        }
                    }
                }
                ops(&mut s, target).puts.insert(p.clone());
                unmark(&mut s, target, &p);
            }
        }
    }
    // Moves of unchanged files into their overlay.
    for (p, i) in pending {
        if handled.contains(p) || !has_file(new, p) {
            continue;
        }
        move_into(&mut s, *i, p)?;
    }
    // Moves of unchanged files to `main`.
    for p in publish {
        if handled.contains(p) || !has_file(new, p) || sticky.contains_key(p) {
            continue;
        }
        publish_into(&mut s, p);
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
                    let record = crate::oplog::parse_line(line)?.record;
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
    pub fn save_view(&self, locked: &crate::store::LockedProject<'_>, ctx: &Ctx, view: &View) -> Result<String> {
        locked.check(&self.p)?;
        self.check_view_access(ctx, view)?;
        let id = view.id()?;
        let public = self.public_layers(&view.layers)?;
        if public.len() != view.layers.len() {
            for (position, layer) in view.layers.iter().enumerate() {
                if !self.need_overlay(&layer.overlay)?.hidden {
                    continue;
                }
                let orepo = self.overlay_repo(&layer.overlay)?;
                let _lock = orepo.p.lock_maintenance()?;
                if orepo.p.export_jsonl()?.lines().any(|line| {
                    crate::oplog::parse_line(line).is_ok_and(|s| s.record.op == "view.hidden" && s.record.data["view_id"] == id)
                }) {
                    continue;
                }
                let mut record = crate::model::DraftRecord::new("view.hidden");
                record.data =
                    serde_json::json!({"view_id":id,"shared":view.shared,"public":public,"position":position,"layer":layer});
                orepo.commit_record(&_lock, ctx, record)?;
            }
        } else if self.p.db.saved_view(&id)?.is_none() {
            let mut record = crate::model::DraftRecord::new("view.save");
            record.data = serde_json::json!({"view_id":id,"view":view});
            self.commit_record(locked, ctx, record)?;
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
        self.point_overlay_lines(ctx, line, &view.layers, "view")
    }

    pub fn create_line_view(
        &self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        name: &str,
        view: &View,
        owner: u32,
    ) -> Result<Line> {
        locked.check(&self.p)?;
        Ok(self.create_named_lines(locked, ctx, vec![name.to_string()], &view.shared, owner, None, Some(view))?.remove(0))
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
        Ok(self.open_overlay_info_with(name, |name| self.overlay_project(name).map(Some))?.expect("blocking overlay open"))
    }

    fn open_overlay_info_with(
        &self,
        name: &str,
        open: impl FnOnce(&str) -> Result<Option<Project<'a>>>,
    ) -> Result<Option<(Option<Info>, Option<Project<'a>>)>> {
        if crate::store::check_name("overlay", name).is_err() {
            return Ok(Some((None, None)));
        }
        // A removed cache overlay (`layr overlay remove`) is gone, also from the history of its own
        // project, which would otherwise name it again.
        if self.p.db.overlay_config(&format!("overlay.{name}.removed"))?.is_some() {
            return Ok(Some((None, None)));
        }
        let platform_key = format!("overlay.{name}.platform");
        let mut value = self.p.db.overlay_config(&format!("overlay.{name}"))?;
        let mut platform = None;
        let mut project = None;
        if value.is_none() {
            project = match open(name) {
                Ok(Some(p)) => Some(p),
                Ok(None) => return Ok(None),
                Err(_) => None,
            };
            if let Some(op) = &project {
                if op.db.overlay_config("overlay")?.is_some_and(|v| v["project"] == self.p.id && v["name"] == name) {
                    value = op.db.overlay_config(&format!("overlay.{name}"))?;
                    platform = op.db.overlay_config(&platform_key)?;
                }
            }
        } else {
            platform = self.p.db.overlay_config(&platform_key)?;
        }
        let v = match value {
            Some(v) if v.is_object() => v,
            _ => return Ok(Some((None, project))),
        };
        Ok(Some((
            Some(Info {
                hidden: v["hidden"].as_bool().unwrap_or(false),
                platform: platform.and_then(|p| p.as_str().and_then(platform_name)),
                owner: v["owner"].as_u64().unwrap_or(0) as u32,
                mode: v["mode"].as_str().and_then(Mode::parse).unwrap_or(Mode::Shared),
                root: v["root"].as_str().unwrap_or("").to_string(),
            }),
            project,
        )))
    }

    /// Whether overlay `name` shares with the machines that the project trusts (property `trust`):
    /// its readers follow the trusted machines, and the paper key need not open it.
    pub fn shares_with_machines(&self, name: &str) -> Result<bool> {
        Ok(self.overlay_props(name)?.trust.is_some())
    }

    /// Whether overlay `name` keeps a limited history (property `history`): a fetch takes only the
    /// states that its records keep, and a push deletes the others on a store remote.
    pub fn limited_history(&self, name: &str) -> Result<bool> {
        Ok(self.overlay_props(name)?.history.is_some())
    }

    /// Whether overlay `name` holds build output that a build makes again: an overlay of one platform
    /// (property `platform`). `overlay remove` takes only such an overlay, and a store remote deletes
    /// its states of older key epochs.
    pub fn regenerable_overlay(&self, name: &str) -> Result<bool> {
        Ok(self.overlay_info(name)?.is_some_and(|i| i.platform.is_some()))
    }

    /// Whether overlay `name` holds build output (a cache): an overlay of one platform, or a folder overlay
    /// (`cache.<folder>` of layr before 0.44). A plain fetch takes only its records (docs/remotes.md).
    pub fn cache_data_overlay(&self, name: &str) -> Result<bool> {
        Ok(self.regenerable_overlay(name)? || self.chunked_overlay(name)?)
    }

    /// Whether overlay `name` keeps one line per platform for each line: a folder overlay without
    /// `platform` (a `cache.<folder>` overlay of layr before 0.44), which holds the build output of
    /// every platform (`line-<line id>.<os>-<arch>`, docs/cache-overlays.md section 6).
    pub fn per_platform_lines(&self, name: &str) -> Result<bool> {
        Ok(self.chunked_overlay(name)? && self.overlay_info(name)?.is_some_and(|i| i.platform.is_none()))
    }

    /// The line of `line` on the build-output overlay `name` (project `orepo`), made at `start` when
    /// missing: its line of this platform in an overlay of every platform (`per_platform_lines`),
    /// else its line (`ensure_overlay_line`). The caller holds the lock of `orepo`.
    pub fn ensure_build_line(
        &self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        orepo: &Repo,
        name: &str,
        info: &Info,
        line: &Line,
        start: &str,
    ) -> Result<Line> {
        locked.check(&orepo.p)?;
        if !self.per_platform_lines(name)? {
            return orepo.ensure_overlay_line(locked, ctx, info, line, start);
        }
        let own = cache_line_name(&line.id);
        if let Some(l) = orepo.p.db.line_by_name(&own)? {
            return Ok(l);
        }
        // `line-<id>` of layr 0.24 and older, made on this platform: it takes the platform name.
        if let Some(l) = find_overlay_line(&orepo.p.db, info, line)?.filter(|l| l.name == format!("line-{}", line.id)) {
            return orepo.overlay_line_rename(locked, ctx, l, &own);
        }
        orepo.overlay_line_create(locked, ctx, &own, start, line.owner, None)
    }

    /// Whether a store remote keeps the files of overlay `name` in chunks: a folder overlay.
    pub fn chunked_overlay(&self, name: &str) -> Result<bool> {
        Ok(self.overlay_setting(name, "folder")?.is_some_and(|v| !v.is_null()))
    }

    pub fn need_overlay(&self, name: &str) -> Result<Info> {
        self.overlay_info(name)?.ok_or_else(|| {
            crate::errors::error(crate::errors::Kind::Failed, format!("error: no overlay '{name}' in project {}", self.p.name))
        })
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
        let info = info.ok_or_else(|| {
            crate::errors::error(crate::errors::Kind::Failed, format!("error: no overlay '{name}' in project {}", self.p.name))
        })?;
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
                    out.extend(overlay_files(&root)?);
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

    /// The local overlays for cleanup; a busy recovery defers the whole pass.
    pub(crate) fn maintenance_overlay_projects(&self) -> Result<Option<Vec<Project<'a>>>> {
        let mut projects = Vec::new();
        for name in self.overlay_candidates()? {
            // An overlay of another platform that this machine never fetched has no project here.
            if !self.p.store.project_dir(&format!("{}%{name}", self.p.name))?.exists() {
                continue;
            }
            let open = |name: &str| Project::open_maintenance(self.p.store, &format!("{}%{name}", self.p.name));
            let Some((info, project)) = self.open_overlay_info_with(&name, open)? else { return Ok(None) };
            if info.is_none() {
                continue;
            }
            let project = match project {
                Some(p) => p,
                None => {
                    let Some(p) = open(&name)? else { return Ok(None) };
                    p
                }
            };
            projects.push(project);
        }
        Ok(Some(projects))
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

    /// Record data that sets the view of the working folder of `line` to the public overlays of
    /// `layers` (`view_key`). Hidden overlays keep their place in their own project
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
            let key = format!("hidden.view.{line}");
            if orepo.p.db.overlay_config(&key)?.as_ref() == Some(&value) {
                continue;
            }
            let _lock = orepo.p.lock_maintenance()?;
            let mut record = crate::model::DraftRecord::new("overlay.use");
            record.data = serde_json::json!({"config":{key:value}});
            orepo.commit_record(&_lock, ctx, record)?;
        }
        Ok(())
    }

    /// The parts of a state on the hidden overlays that the caller reads (`layers` at that state): each goes to the
    /// records of its own overlay as `hidden.part.<state>`, so the public state record never names a hidden overlay, and
    /// the remote copy of the overlay encrypts it for the overlay's readers (docs/overlays.md section 1a).
    pub fn record_hidden_parts(&self, ctx: &Ctx, state: &str, layers: &[Layer]) -> Result<()> {
        for l in layers {
            if !self.overlay_info(&l.overlay)?.is_some_and(|i| i.hidden)
                || !self.reads_overlay(&l.overlay, ctx.caller.uid, ctx.caller.gid)?
            {
                continue;
            }
            let orepo = self.overlay_repo(&l.overlay)?;
            let key = format!("hidden.part.{state}");
            if orepo.p.db.overlay_config(&key)?.and_then(|v| v.as_str().map(str::to_owned)).as_deref() == Some(l.state.as_str()) {
                continue;
            }
            let _lock = orepo.p.lock_maintenance()?;
            let mut record = crate::model::DraftRecord::new("overlay.use");
            record.data = serde_json::json!({"config":{key:l.state}});
            orepo.commit_record(&_lock, ctx, record)?;
        }
        Ok(())
    }

    /// The new state `to` names the same parts of the hidden overlays as `from` (a replayed state).
    pub fn copy_hidden_parts(&self, ctx: &Ctx, from: &str, to: &str) -> Result<()> {
        let mut layers = Vec::new();
        for name in self.overlay_names()? {
            if let Some(part) = self.hidden_part(&name, from)? {
                layers.push(Layer { overlay: name, state: part });
            }
        }
        self.record_hidden_parts(ctx, to, &layers)
    }

    /// The part of the hidden overlay `name` that the state `state` named when it was saved, if this machine reads it.
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
        let who = match self.ctx.filter(|ctx| ctx.caller.uid == uid && ctx.caller.gid == gid) {
            Some(ctx) => Who::caller(&ctx.caller),
            None => Who::of(uid, gid),
        };
        if Policy::load(&op).check(&who, "read").is_err() {
            return Ok(false);
        }
        // The cache of the view of overlay X (docs/overlays.md section 1): only a reader of X reads it, also when
        // the cache's own readers are older than a change of X's readers.
        match self.overlay_setting(name, VIEW)?.and_then(|v| v.as_str().map(str::to_owned)) {
            Some(x) if x != name => self.reads_overlay(&x, uid, gid),
            _ => Ok(true),
        }
    }

    pub(crate) fn owner_reads(&self, name: &str, owner: u32) -> Result<bool> {
        let gid = crate::ctx::Caller::from_uid(owner).gid;
        self.reads_overlay(name, owner, gid)
    }

    /// The owner of a line reads the overlay project `op` (`reads_overlay` for an open project).
    fn project_reads(op: &Project, owner: u32) -> bool {
        let gid = crate::ctx::Caller::from_uid(owner).gid;
        owner == 0 || Policy::load(op).check(&Who::of(owner, gid), "read").is_ok()
    }

    // ----- the layers of a line -----

    /// The overlays a line shows, bottom first: the overlays of its view, each at the head of the
    /// line on that overlay (docs/overlays.md section 1).
    pub fn layers(&self, line: &Line) -> Result<Vec<Layer>> {
        let hidden = self.hidden_overlays()?;
        self.layers_with(line, hidden.iter().map(|(name, info, project)| (name.as_str(), info, project)))
    }

    /// The layers of many lines (line id to layers). The overlays are read once, not once per line.
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
        if self.p.is_overlay() {
            return Ok(Vec::new());
        }
        // The view of the working folder chooses the public overlays and their order. A line
        // without a view (made before views) shows the overlays of its old stack.
        let names = match self.folder_view(line)? {
            Some(names) => names,
            None => self.old_stack(line)?.into_iter().map(|l| l.overlay).collect(),
        };
        let mut layers = Vec::new();
        for name in names {
            if let Some(l) = self.overlay_layer(Some(line), &line.head, &name)? {
                layers.push(l);
            }
        }
        let hidden_overlays: Vec<(&'b str, &'b Info, &'b Project<'a>)> = hidden_overlays.into_iter().collect();
        // The layer of a hidden overlay that the owner reads: the head of the line on it, else the part of the head.
        let hidden_layer = |name: &str, info: &Info, op: &Project| -> Result<Option<Layer>> {
            if !Self::project_reads(op, line.owner) {
                return Ok(None);
            }
            let state = match find_overlay_line(&op.db, info, line)? {
                Some(ol) => ol.head,
                None => match op.db.overlay_config(&format!("hidden.part.{}", line.head))?.and_then(|v| v.as_str().map(str::to_owned)) {
                    Some(part) => part,
                    None => return Ok(None),
                },
            };
            Ok(Some(Layer { overlay: name.to_string(), state }))
        };
        // A hidden overlay keeps its place in the view in its own records (`hidden.view.<line id>`).
        let mut hidden = Vec::new();
        for (name, info, op) in &hidden_overlays {
            let selection = hidden_view(op, &line.id)?;
            if selection.as_ref().is_some_and(|s| s["enabled"] == false) {
                continue;
            }
            let Some(layer) = hidden_layer(name, info, op)? else { continue };
            let position = selection.as_ref().and_then(|s| s["position"].as_u64()).unwrap_or(u64::MAX);
            hidden.push((position, layer));
        }
        hidden.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.overlay.cmp(&b.1.overlay)));
        for (position, layer) in hidden {
            layers.insert((position as usize).min(layers.len()), layer);
        }
        // The view is seen from one overlay (owner decision 2026-11-15): the path of the tree to its top.
        // Overlays that are no node of the tree (folder overlays of older data) stay above it. The
        // settings come from the projects open here, so a view opens no project twice.
        let setting = |name: &str, suffix: &str| -> Result<Option<serde_json::Value>> {
            let key = format!("overlay.{name}.{suffix}");
            match hidden_overlays.iter().find(|(n, _, _)| *n == name) {
                Some((_, _, op)) => op.db.overlay_config(&key),
                None => self.p.db.overlay_config(&key),
            }
        };
        let names: Vec<String> = layers.iter().map(|l| l.overlay.clone()).collect();
        let Some(path) = self.view_path(&names, &setting)? else { return Ok(layers) };
        let mut out = Vec::new();
        for name in path {
            if let Some(l) = layers.iter().find(|l| l.overlay == name) {
                out.push(l.clone());
                continue;
            }
            let layer = match hidden_overlays.iter().find(|(n, _, _)| *n == name) {
                Some((n, info, op)) => hidden_layer(n, info, op)?,
                None if self.overlay_info(&name)?.is_some_and(|i| !i.hidden) => self.overlay_layer(Some(line), &line.head, &name)?,
                None => None,
            };
            out.extend(layer);
        }
        for l in layers {
            if !out.iter().any(|o| o.overlay == l.overlay) && setting(&l.overlay, "folder")?.is_some_and(|v| !v.is_null()) {
                out.push(l);
            }
        }
        Ok(out)
    }

    /// The path of the tree that a view of the overlays `names` (bottom first, as a working folder
    /// records them) shows: the path to its top (`view_top`). None: the view of `main`. `setting`
    /// reads `overlay.<name>.<suffix>`. An overlay without a recorded parent (older layr) takes the
    /// overlay below it in `names`, so an existing folder keeps its content; outside `names` it takes
    /// the overlay below it in the old project order (`derived_parent`).
    fn view_path(
        &self,
        names: &[String],
        setting: &dyn Fn(&str, &str) -> Result<Option<serde_json::Value>>,
    ) -> Result<Option<Vec<String>>> {
        let mut nodes = Vec::new();
        for n in names {
            if !setting(n, "folder")?.is_some_and(|v| !v.is_null()) {
                nodes.push(n.clone());
            }
        }
        let parent = |n: &str| -> Result<Option<String>> {
            if let Some(v) = setting(n, PARENT)? {
                return Ok(v.as_str().filter(|p| *p != MAIN).map(str::to_owned));
            }
            match nodes.iter().position(|x| x == n) {
                Some(0) => Ok(None),
                Some(i) => Ok(Some(nodes[i - 1].clone())),
                None => self.derived_parent(n),
            }
        };
        let path_of = |top: &str| -> Result<Vec<String>> {
            let mut out = vec![top.to_string()];
            let mut cur = top.to_string();
            while let Some(p) = parent(&cur)? {
                if out.contains(&p) || out.len() > MAX_DEPTH {
                    bail!("the parents of overlay '{top}' form a loop; set one with 'layr overlay set <name> --parent <overlay>'");
                }
                out.push(p.clone());
                cur = p;
            }
            out.reverse();
            Ok(out)
        };
        let mut last = None;
        for cand in nodes.iter().rev() {
            let path = path_of(cand)?;
            if nodes.iter().all(|n| path.contains(n)) {
                return Ok(Some(path));
            }
            last.get_or_insert(path);
        }
        Ok(last)
    }

    /// The layers that layr before 0.44 kept for `line` in the setting `stack.<line id>`. Only
    /// `overlay migrate-parts` and a line that it did not move yet read them.
    pub fn old_stack(&self, line: &Line) -> Result<Vec<Layer>> {
        let key = format!("stack.{}", line.id);
        let value = if line.machine == self.p.store.machine.id {
            self.p.config_value(&key)?
        } else {
            self.p.config_value(&format!("{key}@{}", line.machine))?
        };
        let value = match value {
            Some(value) => Some(value),
            None => self.p.db.overlay_config(&key)?,
        };
        Ok(value.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default())
    }

    /// The view of the working folder of `line`: the names of the public overlays it shows,
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

    /// The layer of overlay `name` for `line` at `state`. At the head of the line: the head of the
    /// line on the overlay, else the part that the head names. At an older state: the part that the
    /// state names. Else the empty first state of the overlay. None for an overlay that is not on
    /// this machine.
    pub fn overlay_layer(&self, line: Option<&Line>, state: &str, name: &str) -> Result<Option<Layer>> {
        let Some(info) = self.overlay_info(name)? else { return Ok(None) };
        let mut layer = Layer { overlay: name.to_string(), state: info.root.clone() };
        if let Some(line) = line.filter(|l| l.head == state) {
            if let Ok(op) = self.overlay_project(name) {
                if let Some(ol) = find_overlay_line(&op.db, &info, line)? {
                    layer.state = ol.head;
                    return Ok(Some(layer));
                }
            }
            // A line of layr before 0.44 that `overlay migrate-parts` did not move yet: the state that
            // its old stack names.
            if let Some(l) = self.old_stack(line)?.into_iter().find(|l| l.overlay == name) {
                return Ok(Some(l));
            }
        }
        let part = if info.hidden {
            self.hidden_part(name, state)?
        } else {
            self.p.db.state(state)?.and_then(|s| s.parts.get(name).cloned())
        };
        if let Some(part) = part {
            layer.state = part;
        }
        Ok(Some(layer))
    }

    /// The project order of overlays: the names that `layr overlay order` set (setting
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

    // ----- the tree of patch overlays (docs/overlays.md section 1, owner decision 2026-11-15) -----

    /// Whether overlay `name` is of kind `patches`: a node of the tree. A folder overlay (a cache) is
    /// no node: it has no parent and every view has it.
    pub fn patches_overlay(&self, name: &str) -> Result<bool> {
        Ok(self.overlay_info(name)?.is_some() && !self.chunked_overlay(name)?)
    }

    /// The parent of the patches overlay `name`; None is `main`. A recorded parent is the setting
    /// `overlay.<name>.parent` (in the overlay's own records when it is hidden). An overlay of an
    /// older layr has none: its parent is the overlay below it in the old project order
    /// (`derived_parent`), so an existing folder keeps its content.
    pub fn overlay_parent(&self, name: &str) -> Result<Option<String>> {
        match self.overlay_setting(name, PARENT)? {
            Some(v) => Ok(v.as_str().filter(|p| *p != MAIN).map(str::to_owned)),
            None => self.derived_parent(name),
        }
    }

    /// The parent of an overlay without a recorded parent: the patches overlay below it in the
    /// project order of older layr (`project_order`). A public overlay never takes a hidden parent:
    /// a public record would then depend on a name that some readers do not know.
    fn derived_parent(&self, name: &str) -> Result<Option<String>> {
        let hidden = self.overlay_info(name)?.is_some_and(|i| i.hidden);
        let order = self.project_order()?;
        let Some(pos) = order.iter().position(|n| n == name) else { return Ok(None) };
        for cand in order[..pos].iter().rev() {
            let Some(info) = self.overlay_info(cand)? else { continue };
            if !self.patches_overlay(cand)? || (info.hidden && !hidden) {
                continue;
            }
            return Ok(Some(cand.clone()));
        }
        Ok(None)
    }

    /// The path of the tree from `main` to `top`, bottom first, without `main` and with `top`. A
    /// loop in the parents (records of two machines that crossed) is an error, never a view.
    pub fn overlay_path(&self, top: &str) -> Result<Vec<String>> {
        let mut out = vec![top.to_string()];
        let mut cur = top.to_string();
        while let Some(p) = self.overlay_parent(&cur)? {
            if out.contains(&p) || out.len() > MAX_DEPTH {
                bail!("the parents of overlay '{top}' form a loop; set one with 'layr overlay set <name> --parent <overlay>'");
            }
            out.push(p.clone());
            cur = p;
        }
        out.reverse();
        Ok(out)
    }

    /// The overlays whose path goes through `name` (its children, their children and so on).
    pub fn overlay_descendants(&self, name: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for other in self.overlay_names()? {
            if other != name && self.patches_overlay(&other)? && self.overlay_path(&other).is_ok_and(|p| p.iter().any(|n| n == name)) {
                out.push(other);
            }
        }
        Ok(out)
    }

    /// The overlay that the working folder of `line` shows (None: `main`): the top of its path.
    pub fn line_view_top(&self, line: &Line) -> Result<Option<String>> {
        for l in self.layers(line)?.into_iter().rev() {
            if self.patches_overlay(&l.overlay)? {
                return Ok(Some(l.overlay));
            }
        }
        Ok(None)
    }

    /// The state of the paths of each overlay of the view of `line` (docs/overlays.md section 1): its patches
    /// (with the version of the parent that each was made from), its own files, the paths it hides, and the
    /// patches whose parent changed since (they wait for a merge with the parent, `rebase_patches`).
    pub fn patch_report(&self, line: &Line) -> Result<Vec<PatchReport>> {
        let layers = self.layers(line)?;
        let shared = self.state_root(&line.head)?;
        let mut roots = Vec::new();
        let mut removed = Vec::new();
        for l in &layers {
            let root = self.layer_root(l).ok_or_else(|| anyhow!("overlay '{}' is not on this machine", l.overlay))?;
            removed.push(removed_paths(&root)?);
            roots.push(root);
        }
        let mut out = Vec::new();
        for (i, l) in layers.iter().enumerate() {
            if !self.patches_overlay(&l.overlay)? {
                continue;
            }
            let meta = read_meta(roots[i].as_path())?;
            let mut r = PatchReport { overlay: l.overlay.clone(), hidden: meta.removed.iter().cloned().collect(), ..Default::default() };
            for p in overlay_files(&roots[i])? {
                match meta.bases.get(&p) {
                    Some(h) => {
                        let now = match below_root(&shared, &roots, &removed, i, &p) {
                            Some(root) => version_hash(root.as_path(), &p)?,
                            None => None,
                        };
                        if now.as_ref() != Some(h) {
                            r.waiting.push(p.clone());
                        }
                        r.patches.push(p);
                    }
                    None => r.own.push(p),
                }
            }
            out.push(r);
        }
        Ok(out)
    }

    /// The overlays of a new line folder of `owner`: the path to the overlay of the folder where
    /// `line new` ran, else none (the view of `main`). The path stops below the first overlay that the
    /// owner does not read, so the folder of a non-reader never gets its files; an overlay of another
    /// platform or not on this machine is left out.
    pub fn default_overlays(&self, ctx: &Ctx, owner: u32) -> Result<Vec<String>> {
        // The folder of the command: the working folder, or `--project` with `--line`.
        let folder_line = ctx
            .locate()
            .ok()
            .filter(|l| l.project == self.p.name)
            .and_then(|l| l.line)
            .and_then(|n| self.p.db.line_by_name(&n).ok().flatten());
        let Some(folder_line) = folder_line else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for name in self.layers(&folder_line)?.into_iter().map(|l| l.overlay) {
            if !self.patches_overlay(&name)? {
                continue;
            }
            let Some(info) = self.overlay_info(&name)? else { break };
            if !self.owner_reads(&name, owner)? {
                break;
            }
            if other_platform_overlay(info.platform.as_deref())
                || self.layer_root(&Layer { overlay: name.clone(), state: info.root.clone() }).is_none()
            {
                continue;
            }
            out.push(name);
        }
        Ok(out)
    }

    /// Every principal that reads `child` (its readers and its owner) reads `ancestor` too
    /// (docs/overlays.md section 1, readers rule). A principal reads `ancestor` when its policy names
    /// it or everyone with the reader role, or, for a user, when the policy gives the user's uid or
    /// groups that role. `main` is read by every reader of the project.
    pub fn readers_covered(child: &Policy, ancestor: &Policy) -> Option<String> {
        let reads = |k: &str| -> bool {
            let denied = |key: &str| ancestor.deny.get(key).is_some_and(|v| v.iter().any(|a| a == "read" || a == "*"));
            let ok = |key: &str| ancestor.access.get(key).is_some_and(|r| *r >= crate::access::Role::Reader) && !denied(key);
            if denied(k) {
                return false;
            }
            if ok("*") || ok(k) {
                return true;
            }
            match k.strip_prefix("user:").and_then(|u| u.parse::<u32>().ok()) {
                Some(uid) => {
                    let gid = crate::ctx::Caller::from_uid(uid).gid;
                    ancestor.check(&Who::of(uid, gid), "read").is_ok()
                }
                None => false,
            }
        };
        child.access.iter().filter(|(_, r)| **r >= crate::access::Role::Reader).map(|(k, _)| k).find(|k| !reads(k)).cloned()
    }

    /// The readers rule for overlay `name` with the policy `policy` and the parent `parent`: each
    /// ancestor is read by every reader of `name`, and `name` is read by every reader of each
    /// descendant. Returns the refusal text.
    pub fn check_readers_rule(&self, name: &str, policy: &Policy, parent: Option<&str>) -> Result<Option<String>> {
        let mut ancestors = Vec::new();
        if let Some(p) = parent {
            ancestors = self.overlay_path(p)?;
        }
        for a in &ancestors {
            let ap = Policy::load(&self.overlay_project(a)?);
            if let Some(who) = Self::readers_covered(policy, &ap) {
                return Ok(Some(format!(
                    "{} reads overlay '{name}' but not its ancestor '{a}': a reader of an overlay reads every overlay below it",
                    crate::access::show_principal(&who)
                )));
            }
        }
        for d in self.overlay_descendants(name)? {
            let dp = Policy::load(&self.overlay_project(&d)?);
            if let Some(who) = Self::readers_covered(&dp, policy) {
                return Ok(Some(format!(
                    "{} reads overlay '{d}', which is above '{name}': a reader of an overlay reads every overlay below it",
                    crate::access::show_principal(&who)
                )));
            }
        }
        Ok(None)
    }

    /// The parts that a new state of a line with `layers` names: the state of each overlay that
    /// keeps parts (`parts_overlay`).
    pub fn parts_of(&self, layers: &[Layer]) -> Result<BTreeMap<String, String>> {
        let mut out = BTreeMap::new();
        for l in layers {
            if self.overlay_info(&l.overlay)?.is_some_and(|i| parts_overlay(&i)) {
                out.insert(l.overlay.clone(), l.state.clone());
            }
        }
        Ok(out)
    }

    /// A new state of a line keeps the parts of `head` on the overlays that its view does not show
    /// (`layers`): a folder that shows fewer overlays does not clear the others (slice 5).
    pub fn carry_parts(&self, parts: &mut BTreeMap<String, String>, head: &str, layers: &[Layer]) -> Result<()> {
        if self.p.is_overlay() {
            return Ok(());
        }
        for (overlay, part) in self.p.db.state(head)?.map(|s| s.parts).unwrap_or_default() {
            if !layers.iter().any(|l| l.overlay == overlay) && self.overlay_info(&overlay)?.is_some_and(|i| parts_overlay(&i)) {
                parts.entry(overlay).or_insert(part);
            }
        }
        Ok(())
    }

    /// The layers of `line` when it shows its state `state`: the line's stack, each overlay that
    /// keeps parts at the part that `state` names. A state with parts that names no part on such a
    /// overlay is clear on it (the empty first state). A state without parts (saved before them, or
    /// without overlays) and the head itself keep the stack as it is.
    pub fn layers_at(&self, line: &Line, state: &str) -> Result<Vec<Layer>> {
        let mut layers = self.layers(line)?;
        if self.p.is_overlay() || state == line.head || layers.is_empty() {
            return Ok(layers);
        }
        // A hidden overlay: the part that the state recorded in the overlay's own records.
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
            if parts_overlay(&info) {
                l.state = parts.get(&l.overlay).cloned().unwrap_or(info.root);
            }
        }
        Ok(layers)
    }

    /// The overlays of the view of a state (docs/overlays.md, section 1a): the parts that the state
    /// names, in the order of the stack of `line` and then of the project's overlays. Empty for a
    /// state without parts.
    pub fn part_layers(&self, line: Option<&Line>, state: &str) -> Result<Vec<Layer>> {
        if self.p.is_overlay() {
            return Ok(Vec::new());
        }
        let mut parts = self.p.db.state(state)?.map(|s| s.parts).unwrap_or_default();
        // The hidden overlays that this machine reads: the part that the state recorded in each overlay's own records.
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
        // A build-output overlay is a folder, not an overlay of the file stack: `line new` places its part.
        let mut out = Vec::new();
        for o in order {
            if let Some(s) = parts.get(&o) {
                if hidden.contains(&o) || self.overlay_info(&o)?.is_some_and(|i| parts_overlay(&i)) {
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
                return Err(crate::errors::error(
                    crate::errors::Kind::Failed,
                    format!(
                        "error: line '{}' shows overlay '{}', which is not on this machine; run 'layr overlay use --remove {}'",
                        line.name, l.overlay, l.overlay
                    ),
                ));
            }
            if !self.owner_reads(&l.overlay, line.owner)? {
                return Err(crate::errors::error(
                    crate::errors::Kind::Failed,
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
                return Err(crate::errors::error(
                    crate::errors::Kind::Failed,
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

    /// Whether automatic states of `line` hold only the shared head (`working_snapshot_over`): its folder
    /// shows the view of an overlay, so the next named save writes its changes into that overlay
    /// (docs/overlays.md section 1, write rule). Then no automatic state that other readers of the project
    /// read holds a change that is meant to stay in the overlay.
    pub fn private_view(&self, line: &Line) -> Result<bool> {
        if self.p.is_overlay() {
            return Ok(false);
        }
        Ok(self.line_view_top(line)?.is_some())
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
                if let Some(ol) = find_overlay_line(&op.db, info, line)? {
                    states.push(ol.head);
                }
            }
            for s in states {
                let root = op.state_path(&s);
                if root.exists() {
                    out.extend(overlay_files(&root)?);
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
        let _publication = self.lock_base(&path)?;
        if self.base_ready(&path)? {
            return Ok(path);
        }
        let view = self.assemble(line, &line.head, &layers)?;
        self.place_base(&view, &path)?;
        Ok(path)
    }

    fn lock_base(&self, path: &Path) -> Result<std::fs::File> {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = self.p.meta_dir().join("base-locks");
        std::fs::create_dir_all(&dir)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(dir.join(path.file_name().context("base has no name")?))?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)?;
        Ok(lock)
    }
    fn base_ready(&self, path: &Path) -> Result<bool> {
        if !path.exists() {
            return Ok(false);
        }
        if crate::fs::host().namespace.info(path)?.readonly {
            return Ok(true);
        }
        // An interrupted older publication may have left a writable derived cache.
        // Rebuild it under its publication lock instead of mutating a leased tree.
        crate::fs::remove(path)?;
        Ok(false)
    }

    fn place_base(&self, view: &Temp, path: &Path) -> Result<()> {
        let dir = self.p.base_dir();
        if !dir.exists() {
            std::fs::create_dir_all(&dir)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        // Bases are derived immutable trees. Freeze through the operation contract rather
        // than changing a mounted tree while its readable lease is still alive.
        match crate::fs::snapshot(&view.path, path, true) {
            Ok(()) => Ok(()),
            Err(error)
                if error.chain().any(|cause| {
                    cause.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::AlreadyExists)
                }) && crate::fs::host().namespace.info(path).is_ok_and(|info| info.readonly) =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// The full tree for `head` and `layers`, as a temporary read-only snapshot: a writable
    /// snapshot of the head state, the files of each overlay state from the bottom (an upper file
    /// wins), read-only.
    pub fn assemble(&self, line: &Line, head: &str, layers: &[Layer]) -> Result<Temp> {
        let _t = crate::trace::span("assemble");
        let t = Temp::source(
            &self.state_src(head)?,
            self.p.work_dir().join(crate::store::temp_name()),
            crate::fs::Protection::Writable,
        )?;
        if let Err(error) = self.put_layers(&t.path, layers, self.own(line)) {
            return Err(t.abort(error));
        }
        let captured = match self.temp_snapshot(&t.path, true) {
            Ok(captured) => captured,
            Err(error) => return Err(t.abort(error)),
        };
        if let Err(error) = t.discard() {
            return Err(captured.abort(error));
        }
        Ok(captured)
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
    /// The delete markers of a layer (`Meta::removed`) take those paths of the lower layers out first; its
    /// metadata folder never comes into a view.
    pub fn put_layers(&self, dst: &Path, layers: &[Layer], own: Option<(u32, u32)>) -> Result<()> {
        for l in layers {
            if let Some(root) = self.layer_root(l) {
                let removed = removed_paths(&root)?;
                if !removed.is_empty() {
                    // `dst` is a writable snapshot of the store (`assemble`, the line's base), not a folder of the caller.
                    let t = Tree::open(dst)?;
                    for p in &removed {
                        if t.stat(p).is_some_and(|m| !m.is_dir()) {
                            t.remove(p)?;
                            prune(&t, p);
                        }
                    }
                }
                for rel in overlay_files(&root)? {
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
        } else {
            let _publication = self.lock_base(&path)?;
            if !self.base_ready(&path)? {
                self.place_base(&view, &path)?;
            }
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
                let _ = crate::fs::remove(&e.path());
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
                let _ = crate::fs::remove(&e.path());
            }
        }
        Ok(())
    }

    /// Records in an overlay: a new line `name` of `owner` at `start`, with the id of the line of the
    /// project (`id`) when it has one: the same line on every overlay.
    pub(crate) fn overlay_line_create(
        &self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        name: &str,
        start: &str,
        owner: u32,
        id: Option<&str>,
    ) -> Result<Line> {
        locked.check(&self.p)?;
        // A line of the project that came back (an undo of its delete) finds its old overlay line deleted: a new id.
        let id = id.filter(|id| self.p.db.line_by_id(id).ok().flatten().is_none());
        let line = Line {
            id: id.map(str::to_owned).unwrap_or_else(ids::new_id),
            name: name.to_string(),
            head: start.to_string(),
            owner,
            machine: self.p.store.machine.id.clone(),
        };
        let mut rec = crate::model::DraftRecord::new("line.create");
        rec.lines.insert(
            line.id.clone(),
            LineChange {
                transition: crate::model::LineTransition::Create { after: self.line_state(&line, start) },
                working_before: None,
                working_after: None,
            },
        );
        rec.data = serde_json::json!({"name": name, "start": start});
        locked.append(rec, &ctx.actor())?;
        Ok(line)
    }

    /// The line of `line` on this overlay (an overlay's own repository), made at `start` when
    /// missing. A line with a name of layr before 0.44 takes the name of `line`.
    pub fn ensure_overlay_line(
        &self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        info: &Info,
        line: &Line,
        start: &str,
    ) -> Result<Line> {
        locked.check(&self.p)?;
        let name = overlay_line_name(info, line);
        match find_overlay_line(&self.p.db, info, line)? {
            Some(l) if l.name == name => Ok(l),
            Some(l) => self.overlay_line_rename(locked, ctx, l, &name),
            None => self.overlay_line_create(
                locked,
                ctx,
                &name,
                start,
                line.owner,
                (info.mode == Mode::Follow).then_some(line.id.as_str()),
            ),
        }
    }

    /// Point the line of `line` on each overlay of `layers` at the state of the layer (made when
    /// missing): the line shows these states from now on. The caller holds the project lock and
    /// no lock of an overlay.
    pub fn point_overlay_lines(&self, ctx: &Ctx, line: &Line, layers: &[Layer], op: &str) -> Result<()> {
        for l in layers {
            let Some(info) = self.overlay_info(&l.overlay)? else { continue };
            if info.mode == Mode::Shared {
                continue;
            }
            let Ok(orepo) = self.overlay_repo(&l.overlay) else { continue };
            let _lock = orepo.p.lock_maintenance()?;
            let ol = orepo.ensure_overlay_line(&_lock, ctx, &info, line, &l.state)?;
            if ol.head != l.state {
                move_overlay_line(&_lock, &orepo, ctx, op, &ol, &l.state, Vec::new())?;
            }
        }
        Ok(())
    }

    /// A line of the project takes a new name: its line on each overlay of this machine takes it
    /// too. The caller holds the project lock.
    pub fn rename_overlay_lines(&self, ctx: &Ctx, line: &Line, name: &str) -> Result<()> {
        if self.p.is_overlay() {
            return Ok(());
        }
        let renamed = Line { name: name.to_string(), ..line.clone() };
        for overlay in self.overlay_names()? {
            let Some(info) = self.overlay_info(&overlay)? else { continue };
            // An overlay of every platform keeps one line per platform, named by the line id.
            if info.mode == Mode::Shared || self.per_platform_lines(&overlay)? {
                continue;
            }
            let Ok(orepo) = self.overlay_repo(&overlay) else { continue };
            let _lock = orepo.p.lock_maintenance()?;
            if let Some(ol) = find_overlay_line(&orepo.p.db, &info, line)? {
                orepo.overlay_line_rename(&_lock, ctx, ol, &overlay_line_name(&info, &renamed))?;
            }
        }
        Ok(())
    }

    pub(crate) fn overlay_line_rename(
        &self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        l: Line,
        name: &str,
    ) -> Result<Line> {
        locked.check(&self.p)?;
        let mut after = self.line_state(&l, &l.head);
        after.name = name.to_string();
        let mut rec = crate::model::DraftRecord::new("line.rename");
        rec.lines.insert(
            l.id.clone(),
            LineChange {
                transition: crate::model::LineTransition::Move { before: self.line_state(&l, &l.head), after: after },
                working_before: None,
                working_after: None,
            },
        );
        rec.data = serde_json::json!({"from": l.name, "to": name});
        locked.append(rec, &ctx.actor())?;
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
        let _lock = orepo.p.lock_maintenance()?;
        let lines = self.lines()?;
        let stacks = self.stacks(&lines)?;
        let excess = self.history_excess(&orepo, name, keep_n, &lines, &stacks)?;
        Self::delete_excess(&_lock, ctx, &orepo, keep_n, excess)
    }

    /// The present states of cache overlay `name` that nothing keeps: beyond its history limit (all
    /// states when it has none), not shown by a line, or of a deleted line. `fsck --overlays` reports
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
        // The working content of a live automatic state of the project stays (`save_private_working`).
        let (live, _) = self.auto_states(orepo)?;
        Ok(all.into_iter().filter(|id| !keep.contains(id) && !live.contains(id)).collect())
    }

    /// The automatic states of the overlay project `orepo` that keep the working content of an automatic
    /// state of the project (`auto.<state>`): those whose state of the project is live, and the settings
    /// whose state of the project is gone (with the overlay state they name).
    pub fn auto_states(&self, orepo: &Repo) -> Result<(BTreeSet<String>, Vec<(String, String)>)> {
        let mut live = BTreeSet::new();
        let mut dead = Vec::new();
        for (key, value) in orepo.p.db.config_all()? {
            let Some(state) = key.strip_prefix("auto.") else { continue };
            let Some(id) = serde_json::from_str::<serde_json::Value>(&value).ok().and_then(|v| v.as_str().map(str::to_owned)) else { continue };
            let gone = self.p.db.state_flags(state)?.is_none_or(|(_, deleted)| deleted);
            if gone {
                dead.push((key, id));
            } else {
                live.insert(id);
            }
        }
        Ok((live, dead))
    }

    /// `gc`: the overlay states that kept the working content of automatic states of the project that are
    /// gone (a dropped automatic state, a deleted line) go too, with their settings.
    pub fn drop_dead_auto_states(&self, ctx: &Ctx) -> Result<usize> {
        let mut n = 0;
        for name in self.overlay_names()? {
            let Ok(orepo) = self.overlay_repo(&name) else { continue };
            let _lock = orepo.p.lock_maintenance()?;
            let (_, dead) = self.auto_states(&orepo)?;
            if dead.is_empty() {
                continue;
            }
            let present: BTreeSet<String> = orepo.p.db.present_states()?.into_iter().map(|(id, _)| id).collect();
            let mut deleted = Vec::new();
            let mut cfg = serde_json::Map::new();
            for (key, id) in dead {
                if present.contains(&id) && orepo.p.delete_state(&id)? {
                    deleted.push(id);
                }
                cfg.insert(key, serde_json::Value::Null);
            }
            n += deleted.len();
            let mut rec = crate::model::DraftRecord::new("overlay.use");
            rec.data = serde_json::json!({"config": cfg});
            orepo.commit_record(&_lock, ctx, rec)?;
            if !deleted.is_empty() {
                let mut rec = crate::model::DraftRecord::new("gc");
                rec.data = serde_json::json!({"deleted": deleted, "auto": true});
                orepo.commit_record(&_lock, ctx, rec)?;
            }
        }
        Ok(n)
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
        // An overlay keeps nothing for a line that is gone (docs/cache-overlays.md).
        for ol in orepo.p.db.lines()? {
            if orphan_overlay_line(&ol, &self.p.db)? {
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
        // The parts that the newest `history` states of each line name. A live line whose line on a cache overlay was
        // deleted (its caches were freed after a merge, `free_merged_caches`) keeps no part there.
        for line in lines {
            if info.platform.is_some()
                && orepo.p.db.line_deleted(&line.id)?
                && find_overlay_line(&orepo.p.db, &info, line)?.is_none()
            {
                continue;
            }
            let mut at = Some(line.head.clone());
            for _ in 0..keep_n {
                let Some(id) = at.take() else { break };
                let Some(st) = self.p.db.state(&id)? else { break };
                let part = if info.hidden { self.hidden_part(name, &id)? } else { st.parts.get(name).cloned() };
                keep.extend(part);
                at = st.parents.first().cloned();
            }
        }
        Ok(Some(keep))
    }

    /// Delete the states `excess` of the overlay project `orepo` that are still present, and
    /// record it. The caller holds the overlay project's lock.
    fn delete_excess(
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        orepo: &Repo,
        keep_n: usize,
        excess: Vec<String>,
    ) -> Result<Vec<String>> {
        locked.check(&orepo.p)?;
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
            let mut rec = crate::model::DraftRecord::new("gc");
            rec.data = serde_json::json!({"deleted": deleted, "history": keep_n});
            orepo.commit_record(locked, ctx, rec)?;
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
        let follow = self.overlay_info(name)?.is_some_and(|i| i.mode == Mode::Follow);
        let orphans = if follow { self.orphan_cache_lines(&orepo)? } else { Vec::new() };
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
        let _lock = orepo.p.lock_maintenance()?;
        let c = if orepo.p.db.records_mark()? == c.mark {
            c
        } else {
            match self.overlay_clean(&c.name, lines, stacks)? {
                Some(c) => c,
                None => return Ok(()),
            }
        };
        self.drop_overlay_lines(&_lock, ctx, &orepo, &c.name, &c.orphans)?;
        // The states of the dropped lines are dead now; `delete_excess` skips them.
        if let Some(keep_n) = c.keep_n {
            Self::delete_excess(&_lock, ctx, &orepo, keep_n, c.excess)?;
        }
        Ok(())
    }

    /// The layers of the new line `line` that starts at `start`, from the line `source` when it
    /// has one: each overlay of the default view at the part that the start state names, or the
    /// source's state of the overlay when the start is the source's head (the source may have
    /// changed the overlay since its last state), else the empty first state. The new line gets
    /// its line on each overlay at that state (docs/overlays.md section 1).
    pub fn inherit_layers(&self, ctx: &Ctx, source: Option<&Line>, start: &str, line: &Line) -> Result<Vec<Layer>> {
        let start_parts = self.p.db.state(start)?.map(|s| s.parts).unwrap_or_default();
        let at_head = source.is_some_and(|s| s.head == start);
        let source_layers = match source {
            Some(s) => self.layers(s)?,
            None => Vec::new(),
        };
        let mut out = Vec::new();
        for name in self.default_overlays(ctx, line.owner)? {
            let Some(info) = self.overlay_info(&name)? else { continue };
            let from_source = source_layers.iter().find(|l| l.overlay == name).map(|l| l.state.clone());
            let orepo = self.overlay_repo(&name)?;
            let _lock = orepo.p.lock_maintenance()?;
            let begin = match info.mode {
                Mode::Follow => {
                    let named = if info.hidden { self.hidden_part(&name, start)? } else { start_parts.get(&name).cloned() };
                    let state = if at_head { from_source.or(named) } else { named.or(from_source) };
                    state.unwrap_or(info.root.clone())
                }
                Mode::Shared => match from_source {
                    Some(s) => s,
                    None => {
                        orepo.p.db.line_by_name(&format!("user-{}", info.owner))?.map(|l| l.head).unwrap_or(info.root.clone())
                    }
                },
            };
            if self.layer_root(&Layer { overlay: name.clone(), state: begin.clone() }).is_none() {
                continue;
            }
            let ol = orepo.ensure_overlay_line(&_lock, ctx, &info, line, &begin)?;
            // A line of the same name that an earlier line left starts again at the new line's state.
            let ol = if info.mode == Mode::Follow && ol.head != begin {
                move_overlay_line(&_lock, &orepo, ctx, "line.create", &ol, &begin, Vec::new())?;
                Line { head: begin, ..ol }
            } else {
                ol
            };
            out.push(Layer { overlay: name, state: ol.head });
        }
        self.record_hidden_selection(ctx, &line.id, &out)?;
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
    let working =
        Temp::source(&repo.state_src(prev)?, repo.p.work_dir().join(crate::store::temp_name()), crate::fs::Protection::Writable)?;
    if let Err(error) = ops.apply(&working.path, src, own) {
        return Err(working.abort(error));
    }
    let captured = crate::fs::freeze_path_owned(&working.path, &dst, crate::fs::CapturePurpose::State)?;
    let info = match crate::fs::host().namespace.info(&dst) {
        Ok(info) => info,
        Err(error) => return Err(captured.abort(error)),
    };
    captured.release();
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
pub fn move_overlay_line(
    locked: &crate::store::LockedProject<'_>,
    orepo: &Repo,
    ctx: &Ctx,
    op: &str,
    ol: &Line,
    head: &str,
    states: Vec<StateRec>,
) -> crate::db::PublishResult<()> {
    locked.check(&orepo.p)?;
    let mut rec = crate::model::DraftRecord::new(op);
    rec.states = states;
    let st = |h: &str| LineState { name: ol.name.clone(), head: h.to_string(), owner: ol.owner, machine: ol.machine.clone() };
    rec.lines.insert(
        ol.id.clone(),
        LineChange {
            transition: crate::model::LineTransition::Move { before: st(&ol.head), after: st(head) },
            working_before: None,
            working_after: None,
        },
    );
    rec.data = serde_json::json!({"line": ol.id});
    orepo.commit_record(locked, ctx, rec)?;
    Ok(())
}

/// What a commit in a line with overlays made.
pub struct Committed {
    pub shared: Option<StateRec>,
    pub overlays: Vec<(String, StateRec)>,
    /// The changes of the whole tree, for the summary (when asked for).
    pub fds: Vec<crate::diff::FileDiff>,
}

/// The layer that a named save writes (docs/overlays.md section 1, write rule): the top of the view,
/// or with `save --to <name>` an ancestor on the path of the view (`main` names the bottom layer). A
/// name that is not on the path (unknown, above the view, or a hidden overlay that the owner does not
/// read) gets one text, so the refusal does not tell that a hidden overlay exists.
pub fn save_target(repo: &Repo, layers: &[Layer], line: &Line, to: Option<&str>) -> Result<Owner> {
    let mut nodes = Vec::new();
    for (i, l) in layers.iter().enumerate() {
        if repo.patches_overlay(&l.overlay)? {
            nodes.push(i);
        }
    }
    match to {
        None => Ok(nodes.last().map(|i| Owner::Overlay(*i)).unwrap_or(Owner::Shared)),
        Some(MAIN) => Ok(Owner::Shared),
        Some(name) => match nodes.iter().find(|i| layers[**i].overlay == name) {
            Some(i) => Ok(Owner::Overlay(*i)),
            None => Err(crate::cmd::exit(
                1,
                format!(
                    "error: --to {name}: overlay '{name}' is not on the path of the view of line '{}' (main{}); name main or an overlay of that path",
                    line.name,
                    nodes.iter().map(|i| format!(", {}", layers[*i].overlay)).collect::<String>()
                ),
            )),
        },
    }
}

/// Commit the index of a line with overlays (docs/overlays.md, section 3.2). The caller holds the
/// project lock and has checked that the caller owns the line. `new_tree` is a read-only snapshot
/// of the index; it becomes the line's base. `to` is `save --to` (`save_target`). Returns None when
/// there is nothing to commit.
#[allow(clippy::too_many_arguments)]
pub fn commit_layers(
    locked: &crate::store::LockedProject<'_>,
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    layers: &[Layer],
    new_tree: Temp,
    message: &str,
    allow_empty: bool,
    author: Option<(String, String)>,
    summary: bool,
    to: Option<&str>,
) -> Result<Option<Committed>> {
    locked.check(&repo.p)?;
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
    // `layr overlay move main`: these paths go to `main` whole.
    let mut publish = BTreeSet::new();
    for (p, o) in repo.pending_moves(line) {
        if o == MAIN {
            publish.insert(p);
            continue;
        }
        match by_name.get(o.as_str()) {
            Some(i) => {
                pending.insert(p, *i);
            }
            None => bail!("'{p}' is to move to overlay '{o}', which line '{}' does not show", line.name),
        }
    }
    // The layer of this save: the top of the view, or the ancestor of `save --to` (docs/overlays.md section 1).
    let target = save_target(repo, layers, line, to)?;
    // A conflict of a patch with its parent (`rebase_patches`) is resolved by the next save of that path.
    let mut resolving = BTreeSet::new();
    for (i, l) in layers.iter().enumerate() {
        for c in orepos[i].p.db.state(&l.state)?.map(|s| s.conflicts).unwrap_or_default() {
            if c.kind() == PARENT_CONFLICT {
                resolving.insert((i, c.path.clone()));
            }
        }
    }
    // The files of a quiet overlay come from the working folder, unstaged: they stay in their overlay.
    let mut sticky = BTreeMap::new();
    for (i, l) in layers.iter().enumerate() {
        if repo.overlay_info(&l.overlay)?.is_some() && repo.overlay_props(&l.overlay)?.quiet {
            for p in overlay_files(&roots[i])? {
                sticky.entry(p).or_insert(i);
            }
        }
    }
    let mut kinds = Vec::new();
    for l in layers {
        kinds.push(LayerKind { name: l.overlay.clone(), patches: repo.patches_overlay(&l.overlay)? });
    }
    let mut s = split(&base, &new_tree.path, &shared_root, &roots, &pending, &sticky, target, &resolving, &kinds, &publish)?;
    if s.shared.is_empty() && s.overlays.iter().all(|o| o.is_empty()) && !allow_empty {
        return Ok(None);
    }
    // `save --to <ancestor>` of a file with a patch above the ancestor: the versions of the ancestor are
    // not in the saved tree. They go to a copy of it, written through `Tree` (never by path), and the
    // target takes them from there.
    let down_tree = if s.down.is_empty() { None } else { Some(repo.temp_work(&new_tree.path)?) };
    if let Some(work) = &down_tree {
        let t = Tree::open(&work.path)?;
        let own = repo.own(line);
        for (p, d) in std::mem::take(&mut s.down) {
            if t.stat(&p).is_some() {
                t.remove(&p)?;
            }
            t.mkdirs_for(&p, own)?;
            {
                use std::io::Write;
                let mut f = t.create(&p, d.mode)?;
                f.write_all(&d.data)?;
            }
            let ops = match target {
                Owner::Shared => &mut s.shared,
                Owner::Overlay(i) => &mut s.overlays[i],
            };
            if ops.puts.contains(&p) {
                ops.srcs.insert(p.clone(), work.path.clone());
            }
            if let Some((a, h)) = d.base_of {
                s.overlays[a].blobs.insert(h, (work.path.clone(), p.clone()));
            }
        }
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
        // The line of `line` on the overlay, made at the state that the folder shows when missing.
        let ol = {
            let _lock = orepo.p.lock_maintenance()?;
            match orepo.ensure_overlay_line(&_lock, ctx, &info, line, &l.state) {
                Ok(ol) => Some(ol),
                Err(e) => {
                    discard(&made, &orepos);
                    return Err(e);
                }
            }
        };
        if ol.as_ref().is_some_and(|ol| ol.head != l.state) {
            discard(&made, &orepos);
            return Err(crate::errors::error(crate::errors::Kind::Failed,
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
                if (c.uses_markers()) && crate::repo::conflict_open(&root, c)? {
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
    // A line has its own history (owner decision 2026-11-11): a save that changes only overlays
    // that keep parts makes a state of the line with the same base tree and the new parts. A
    // protected line moves only through merges, so there the parts wait in the stack as before.
    let parts_only = s.shared.is_empty()
        && made
            .iter()
            .any(|(i, _, _)| repo.overlay_info(&layers[*i].overlay).ok().flatten().is_some_and(|info| parts_overlay(&info)))
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
                // The parts of the build-output overlays (slice 6), as in a save without overlays.
                for (overlay, part) in repo.save_named_cache_parts(locked, ctx, line, &st.id) {
                    st.parts.entry(overlay).or_insert(part);
                }
                // Content conflicts stay listed while the committed file has markers.
                let root = repo.p.state_path(&st.id);
                for c in &head.conflicts {
                    if (c.uses_markers()) && crate::repo::conflict_open(&root, c)? {
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
        let _lock = orepo.p.lock_maintenance()?;
        match &ol {
            Some(ol) => move_overlay_line(&_lock, orepo, ctx, "commit", ol, &st.id, vec![st.clone()])?,
            None => bail!("line '{}' has no line in overlay '{}'", line.name, layers[i].overlay),
        }
        overlays.push((layers[i].overlay.clone(), st));
    }
    let new_head = shared.as_ref().map(|s| s.id.clone()).unwrap_or(line.head.clone());
    let public = repo.public_layers(&new_layers)?;
    let mut data = serde_json::json!({"line":line.id});
    if !public.is_empty() {
        // The states that the save made on the public overlays: `undo` moves the line back from them.
        let made: Vec<&(String, StateRec)> =
            overlays.iter().filter(|(n, _)| !repo.need_overlay(n).map(|i| i.hidden).unwrap_or(true)).collect();
        data["overlays"] = serde_json::json!(made.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>());
        data["made"] =
            serde_json::json!(made.iter().map(|(n, st)| serde_json::json!({"overlay": n, "state": st.id})).collect::<Vec<_>>());
    }
    let states: Vec<StateRec> = shared.iter().cloned().collect();
    if shared.is_some() || overlays.iter().any(|(n, _)| !repo.need_overlay(n).map(|i| i.hidden).unwrap_or(true)) {
        repo.move_head(locked, ctx, "commit", line, &new_head, states, None, data)?;
    }
    if let Some(state) = &shared {
        repo.record_hidden_parts(ctx, &state.id, &new_layers)?;
    }
    let fds = if summary { crate::diff::file_diffs(&base, &new_tree.path, &s.list)? } else { Vec::new() };
    repo.install_base(line, new_tree, &new_head, &new_layers)?;
    let mut moved: BTreeSet<&String> = overlays.iter().map(|(name, _)| name).collect();
    // The history limit of each overlay counts the parts of the new head: a part of the old head
    // that no state keeps now goes.
    moved.extend(shared.iter().flat_map(|s| s.parts.keys()));
    if shared.is_some() {
        moved.extend(head.parts.keys());
    }
    for name in moved {
        repo.after_overlay_move(ctx, name);
    }
    Ok(Some(Committed { shared, overlays, fds }))
}

/// A three-way text merge of one file (`git merge-file -p`): the result and whether it is clean. None when a
/// version is binary or too large for a text merge.
fn merge_text(base: &[u8], ours: &[u8], theirs: &[u8], label: &str) -> Result<Option<(Vec<u8>, bool)>> {
    let binary = |d: &[u8]| d.len() as u64 > fsutil::MAX_TEXT || d[..d.len().min(8000)].contains(&0);
    if binary(base) || binary(ours) || binary(theirs) {
        return Ok(None);
    }
    let dir = std::env::temp_dir().join(format!("layr-rebase-{}", ids::new_id()));
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    }
    let res = (|| -> Result<Option<(Vec<u8>, bool)>> {
        let (o, b, t) = (dir.join("ours"), dir.join("base"), dir.join("theirs"));
        std::fs::write(&o, ours)?;
        std::fs::write(&b, base)?;
        std::fs::write(&t, theirs)?;
        let out = std::process::Command::new("git")
            .args(["merge-file", "-p", "-L", "overlay", "-L", "base", "-L", label])
            .arg(&o)
            .arg(&b)
            .arg(&t)
            .output()?;
        match out.status.code() {
            Some(0) => Ok(Some((out.stdout, true))),
            Some(n) if n > 0 && n < 128 => Ok(Some((out.stdout, false))),
            _ => bail!("git merge-file failed: {}", String::from_utf8_lossy(&out.stderr)),
        }
    })();
    let _ = std::fs::remove_dir_all(&dir);
    res
}

/// The merge of each patch of the view of `line` with its parent (docs/overlays.md section 1, "a patch is a
/// patch"): when the version of the parent view of a patched path is no longer the base of the patch, a
/// three-way merge (base: the recorded version, ours: the overlay's version, theirs: the parent's version
/// now). A clean merge gives the overlay a new version and base. An overlap gives the file conflict markers
/// and a conflict that the next save resolves; a binary file and a parent that deleted or renamed the path
/// give a conflict of kind `parent`, and the overlay keeps its version. Only a reader of every overlay of
/// the path merges: for another caller the patches wait for the next reader (`patch_report`). The overlays
/// are merged from the bottom, so an upper overlay merges with the merged lower one. Returns what it did,
/// as lines of text; the working folder takes the new versions (a local change in the way refuses).
pub fn rebase_patches(ctx: &Ctx, repo: &Repo, line: &Line) -> Result<Vec<String>> {
    let mut msgs = Vec::new();
    if repo.p.is_overlay() || ctx.caller.uid != line.owner || line.machine != repo.p.store.machine.id {
        return Ok(msgs);
    }
    let layers = repo.layers(line)?;
    if layers.is_empty() {
        return Ok(msgs);
    }
    for l in &layers {
        if !repo.reads_overlay(&l.overlay, ctx.caller.uid, ctx.caller.gid)? || repo.layer_root(l).is_none() {
            return Ok(msgs);
        }
    }
    let shared = repo.state_root(&line.head)?;
    let own = repo.own(line);
    // The full tree that the folder shows now: the folder takes the new versions from it.
    let shown = repo.head_tree(line)?;
    // A path with a local change waits: the merge must not take the place of work that is not saved.
    // The folder is read only when a patch has a parent that changed (a named save calls this each time).
    let mut local: Option<BTreeSet<String>> = None;
    let mut new_layers = layers.clone();
    let mut made: Vec<(usize, Line, StateRec)> = Vec::new();
    for i in 0..new_layers.len() {
        let name = new_layers[i].overlay.clone();
        if !repo.patches_overlay(&name)? {
            continue;
        }
        let roots: Vec<PathBuf> = new_layers.iter().map(|l| repo.layer_root(l).expect("checked above")).collect();
        let removed: Vec<BTreeSet<String>> = roots.iter().map(|r| removed_paths(r)).collect::<Result<_>>()?;
        let meta = read_meta(roots[i].as_path())?;
        let orepo = repo.overlay_repo(&name)?;
        let work = orepo.temp_work(&roots[i])?;
        let mut ops = Ops::default();
        let mut conflicts = Vec::new();
        for (p, b) in &meta.bases {
            if !has_file(&roots[i], p) {
                continue;
            }
            let under = below_root(&shared, &roots, &removed, i, p);
            let now = match &under {
                Some(r) => version_hash(r.as_path(), p)?,
                None => None,
            };
            if now.as_deref() == Some(b.as_str()) {
                continue;
            }
            if local.is_none() {
                let (st, _scan, _it, _idx) = repo.status(line)?;
                let mut v = st.local_paths();
                v.extend(st.ignored.iter().cloned());
                local = Some(v.into_iter().collect());
            }
            if local.as_ref().is_some_and(|l| l.contains(p)) {
                msgs.push(format!("overlay {name}: {p} waits for a merge with the parent: it has local changes; save them (the save merges it), or restore them and run 'layr overlay use {}'", layers.last().map(|l| l.overlay.as_str()).unwrap_or(&name)));
                continue;
            }
            let conflict =
                |kind: crate::model::ConflictKind| crate::model::Conflict::new(p, kind, crate::model::ConflictSides::Unrecorded);
            let blob = format!("{META_BASES}/{b}");
            let (Some(r), Some(now)) = (under, now) else {
                msgs.push(format!("overlay {name}: {p}: the parent deleted or renamed it; the overlay keeps its version (a conflict)"));
                conflicts.push(conflict(crate::model::ConflictKind::Parent));
                continue;
            };
            let texts = (fsutil::read_file(roots[i].as_path(), &blob), fsutil::read_file(roots[i].as_path(), p), fsutil::read_file(r.as_path(), p));
            let merged = match texts {
                (Ok(base), Ok(ours), Ok(theirs)) => merge_text(&base, &ours, &theirs, "parent")?,
                _ => None,
            };
            match merged {
                Some((data, clean)) => {
                    // The snapshot of a state that may come from another machine: written through `Tree`, never
                    // by path, so a link in it is replaced, not followed.
                    let t = Tree::open(&work.path)?;
                    let mode = fsutil::lmeta(roots[i].as_path(), p).map(|m| if m.executable() { 0o755 } else { 0o644 }).unwrap_or(0o644);
                    if t.stat(p).is_some() {
                        t.remove(p)?;
                    }
                    t.mkdirs_for(p, own)?;
                    {
                        use std::io::Write;
                        let mut f = t.create(p, mode)?;
                        f.write_all(&data)?;
                    }
                    ops.puts.insert(p.clone());
                    ops.bases.insert(p.clone(), now.clone());
                    ops.blobs.insert(now.clone(), (r.clone(), p.clone()));
                    if clean {
                        msgs.push(format!("overlay {name}: {p}: merged with the parent"));
                    } else {
                        msgs.push(format!("overlay {name}: {p}: conflict with the parent; fix the markers and save"));
                        conflicts.push(conflict(crate::model::ConflictKind::Content));
                    }
                }
                None => {
                    msgs.push(format!("overlay {name}: {p}: a binary file changed in the parent; the overlay keeps its version (a conflict)"));
                    conflicts.push(conflict(crate::model::ConflictKind::Parent));
                }
            }
        }
        // A conflict of kind `parent` that is open already is not made again.
        let open: BTreeSet<String> = orepo
            .p
            .db
            .state(&new_layers[i].state)?
            .map(|s| s.conflicts.into_iter().filter(|c| c.kind() == PARENT_CONFLICT).map(|c| c.path).collect())
            .unwrap_or_default();
        conflicts.retain(|c| !(c.kind() == PARENT_CONFLICT && open.contains(&c.path)));
        if ops.is_empty() && conflicts.is_empty() {
            continue;
        }
        let info = repo.need_overlay(&name)?;
        let _lock = orepo.p.lock_maintenance()?;
        let ol = orepo.ensure_overlay_line(&_lock, ctx, &info, line, &new_layers[i].state)?;
        if ol.head != new_layers[i].state {
            bail!("overlay '{name}' changed in another line since this line took it; run 'layr overlay use {name}'");
        }
        let prev = new_layers[i].state.clone();
        let mut st = next_state(&orepo, ctx, &prev, &work.path, &ops, own, vec![prev.clone()], "Merge the patches with the parent", Some(&ol.id))?;
        if let Some(old) = orepo.p.db.state(&prev)? {
            st.conflicts.extend(old.conflicts.into_iter().filter(|c| c.kind() == PARENT_CONFLICT));
        }
        st.conflicts.extend(conflicts);
        new_layers[i].state = st.id.clone();
        made.push((i, ol, st));
    }
    if made.is_empty() {
        return Ok(msgs);
    }
    // One step: the folder takes the new versions, then the overlay lines move. A change of the folder that
    // came during the merge refuses, and then no new state is recorded (the patches wait).
    let discard = |made: &[(usize, Line, StateRec)]| {
        for (i, _, st) in made {
            if let Ok(o) = repo.overlay_repo(&layers[*i].overlay) {
                o.discard_states(std::slice::from_ref(st));
            }
        }
    };
    let step = (|| -> Result<Temp> {
        let view = repo.assemble(line, &line.head, &new_layers)?;
        let (st, _scan, _it, _idx) = repo.status(line)?;
        let mut now = st.local_paths();
        now.extend(st.ignored.iter().cloned());
        now.extend(local.iter().flatten().cloned());
        repo.apply_delta(&shown, &view.path, &repo.working(line), own, &now, false)?;
        Ok(view)
    })();
    let view = match step {
        Ok(v) => v,
        Err(e) => {
            discard(&made);
            return Err(e);
        }
    };
    for (n, (i, ol, st)) in made.iter().enumerate() {
        let orepo = repo.overlay_repo(&layers[*i].overlay)?;
        let _lock = orepo.p.lock_maintenance()?;
        if let Err(e) = move_overlay_line(&_lock, &orepo, ctx, "overlay.rebase", ol, &st.id, vec![st.clone()]) {
            discard(&made[n..]);
            return Err(e.into());
        }
    }
    repo.drop_stage(line)?;
    repo.install_base(line, view, &line.head, &new_layers)?;
    Ok(msgs)
}

/// Change the stack of a line to `layers` and its working folder with it: overlay files come and
/// go, local changes stay (a change in the way refuses, unless `force`). The caller holds the
/// project lock and owns the line.
pub fn set_layers(
    locked: &crate::store::LockedProject<'_>,
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    layers: &[Layer],
    force: bool,
    op: &str,
) -> Result<Vec<EntryChange>> {
    locked.check(&repo.p)?;
    let old = repo.head_tree(line)?;
    let (st, _scan, _it, _idx) = repo.status(line)?;
    // An ignored file of the user (a key of its own) is a local file too: never overwritten.
    let mut local = st.local_paths();
    local.extend(st.ignored.iter().cloned());
    set_layers_from(locked, ctx, repo, line, &old, &local, layers, force, op)
}

/// `set_layers` from the full tree `old` that the folder showed and its local paths `local`, both taken
/// before the overlay lines moved.
#[allow(clippy::too_many_arguments)]
pub fn set_layers_from(
    locked: &crate::store::LockedProject<'_>,
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    old: &Path,
    local: &BTreeSet<String>,
    layers: &[Layer],
    force: bool,
    op: &str,
) -> Result<Vec<EntryChange>> {
    let old = old.to_path_buf();
    let view = repo.assemble(line, &line.head, layers)?;
    let wdir = repo.working(line);
    let applied = repo.apply_delta(&old, &view.path, &wdir, repo.own(line), local, force)?;
    let mut rec = crate::model::DraftRecord::new(op);
    rec.data = serde_json::json!({
        "line": line.id,
        "folder_view": repo.public_view_data(&line.id, layers)?,
    });
    repo.point_overlay_lines(ctx, line, layers, op)?;
    repo.record_hidden_selection(ctx, &line.id, layers)?;
    let shown: Vec<String> = repo.public_layers(&repo.layers(line)?)?.into_iter().map(|l| l.overlay).collect();
    let want: Vec<String> = repo.public_layers(layers)?.into_iter().map(|l| l.overlay).collect();
    // Only a change of the public overlays is a record of the project: a hidden overlay keeps its place in its
    // own records (`record_hidden_selection`).
    if shown != want {
        repo.commit_record(locked, ctx, rec)?;
    }
    repo.drop_stage(line)?;
    repo.install_base(line, view, &line.head, layers)?;
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reader_of_an_overlay_must_read_its_ancestor() {
        use crate::access::Role;
        let policy = |entries: &[(&str, Role)]| {
            let mut p = Policy::default();
            for (k, r) in entries {
                p.access.insert(k.to_string(), *r);
            }
            p
        };
        let child = policy(&[("user:4242", Role::Admin), ("group:777", Role::Reader)]);
        assert_eq!(Repo::readers_covered(&child, &policy(&[("user:4242", Role::Admin)])).as_deref(), Some("group:777"));
        assert_eq!(Repo::readers_covered(&child, &policy(&[("user:4242", Role::Admin), ("group:777", Role::Reader)])), None);
        assert_eq!(Repo::readers_covered(&child, &policy(&[("*", Role::Reader)])), None, "everyone reads the ancestor");
        let mut denied = policy(&[("*", Role::Reader)]);
        denied.deny.insert("group:777".into(), vec!["read".into()]);
        denied.access.insert("group:777".into(), Role::Reader);
        assert!(Repo::readers_covered(&child, &denied).is_some(), "a deny rule for read wins");
        assert_eq!(Repo::readers_covered(&policy(&[("group:777", Role::None)]), &policy(&[])), None, "no role is no reader");
    }

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
        assert_eq!(MergeKind::parse("union-theirs"), Some(MergeKind::UnionTheirs));
        assert_eq!(MergeKind::parse("ours"), None);
        let p = Props {
            history: Some(1),
            quiet: true,
            merge: MergeKind::UnionTheirs,
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
                "prune=pnpm",
                "platform=linux-x86_64",
                "trust=protected"
            ]
        );
        // Defaults are no settings at all, and unknown values read as defaults.
        assert!(PROPS.iter().all(|k| Props::default().value(k).is_null()));
        let odd = Props::from_settings(|k| Some(serde_json::json!(if k == "history" { "many" } else { "?" })));
        assert_eq!(odd, Props::default());
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
            ..Default::default()
        };
        ops.apply(&dst, &src, None).unwrap();
        assert_eq!(std::fs::read_to_string(dst.join("c/moved.txt")).unwrap(), "moved and changed");
        assert_eq!(std::fs::read_to_string(dst.join("new.txt")).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(dst.join("keep.txt")).unwrap(), "keep");
        assert!(!dst.join("a").exists(), "empty folders go, as in git trees");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ops_write_and_clear_overlay_metadata() {
        let root = std::env::temp_dir().join(format!("layr-marks-{}", ids::new_id()));
        let (src, dst) = (root.join("src"), root.join("dst"));
        for d in [&src, &dst] {
            std::fs::create_dir_all(d).unwrap();
        }
        let ops = Ops { marks: ["b.c".to_string(), "a/x.c".to_string(), "new\nline.c".to_string()].into(), ..Default::default() };
        assert!(!ops.is_empty(), "a marker alone is a change of the layer");
        ops.apply(&dst, &src, None).unwrap();
        let meta = read_meta(dst.as_path()).unwrap();
        assert_eq!(meta.removed, BTreeSet::from(["a/x.c".to_string(), "b.c".to_string(), "new\nline.c".to_string()]), "a newline in a path works");
        std::fs::write(dst.join("kept.c"), "x").unwrap();
        assert_eq!(overlay_files(&dst).unwrap(), ["kept.c"], "a view never gets the metadata");
        // A patch base: the hash of the version below, and its content in the metadata folder.
        std::fs::write(src.join("lib.c"), "parent version").unwrap();
        let h = version_hash(src.as_path(), "lib.c").unwrap().unwrap();
        let ops = Ops {
            unmarks: ["b.c".to_string()].into(),
            bases: [("lib.c".to_string(), h.clone())].into(),
            blobs: [(h.clone(), (src.clone(), "lib.c".to_string()))].into(),
            ..Default::default()
        };
        ops.apply(&dst, &src, None).unwrap();
        let meta = read_meta(dst.as_path()).unwrap();
        assert_eq!(meta.bases.get("lib.c"), Some(&h));
        assert!(!meta.removed.contains("b.c"));
        assert_eq!(std::fs::read_to_string(dst.join(format!("{META_BASES}/{h}"))).unwrap(), "parent version");
        // A delete drops the base and its blob.
        Ops { dels: ["lib.c".to_string()].into(), unmarks: meta.removed.clone(), ..Default::default() }.apply(&dst, &src, None).unwrap();
        assert!(!dst.join(META_DIR).exists(), "no metadata: no folder");
        // Metadata of another machine: only relative paths inside the tree count; a newer version is refused.
        std::fs::create_dir_all(dst.join(META_DIR)).unwrap();
        let bad = r#"{"v":1,"removed":["/etc/passwd","../outside","a/../../b","./c",".layrmeta/meta.json","ok/d.c"],"bases":{"x":"nothex","../y":"00"}}"#;
        std::fs::write(dst.join(META_FILE), bad).unwrap();
        let meta = read_meta(dst.as_path()).unwrap();
        assert_eq!(meta.removed, BTreeSet::from(["ok/d.c".to_string()]));
        assert!(meta.bases.is_empty());
        std::fs::write(dst.join(META_FILE), r#"{"v":2,"removed":["a"]}"#).unwrap();
        assert!(read_meta(dst.as_path()).is_err(), "a newer version is refused, not read as no markers");
        // A link in place of the metadata folder is not followed.
        std::fs::remove_dir_all(dst.join(META_DIR)).unwrap();
        std::os::unix::fs::symlink(&src, dst.join(META_DIR)).unwrap();
        assert!(read_meta(dst.as_path()).unwrap().is_empty());
        // Links inside the metadata folder of a state from another machine: a write never follows them.
        std::fs::remove_file(dst.join(META_DIR)).unwrap();
        let outside = root.join("outside");
        std::fs::create_dir_all(outside.join("dir")).unwrap();
        std::fs::write(outside.join("file"), "keep").unwrap();
        std::fs::write(outside.join("dir/other"), "keep").unwrap();
        std::fs::create_dir_all(dst.join(META_DIR)).unwrap();
        // A link in place of `meta.json` is refused: layr cannot know what such a state hides.
        std::os::unix::fs::symlink(outside.join("file"), dst.join(META_FILE)).unwrap();
        assert!(read_meta(dst.as_path()).is_err());
        std::fs::remove_file(dst.join(META_FILE)).unwrap();
        std::fs::write(dst.join(META_FILE), r#"{"v":1,"removed":["old.c"]}"#).unwrap();
        std::os::unix::fs::symlink(outside.join("file"), dst.join(META_DIR).join("meta.json.tmp")).unwrap();
        std::os::unix::fs::symlink(outside.join("dir"), dst.join(META_BASES)).unwrap();
        let ops = Ops {
            marks: ["gone.c".to_string()].into(),
            bases: [("lib.c".to_string(), h.clone())].into(),
            blobs: [(h.clone(), (src.clone(), "lib.c".to_string()))].into(),
            ..Default::default()
        };
        std::fs::write(dst.join("lib.c"), "patched").unwrap();
        ops.apply(&dst, &src, None).unwrap();
        assert_eq!(std::fs::read_to_string(outside.join("file")).unwrap(), "keep", "no write through a link");
        assert_eq!(std::fs::read_dir(outside.join("dir")).unwrap().count(), 1, "nothing is added or removed outside");
        assert!(std::fs::symlink_metadata(dst.join(META_FILE)).unwrap().is_file());
        assert!(std::fs::symlink_metadata(dst.join(META_BASES)).unwrap().is_dir());
        assert!(!dst.join(META_DIR).join("meta.json.tmp").exists());
        let meta = read_meta(dst.as_path()).unwrap();
        assert!(meta.removed.contains("gone.c") && meta.bases.get("lib.c") == Some(&h));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn metadata_merges_by_structure() {
        let root = std::env::temp_dir().join(format!("layr-meta-merge-{}", ids::new_id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("kept.c"), "x").unwrap();
        let m = |removed: &[&str], bases: &[(&str, &str)]| Meta {
            removed: removed.iter().map(|s| s.to_string()).collect(),
            bases: bases.iter().map(|(p, h)| (p.to_string(), h.to_string())).collect(),
        };
        let base = m(&["a", "b"], &[("kept.c", "1")]);
        let ours = m(&["a", "c"], &[("kept.c", "1")]);
        let theirs = m(&["b", "d", "kept.c"], &[("kept.c", "2")]);
        let out = merge_meta(&base, &ours, &theirs, &root);
        assert_eq!(out.removed, BTreeSet::from(["c".to_string(), "d".to_string()]), "each side's change wins; a held file is never hidden");
        assert_eq!(out.bases.get("kept.c").map(String::as_str), Some("2"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The trees of `split`: the shared state, one overlay, the base (both) and the new tree.
    #[cfg(target_os = "macos")]
    fn split_trees(root: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let [shared, overlay, base, new] = ["shared", "overlay", "base", "new"].map(|d| root.join(d));
        for d in [&shared, &overlay, &base, &new] {
            std::fs::create_dir_all(d).unwrap();
        }
        for (d, files) in [
            (&shared, &[("core.c", "core"), ("gone.c", "gone"), ("moved.c", "moved"), (".env", "public env")][..]),
            (&overlay, &[(".env", "private env")][..]),
            (&base, &[("core.c", "core"), ("gone.c", "gone"), ("moved.c", "moved"), (".env", "private env")][..]),
            (&new, &[("core.c", "core patched"), ("moved2.c", "moved"), ("new.c", "new"), (".env", "private env 2")][..]),
        ] {
            for (f, text) in files {
                std::fs::write(d.join(f), text).unwrap();
            }
        }
        (shared, overlay, base, new)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_save_writes_every_change_to_its_target() {
        let root = std::env::temp_dir().join(format!("layr-split-{}", ids::new_id()));
        let (shared, overlay, base, new) = split_trees(&root);
        let roots = vec![overlay];
        let kinds = vec![LayerKind { name: "pro".into(), patches: true }];
        let no_paths: BTreeSet<String> = BTreeSet::new();
        let none = BTreeMap::new();
        // The view of the overlay: `main` never changes; copies, new files and delete markers go to the overlay.
        let no = BTreeSet::new();
        let s = split(&base, &new, &shared, &roots, &none, &none, Owner::Overlay(0), &no, &kinds, &no_paths).unwrap();
        assert!(s.shared.is_empty(), "main never changes: {:?}", s.shared);
        // A file saved equal to the version of main: the overlay drops its entry.
        std::fs::write(new.join(".env"), "public env").unwrap();
        let eq = split(&base, &new, &shared, &roots, &none, &none, Owner::Overlay(0), &no, &kinds, &no_paths).unwrap();
        assert!(eq.overlays[0].dels.contains(".env") && !eq.overlays[0].puts.contains(".env"), "equal to the parent is not stored");
        std::fs::write(new.join(".env"), "private env 2").unwrap();
        let o = &s.overlays[0];
        for p in ["core.c", "new.c", "moved2.c", ".env"] {
            assert!(o.puts.contains(p), "{p} goes to the overlay");
        }
        assert_eq!(o.marks, BTreeSet::from(["gone.c".to_string(), "moved.c".to_string()]), "deletes of files of main are markers");
        assert!(o.bases.contains_key("core.c"), "a new patch records the version of main as its base");
        assert!(!o.bases.contains_key("new.c"), "a new file is no patch");
        // `--to main`: a change of a file that the overlay patches cannot go to main when it overlaps the patch.
        let e = split(&base, &new, &shared, &roots, &none, &none, Owner::Shared, &no, &kinds, &no_paths).err().expect("refused");
        assert!(format!("{e:#}").contains("touches the patch of overlay 'pro'"), "{e:#}");
        // Only the change of the save goes to main (a cherry-pick): the overlay keeps its patch.
        let lines = |a: &str, z: &str| format!("{a}\n2\n3\n4\n5\n{z}\n");
        std::fs::write(shared.join(".env"), lines("1", "public")).unwrap();
        std::fs::write(roots[0].join(".env"), lines("1", "private")).unwrap();
        std::fs::write(base.join(".env"), lines("1", "private")).unwrap();
        std::fs::write(new.join(".env"), lines("one", "private")).unwrap();
        let s = split(&base, &new, &shared, &roots, &none, &none, Owner::Shared, &no, &kinds, &no_paths).unwrap();
        assert_eq!(s.down[".env"].data, lines("one", "public").into_bytes(), "main gets the change, not the patch");
        assert!(s.shared.puts.contains(".env") && s.overlays[0].puts.contains(".env") && !s.overlays[0].dels.contains(".env"));
        assert!(s.shared.puts.contains("core.c") && s.shared.puts.contains("new.c"));
        assert!(s.shared.dels.contains("gone.c"));
        assert!(s.overlays[0].marks.is_empty());
        // A file that only the overlay has is refused.
        std::fs::write(roots[0].join("own.c"), "own").unwrap();
        std::fs::write(base.join("own.c"), "own").unwrap();
        std::fs::write(new.join("own.c"), "own 2").unwrap();
        let e = split(&base, &new, &shared, &roots, &none, &none, Owner::Shared, &no, &kinds, &no_paths).err().expect("refused");
        assert!(format!("{e:#}").contains("is a file of overlay 'pro' only"), "{e:#}");
        // `--to main` of a delete of a patched file is refused (in Git: a modify/delete conflict): the patch stays.
        std::fs::remove_file(new.join(".env")).unwrap();
        std::fs::write(new.join("own.c"), "own").unwrap();
        let e = split(&base, &new, &shared, &roots, &none, &none, Owner::Shared, &no, &kinds, &no_paths).err().expect("refused");
        assert!(format!("{e:#}").contains("'.env' has a patch of overlay 'pro', and its delete in main would delete the patch too"), "{e:#}");
        // The same delete without --to: the overlay drops its patch and hides the file, and main keeps it.
        let s = split(&base, &new, &shared, &roots, &none, &none, Owner::Overlay(0), &no, &kinds, &no_paths).unwrap();
        assert!(s.overlays[0].dels.contains(".env") && s.overlays[0].marks.contains(".env") && !s.shared.dels.contains(".env"));
        // A delete of a file that only the overlay has cannot go to main.
        std::fs::write(new.join(".env"), lines("one", "private")).unwrap();
        std::fs::remove_file(new.join("own.c")).unwrap();
        let e = split(&base, &new, &shared, &roots, &none, &none, Owner::Shared, &no, &kinds, &no_paths).err().expect("refused");
        assert!(format!("{e:#}").contains("so main has nothing to delete"), "{e:#}");
        std::fs::write(new.join("own.c"), "own 2").unwrap();
        // `overlay move main`: the whole file goes to main, and the overlay drops it.
        let publish = BTreeSet::from(["own.c".to_string(), ".env".to_string()]);
        let s = split(&base, &new, &shared, &roots, &none, &none, Owner::Overlay(0), &no, &kinds, &publish).unwrap();
        assert!(s.shared.puts.contains("own.c") && s.overlays[0].dels.contains("own.c"));
        assert!(s.shared.puts.contains(".env") && s.overlays[0].dels.contains(".env") && s.down.is_empty());
        for f in ["own.c"] {
            for d in [&roots[0], &base, &new] {
                std::fs::remove_file(d.join(f)).unwrap();
            }
        }
        // `overlay move` names its overlay: the file leaves main whatever the target is.
        let moves = BTreeMap::from([("core.c".to_string(), 0usize)]);
        let s = split(&base, &new, &shared, &roots, &moves, &none, Owner::Shared, &no, &kinds, &no_paths).unwrap();
        assert!(s.overlays[0].puts.contains("core.c") && s.shared.dels.contains("core.c"));
        // A patch moved to an overlay: the layers below keep their version, and the overlay records its base.
        let moves = BTreeMap::from([(".env".to_string(), 0usize)]);
        let s = split(&base, &new, &shared, &roots, &moves, &none, Owner::Shared, &no, &kinds, &no_paths).unwrap();
        assert!(s.overlays[0].puts.contains(".env") && !s.shared.dels.contains(".env"), "main keeps its .env");
        assert!(s.overlays[0].bases.contains_key(".env"), "the moved patch has the version of main as its base");
        // A file of a quiet overlay stays in it, also with `--to main`.
        let quiet = BTreeMap::from([(".env".to_string(), 0usize)]);
        let s = split(&base, &new, &shared, &roots, &none, &quiet, Owner::Shared, &no, &kinds, &no_paths).unwrap();
        assert!(s.overlays[0].puts.contains(".env") && !s.shared.puts.contains(".env"), "a quiet file never goes to main");
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
            let mut caller = Caller::from_uid(1000);
            caller.gid = 1000;
            caller.user = "reader".into();
            caller.home = root.join("home");
            let actor = crate::model::Actor { uid: caller.uid, name: caller.user.clone(), session: None };
            let create = |name: &str, config: serde_json::Value| -> Result<Project<'_>> {
                let dir = store.project_dir(name)?;
                for folder in ["meta/db", "meta/scan", "states", "lines"] {
                    std::fs::create_dir_all(dir.join(folder))?;
                }
                let p = Project {
                    lease: Default::default(),
                    store: &store,
                    name: name.into(),
                    db: crate::db::Db::open(&crate::store::db_file(&dir))?,
                    dir,
                    id: ids::new_id(),
                };
                let mut record = crate::model::DraftRecord::new("project");
                record.data = json!({"id":p.id,"name":name,"config":config});
                p.lock_maintenance()?.append(record, &actor)?;
                Ok(p)
            };
            let access = json!({format!("user:{}", caller.uid):"admin"});
            let parent = create("p", json!({"access":access}))?;
            let head = ids::new_id();
            let line_id = ids::new_id();
            parent.db.test_connection().execute(
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
                    captures: Default::default(),
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
    fn maintenance_retirement_reads_hidden_heads_and_defers_busy_recovery() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = Repo::open(&fixture.ctx, "p")?;
        let overlay = repo.overlay_project("private")?;
        overlay.db.test_connection().execute(
            "INSERT INTO lines (id,name,owner,machine,head,time,ord) VALUES (?1,'main',?2,?3,?4,0,'1')",
            (ids::new_id(), fixture.ctx.caller.uid, &fixture.ctx.store.machine.id, &fixture.head),
        )?;
        let _lock = repo.p.lock_maintenance()?;
        #[cfg(target_os = "macos")]
        {
            let held =
                std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(overlay.meta_dir().join("lock"))?;
            rustix::fs::flock(&held, rustix::fs::FlockOperation::LockExclusive)?;
            let pending = overlay.meta_dir().join("pending/test");
            std::fs::create_dir_all(&pending)?;
            let queue = repo.p.meta_dir().join("object-native");
            std::fs::create_dir(&queue)?;
            let marker = queue.join(&fixture.head);
            std::fs::write(&marker, b"")?;
            assert!(crate::retention::overlay_named_states_maintenance(&repo)?.is_none());
            crate::objstore::retire_native_maintenance(&repo)?;
            assert!(marker.exists());
            std::fs::remove_dir(&pending)?;
            drop(held);
        }
        let named = crate::retention::overlay_named_states_maintenance(&repo)?.expect("available overlays");
        assert!(named.contains(&fixture.head));
        Ok(())
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
        overlay.db.test_connection().execute(
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
        let mut record = crate::model::DraftRecord::new("config");
        record.data = json!({"config":{
            "overlay.remote":{"hidden":false},"overlay.remote.quiet":true,
            "overlay.unavailable":{"hidden":true}
        }});
        repo.p.lock_maintenance().unwrap().append(record, &fixture.ctx.actor())?;
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
