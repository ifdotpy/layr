//! Build-output overlays (docs/overlays.md, section 1a, slices 3 and 6; docs/cache-overlays.md).
//!
//! A cache is an ordinary overlay: a folder overlay with `quiet`, `history=last`,
//! `merge=union-theirs`, `platform` and `trust=protected`. The general folder overlay code
//! (`folder_path`, `folder_overlays`) is in `overlay.rs`. This module keeps what build output
//! still needs and what old data needs:
//!
//! - The overlay of a `regenerable` folder for this platform, made at its first save
//!   (`ensure_cache_overlay`), and the parts that a named save makes (`save_cache_overlays`).
//! - The platform key and its parts (`cache_key`): which part fits a line.
//! - Which part a new line takes (`cache_source`): the part that a state names, else the parts of
//!   older data (`cache_of`, the overlay line of the source line), with the `trust` rule.
//! - Overlay lines per platform (`line-<id>.<os>-<arch>`) of a `cache.<folder>` overlay that an
//!   older layr made, and the old cache fields of state records (`cache_by`, `cache_shared`).
//! - Prune and size limits of the folder, and the cleanup of lines that are gone.

use crate::ctx::Ctx;
use crate::errors::{error, Kind as ErrorKind};
use crate::fsutil;
use crate::ids;
use crate::ignore_rules::Rules;
use crate::model::{Line, LineChange, StateKind, StateRec};
use crate::overlay::{files_of, MergeKind, Mode, Props};
use crate::repo::Repo;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

thread_local! {
    /// The prune limit of this thread: `HOOK_LIMIT` (a command waits for the prune), or a longer one that
    /// `Repo::with_prune_limit` sets for the background follow pass.
    static PRUNE_LIMIT: std::cell::Cell<std::time::Duration> = const { std::cell::Cell::new(crate::cache_hook::HOOK_LIMIT) };
}

/// The name of the build-output overlay of `folder` on `platform` (docs/overlays.md, section 1a,
/// slices 3 and 6): `<folder>.<platform>` with `/` as `.`, or a hash when the folder has other
/// characters. Older layr named the one overlay of all platforms `cache.<folder>`; such an overlay
/// stays the overlay of its folder (`folder_overlay_of`).
pub fn cache_overlay_name(folder: &str, platform: &str) -> String {
    let name = format!("{}.{platform}", folder.replace('/', "."));
    if crate::store::check_name("overlay", &name).is_ok() && !name.starts_with('.') {
        return name;
    }
    use layr_hashers::Digest;
    format!("folder-{}.{platform}", &hex::encode(layr_hashers::ContentAddress::digest(folder.as_bytes()))[..12])
}

thread_local! {
    /// The overlays of the view of a line that `line new` prepares, before its record names them.
    static VIEW_HINT: std::cell::RefCell<BTreeMap<String, Vec<String>>> = const { std::cell::RefCell::new(BTreeMap::new()) };
}

/// The overlays of the view of the new line `line_id` for the caches that it takes (`place_caches`).
pub fn hint_view(line_id: &str, overlays: Vec<String>) {
    VIEW_HINT.with(|h| h.borrow_mut().insert(line_id.to_string(), overlays));
}

/// The properties of a build-output overlay of this platform (docs/cache-overlays.md, section 2).
pub fn cache_props() -> Props {
    Props {
        history: Some(1),
        quiet: true,
        merge: MergeKind::UnionTheirs,
        prune: None,
        platform: Some(crate::model::cache_platform()),
        trust: Some(crate::overlay::Trust::Protected),
    }
}

/// What a folder of a folder overlay is in one line (`FolderIn` in `Repo::folder_kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FolderIn {
    /// A cache path with no versioned files in the head: the overlay's cache applies.
    Cache,
    /// A cache path whose folder holds versioned files in the head: no cache.
    Versioned,
    /// Not a cache path of the line.
    Other,
}

/// Whether a caller owns a private preparation copy or is touching a live build location.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CacheAccess {
    Prepared,
    Live,
}

/// Proof that classification excluded versioned files. Only `cache_folder` constructs this.
struct CacheFolder<'a> {
    root: &'a Path,
    relative: &'a str,
}

/// Where a line takes caches from.
pub enum Source<'s> {
    Line(&'s Line),
    State(&'s str),
    /// A shared cache of this exact state, with no fallback to its ancestors.
    ExactState(&'s str, &'s str, Option<&'s str>),
}

/// The platform key of a cache (docs/cache-overlays.md, section 6): `<os>-<arch>`, and with key
/// parts the first 16 hex digits of the SHA-256 of their names and values.
pub fn platform_key_of(parts: &BTreeMap<String, String>) -> String {
    let base = crate::model::cache_platform();
    if parts.is_empty() {
        return base;
    }
    let text: String = parts.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    format!("{base}-{}", short_hash(text.as_bytes()))
}

/// The key with only the key files `key_files` of the working folder `w` as parts.
pub fn platform_key(w: &Path, key_files: &[String]) -> String {
    platform_key_of(&file_parts(w, key_files))
}

/// The first 16 hex digits of the SHA-256 of `data`.
fn short_hash(data: &[u8]) -> String {
    use layr_hashers::Digest;
    let mut h = layr_hashers::ContentAddress::new();
    h.update(data);
    hex::encode(h.finalize())[..16].to_string()
}

/// The key parts of the key files: `file:<path>`, the hash of the file in the working folder `w`, or `-`
/// when it is missing. layr knows no toolchains: the files name them.
fn file_parts(w: &Path, key_files: &[String]) -> BTreeMap<String, String> {
    let read = |f: &str| fsutil::safe_join(w, f).ok().and_then(|p| std::fs::read(p).ok());
    key_files.iter().map(|f| (format!("file:{f}"), read(f).map_or("-".to_string(), |d| short_hash(&d)))).collect()
}

/// The names of the parts in which `ours` and `theirs` differ. A toolchain that one side could not
/// name (`-`: no toolchain command for that user, such as root without rustc) is no difference.
pub fn key_differences(ours: &BTreeMap<String, String>, theirs: &BTreeMap<String, String>) -> Vec<String> {
    let names: BTreeSet<&String> = ours.keys().chain(theirs.keys()).collect();
    let unknown = |v: Option<&String>| v.is_none_or(|v| v == "-");
    names
        .into_iter()
        .filter(|n| ours.get(*n) != theirs.get(*n))
        .filter(|n| !(n.as_str() == "toolchain" && (unknown(ours.get(*n)) || unknown(theirs.get(*n)))))
        .cloned()
        .collect()
}

/// The logical execution location, independent of a temporary snapshot/preparation folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildLocation {
    pub workspace: PathBuf,
    pub target: PathBuf,
}

impl BuildLocation {
    pub fn new(workspace: &Path, folder: &str) -> Result<Self> {
        let location = Self { workspace: workspace.to_path_buf(), target: fsutil::safe_join(workspace, folder)? };
        anyhow::ensure!(location.valid(), "a cache build location must be absolute and normalized");
        Ok(location)
    }

    fn valid(&self) -> bool {
        self.workspace.is_absolute()
            && self.target.is_absolute()
            && self.target != self.workspace
            && self.target.starts_with(&self.workspace)
            && !self.target.components().any(|part| part == std::path::Component::ParentDir)
    }
}

/// Regenerable output is disposable; sharing it between build locations requires explicit consent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheMobility {
    LocationBound,
    Portable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Provenance {
    location: BuildLocation,
    mobility: CacheMobility,
}

pub(crate) const PROVENANCE: &str = ".layr-cache-location";

impl Provenance {
    fn fits(&self, other: &Self) -> bool {
        self.mobility == other.mobility && (self.mobility == CacheMobility::Portable || self.location == other.location)
    }

    fn parse(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice::<Self>(bytes).ok().filter(|provenance| provenance.location.valid())
    }

    fn read(folder: &Path) -> Option<Self> {
        Self::parse(&std::fs::read(folder.join(PROVENANCE)).ok()?)
    }

    fn read_at(dir: &std::fs::File) -> Option<Self> {
        Self::parse(&fsutil::read_at(dir, PROVENANCE)?)
    }

    /// Publish without replacement: a concurrent restore owns any marker it installed first.
    fn write(&self, dir: &std::fs::File, uid: u32, gid: u32) -> Result<()> {
        use rustix::fs::{AtFlags, Mode, OFlags};
        use std::io::Write;
        let temp = format!("{PROVENANCE}.tmp-{}", ids::new_id());
        let fd = rustix::fs::openat(
            dir,
            temp.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o644),
        )?;
        let mut file = std::fs::File::from(fd);
        let installed = (|| -> Result<()> {
            file.write_all(&serde_json::to_vec(self)?)?;
            rustix::fs::fchown(&file, Some(rustix::fs::Uid::from_raw(uid)), Some(rustix::fs::Gid::from_raw(gid)))?;
            file.sync_all()?;
            match rustix::fs::linkat(dir, temp.as_str(), dir, PROVENANCE, AtFlags::empty()) {
                Ok(()) => Ok(()),
                Err(rustix::io::Errno::EXIST) => {
                    anyhow::ensure!(
                        Self::read_at(dir).as_ref() == Some(self),
                        "conflicting location provenance; output left intact"
                    );
                    Ok(())
                }
                Err(e) => Err(e.into()),
            }
        })();
        let cleaned = rustix::fs::unlinkat(dir, temp.as_str(), AtFlags::empty());
        installed?;
        cleaned?;
        dir.sync_all()?;
        Ok(())
    }
}

/// Marker maintenance in a private copy must not look like an output change. Call separately
/// before and after the adapter, reopening its directory so adapter changes retain their times.
fn preserve_directory_mtime(dir: &std::fs::File, edit: impl FnOnce() -> Result<()>) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let before = dir.metadata()?;
    let result = edit();
    fsutil::set_mtime(dir, before.mtime(), before.mtime_nsec())?;
    result
}

/// Publish provenance without hiding adapter rewrites that retain Cargo's file timestamps.
fn publish_relocated(dir: &std::fs::File, wanted: &Provenance, uid: u32, gid: u32, changed: bool) -> Result<()> {
    if changed {
        wanted.write(dir, uid, gid)
    } else {
        preserve_directory_mtime(dir, || wanted.write(dir, uid, gid))
    }
}

/// The cache key of a line (`Repo::cache_key`): the platform key and its parts.
pub struct CacheKey {
    pub key: String,
    pub parts: BTreeMap<String, String>,
}

impl CacheKey {
    /// Whether a cache state with this platform key and these parts fits: the same platform, and
    /// equal parts (`key_differences`). Missing location provenance is always incompatible.
    pub fn fits(&self, platform: Option<&str>, parts: &BTreeMap<String, String>) -> bool {
        let provenance =
            |parts: &BTreeMap<String, String>| parts.get("location").and_then(|value| Provenance::parse(value.as_bytes()));
        let (Some(ours), Some(theirs)) = (provenance(&self.parts), provenance(parts)) else { return false };
        if !ours.fits(&theirs) {
            return false;
        }
        let mut theirs_parts = parts.clone();
        if ours.mobility == CacheMobility::Portable {
            theirs_parts.insert("location".into(), self.parts["location"].clone());
        }
        let base = crate::model::cache_platform();
        let ours = self.key == base || self.key.starts_with(&format!("{base}-"));
        ours && !other_platform(platform) && platform.is_some() && key_differences(&self.parts, &theirs_parts).is_empty()
    }
}

/// A state of a cache overlay made on another operating system or processor: a machine never
/// imports it from a layr remote (docs/cache-overlays.md, section 6).
pub fn other_platform(platform: Option<&str>) -> bool {
    let base = crate::model::cache_platform();
    platform.is_some_and(|p| p != base && !p.starts_with(&format!("{base}-")))
}

/// After the pin of the cache state `sid`: whether its tree is still here. `gc` can evict it between the choice of
/// the cache and the pin (`cmd::admin::evict_caches`); the pin keeps it from then on. False: the folder starts
/// cold, with a warning.
/// Whether a read of cache state `id` can give its tree: the state is here, or it is in the object
/// store and not freed. A freed state stays freed (`Repo::check_state`), even while its objects remain.
fn cache_readable(orepo: &Repo, id: &str) -> Result<bool> {
    if orepo.p.has_state(id) {
        return Ok(true);
    }
    if matches!(orepo.p.db.state_flags(id)?, Some((_, true))) {
        return Ok(false);
    }
    Ok(crate::objstore::verified(&orepo.p, id)?.is_some())
}

fn cache_here(orepo: &Repo, sid: &str, folder: &str) -> Result<bool> {
    if cache_readable(orepo, sid)? {
        return Ok(true);
    }
    eprintln!("layr: no cache for {folder}: gc evicted state {} meanwhile; the folder starts cold", ids::short(sid));
    Ok(false)
}

impl<'a> Repo<'a> {
    /// The key files of overlay `name` (`overlay.<name>.key-files`).
    pub fn key_files(&self, name: &str) -> Result<Vec<String>> {
        Ok(self
            .overlay_setting(name, "key-files")?
            .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
            .unwrap_or_default())
    }

    /// The platform key of overlay `name` for the working folder `w` of a line of `uid`, and its parts:
    /// the key files, the environment that changes the build in the folder (the values that the project
    /// pins, else the caller's), and the output of the toolchain command, both named by the folder's
    /// cache adapter (docs/cache-overlays.md, section 6).
    pub fn cache_key(&self, ctx: &Ctx, name: &str, w: &Path, uid: u32) -> Result<CacheKey> {
        self.cache_key_at(ctx, name, w, w, uid)
    }

    pub fn cache_key_at(&self, ctx: &Ctx, name: &str, w: &Path, at: &Path, uid: u32) -> Result<CacheKey> {
        let mut parts = file_parts(w, &self.key_files(name)?);
        let folder = self.overlay_setting(name, "folder")?.and_then(|v| v.as_str().map(str::to_owned));
        if let Some(folder) = folder.as_deref() {
            parts.insert("location".into(), serde_json::to_string(&self.provenance(folder, w, at)?)?);
        }
        if let Some(path) = folder.and_then(|f| fsutil::safe_join(w, &f).ok()) {
            let setting = self.overlay_props(name)?.prune;
            if let Some(adapter) = crate::cache_hook::adapter_for(setting.as_deref(), &path) {
                let pinned = adapter.pinned_env(&path);
                let extra: Vec<String> =
                    self.p.config_value("cache.key-env")?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
                let names = adapter.key_env().iter().map(|n| n.to_string()).chain(extra);
                for n in names {
                    let value = pinned.iter().find(|(k, _)| *k == n).map(|(_, v)| v.as_str()).or(ctx.env(&n));
                    parts.insert(format!("env:{n}"), value.map_or("-".to_string(), |v| short_hash(v.as_bytes())));
                }
                if let Some(t) = adapter.toolchain_command() {
                    let out = crate::cache_hook::toolchain_output(&t, path.parent().unwrap_or(w), uid);
                    parts.insert("toolchain".to_string(), out.map_or("-".to_string(), |o| short_hash(o.as_bytes())));
                }
            }
        }
        Ok(CacheKey { key: platform_key_of(&parts), parts })
    }

    /// Per-folder opt-in, using the same path-list/attribute mechanisms as `regenerable`.
    pub fn cache_mobility(&self, folder: &str, w: &Path) -> CacheMobility {
        if self.p.config_strings("cache.portable").iter().any(|p| p == folder)
            || Rules::attribute(w, "cache-portable").ignored(folder, true)
        {
            CacheMobility::Portable
        } else {
            CacheMobility::LocationBound
        }
    }

    fn provenance(&self, folder: &str, root: &Path, at: &Path) -> Result<Provenance> {
        Ok(Provenance { location: BuildLocation::new(at, folder)?, mobility: self.cache_mobility(folder, root) })
    }

    fn cache_folder<'f>(&self, line: &Line, state: &str, root: &'f Path, folder: &'f str) -> Result<Option<CacheFolder<'f>>> {
        Ok(match self.folder_kind_at(line, state, root, folder)? {
            FolderIn::Cache => Some(CacheFolder { root, relative: folder }),
            FolderIn::Versioned | FolderIn::Other => None,
        })
    }

    /// Destruction is allowed only in a private copy. Live builds do not share Layr's line lock,
    /// and a negative writer probe cannot establish exclusion on every supported platform.
    fn cold_cache(&self, cache: &CacheFolder<'_>, at: &Path, access: CacheAccess, uid: u32, gid: u32) -> Result<()> {
        let (root, folder) = (cache.root, cache.relative);
        let path = fsutil::safe_join(root, folder)?;
        if path.symlink_metadata().is_ok() {
            anyhow::ensure!(access == CacheAccess::Prepared,
                "cache {folder}: incompatible or missing location provenance; cannot exclude active builds; output left intact. Stop builds and remove the cache folder. Before rebuilding, declare this exact folder regenerable (a literal .layrattributes rule or the regenerable config list), then rerun Layr to establish cold output; a wildcard alone cannot initialize an absent folder");
            fsutil::remove_entry(root, folder)?;
        }
        // Unlike make_cache, creation must fail on a racing pre-existing directory: never stamp it.
        let tree = crate::tree::Tree::open(root)?;
        tree.mkdirs_for(folder, Some((uid, gid)))?;
        tree.create_subvolume(folder)?;
        let dir = tree.open_folder(folder)?;
        self.provenance(folder, root, at)?.write(&dir, uid, gid)?;
        tree.chown(folder, uid, gid)
    }

    /// Existing paths, declared literal paths, and saved overlays all need classification.
    pub fn cache_candidates(&self, root: &Path) -> Result<Vec<String>> {
        let mut folders = self.cache_paths(root)?;
        folders.extend(self.folder_overlays()?.into_iter().map(|(_, folder)| folder));
        folders.sort();
        folders.dedup();
        Ok(folders)
    }

    pub fn initialize_missing_caches(&self, line: &Line) -> Result<()> {
        let root = self.working(line);
        let (uid, gid) = self.owner_ids(line);
        for folder in self.cache_candidates(&root)? {
            let Some(cache) = self.cache_folder(line, &line.head, &root, &folder)? else { continue };
            if fsutil::safe_join(&root, &folder)?.symlink_metadata().is_err() {
                self.cold_cache(&cache, &root, CacheAccess::Live, uid, gid)?;
            }
        }
        Ok(())
    }

    pub(crate) fn prepare_cache_locations(&self, line: &Line) -> Result<()> {
        self.validate_cache_locations(line, &self.working(line), &self.working(line), CacheAccess::Live)
    }

    pub fn validate_cache_locations(&self, line: &Line, root: &Path, at: &Path, access: CacheAccess) -> Result<()> {
        let folders = self.cache_candidates(root)?;
        self.relocate_folders(line, &folders, root, at, access)
    }

    /// Whether `folder` is a cache path of the working folder `w`: the machine setting names it, or
    /// a `.layrattributes` file marks it regenerable and the ignore rules ignore it. The folder may
    /// be absent (docs/cache-overlays.md, section 3).
    pub fn is_cache_path(&self, folder: &str, w: &Path) -> bool {
        self.p.config_strings("regenerable").iter().any(|p| p == folder)
            || (Rules::attribute(w, "regenerable").ignored(folder, true) && Rules::new(w).ignored(folder, true))
    }

    /// Whether folder overlay `name` takes part in the working folder `w`. A folder overlay of one
    /// platform (property `platform`) holds build output: only where its folder is a cache path of
    /// the line. Other folder overlays always do.
    pub fn applies_in(&self, name: &str, folder: &str, w: &Path) -> Result<bool> {
        let platform = self.overlay_info(name)?.is_some_and(|i| i.platform.is_some());
        Ok(!platform || self.is_cache_path(folder, w))
    }

    /// What `folder` is in `line`: a cache path, a cache path that holds versioned files in the head
    /// (not a cache), or no cache path of the line.
    pub fn folder_kind(&self, line: &Line, folder: &str) -> Result<FolderIn> {
        self.folder_kind_at(line, &line.head, &self.working(line), folder)
    }

    fn folder_kind_at(&self, line: &Line, state: &str, root: &Path, folder: &str) -> Result<FolderIn> {
        if !self.is_cache_path(folder, root) {
            return Ok(FolderIn::Other);
        }
        if crate::repo::holds_files_in(&self.state_src(state)?, folder)
            || self.current_stage(line)?.is_some_and(|stage| crate::repo::holds_files(&stage, folder))
        {
            return Ok(FolderIn::Versioned);
        }
        for layer in self.layers(line)? {
            if crate::repo::holds_files_in(&self.overlay_repo(&layer.overlay)?.state_src(&layer.state)?, folder) {
                return Ok(FolderIn::Versioned);
            }
        }
        Ok(FolderIn::Cache)
    }

    /// The cache overlays that `overlay remove` removed and that gc has not freed yet, with their
    /// empty first state. The setting `overlay.<name>.removed` holds that state's id as a string.
    pub fn removed_overlays(&self) -> Result<Vec<(String, String)>> {
        let mut out = BTreeSet::new();
        if self.p.is_overlay() {
            return Ok(Vec::new());
        }
        for (key, value) in self.p.db.config_all()? {
            let base = key.split('@').next().unwrap_or("");
            if let Some(name) = base.strip_prefix("overlay.").and_then(|s| s.strip_suffix(".removed")) {
                let root: String = serde_json::from_str(&value)?;
                out.insert((name.to_string(), root));
            }
        }
        Ok(out.into_iter().collect())
    }

    /// Free the states of a removed cache overlay: no line of it is kept, so its states go, except
    /// its empty first state. Like the states of a deleted line, they go at gc (docs/cache-overlays.md).
    /// The caller holds the project lock.
    fn free_removed_overlay(&self, ctx: &Ctx, name: &str, root: &str) -> Result<()> {
        if !self.p.store.project_dir(&format!("{}%{name}", self.p.name))?.exists() {
            return Ok(());
        }
        let orepo = self.overlay_repo(name)?;
        let _lock = orepo.p.lock_maintenance()?;
        for ol in orepo.p.db.lines()? {
            let mut rec = crate::model::DraftRecord::new("line.delete");
            rec.lines.insert(
                ol.id.clone(),
                LineChange {
                    transition: crate::model::LineTransition::Delete { before: Some(orepo.line_state(&ol, &ol.head)) },
                    working_before: None,
                    working_after: None,
                },
            );
            rec.data = serde_json::json!({"name": ol.name});
            orepo.commit_record(&_lock, ctx, rec)?;
        }
        let mut deleted = Vec::new();
        for (id, _) in orepo.p.db.present_states()? {
            if id != root && orepo.p.delete_state(&id)? {
                deleted.push(id);
            }
        }
        if !deleted.is_empty() {
            let mut rec = crate::model::DraftRecord::new("gc");
            rec.data = serde_json::json!({"deleted": deleted, "overlay": name, "removed": true});
            orepo.commit_record(&_lock, ctx, rec)?;
        }
        Ok(())
    }

    /// Delete the cache overlay lines (`line-<id>`) of deleted project lines: a deleted line keeps
    /// no caches (docs/cache-overlays.md). Their states go at once (`apply_history`). Returns the overlay
    /// and the line name of each deleted overlay line. The caller holds the project lock.
    pub fn drop_orphan_cache_lines(&self, ctx: &Ctx) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        if self.p.is_overlay() {
            return Ok(out);
        }
        self.free_removed_overlays(ctx)?;
        for name in self.overlay_names()? {
            if self.overlay_info(&name)?.is_none_or(|i| i.mode != crate::overlay::Mode::Follow) {
                continue;
            }
            // An overlay that is not on this machine has no lines to delete here.
            if !self.p.store.project_dir(&format!("{}%{name}", self.p.name))?.exists() {
                continue;
            }
            let orepo = self.overlay_repo(&name)?;
            let dropped = {
                let _lock = orepo.p.lock_maintenance()?;
                let orphans = self.orphan_cache_lines(&orepo)?;
                self.drop_overlay_lines(&_lock, ctx, &orepo, &name, &orphans)?
            };
            // Their states go at once: `history` keeps only the heads of the overlay lines that stay
            // and the states that lines show (it takes the overlay's lock itself).
            if !dropped.is_empty() {
                self.apply_history(ctx, &name)?;
            }
            out.extend(dropped);
        }
        Ok(out)
    }

    /// Free the states of the cache overlays that `overlay remove` removed. The caller holds the
    /// project lock.
    pub fn free_removed_overlays(&self, ctx: &Ctx) -> Result<()> {
        for (name, root) in self.removed_overlays()? {
            self.free_removed_overlay(ctx, &name, &root)?;
        }
        Ok(())
    }

    /// The lines of the overlay project `orepo` whose line of the project is gone. It only reads.
    pub fn orphan_cache_lines(&self, orepo: &Repo) -> Result<Vec<Line>> {
        let mut out = Vec::new();
        for ol in orepo.p.db.lines()? {
            if crate::overlay::orphan_overlay_line(&ol, &self.p.db)? {
                out.push(ol);
            }
        }
        Ok(out)
    }

    /// Delete these lines of the overlay project `orepo`. Returns the overlay and line names. The
    /// caller holds the locks of the project and of the overlay project.
    pub fn drop_overlay_lines(
        &self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        orepo: &Repo,
        name: &str,
        lines: &[Line],
    ) -> Result<Vec<(String, String)>> {
        locked.check(&orepo.p)?;
        let mut out = Vec::new();
        for ol in lines {
            let mut rec = crate::model::DraftRecord::new("line.delete");
            rec.lines.insert(
                ol.id.clone(),
                LineChange {
                    transition: crate::model::LineTransition::Delete { before: Some(orepo.line_state(ol, &ol.head)) },
                    working_before: None,
                    working_after: None,
                },
            );
            rec.data = serde_json::json!({"name": ol.name});
            orepo.commit_record(locked, ctx, rec)?;
            out.push((name.to_string(), ol.name.clone()));
        }
        Ok(out)
    }

    /// The cache overlay of `folder`, made when missing. The caller holds the project lock.
    fn ensure_cache_overlay(&self, locked: &crate::store::LockedProject<'_>, ctx: &Ctx, folder: &str) -> Result<String> {
        locked.check(&self.p)?;
        if let Some(n) = self.folder_overlay_of(folder)? {
            self.sync_cache_readers(ctx, &n);
            return Ok(n);
        }
        let name = cache_overlay_name(folder, &crate::model::cache_platform());
        if self.overlay_info(&name)?.is_some() {
            bail!("overlay '{name}' exists and does not hold folder {folder}");
        }
        // The project setting `cache.key-files` gives new cache overlays their key files.
        let key_files = self.p.config_value("cache.key-files")?.filter(|v| v.is_array());
        crate::overlay::create::make_overlay(
            locked,
            ctx,
            self,
            &name,
            &self.cache_readers(&name)?,
            Mode::Follow,
            false,
            &cache_props(),
            Some(folder),
            key_files.as_ref(),
            None,
        )?;
        Ok(name)
    }

    /// The cache overlay of `folder` in the view of overlay `x`, made when missing (docs/overlays.md section 1,
    /// caches per view): a folder overlay with the cache properties and `view=<x>`, whose readers are the
    /// readers of `x` (not the trusted machines), hidden when `x` is hidden. The caller holds the project lock.
    fn ensure_view_cache_overlay(
        &self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        folder: &str,
        x: &str,
    ) -> Result<String> {
        locked.check(&self.p)?;
        for (n, f, v) in self.view_cache_overlays()? {
            if f == folder && v == x {
                return Ok(n);
            }
        }
        let name = format!("{}.view.{x}", cache_overlay_name(folder, &crate::model::cache_platform()));
        crate::store::check_name("overlay", &name)?;
        if self.overlay_info(&name)?.is_some() {
            bail!("overlay '{name}' exists and does not hold folder {folder} of the view of {x}");
        }
        let info = self.need_overlay(x)?;
        let policy = crate::access::Policy::load(&self.overlay_project(x)?);
        let readers: Vec<String> =
            policy.access.iter().filter(|(_, r)| **r >= crate::access::Role::Reader).map(|(k, _)| k.clone()).collect();
        let key_files = self.p.config_value("cache.key-files")?.filter(|v| v.is_array());
        crate::overlay::create::make_overlay(
            locked,
            ctx,
            self,
            &name,
            &readers,
            Mode::Follow,
            info.hidden,
            &cache_props(),
            Some(folder),
            key_files.as_ref(),
            None,
        )?;
        let mut rec = crate::model::DraftRecord::new("config");
        rec.data = serde_json::json!({"config": {format!("overlay.{name}.{}", crate::overlay::VIEW): x}});
        if info.hidden {
            let orepo = self.overlay_repo(&name)?;
            let lock = orepo.p.lock_maintenance()?;
            orepo.commit_record(&lock, ctx, rec)?;
        } else {
            self.commit_record(locked, ctx, rec)?;
        }
        Ok(name)
    }

    /// The readers of cache overlay `name` besides its owner: the machines this project trusts (this
    /// machine included), so that store copies and peer sync give caches only to them. A limited
    /// machine reads it while its trust lasts and when the trust names it (docs/ci-trust.md). Local
    /// users take a cache by the rules of `usable_cache`.
    fn cache_readers(&self, name: &str) -> Result<Vec<String>> {
        let terms = self.p.trust_terms(&[])?;
        let now = crate::ids::now_ms();
        Ok(crate::store_remote::overlays::trusted_keys(self)?
            .into_keys()
            .filter(|m| terms.get(m).is_none_or(|t| t.live(now) && t.reads(name)))
            .map(|m| format!("machine:{m}"))
            .collect())
    }

    /// Add machines that the project trusts since the cache overlay was made to its readers. Only a
    /// key manager of the overlay may; elsewhere the readers stay, and the next save on a manager
    /// adds them. A failure leaves the readers as they were.
    pub(crate) fn sync_cache_readers(&self, ctx: &Ctx, name: &str) {
        let r = (|| -> Result<()> {
            let orepo = self.overlay_repo(name)?;
            let have: BTreeSet<String> = orepo
                .policy()
                .access
                .iter()
                .filter(|(_, r)| **r >= crate::access::Role::Reader)
                .map(|(p, _)| p.clone())
                .collect();
            let want = self.cache_readers(name)?;
            if want.iter().all(|m| have.contains(m)) || !orepo.is_admin(ctx) {
                return Ok(());
            }
            crate::overlay_managers::of(self, name)?.require(&ctx.store.machine.id)?;
            let info = self.need_overlay(name)?;
            // Machines whose trust ended leave the list (their key slots stay until a new epoch, docs/ci-trust.md).
            let terms = self.p.trust_terms(&[])?;
            let now = crate::ids::now_ms();
            let ended = |p: &str| p.strip_prefix("machine:").and_then(|m| terms.get(m)).is_some_and(|t| !t.live(now));
            let mut readers: Vec<String> =
                have.into_iter().filter(|p| *p != format!("user:{}", info.owner) && !ended(p)).collect();
            for m in want {
                if !readers.contains(&m) {
                    readers.push(m);
                }
            }
            let _lock = orepo.p.lock_maintenance()?;
            let mut record = crate::model::DraftRecord::new("overlay.readers");
            record.data = serde_json::json!({"config":{"access": crate::overlay::create::access_map(info.owner, &readers)}});
            orepo.commit_record(&_lock, ctx, record).map(|_| ()).map_err(anyhow::Error::from)
        })();
        if let Err(e) = r {
            eprintln!("warning: overlay '{name}': readers not updated: {e:#}");
        }
    }

    /// The user a new cache state records as its maker (`cache_by`): the caller, or the line owner when root
    /// saves an owner's line (the owner's tools made the files; root may act for any user, so the cache is
    /// trusted no wider than the owner's own).
    fn cache_maker(&self, ctx: &Ctx, line: &Line) -> u32 {
        if ctx.caller.uid == 0 {
            self.owner_ids(line).0
        } else {
            ctx.caller.uid
        }
    }

    /// Save the nested layers at `paths` of the line's working folder as new states of their
    /// cache overlays, on the overlay line of this line. `shared`: saved on a protected line, so
    /// lines of other users may take them. The caller holds the project lock. Returns the folder,
    /// the overlay and the new state of each.
    pub fn save_cache_overlays(
        &self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        line: &Line,
        paths: &[String],
        shared: bool,
        project_state: &str,
    ) -> Result<Vec<(String, String, String)>> {
<<<<<<< head
        locked.check(&self.p)?;
        self.save_cache_overlays_in(locked, ctx, line, &self.working(line), paths, shared, project_state)
=======
        self.save_cache_overlays_in(ctx, line, line, &self.working(line), &self.working(line), paths, shared, project_state)
>>>>>>> 109c68c98ff17551a91d01a1e865cbaf
    }

    /// Save to `line` from `build`'s context at `w` (a check copy may have a different source head).
    #[allow(clippy::too_many_arguments)]
    pub fn save_cache_overlays_in(
        &self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        line: &Line,
        build: &Line,
        w: &Path,
        at: &Path,
        paths: &[String],
        shared: bool,
        project_state: &str,
    ) -> Result<Vec<(String, String, String)>> {
        locked.check(&self.p)?;
        let mut out = Vec::new();
        // A cache built in the view of an overlay X holds compiled private code (docs/overlays.md section 1,
        // caches per view): it goes to the cache of that view, which only readers of X read.
        let view = self.line_view_top(line)?;
        for p in paths {
            let cp = fsutil::safe_join(w, p)?;
            if !crate::fs::host().namespace.managed(&cp) {
                continue;
            }
            let Some(_cache) = self.cache_folder(build, &build.head, w, p)? else { continue };
            let provenance = self.provenance(p, w, at)?;
            anyhow::ensure!(Provenance::read(&cp).as_ref() == Some(&provenance),
                "cache {p}: missing or incompatible location provenance; output left intact. Stop builds, discard this cache, declare its exact path regenerable with a literal rule or config entry, and rerun Layr before rebuilding; a wildcard alone cannot initialize an absent folder");
            let name = match &view {
                Some(x) => self.ensure_view_cache_overlay(locked, ctx, p, x)?,
                None => self.ensure_cache_overlay(locked, ctx, p)?,
            };
            let info = self.need_overlay(&name)?;
            let orepo = self.overlay_repo(&name)?;
            let CacheKey { key, parts } = self.cache_key_at(ctx, &name, w, at, line.owner)?;
            let id = ids::new_id();
            {
                let _lock = orepo.p.lock_maintenance()?;
                let ol = self.ensure_build_line(&_lock, ctx, &orepo, &name, &info, line, &info.root)?;
                let dst = orepo.p.state_path(&id);
                let captured = ctx.captures.snapshot(&cp, &dst)?;
                let sub = crate::fs::host().namespace.info(&dst)?;
                let st = StateRec {
                    id: id.clone(),
                    kind: StateKind::Commit,
                    change_id: Some(ids::new_id()),
                    derived_from: None,
                    context: None,
                    parents: vec![ol.head.clone()],
                    line: Some(ol.id.clone()),
                    subvol: sub.uuid,
                    author: ctx.author(),
                    message: format!("cache {p} of line {}", line.name),
                    time: ids::now_ms(),
                    running: false,
                    conflicts: Vec::new(),
                    caches: Default::default(),
                    cache_by: Some(self.cache_maker(ctx, line)),
                    cache_shared: shared,
                    cache_platform: Some(key),
                    cache_key_parts: parts,
                    parts: Default::default(),
                    cache_of: Some(project_state.to_string()),
                    tree: None,
                    times: None,
                };
                if let Err(e) = crate::overlay::move_overlay_line(&_lock, &orepo, ctx, "commit", &ol, &id, vec![st]) {
                    if !matches!(e, crate::db::CommitResolution::NotCommitted(_)) {
                        captured.release();
                    }
                    return Err(e.into());
                }
                captured.release();
            }
            self.after_overlay_move(ctx, &name);
            out.push((p.clone(), name, id));
        }
        Ok(out)
    }

    /// A state of overlay `name` that a line of `uid` may show: on this machine, saved on this
    /// platform, and made by `uid` or on a protected line (a cache holds code that review does not
    /// see). The empty first state is no cache.
    fn usable_cache(&self, orepo: &Repo, root: &str, id: &str, uid: u32, key: &CacheKey, protected: bool) -> Result<bool> {
        // A part whose native copy was retired is here as objects: a read builds the copy again (`state_root`).
        if id == root || !cache_readable(orepo, id)? {
            return Ok(false);
        }
        let Some(st) = orepo.p.db.state(id)? else { return Ok(false) };
        let allowed = uid == 0 || st.cache_shared || st.cache_by == Some(uid) || !protected;
        Ok(allowed && key.fits(st.cache_platform.as_deref(), &st.cache_key_parts))
    }

    /// Whether overlay `name` has `trust=protected`: a line takes a part that another user saved
    /// only from a protected line (docs/overlays.md, section 1a, slice 6). A folder overlay without
    /// the property is protected, as every cache overlay was before the property.
    pub fn trust_protected(&self, name: &str) -> Result<bool> {
        Ok(self.overlay_props(name)?.trust != Some(crate::overlay::Trust::Readers))
    }

    /// Says why a line takes no cache of overlay `name` (folder `folder`) from the line `from`: the
    /// newest cache of that line on this platform was made with other key parts (`ours`).
    fn report_key_miss(&self, name: &str, folder: &str, from: &Source, uid: u32, ours: &BTreeMap<String, String>) {
        let Source::Line(l) = from else { return };
        let found = (|| -> Result<Option<(String, BTreeMap<String, String>)>> {
            let info = self.need_overlay(name)?;
            let orepo = self.overlay_repo(name)?;
            let Some(ol) = crate::overlay::find_overlay_line(&orepo.p.db, &info, l)? else { return Ok(None) };
            let Some(st) = orepo.p.db.state(&ol.head)? else { return Ok(None) };
            let allowed = uid == 0 || st.cache_shared || st.cache_by == Some(uid) || !self.trust_protected(name)?;
            let same_platform = st.cache_platform.as_deref().is_some_and(|p| !other_platform(Some(p)));
            Ok((ol.head != info.root && allowed && same_platform && !st.cache_key_parts.is_empty())
                .then_some((ol.head, st.cache_key_parts)))
        })();
        if let Ok(Some((id, theirs))) = found {
            let differ = key_differences(ours, &theirs);
            if !differ.is_empty() {
                eprintln!(
                    "layr: {folder}: the cache of line {} is not taken (state {}): the key differs in {}",
                    l.name,
                    ids::short(&id),
                    differ.join(", ")
                );
            }
        }
    }

    /// The cache state of overlay `name` that a line of `uid` takes: the newest cache of the
    /// source line, or for a state the cache of the nearest ancestor (the state itself first):
    /// the newest cache saved with that state (`cache_of`), else the newest cache of the line that
    /// made it. A state imported from git has no line, so without `cache_of` the walk would reach
    /// an old state of some line and take its old cache.
    pub fn cache_source(&self, name: &str, from: &Source, uid: u32, key: &CacheKey) -> Result<Option<String>> {
        let info = self.need_overlay(name)?;
        let orepo = self.overlay_repo(name)?;
        let protected = self.trust_protected(name)?;
        if let Source::ExactState(wanted, target_line, expected) = from {
            let own_line = match self.p.db.line_by_id(target_line)? {
                Some(l) => crate::overlay::find_overlay_line(&orepo.p.db, &info, &l)?.map(|ol| ol.id),
                None => None,
            };
            for (id, of) in orepo.p.db.cache_states_known()? {
                if expected.is_some_and(|value| value != id.as_str()) {
                    continue;
                }
                let Some(cache) = orepo.p.db.state(&id)? else { continue };
                // The target's own saved cache is not a new remote candidate.
                if own_line.is_some()
                    && cache.line == own_line
                    && orepo.p.db.state_machine(&id)?.as_deref() == Some(self.p.store.machine.id.as_str())
                {
                    continue;
                }
                if !cache.cache_shared || !self.usable_cache(&orepo, &info.root, &id, uid, key, protected)? {
                    continue;
                }
                if of == *wanted {
                    return Ok(Some(id));
                }
                let Some(saved) = self.p.db.state(&of)? else { continue };
                if saved.kind != crate::model::StateKind::Auto || saved.parents.first().map(String::as_str) != Some(*wanted) {
                    continue;
                }
                // An automatic save can include unsaved changes. Only a matching source tree is safe.
                let head = self.state_src(wanted)?;
                let saved = match self.state_src(&of) {
                    Ok(saved) => saved,
                    Err(e) => {
                        eprintln!("layr: cache {} skipped: source state unavailable: {e:#}", ids::short(&id));
                        continue;
                    }
                };
                let mut different = false;
                for change in crate::changes::candidates(&head, &saved)? {
                    if crate::changes::really_changed(&head, &saved, &change)? {
                        different = true;
                        break;
                    }
                }
                if !different {
                    return Ok(Some(id));
                }
            }
            return Ok(None);
        }
        let of_line = |line_id: &str| -> Result<Option<String>> {
            let Some(line) = self.p.db.line_by_id(line_id)? else { return Ok(None) };
            let Some(ol) = crate::overlay::find_overlay_line(&orepo.p.db, &info, &line)? else { return Ok(None) };
            Ok(self.usable_cache(&orepo, &info.root, &ol.head, uid, key, protected)?.then_some(ol.head))
        };
        // The newest usable cache saved with each project state. `save --overlays` on a line with
        // no named change saves with an automatic state over the head: that cache counts for the
        // head too.
        let mut saved_with: std::collections::HashMap<String, String> = Default::default();
        // The project states that each usable cache state counts for.
        let mut counts_for: std::collections::HashMap<String, Vec<String>> = Default::default();
        for (id, of) in orepo.p.db.cache_states_known()? {
            if !self.usable_cache(&orepo, &info.root, &id, uid, key, protected)? {
                continue;
            }
            if let Some(st) = self.p.db.state(&of)? {
                if st.kind == crate::model::StateKind::Auto {
                    if let Some(parent) = st.parents.first() {
                        saved_with.entry(parent.clone()).or_insert_with(|| id.clone());
                        counts_for.entry(id.clone()).or_default().push(parent.clone());
                    }
                }
            }
            counts_for.entry(id.clone()).or_default().push(of.clone());
            saved_with.entry(of).or_insert(id);
        }
        let start = match from {
            Source::Line(l) => {
                if let Some(s) = of_line(&l.id)? {
                    // The line's own cache was built at an older head, and a protected line or a check
                    // runner (CI) saved a shared cache with the head itself: that one fits the head better.
                    // A user's cache of an unprotected line at the same head does not count (t14).
                    let shared = |id: &&String| orepo.p.db.state(id).ok().flatten().is_some_and(|st| st.cache_shared);
                    let at_head = saved_with
                        .get(&l.head)
                        .filter(shared)
                        .filter(|_| !counts_for.get(&s).is_some_and(|of| of.contains(&l.head)));
                    return Ok(Some(at_head.cloned().unwrap_or(s)));
                }
                l.head.clone()
            }
            Source::State(s) => s.to_string(),
            Source::ExactState(_, _, _) => unreachable!(),
        };
        const SEARCH: usize = 200;
        let mut seen = BTreeSet::new();
        let mut q = std::collections::VecDeque::from([start]);
        while let Some(id) = q.pop_front() {
            if seen.len() >= SEARCH || !seen.insert(id.clone()) {
                continue;
            }
            // The newest part saved with the state: a named save, a check or a CI job of it.
            if let Some(s) = saved_with.get(&id) {
                return Ok(Some(s.clone()));
            }
            let Some(rec) = self.p.db.state(&id)? else { continue };
            // The part that the state names on this overlay (`save -m`, slice 6): an unchanged
            // folder names a part saved with an older state.
            if let Some(part) = rec.parts.get(name) {
                if self.usable_cache(&orepo, &info.root, part, uid, key, protected)? {
                    return Ok(Some(part.clone()));
                }
            }
            if let Some(l) = &rec.line {
                if let Some(s) = of_line(l)? {
                    return Ok(Some(s));
                }
            }
            q.extend(rec.parents);
        }
        Ok(None)
    }

    /// The cache folders of `line` with no cache saved with its head: a cache in the working folder whose
    /// newest saved state on this line's overlay line was saved with another state, or no state was saved.
    /// `save --overlays` with no named change saves with an automatic state over the head: that counts, unless the
    /// save ran with no build (`set_unbuilt`). A damaged folder is left out: the save refuses it until a prune of it
    /// succeeds.
    pub fn caches_behind_head(&self, line: &Line) -> Result<Vec<String>> {
        let w = self.working(line);
        let damaged = self.damaged_folders(line)?;
        let unbuilt = self.unbuilt_file(line).exists();
        let mut out = Vec::new();
        for (name, folder) in self.folder_overlays()? {
            if damaged.contains(&folder) || !fsutil::safe_join(&w, &folder).is_ok_and(|p| crate::fs::host().namespace.managed(&p))
            {
                continue;
            }
            let orepo = self.overlay_repo(&name)?;
            let ol = crate::overlay::find_overlay_line(&orepo.p.db, &self.need_overlay(&name)?, line)?;
            let of = match ol {
                Some(ol) => orepo.p.db.state(&ol.head)?.and_then(|s| s.cache_of),
                None => None,
            };
            let saved = match of {
                _ if unbuilt => false,
                Some(of) if of == line.head => true,
                Some(of) => {
                    self.p.db.state(&of)?.is_some_and(|st| st.kind == StateKind::Auto && st.parents.first() == Some(&line.head))
                }
                None => false,
            };
            if !saved {
                out.push(folder);
            }
        }
        Ok(out)
    }

    /// Put the cache overlays into the new working folder `w` of `line` (made from `start`, or
    /// from the line `source`): each folder overlay whose folder the ignore rules ignore gets a
    /// writable snapshot of its cache state unless the state holds files there (it replaces an old
    /// cache of the state), and the line gets an
    /// overlay line at that state. Returns the folders that got a cache.
    /// Whether this machine has the overlay `name` of the cache folder `folder`; prints why not.
    fn has_overlay_here(&self, name: &str, folder: &str) -> bool {
        match self.overlay_repo(name) {
            Ok(_) => true,
            Err(e) => {
                eprintln!(
                    "layr: no cache for {folder}: overlay {name} is not on this machine ({e:#}); a trusted reader gets it at the next fetch"
                );
                false
            }
        }
    }

    pub fn take_caches(&self, ctx: &Ctx, line: &Line, start: &str, source: Option<&Line>, w: &Path) -> Result<Vec<String>> {
        let mut taken = Vec::new();
        for (name, folder, sid) in self.place_caches(ctx, line, start, source, w, &self.working(line))? {
            let info = self.need_overlay(&name)?;
            let orepo = self.overlay_repo(&name)?;
            let _lock = orepo.p.lock_maintenance()?;
            let ol = self.ensure_build_line(&_lock, ctx, &orepo, &name, &info, line, &sid)?;
            // A line of the same name that an earlier line left starts again at the cache it takes.
            if ol.head != sid {
                crate::overlay::move_overlay_line(&_lock, &orepo, ctx, "line.create", &ol, &sid, Vec::new())?;
            }
            taken.push(folder);
        }
        // The new files have the time of the checkout; the ones the caches were built from get their time back.
        if let Err(e) = crate::source_times::apply(w, &taken) {
            eprintln!("warning: source file times of the caches not set: {e:#}");
        }
        Ok(taken)
    }

    /// The caches of a folder `w` that only lives for one command (a check copy): as `take_caches`,
    /// without overlay lines, so the copy writes no record. Returns the folders that got a cache.
    pub fn copy_caches(&self, ctx: &Ctx, line: &Line, start: &str, w: &Path, at: &Path) -> Result<Vec<String>> {
        let taken: Vec<String> =
            self.place_caches(ctx, line, start, None, w, at)?.into_iter().map(|(_, folder, _)| folder).collect();
        if let Err(e) = crate::source_times::apply(w, &taken) {
            eprintln!("warning: source file times of the caches not set: {e:#}");
        }
        Ok(taken)
    }

    /// The writable cache snapshots of `take_caches` in `w`: overlay name, folder and cache state.
    #[allow(clippy::too_many_arguments)]
    fn place_caches(
        &self,
        ctx: &Ctx,
        line: &Line,
        start: &str,
        source: Option<&Line>,
        w: &Path,
        at: &Path,
    ) -> Result<Vec<(String, String, String)>> {
        let mut taken = Vec::new();
        if self.p.is_overlay() {
            return Ok(taken);
        }
        let overlays = self.folder_overlays()?;
        if overlays.is_empty() && self.view_cache_overlays()?.is_empty() {
            return Ok(taken);
        }
        // The start state (its caches are read) and the source head stay for the command.
        let mut held = vec![start];
        held.extend(source.map(|s| s.head.as_str()));
        self.hold(self.pin(&held)?);
        let ignored = Rules::new(w);
        let (uid, gid) = self.owner_ids(line);
        // The folders of the caches of `main`, and of the views that the line's path holds.
        let mut folders = overlays;
        // The view of a line that `line new` makes is recorded with the line: until then its overlays are the hint.
        let mut path: Vec<String> = self.layers(line)?.into_iter().map(|l| l.overlay).collect();
        if path.is_empty() {
            path = VIEW_HINT.with(|h| h.borrow().get(&line.id).cloned()).unwrap_or_default();
        }
        for (n, f, v) in self.view_cache_overlays()? {
            if path.contains(&v) && !folders.iter().any(|(_, x)| *x == f) {
                folders.push((n, f));
            }
        }
        for (name, folder) in folders {
            // Versioned files win: a folder that the start state holds files in comes from the
            // state. Otherwise the cache below replaces what the folder holds now: an old cache
            // of the state (`states/<id>@<folder>`, applied before), else the line would start
            // with that stale copy.
            if !ignored.ignored(&folder, true) || crate::repo::holds_files_in(&self.state_src(start)?, &folder) {
                continue;
            }
            if !self.applies_in(&name, &folder, w)? {
                continue;
            }
            let Some(cache) = self.cache_folder(line, start, w, &folder)? else { continue };
            // This freshly materialized copy belongs to the command, not to a live build.
            self.cold_cache(&cache, at, CacheAccess::Prepared, uid, gid)?;
            // A machine that does not read the overlay (no key here yet) starts this folder cold.
            if !self.has_overlay_here(&name, &folder) {
                continue;
            }
            let from = match source {
                Some(s) => Source::Line(s),
                None => Source::State(start),
            };
            // The cache of the view of the line (docs/overlays.md section 1, caches per view), else of the
            // nearest view below it whose key fits, else of `main`. The view of `main` takes only the cache of
            // `main`.
            let mut found = None;
            for cand in self.cache_chain(line, &path, &folder)? {
                if !self.has_overlay_here(&cand, &folder) {
                    continue;
                }
                let key = self.cache_key_at(ctx, &cand, w, at, uid)?;
                if let Some(sid) = self.cache_source(&cand, &from, uid, &key)? {
                    found = Some((cand, sid));
                    break;
                }
            }
            let Some((name, sid)) = found else {
                let key = self.cache_key_at(ctx, &name, w, at, uid)?;
                self.report_key_miss(&name, &folder, &from, uid, &key.parts);
                continue;
            };
            let orepo = self.overlay_repo(&name)?;
            orepo.hold(orepo.pin(&[sid.as_str()])?);
            if !cache_here(&orepo, &sid, &folder)? {
                continue;
            }
            let _t =
                crate::trace::named(|| format!("take_caches {folder} of line {} from state {sid} (line {})", line.name, line.id));
            let cp = fsutil::safe_join(w, &folder)?;
            if cp.symlink_metadata().is_ok() {
                fsutil::remove_entry(w, &folder)?;
            }
            fsutil::make_parents(w, &folder, Some((uid, gid)))?;
            let uniform = crate::fs::cache(&orepo.state_root(&sid)?, &cp)?;
            let source_owner = orepo.p.db.state(&sid)?.and_then(|s| s.cache_by);
            crate::repo::give_cache_tree(&cp, uniform == layr_fs_core::OwnerLayout::Uniform, source_owner, uid, gid)?;
            taken.push((name, folder, sid));
        }
        Ok(taken)
    }

    /// Bring the caches of `from` into the line (`merge` of a line): for each
    /// folder overlay, a fast-forward of the line's overlay line, or a `union-theirs` state; the
    /// files of the incoming state are copied over the line's folder (a missing or empty folder
    /// gets one snapshot). The caller holds no project lock: each folder takes its cache overlay's
    /// lock only, and the caller prunes the folders it gets (`prune_cache_folder`). Returns folder and new state.
    /// Folders in `skip` keep their caches (a follow pass takes each folder from one line only).
    pub fn pull_caches(&self, ctx: &Ctx, line: &Line, from: &Source, skip: &BTreeSet<String>) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        if self.p.is_overlay() {
            return Ok(out);
        }
        let source_head = match from {
            Source::Line(l) => l.head.clone(),
            Source::State(s) | Source::ExactState(s, _, _) => s.to_string(),
        };
        self.hold(self.pin(&[line.head.as_str(), source_head.as_str()])?);
        self.prepare_cache_locations(line)?;
        let w = self.working(line);
        let (uid, gid) = self.owner_ids(line);
        let own = self.own(line);
        let shared = self.policy().rule_for(&line.name).is_some();
        // The caches of `main`, and the caches of the views that the path of the line holds (docs/overlays.md
        // section 1, caches per view): a merge unites the caches of a view only in a line that shows it.
        let mut list = self.folder_overlays()?;
        let path: Vec<String> = self.layers(line)?.into_iter().map(|l| l.overlay).collect();
        for (n, f, v) in self.view_cache_overlays()? {
            if path.contains(&v) && self.owner_reads(&n, line.owner).unwrap_or(false) {
                list.push((n, f));
            }
        }
        for (name, folder) in list {
            if skip.contains(&folder) || !self.applies_in(&name, &folder, &w)? || !self.has_overlay_here(&name, &folder) {
                continue;
            }
            let Some(_cache) = self.cache_folder(line, &line.head, &w, &folder)? else { continue };
            let key = self.cache_key(ctx, &name, &w, uid)?;
            let Some(sid) = self.cache_source(&name, from, uid, &key)? else {
                self.report_key_miss(&name, &folder, from, uid, &key.parts);
                continue;
            };
            let info = self.need_overlay(&name)?;
            let orepo = self.overlay_repo(&name)?;
            orepo.hold(orepo.pin(&[sid.as_str()])?);
            if !cache_here(&orepo, &sid, &folder)? {
                continue;
            }
            // Relocate incoming portable output in an owner-readable private staging copy.
            // A failing hook must not reset or damage the destination's live build output.
            let mut incoming = orepo.state_root(&sid)?;
            let mut staged = None;
            let wanted = self.provenance(&folder, &w, &w)?;
            let have = Provenance::read(&incoming);
            anyhow::ensure!(
                have.as_ref().is_some_and(|have| wanted.fits(have)),
                "cache {folder}: incoming cache has no compatible live provenance; output left intact"
            );
            if have.as_ref() != Some(&wanted) {
                let root = w.join(format!(".layr-cache-take-{}", ids::new_id()));
                std::fs::create_dir(&root)?;
                fsutil::chown_nofollow(&root, uid, gid)?;
                staged = Some(crate::repo::Temp { path: root.clone() });
                let tree = crate::tree::Tree::open(&root)?;
                let path = fsutil::safe_join(&root, &folder)?;
                fsutil::make_parents(&root, &folder, Some((uid, gid)))?;
                let uniform = btrfs::cache_snapshot(&incoming, &path)?;
                let source_owner = orepo.p.db.state(&sid)?.and_then(|state| state.cache_by);
                crate::repo::give_cache_tree(&path, uniform, source_owner, uid, gid)?;
                let props = self.overlay_props(&name)?;
                let adapter = crate::cache_hook::adapter_for(props.prune.as_deref(), &fsutil::safe_join(&w, &folder)?);
                let setting = adapter.map(|a| a.name()).or(props.prune.as_deref());
                // This private copy loses its old marker before relocation, never after the hook.
                let dir = tree.open_folder(&folder)?;
                preserve_directory_mtime(&dir, || Ok(rustix::fs::unlinkat(&dir, PROVENANCE, rustix::fs::AtFlags::empty())?))?;
                let done = crate::cache_hook::relocate(setting, &path, &wanted.location.target, uid);
                if !done.log.is_empty() {
                    eprintln!("layr: relocate {}", done.log);
                }
                let crate::cache_hook::Relocation::Reused { changed } = done.outcome else {
                    anyhow::bail!("cache {folder}: relocation failed; live output left intact");
                };
                let dir = tree.open_folder(&folder)?;
                publish_relocated(&dir, &wanted, uid, gid, changed)?;
                incoming = path;
            }
            let made = {
                let _lock = orepo.p.lock_maintenance()?;
                let ol = self.ensure_build_line(&_lock, ctx, &orepo, &name, &info, line, &info.root)?;
                let cp = fsutil::safe_join(&w, &folder)?;
                if ol.head == sid && matches!(from, Source::ExactState(_, _, _)) {
                    if crate::repo::holds_files_in(&self.state_src(&line.head)?, &folder) {
                        return Err(error(
                            ErrorKind::Failed,
                            format!("error: {folder} holds files of the line's head; it is no cache"),
                        ));
                    }
                    let src = incoming.clone();
                    if cp.symlink_metadata().is_err() {
                        fsutil::make_parents(&w, &folder, Some((uid, gid)))?;
                        crate::fs::snapshot(&src, &cp, false)?;
                        crate::repo::give_tree(&cp, uid, gid)?;
                    } else {
                        let ours: BTreeSet<String> = files_of(&cp)?.into_iter().collect();
                        crate::overlay::union_ops(&ours, files_of(&src)?).apply(&cp, &src, own)?;
                    }
                    sid.clone()
                } else if ol.head == sid || orepo.is_ancestor(&sid, &ol.head)? {
                    continue;
                } else {
                    let compatible =
                        orepo.p.db.state(&ol.head)?.is_some_and(|st| key.fits(st.cache_platform.as_deref(), &st.cache_key_parts));
                    let fast = !compatible || ol.head == info.root || orepo.is_ancestor(&ol.head, &sid)?;
                    let (head, states) = if fast {
                        (sid.clone(), Vec::new())
                    } else {
                        let mut st =
                            crate::overlay::union_state(&orepo, ctx, &ol, &ol.head, &sid, &format!("cache {folder}: union"))?;
                        st.cache_by = Some(self.cache_maker(ctx, line));
                        st.cache_shared = shared;
                        st.cache_platform = Some(key.key.clone());
                        st.cache_key_parts = key.parts.clone();
                        (st.id.clone(), vec![st])
                    };
                    // The working folder first: a failure leaves the overlay line where it was.
<<<<<<< head
                    let src = orepo.state_root(&sid)?;
                    let empty =
                        cp.symlink_metadata().is_err() || (crate::fs::host().namespace.managed(&cp) && files_of(&cp)?.is_empty());
=======
                    let src = incoming.clone();
                    let empty = cp.symlink_metadata().is_err() || (btrfs::is_subvolume(&cp) && files_of(&cp)?.is_empty());
>>>>>>> 109c68c98ff17551a91d01a1e865cbaf
                    let r = if crate::repo::holds_files_in(&self.state_src(&line.head)?, &folder) {
                        Err(error(ErrorKind::Failed, format!("error: {folder} holds files of the line's head; it is no cache")))
                    } else if empty {
                        (|| -> Result<()> {
                            if cp.symlink_metadata().is_ok() {
                                fsutil::remove_entry(&w, &folder)?;
                            }
                            fsutil::make_parents(&w, &folder, Some((uid, gid)))?;
                            crate::fs::snapshot(&src, &cp, false)?;
                            crate::repo::give_tree(&cp, uid, gid)
                        })()
                    } else {
                        let ours: BTreeSet<String> = files_of(&cp)?.into_iter().collect();
                        crate::overlay::union_ops(&ours, files_of(&src)?).apply(&cp, &src, own)
                    };
                    if let Err(e) = r
                        .map_err(crate::db::PublishError::from)
                        .and_then(|_| crate::overlay::move_overlay_line(&_lock, &orepo, ctx, "merge", &ol, &head, states.clone()))
                    {
                        if matches!(e, crate::db::CommitResolution::NotCommitted(_)) {
                            orepo.discard_states(&states);
                        }
                        return Err(e.into());
                    }
                    head
                }
            };
            self.after_overlay_move(ctx, &name);
            self.relocate_caches(line, std::slice::from_ref(&folder), &w, CacheAccess::Live)?;
            drop(staged);
            out.push((folder, made));
        }
        Ok(out)
    }

    /// The cache adapter hook (issue #167, docs/cache-overlays.md section 19) for `folder`, a folder cache of
    /// `line` that is in its working copy now. Core runs the adapter that the `prune` property of `overlay`
    /// names. It never fails: the outcome is one log line on stderr.
    pub fn prune_cache(&self, line: &Line, overlay: &str, folder: &str) {
        let root = self.working(line);
        self.prune_folder(line, Some(overlay), folder, &root);
    }

    /// The hook for a folder that the caller has in the working copy: the overlay is looked up from the folder
    /// (no overlay yet: the adapter is detected).
    pub fn prune_cache_folder(&self, line: &Line, folder: &str) {
        match self.folder_overlay_of(folder) {
            Ok(overlay) => {
                let root = self.working(line);
                self.prune_folder(line, overlay.as_deref(), folder, &root);
            }
            Err(e) => eprintln!("layr: prune {folder}: not pruned: {e:#}"),
        }
    }

    /// The build hook (`warm`) for `folder` of `line`: the adapter of its overlay (the `prune` property names it)
    /// builds the project's standard commands into the folder. It never fails: the outcome is one log line. Returns
    /// whether the build ran to its end.
    pub fn warm_cache_folder(&self, line: &Line, folder: &str) -> bool {
        let (uid, _) = self.owner_ids(line);
        let setting = match self.folder_overlay_of(folder).and_then(|o| o.map(|name| self.overlay_props(&name)).transpose()) {
            Ok(props) => props.and_then(|p| p.prune),
            Err(e) => {
                eprintln!("layr: build {folder}: not built: {e:#}");
                return false;
            }
        };
        let path = match fsutil::safe_join(&self.working(line), folder) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("layr: build {folder}: not built: {e:#}");
                return false;
            }
        };
        let _line = match self.prune_lock(line) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("layr: build {folder}: not built: {e:#}");
                return false;
            }
        };
        let _t = crate::trace::named(|| format!("warm {folder} of line {}", line.name));
        let done = crate::cache_hook::with_scratch(Some(self.hook_scratch()), || {
            crate::cache_hook::run_mode(
                crate::cache_hook::Mode::Warm,
                setting.as_deref(),
                &path,
                uid,
                crate::cache_hook::WARM_LIMIT,
            )
        });
        // Only a damage is recorded here: a mark from an earlier prune stays until a prune succeeds.
        if !done.folder_ok {
            if let Err(e) = self.set_damaged(line, folder, true) {
                eprintln!("layr: build {folder}: cannot record the damaged state: {e:#}");
            }
        }
        if !done.log.is_empty() {
            eprintln!("layr: build {}", done.log);
        }
        done.built
    }

    /// The caller must identify private preparation copies explicitly; live rejections never delete output.
    pub fn relocate_caches(&self, line: &Line, folders: &[String], root: &Path, access: CacheAccess) -> Result<()> {
        self.relocate_folders(line, folders, root, &self.working(line), access)
    }

    pub fn relocate_folders(
        &self,
        line: &Line,
        folders: &[String],
        root: &Path,
        working: &Path,
        access: CacheAccess,
    ) -> Result<()> {
        if self.p.is_overlay() {
            return Ok(());
        }
        let (uid, gid) = self.owner_ids(line);
        for folder in folders {
            let Some(cache) = self.cache_folder(line, &line.head, root, folder)? else { continue };
            let path = fsutil::safe_join(root, folder)?;
            let meta = path.symlink_metadata();
            if meta.as_ref().is_ok_and(|m| !m.is_dir()) {
                continue;
            }
            if meta.is_err() && access == CacheAccess::Live && root != working {
                continue; // Move preflight: establish missing caches only at the final location.
            }
            let tree = crate::tree::Tree::open(root)?;
            let wanted = self.provenance(folder, root, working)?;
            let have = tree.open_folder(folder).ok().and_then(|dir| Provenance::read_at(&dir));
            if have.as_ref() == Some(&wanted) {
                continue;
            }
            if access == CacheAccess::Live || !have.as_ref().is_some_and(|have| wanted.fits(have)) {
                self.cold_cache(&cache, working, access, uid, gid)?;
                continue;
            }
            let setting = self.relocation_setting(line, folder);
            // Only a compatible private preparation copy reaches this relocation path.
            let dir = tree.open_folder(folder)?;
            preserve_directory_mtime(&dir, || Ok(rustix::fs::unlinkat(&dir, PROVENANCE, rustix::fs::AtFlags::empty())?))?;
            let outcome = match setting {
                Ok(setting) => {
                    let done = crate::cache_hook::relocate(setting.as_deref(), &path, &wanted.location.target, uid);
                    if !done.log.is_empty() {
                        eprintln!("layr: relocate {}", done.log);
                    }
                    done.outcome
                }
                Err(e) => {
                    eprintln!("layr: relocate {folder}: not relocated: {e:#}");
                    crate::cache_hook::Relocation::Cold
                }
            };
            if let crate::cache_hook::Relocation::Reused { changed } = outcome {
                let dir = tree.open_folder(folder)?;
                publish_relocated(&dir, &wanted, uid, gid, changed)?;
            } else {
                eprintln!("layr: {folder}: relocation failed; starting cold");
                self.cold_cache(&cache, working, access, uid, gid)?;
            }
        }
        Ok(())
    }

    /// Use the settings of the part this line took, including a view-only cache or a fallback
    /// from a lower view. An empty overlay line did not supply the output being relocated.
    fn relocation_setting(&self, line: &Line, folder: &str) -> Result<Option<String>> {
        let path: Vec<String> = self.layers(line)?.into_iter().map(|layer| layer.overlay).collect();
        for name in self.cache_chain(line, &path, folder)? {
            let info = self.need_overlay(&name)?;
            let orepo = self.overlay_repo(&name)?;
            if crate::overlay::find_overlay_line(&orepo.p.db, &info, line)?.is_some_and(|part| part.head != info.root) {
                return Ok(self.overlay_props(&name)?.prune);
            }
        }
        self.folder_overlay_of(folder)?.map(|name| self.overlay_props(&name)).transpose().map(|props| props.and_then(|p| p.prune))
    }

    /// Runs `f` with `limit` as the prune limit of this thread (the background follow pass), then restores it.
    pub fn with_prune_limit<T>(limit: std::time::Duration, f: impl FnOnce() -> T) -> T {
        let old = PRUNE_LIMIT.with(|l| l.replace(limit));
        let out = f();
        PRUNE_LIMIT.with(|l| l.set(old));
        out
    }

    /// The prune limit of this thread (`with_prune_limit`).
    pub fn prune_limit() -> std::time::Duration {
        PRUNE_LIMIT.with(|l| l.get())
    }

    fn prune_folder(&self, line: &Line, overlay: Option<&str>, folder: &str, root: &Path) {
        let (uid, gid) = self.owner_ids(line);
        let setting = match overlay {
            Some(name) => match self.overlay_props(name) {
                Ok(p) => p.prune,
                Err(e) => return eprintln!("layr: prune {folder}: not pruned: {e:#}"),
            },
            None => None,
        };
        let path = match fsutil::safe_join(root, folder) {
            Ok(p) => p,
            Err(e) => return eprintln!("layr: prune {folder}: not pruned: {e:#}"),
        };
        // The line lock is held while the hook runs. The project (store) lock is not: no caller holds it here.
        let _line = match self.prune_lock(line) {
            Ok(f) => f,
            Err(e) => return eprintln!("layr: prune {folder}: not pruned: {e:#}"),
        };
        // A folder that an interrupted step left damaged is repaired first; it stays marked (and saves skip it) until
        // the repair succeeds. A prune alone cannot tell a whole folder from one with missing files.
        if self.damaged_folders(line).is_ok_and(|d| d.contains(folder)) {
            let prepared = (|| -> Result<_> {
                let tree = crate::tree::Tree::open(root)?;
                let dir = tree.open_folder(folder)?;
                let provenance = self.provenance(folder, root, root)?;
                anyhow::ensure!(
                    Provenance::read_at(&dir).as_ref() == Some(&provenance),
                    "missing or incompatible location provenance; output left intact"
                );
                Ok((tree, provenance))
            })();
            let (tree, provenance) = match prepared {
                Ok(prepared) => prepared,
                Err(e) => return eprintln!("layr: repair {folder}: {e:#}"),
            };
            let fixed = crate::cache_hook::with_scratch(Some(self.hook_scratch()), || {
                crate::cache_hook::run_mode(crate::cache_hook::Mode::Repair, setting.as_deref(), &path, uid, Self::prune_limit())
            });
            if !fixed.log.is_empty() {
                eprintln!("layr: repair {}", fixed.log);
            }
            if !fixed.folder_ok {
                if let Some(dir) =
                    tree.open_folder(folder).ok().filter(|dir| Provenance::read_at(dir).as_ref() == Some(&provenance))
                {
                    fsutil::unlink_at(&dir, PROVENANCE);
                }
                return eprintln!("layr: repair {folder} failed; output is unknown and left intact. Stop builds and remove the cache folder, then rerun Layr to initialize cold output before rebuilding");
            }
            // A successful repair built output here, even when the installer replaced its directory.
            let stamped = tree.open_folder(folder).and_then(|dir| provenance.write(&dir, uid, gid));
            if let Err(e) = stamped {
                return eprintln!("layr: repair {folder}: {e:#}; stop builds and remove the cache folder before rebuilding");
            }
        }
        let done = crate::cache_hook::with_scratch(Some(self.hook_scratch()), || {
            crate::cache_hook::run(setting.as_deref(), &path, uid, Self::prune_limit())
        });
        if let Err(e) = self.set_damaged(line, folder, !done.folder_ok) {
            eprintln!("layr: prune {folder}: cannot record the damaged state: {e:#}");
        }
        if !done.log.is_empty() {
            eprintln!("layr: prune {}", done.log);
        }
    }

    /// The folder for the temporary copies of the cache hooks: on the store's volume, outside every work tree.
    fn hook_scratch(&self) -> PathBuf {
        self.p.work_dir().join("cache-hooks")
    }

    /// The lock of one line for the cache hook. The hooks of a line run one at a time. It is the line lock
    /// (`Project::line_lock`): a command that already holds it in this process takes it again without waiting.
    fn prune_lock(&self, line: &Line) -> Result<crate::store::LineLeaseSet> {
        self.p.line_lock(&line.id, &line.name)
    }

    /// The cache folders of `line` that an interrupted hook left damaged. A save skips them: a repair and a prune of
    /// the folder that succeed clear its mark.
    pub fn damaged_folders(&self, line: &Line) -> Result<BTreeSet<String>> {
        match std::fs::read(self.damaged_file(line)) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeSet::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// The mark of a followed line whose caches were saved with no build after its head moved (a local merge):
    /// `caches_behind_head` counts its cache folders, so the next follow pass builds and saves them.
    fn unbuilt_file(&self, line: &Line) -> PathBuf {
        self.p.meta_dir().join("line-locks").join(format!("{}.unbuilt", line.id))
    }

    pub fn set_unbuilt(&self, line: &Line, unbuilt: bool) -> Result<()> {
        let path = self.unbuilt_file(line);
        if unbuilt {
            return crate::fsutil::atomic_write(&path, line.head.as_bytes(), 0o600);
        }
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    fn damaged_file(&self, line: &Line) -> PathBuf {
        self.p.meta_dir().join("line-locks").join(format!("{}.damaged", line.id))
    }

    /// Marks `folder` damaged, or clears its mark. The caller holds the line's prune lock.
    fn set_damaged(&self, line: &Line, folder: &str, damaged: bool) -> Result<()> {
        let mut all = self.damaged_folders(line)?;
        let changed = if damaged { all.insert(folder.to_string()) } else { all.remove(folder) };
        if !changed {
            return Ok(());
        }
        let path = self.damaged_file(line);
        if all.is_empty() {
            let _ = std::fs::remove_file(&path);
            return Ok(());
        }
        crate::fsutil::atomic_write(&path, &serde_json::to_vec(&all)?, 0o600)
    }

    /// Free the caches of the lines whose work is now in `into` (docs/cache-overlays.md, section 22): `sources`,
    /// or every line of this machine when None. A merge took their caches into `into` before, so the caches move
    /// instead of being copied. Returns the names of the freed lines, and a message for each line that an error
    /// kept. A line that is not merged, has unsaved changes or runs a build keeps its caches.
    pub fn free_merged_caches(&self, ctx: &Ctx, into: &Line, sources: Option<Vec<Line>>) -> (Vec<String>, Vec<String>) {
        let (mut freed, mut errors) = (Vec::new(), Vec::new());
        let sources = match sources.map(Ok).unwrap_or_else(|| self.lines()) {
            Ok(lines) => lines,
            Err(e) => return (freed, vec![format!("{e:#}")]),
        };
        for line in sources {
            let name = line.name.clone();
            match self.free_if_merged(ctx, into, line) {
                Ok(true) => freed.push(name),
                Ok(false) => {}
                Err(e) => errors.push(format!("caches of line '{name}' not freed: {e:#}")),
            }
        }
        (freed, errors)
    }

    /// `free_merged_caches` for one line. True when its caches were freed.
    fn free_if_merged(&self, ctx: &Ctx, into: &Line, line: Line) -> Result<bool> {
        let line = self.localize(line)?;
        if line.id == into.id || line.machine != self.p.store.machine.id || self.p.is_overlay() {
            return Ok(false);
        }
        // Only a merge into the line's target frees it: `line.<name>.upstream`, else `main` (docs/review.md). A merge
        // of main into a work line does not free the caches of main.
        let target = match self.p.config_value(&format!("line.{}.upstream", line.name))? {
            Some(serde_json::Value::String(name)) => name,
            _ => "main".to_string(),
        };
        if target != into.name || target == line.name {
            return Ok(false);
        }
        let w = self.working(&line);
        if !w.is_dir() {
            return Ok(false);
        }
        #[cfg(target_os = "macos")]
        if crate::views::of(self, &line).is_some() {
            return Ok(false);
        }
        // A line that lives on keeps its caches: a protected line, or a line that follows a remote branch.
        if self.policy().rule_for(&line.name).is_some() || crate::cmd::follow_engine::target(self, &line)?.is_some() {
            return Ok(false);
        }
        let folders = self.held_cache_folders(&line)?;
        if folders.is_empty() {
            return Ok(false);
        }
        // A line with no save of its own has no work to merge: a new line can build in its folder now.
        if self.p.db.line_states(&line.id, StateKind::Commit)?.is_empty() {
            return Ok(false);
        }
        if self.authorize_line(ctx, &line, crate::repo::LineOp::Manage).is_err() {
            return Ok(false);
        }
        let into = self.line(&into.id)?;
        self.hold(self.pin(&[line.head.as_str(), into.head.as_str()])?);
        if !crate::cmd::mergecmd::changes_contained(self, &into, &line.head)? {
            return Ok(false);
        }
        let crate::store::LockAttempt::Acquired(_line_lock) = self.p.try_line_lock(&line.id, &line.name)? else { return Ok(false) };
        if !crate::cmd::branch::unsaved_paths(self, &line)?.is_empty() {
            return Ok(false);
        }
        for (_, folder) in &folders {
            if build_running(&fsutil::safe_join(&w, folder)?) {
                return Ok(false);
            }
        }
        let _lock = self.p.lock_maintenance()?;
        if self.line(&line.id)?.head != line.head {
            return Ok(false);
        }
        for (name, folder) in &folders {
            // The folder first: when a later step fails, the saved state stays, and the next merge tries again.
            self.p.trash(&fsutil::safe_join(&w, folder)?)?;
            // The line on the overlay goes with its states: the overlay keeps no part for a line without its line
            // there (`history_kept`). `into` keeps what the merge took.
            if self.p.store.project_dir(&format!("{}%{name}", self.p.name))?.exists() {
                let info = self.need_overlay(name)?;
                let orepo = self.overlay_repo(name)?;
                let _olock = orepo.p.lock_maintenance()?;
                if let Some(ol) = crate::overlay::find_overlay_line(&orepo.p.db, &info, &line)? {
                    self.drop_overlay_lines(&_olock, ctx, &orepo, name, &[ol])?;
                }
            }
            self.apply_history(ctx, name)?;
        }
        Ok(true)
    }

    /// The cache overlays of `line` that hold something: files in the folder, or a saved state of the line.
    fn held_cache_folders(&self, line: &Line) -> Result<Vec<(String, String)>> {
        let w = self.working(line);
        let mut out = Vec::new();
        for (name, folder) in self.folder_overlays()? {
            let Some(info) = self.overlay_info(&name)? else { continue };
            // A folder overlay of no platform can hold files that no build makes again.
            if info.platform.is_none() {
                continue;
            }
            let files = std::fs::read_dir(fsutil::safe_join(&w, &folder)?).is_ok_and(|mut d| d.next().is_some());
            let saved = || -> Result<bool> {
                Ok(self.p.store.project_dir(&format!("{}%{name}", self.p.name))?.exists()
                    && crate::overlay::find_overlay_line(&self.overlay_repo(&name)?.p.db, &info, line)?
                        .is_some_and(|ol| ol.head != info.root))
            };
            if (files || saved()?) && self.folder_kind(line, &folder)? == FolderIn::Cache {
                out.push((name, folder));
            }
        }
        Ok(out)
    }
}

/// Whether a build runs in the cache folder `dir`: a process holds the cargo build lock of one of its profile folders
/// (`<dir>/<profile>/.cargo-lock`).
fn build_running(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    entries.flatten().any(|e| {
        let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).open(e.path().join(".cargo-lock")) else {
            return false;
        };
        matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::folder_path;

    fn location_key(workspace: &str, target: &str, mobility: CacheMobility) -> CacheKey {
        let provenance = Provenance { location: BuildLocation::new(Path::new(workspace), target).unwrap(), mobility };
        let parts = BTreeMap::from([("location".into(), serde_json::to_string(&provenance).unwrap())]);
        CacheKey { key: platform_key_of(&parts), parts }
    }

    #[test]
    fn cache_locations_require_both_absolute_workspace_and_target_identity() {
        let key = location_key("/lines/main", "target", CacheMobility::LocationBound);
        assert!(key.fits(Some(&key.key), &key.parts));
        for other in [
            location_key("/lines/new", "target", CacheMobility::LocationBound),
            location_key("/lines/main", "other-target", CacheMobility::LocationBound),
        ] {
            assert_ne!(key.key, other.key);
            assert!(!key.fits(Some(&other.key), &other.parts));
        }
        assert!(!key.fits(Some(&key.key), &BTreeMap::new()), "even an equal legacy key is cold");
        let malformed = BTreeMap::from([("location".into(), "{}".into())]);
        assert!(!key.fits(Some(&key.key), &malformed));
        assert!(BuildLocation::new(Path::new("relative"), "target").is_err());
    }

    #[test]
    fn provenance_resolution_rejects_every_cache_path_symlink() {
        let root = std::env::temp_dir().join(format!("layr-provenance-{}", crate::ids::new_id()));
        std::fs::create_dir_all(root.join("outside/target")).unwrap();
        std::os::unix::fs::symlink(root.join("outside/target"), root.join("target")).unwrap();
        std::os::unix::fs::symlink(root.join("outside"), root.join("linked")).unwrap();
        let tree = crate::tree::Tree::open(&root).unwrap();
        for folder in ["target", "linked/target"] {
            assert!(tree.open_folder(folder).is_err(), "{folder} must not reach the marker writer");
        }
        assert!(!root.join("outside/target").join(PROVENANCE).exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provenance_validation_and_write_keep_the_resolved_descriptor() {
        let root = std::env::temp_dir().join(format!("layr-provenance-{}", crate::ids::new_id()));
        std::fs::create_dir_all(root.join("parent/target")).unwrap();
        std::fs::create_dir_all(root.join("outside/target")).unwrap();
        let tree = crate::tree::Tree::open(&root).unwrap();
        let dir = tree.open_folder("parent/target").unwrap();
        let provenance =
            Provenance { location: BuildLocation::new(&root, "parent/target").unwrap(), mobility: CacheMobility::LocationBound };
        let uid = rustix::process::geteuid().as_raw();
        let gid = rustix::process::getegid().as_raw();
        provenance.write(&dir, uid, gid).unwrap();
        assert_eq!(Provenance::read_at(&dir), Some(provenance.clone()));
        std::fs::rename(root.join("parent"), root.join("original")).unwrap();
        std::os::unix::fs::symlink(root.join("outside"), root.join("parent")).unwrap();
        provenance.write(&dir, uid, gid).unwrap();
        assert_eq!(Provenance::read(&root.join("original/target")), Some(provenance));
        assert!(!root.join("outside/target").join(PROVENANCE).exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_marker_arriving_after_the_missing_check_is_never_replaced() {
        use std::os::unix::fs::MetadataExt;
        let root = std::env::temp_dir().join(format!("layr-marker-race-{}", crate::ids::new_id()));
        std::fs::create_dir_all(root.join("target")).unwrap();
        let dir = crate::tree::Tree::open(&root).unwrap().open_folder("target").unwrap();
        let wanted =
            Provenance { location: BuildLocation::new(&root, "target").unwrap(), mobility: CacheMobility::LocationBound };
        let foreign = Provenance {
            location: BuildLocation::new(Path::new("/foreign"), "target").unwrap(),
            mobility: CacheMobility::LocationBound,
        };
        let uid = rustix::process::geteuid().as_raw();
        let gid = rustix::process::getegid().as_raw();
        assert!(Provenance::read_at(&dir).is_none());
        // A concurrent restore publishes its marker after the caller's check, before the write.
        foreign.write(&dir, uid, gid).unwrap();
        let marker = root.join("target").join(PROVENANCE);
        let bytes = std::fs::read(&marker).unwrap();
        let before = std::fs::metadata(&marker).unwrap();
        assert!(wanted.write(&dir, uid, gid).unwrap_err().to_string().contains("conflicting location provenance"));
        assert_eq!(std::fs::read(&marker).unwrap(), bytes);
        foreign.write(&dir, uid, gid).unwrap();
        let after = std::fs::metadata(&marker).unwrap();
        assert_eq!(before.ino(), after.ino(), "a matching marker is not rewritten");
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        assert_eq!(std::fs::read_dir(root.join("target")).unwrap().count(), 1, "temporary files are removed");
        std::fs::remove_dir_all(root).unwrap();
    }

    // Exercise the real Cargo adapter, including rewrites that restore every output mtime.
    fn check_relocation_save_detection(dep_info: &str, expected_change: bool) -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-relocation-times-{}", ids::new_id()));
        let target = root.join("target");
        let old = root.join("old");
        std::fs::create_dir_all(&old)?;
        for line in [&root, &old] {
            std::fs::write(line.join("Cargo.toml"), "[workspace]\n")?;
        }
        std::fs::create_dir_all(target.join("debug/deps"))?;
        let fingerprint = target.join("debug/.fingerprint/probe-aaa");
        std::fs::create_dir_all(&fingerprint)?;
        let output = target.join("debug/deps/probe-aaa.d");
        let text = format!(
            "{}/target/debug/deps/probe-aaa.d: {}\n",
            old.display(),
            dep_info.replace("$OLD", &old.display().to_string())
        );
        std::fs::write(&output, &text)?;
        let dir = crate::tree::Tree::open(&root)?.open_folder("target")?;
        let provenance = Provenance { location: BuildLocation::new(&root, "target")?, mobility: CacheMobility::Portable };
        let uid = rustix::process::geteuid().as_raw();
        let gid = rustix::process::getegid().as_raw();
        provenance.write(&dir, uid, gid)?;
        for path in [
            &output,
            &fingerprint,
            &target.join("debug/.fingerprint"),
            &target.join("debug/deps"),
            &target.join("debug"),
            &target,
        ] {
            fsutil::set_mtime(&std::fs::File::open(path)?, 10, 123)?;
        }
        assert!(!crate::repo::folder_changed_since(&target, 20_000));
        preserve_directory_mtime(&dir, || Ok(rustix::fs::unlinkat(&dir, PROVENANCE, rustix::fs::AtFlags::empty())?))?;
        let before = std::fs::metadata(&output)?.modified()?;
        let changed = layr_cache::relocate::relocate(&target, &target)? != 0;
        assert_eq!(changed, expected_change);
        assert_eq!(std::fs::metadata(&output)?.modified()?, before, "Cargo's dep-info time stays");
        if dep_info.contains("CARGO_MANIFEST_DIR") {
            assert!(!fingerprint.exists(), "the adapter deleted the foreign fingerprint");
            assert!(crate::repo::folder_changed_since(&target, 20_000));
        } else {
            assert!(fingerprint.exists());
            assert_eq!(std::fs::read_to_string(&output)? != text, expected_change);
            assert!(!crate::repo::folder_changed_since(&target, 20_000), "rewrites retain all output times");
        }
        publish_relocated(&dir, &provenance, uid, gid, changed)?;
        assert_eq!(Provenance::read_at(&dir), Some(provenance));
        assert_eq!(crate::repo::folder_changed_since(&target, 20_000), expected_change);
        // A second relocation at the destination changes nothing.
        assert_eq!(layr_cache::relocate::relocate(&target, &target)?, 0);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn noop_relocation_keeps_inherited_output() -> Result<()> {
        check_relocation_save_detection("src/lib.rs", false)
    }

    #[test]
    fn relocation_rewrites_count_as_changed_output() -> Result<()> {
        check_relocation_save_detection("$OLD/Cargo.toml", true)
    }

    #[test]
    fn relocation_deletions_count_as_changed_output() -> Result<()> {
        check_relocation_save_detection("src/lib.rs\n# env-dep:CARGO_MANIFEST_DIR=$OLD", true)
    }

    #[test]
    fn output_detection_ignores_only_the_exact_root_marker() -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-output-marker-{}", ids::new_id()));
        std::fs::create_dir_all(root.join("nested"))?;
        let old = |path: &Path| fsutil::set_mtime(&std::fs::File::open(path)?, 10, 0);
        old(&root.join("nested"))?;
        std::fs::write(root.join(PROVENANCE), "metadata")?;
        old(&root)?;
        assert!(!crate::repo::folder_changed_since(&root, 20_000));
        for path in [root.join("nested").join(PROVENANCE), root.join(format!("{PROVENANCE}.tmp")), root.join("output")] {
            std::fs::write(&path, "output")?;
            old(path.parent().unwrap())?;
            assert!(crate::repo::folder_changed_since(&root, 20_000), "{} is output", path.display());
            std::fs::remove_file(&path)?;
            old(path.parent().unwrap())?;
        }
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn only_explicit_portability_on_both_sides_allows_another_location() {
        let key = location_key("/work", "target", CacheMobility::Portable);
        let other = location_key("/lines/new", "build", CacheMobility::Portable);
        assert!(key.fits(Some(&other.key), &other.parts));
        let bound = location_key("/work", "target", CacheMobility::LocationBound);
        assert!(!key.fits(Some(&bound.key), &bound.parts));
        assert!(!bound.fits(Some(&key.key), &key.parts));
        assert!(!key.fits(Some(&key.key), &BTreeMap::new()));
        for location in [
            BuildLocation { workspace: "relative".into(), target: "relative/target".into() },
            BuildLocation { workspace: "/work".into(), target: "/foreign/target".into() },
            BuildLocation { workspace: "/work".into(), target: "/work/../foreign".into() },
        ] {
            let malformed = Provenance { location, mobility: CacheMobility::Portable };
            let parts = BTreeMap::from([("location".into(), serde_json::to_string(&malformed).unwrap())]);
            assert!(!key.fits(Some(&key.key), &parts), "portable provenance still requires a valid location");
        }
    }

    #[test]
    fn folder_paths_and_cache_names() {
        assert_eq!(folder_path("target/").unwrap(), "target");
        assert_eq!(folder_path("web/node_modules").unwrap(), "web/node_modules");
        for bad in ["", "/abs", "a/../b", "./x", ".git", ".git/x", "a//b"] {
            assert!(folder_path(bad).is_err(), "{bad}");
        }
        assert_eq!(cache_overlay_name("target", "macos-aarch64"), "target.macos-aarch64");
        assert_eq!(cache_overlay_name("web/node_modules", "linux-x86_64"), "web.node_modules.linux-x86_64");
        let odd = cache_overlay_name("a b/c", "linux-x86_64");
        assert!(odd.starts_with("folder-") && odd.ends_with(".linux-x86_64"), "{odd}");
        let platform = format!("platform={}", crate::model::cache_platform());
        assert_eq!(cache_props().describe(), ["history=last", "quiet", "merge=union-theirs", &platform, "trust=protected"]);
    }

    #[test]
    fn an_unknown_toolchain_is_no_difference() {
        let parts = |t: &str, env: &str| {
            let mut parts = location_key("/work", "target", CacheMobility::LocationBound).parts;
            parts.extend([("toolchain".to_string(), t.to_string()), ("env:CC".to_string(), env.to_string())]);
            parts
        };
        assert!(key_differences(&parts("aa", "x"), &parts("-", "x")).is_empty(), "root without rustc");
        assert_eq!(key_differences(&parts("aa", "x"), &parts("bb", "x")), ["toolchain"]);
        assert_eq!(key_differences(&parts("-", "x"), &parts("-", "y")), ["env:CC"]);
        let ours = CacheKey { key: platform_key_of(&parts("aa", "x")), parts: parts("aa", "x") };
        let base = crate::model::cache_platform();
        assert!(ours.fits(Some(&format!("{base}-0123456789abcdef")), &parts("-", "x")));
        assert!(!ours.fits(Some("plan9-mips-0123456789abcdef"), &parts("aa", "x")), "another platform");
        assert!(!ours.fits(Some(&base), &BTreeMap::new()), "an older state compares its whole key");
    }

    #[test]
    fn the_platform_key_names_the_key_files_and_their_content() {
        let w = std::env::temp_dir().join(format!("layr-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&w);
        std::fs::create_dir_all(&w).unwrap();
        let base = crate::model::cache_platform();
        assert_eq!(platform_key(&w, &[]), base);
        let files = vec!["Cargo.lock".to_string(), "rust-toolchain.toml".to_string()];
        let missing = platform_key(&w, &files);
        assert!(missing.starts_with(&format!("{base}-")) && missing.len() == base.len() + 17);
        std::fs::write(w.join("Cargo.lock"), "a").unwrap();
        let one = platform_key(&w, &files);
        assert_ne!(one, missing, "content counts");
        assert_eq!(one, platform_key(&w, &files), "the key is stable");
        std::fs::write(w.join("Cargo.lock"), "b").unwrap();
        assert_ne!(one, platform_key(&w, &files));
        assert!(!other_platform(Some(&base)));
        assert!(!other_platform(Some(&one)));
        assert!(!other_platform(None), "an old state names no platform");
        assert!(other_platform(Some("plan9-mips")));
        assert!(other_platform(Some(&format!("{base}x-0011"))));
        let _ = std::fs::remove_dir_all(&w);
    }
}
