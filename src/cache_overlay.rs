//! Cache overlays (docs/cache-overlays.md): folder overlays that hold regenerable build output.
//!
//! A folder overlay owns one folder of the tree. In a working folder that folder is a nested layer
//! (a nested subvolume on btrfs, a marked folder on APFS), so scans, snapshots and automatic
//! states of the line never see its files, as for caches. A state of the overlay is one snapshot
//! of that nested layer; a line shows a state as one writable snapshot at the path. Each line has
//! its own overlay line (`line-<line id>`, follow mode): a line has caches, not a state.
//!
//! `save --caches` makes the cache overlay of each cache path when it is missing (properties:
//! quiet, history last, merge union-theirs, readers `*`) and saves a state of it for the line.

use crate::cmd::exit;
use crate::ctx::Ctx;
use crate::fsutil;
use crate::ids;
use crate::ignore_rules::Rules;
use crate::model::{Line, LineChange, Record, StateKind, StateRec};
use crate::overlay::{files_of, MergeKind, Mode, Props};
use crate::repo::Repo;
use crate::storage as btrfs;
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

thread_local! {
    /// The prune limit of this thread: `HOOK_LIMIT` (a command waits for the prune), or a longer one that
    /// `Repo::with_prune_limit` sets for the background follow pass.
    static PRUNE_LIMIT: std::cell::Cell<std::time::Duration> = const { std::cell::Cell::new(crate::cache_hook::HOOK_LIMIT) };
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
        return Err(exit(128, format!("fatal: invalid folder '{s}': use a relative path inside the line")));
    }
    Ok(t.to_string())
}

/// The name of the cache overlay that `save --caches` makes for `folder`: `cache.<folder>` with
/// `/` as `.`, or a hash when the folder has other characters.
pub fn cache_overlay_name(folder: &str) -> String {
    let name = format!("cache.{}", folder.replace('/', "."));
    if crate::store::check_name("overlay", &name).is_ok() {
        return name;
    }
    use layr_hashers::Digest;
    format!("cache.{}", &hex::encode(layr_hashers::ContentAddress::digest(folder.as_bytes()))[..12])
}

/// The properties of a cache overlay (docs/cache-overlays.md, section 2).
pub fn cache_props() -> Props {
    Props { history: Some(1), quiet: true, merge: MergeKind::UnionTheirs, gc_lru: None, prune: None }
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

/// Where a line takes caches from.
pub enum Source<'s> {
    Line(&'s Line),
    State(&'s str),
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

/// The cache key of a line (`Repo::cache_key`): the platform key and its parts.
pub struct CacheKey {
    pub key: String,
    pub parts: BTreeMap<String, String>,
}

impl CacheKey {
    /// Whether a cache state with this platform key and these parts fits: the same platform, and
    /// equal parts (`key_differences`); a state without parts (an older layr) by its whole key.
    pub fn fits(&self, platform: Option<&str>, parts: &BTreeMap<String, String>) -> bool {
        if parts.is_empty() {
            return platform == Some(self.key.as_str());
        }
        let base = crate::model::cache_platform();
        let ours = self.key == base || self.key.starts_with(&format!("{base}-"));
        ours && !other_platform(platform) && platform.is_some() && key_differences(&self.parts, parts).is_empty()
    }
}

/// A state of a cache overlay made on another operating system or processor: a machine never
/// imports it from a layr remote (docs/cache-overlays.md, section 6).
pub fn other_platform(platform: Option<&str>) -> bool {
    let base = crate::model::cache_platform();
    platform.is_some_and(|p| p != base && !p.starts_with(&format!("{base}-")))
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
        let mut parts = file_parts(w, &self.key_files(name)?);
        let folder = self.overlay_setting(name, "folder")?.and_then(|v| v.as_str().map(str::to_owned));
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

    /// The folder overlays of the project: name and folder.
    pub fn folder_overlays(&self) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for name in self.overlay_names()? {
            if let Some(f) = self.overlay_setting(&name, "folder")?.and_then(|v| v.as_str().map(str::to_owned)) {
                out.push((name, f));
            }
        }
        Ok(out)
    }

    pub fn folder_overlay_of(&self, folder: &str) -> Result<Option<String>> {
        Ok(self.folder_overlays()?.into_iter().find(|(_, f)| f == folder).map(|(n, _)| n))
    }

    /// Whether `folder` is a cache path of the working folder `w`: the machine setting names it, or
    /// a `.layrattributes` file marks it regenerable and the ignore rules ignore it. The folder may
    /// be absent (docs/cache-overlays.md, section 3).
    pub fn is_cache_path(&self, folder: &str, w: &Path) -> bool {
        self.p.config_strings("regenerable").iter().any(|p| p == folder)
            || (Rules::attribute(w, "regenerable").ignored(folder, true) && Rules::new(w).ignored(folder, true))
    }

    /// Whether folder overlay `name` takes part in the working folder `w`. A cache overlay only
    /// where its folder is a cache path of the line; other folder overlays always do.
    pub fn applies_in(&self, name: &str, folder: &str, w: &Path) -> Result<bool> {
        let cache = self.overlay_setting(name, "cache")?.and_then(|v| v.as_bool()).unwrap_or(false);
        Ok(!cache || self.is_cache_path(folder, w))
    }

    /// What `folder` is in `line`: a cache path, a cache path that holds versioned files in the head
    /// (not a cache), or no cache path of the line.
    pub fn folder_kind(&self, line: &Line, folder: &str) -> Result<FolderIn> {
        if !self.is_cache_path(folder, &self.working(line)) {
            return Ok(FolderIn::Other);
        }
        let head = self.state_root(&line.head)?;
        Ok(if crate::repo::holds_files(&head, folder) { FolderIn::Versioned } else { FolderIn::Cache })
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
        let _lock = orepo.p.lock()?;
        for ol in orepo.p.db.lines()? {
            let mut rec = Record::new("line.delete");
            rec.lines.insert(
                ol.id.clone(),
                LineChange {
                    before: Some(orepo.line_state(&ol, &ol.head)),
                    after: None,
                    working_before: None,
                    working_after: None,
                },
            );
            rec.data = serde_json::json!({"name": ol.name});
            orepo.commit_record(ctx, rec)?;
        }
        let mut deleted = Vec::new();
        for (id, _) in orepo.p.db.present_states()? {
            if id != root && orepo.p.delete_state(&id)? {
                deleted.push(id);
            }
        }
        if !deleted.is_empty() {
            let mut rec = Record::new("gc");
            rec.data = serde_json::json!({"deleted": deleted, "overlay": name, "removed": true});
            orepo.commit_record(ctx, rec)?;
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
            if !self.overlay_setting(&name, "cache")?.and_then(|v| v.as_bool()).unwrap_or(false) {
                continue;
            }
            // An overlay that is not on this machine has no lines to delete here.
            if !self.p.store.project_dir(&format!("{}%{name}", self.p.name))?.exists() {
                continue;
            }
            let orepo = self.overlay_repo(&name)?;
            let dropped = {
                let _lock = orepo.p.lock()?;
                let orphans = self.orphan_cache_lines(&orepo)?;
                self.drop_overlay_lines(ctx, &orepo, &name, &orphans)?
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

    /// The lines of the cache overlay project `orepo` whose project line is deleted. It only reads.
    pub fn orphan_cache_lines(&self, orepo: &Repo) -> Result<Vec<Line>> {
        let mut orphans = Vec::new();
        for ol in orepo.p.db.lines()? {
            if let Some(id) = crate::overlay::line_of_overlay_line(&ol.name) {
                if self.p.db.line_deleted(id)? {
                    orphans.push(ol);
                }
            }
        }
        Ok(orphans)
    }

    /// Delete these lines of the overlay project `orepo`. Returns the overlay and line names. The
    /// caller holds the locks of the project and of the overlay project.
    pub fn drop_overlay_lines(&self, ctx: &Ctx, orepo: &Repo, name: &str, lines: &[Line]) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for ol in lines {
            let mut rec = Record::new("line.delete");
            rec.lines.insert(
                ol.id.clone(),
                LineChange { before: Some(orepo.line_state(ol, &ol.head)), after: None, working_before: None, working_after: None },
            );
            rec.data = serde_json::json!({"name": ol.name});
            orepo.commit_record(ctx, rec)?;
            out.push((name.to_string(), ol.name.clone()));
        }
        Ok(out)
    }

    /// The cache overlay of `folder`, made when missing. The caller holds the project lock.
    fn ensure_cache_overlay(&self, ctx: &Ctx, folder: &str) -> Result<String> {
        if let Some(n) = self.folder_overlay_of(folder)? {
            self.sync_cache_readers(ctx, &n);
            return Ok(n);
        }
        let name = cache_overlay_name(folder);
        if self.overlay_info(&name)?.is_some() {
            bail!("overlay '{name}' exists and does not hold folder {folder}");
        }
        // The project setting `cache.key-files` gives new cache overlays their key files.
        let key_files = self.p.config_value("cache.key-files")?.filter(|v| v.is_array());
        crate::cmd::overlaycmd::make_overlay(
            ctx,
            self,
            &name,
            &self.cache_readers()?,
            Mode::Follow,
            false,
            &cache_props(),
            Some(folder),
            true,
            key_files.as_ref(),
        )?;
        Ok(name)
    }

    /// The readers of a cache overlay besides its owner: the machines this project trusts (this
    /// machine included), so that store copies and peer sync give caches only to them. Local users
    /// take a cache by the rules of `usable_cache`.
    fn cache_readers(&self) -> Result<Vec<String>> {
        Ok(crate::store_remote::overlays::trusted_keys(self)?.into_keys().map(|m| format!("machine:{m}")).collect())
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
            let want = self.cache_readers()?;
            if want.iter().all(|m| have.contains(m)) || !orepo.is_admin(ctx) {
                return Ok(());
            }
            crate::overlay_managers::of(self, name)?.require(&ctx.store.machine.id)?;
            let info = self.need_overlay(name)?;
            let mut readers: Vec<String> = have.into_iter().filter(|p| *p != format!("user:{}", info.owner)).collect();
            for m in want {
                if !readers.contains(&m) {
                    readers.push(m);
                }
            }
            let _lock = orepo.p.lock()?;
            let mut record = crate::model::Record::new("overlay.readers");
            record.data = serde_json::json!({"config":{"access": crate::cmd::overlaycmd::access_map(info.owner, &readers)}});
            orepo.commit_record(ctx, record).map(|_| ())
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
        ctx: &Ctx,
        line: &Line,
        paths: &[String],
        shared: bool,
        project_state: &str,
    ) -> Result<Vec<(String, String, String)>> {
        self.save_cache_overlays_in(ctx, line, &self.working(line), paths, shared, project_state)
    }

    /// `save_cache_overlays` from the folder `w` instead of the line's working folder (a check copy).
    pub fn save_cache_overlays_in(
        &self,
        ctx: &Ctx,
        line: &Line,
        w: &Path,
        paths: &[String],
        shared: bool,
        project_state: &str,
    ) -> Result<Vec<(String, String, String)>> {
        let mut out = Vec::new();
        for p in paths {
            let cp = fsutil::safe_join(w, p)?;
            if !btrfs::is_subvolume(&cp) {
                continue;
            }
            let name = self.ensure_cache_overlay(ctx, p)?;
            let info = self.need_overlay(&name)?;
            let orepo = self.overlay_repo(&name)?;
            let CacheKey { key, parts } = self.cache_key(ctx, &name, w, line.owner)?;
            let id = ids::new_id();
            {
                let _lock = orepo.p.lock()?;
                let ol = orepo.ensure_overlay_line(
                    ctx,
                    &Repo::overlay_line_name(&info, &line.id, line.owner),
                    &info.root,
                    line.owner,
                )?;
                let dst = orepo.p.state_path(&id);
                let made = btrfs::snapshot(&cp, &dst, true).and_then(|_| btrfs::subvol_info(&dst));
                let sub = match made {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = btrfs::delete_tree(&dst);
                        return Err(e);
                    }
                };
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
                    cache_of: Some(project_state.to_string()),
                };
                if let Err(e) = crate::overlay::move_overlay_line(&orepo, ctx, "commit", &ol, &id, vec![st]) {
                    let _ = btrfs::delete_tree(&dst);
                    return Err(e);
                }
            }
            self.after_overlay_move(ctx, &name);
            out.push((p.clone(), name, id));
        }
        Ok(out)
    }

    /// A state of overlay `name` that a line of `uid` may show: on this machine, saved on this
    /// platform, and made by `uid` or on a protected line (a cache holds code that review does not
    /// see). The empty first state is no cache.
    fn usable_cache(&self, orepo: &Repo, root: &str, id: &str, uid: u32, key: &CacheKey) -> Result<bool> {
        if id == root || !orepo.p.has_state(id) {
            return Ok(false);
        }
        let Some(st) = orepo.p.db.state(id)? else { return Ok(false) };
        let allowed = uid == 0 || st.cache_shared || st.cache_by == Some(uid);
        Ok(allowed && key.fits(st.cache_platform.as_deref(), &st.cache_key_parts))
    }

    /// Says why a line takes no cache of overlay `name` (folder `folder`) from the line `from`: the
    /// newest cache of that line on this platform was made with other key parts (`ours`).
    fn report_key_miss(&self, name: &str, folder: &str, from: &Source, uid: u32, ours: &BTreeMap<String, String>) {
        let Source::Line(l) = from else { return };
        let found = (|| -> Result<Option<(String, BTreeMap<String, String>)>> {
            let info = self.need_overlay(name)?;
            let orepo = self.overlay_repo(name)?;
            let Some(ol) = orepo.p.db.line_by_name(&Repo::overlay_line_name(&info, &l.id, l.owner))? else { return Ok(None) };
            let Some(st) = orepo.p.db.state(&ol.head)? else { return Ok(None) };
            let allowed = uid == 0 || st.cache_shared || st.cache_by == Some(uid);
            let same_platform = st.cache_platform.as_deref().is_some_and(|p| !other_platform(Some(p)));
            Ok((ol.head != info.root && allowed && same_platform && !st.cache_key_parts.is_empty()).then_some((ol.head, st.cache_key_parts)))
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
        let of_line = |line_id: &str| -> Result<Option<String>> {
            let name = crate::overlay::cache_line_name(line_id);
            // A line that no write moved to its platform name yet (`ensure_overlay_line`) has the old name.
            let ol = match orepo.p.db.line_by_name(&name)? {
                Some(ol) => ol,
                None => match orepo.unnamed_platform_line(&name)? {
                    Some(ol) => ol,
                    None => return Ok(None),
                },
            };
            Ok(self.usable_cache(&orepo, &info.root, &ol.head, uid, key)?.then_some(ol.head))
        };
        // The newest usable cache saved with each project state. `save --caches` on a line with
        // no named change saves with an automatic state over the head: that cache counts for the
        // head too.
        let mut saved_with: std::collections::HashMap<String, String> = Default::default();
        // The project states that each usable cache state counts for.
        let mut counts_for: std::collections::HashMap<String, Vec<String>> = Default::default();
        for (id, of) in orepo.p.db.cache_states()? {
            if !self.usable_cache(&orepo, &info.root, &id, uid, key)? {
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
        };
        const SEARCH: usize = 200;
        let mut seen = BTreeSet::new();
        let mut q = std::collections::VecDeque::from([start]);
        while let Some(id) = q.pop_front() {
            if seen.len() >= SEARCH || !seen.insert(id.clone()) {
                continue;
            }
            if let Some(s) = saved_with.get(&id) {
                return Ok(Some(s.clone()));
            }
            let Some(rec) = self.p.db.state(&id)? else { continue };
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
    /// `save --caches` with no named change saves with an automatic state over the head: that counts, unless the
    /// save ran with no build (`set_unbuilt`). A damaged folder is left out: the save refuses it until a prune of it
    /// succeeds.
    pub fn caches_behind_head(&self, line: &Line) -> Result<Vec<String>> {
        let w = self.working(line);
        let damaged = self.damaged_folders(line)?;
        let unbuilt = self.unbuilt_file(line).exists();
        let mut out = Vec::new();
        for (name, folder) in self.folder_overlays()? {
            if damaged.contains(&folder) || !fsutil::safe_join(&w, &folder).is_ok_and(|p| btrfs::is_subvolume(&p)) {
                continue;
            }
            let orepo = self.overlay_repo(&name)?;
            let ol_name = crate::overlay::cache_line_name(&line.id);
            let ol = match orepo.p.db.line_by_name(&ol_name)? {
                Some(ol) => Some(ol),
                None => orepo.unnamed_platform_line(&ol_name)?,
            };
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
                eprintln!("layr: no cache for {folder}: overlay {name} is not on this machine ({e:#}); a trusted reader gets it at the next fetch");
                false
            }
        }
    }

    pub fn take_caches(&self, ctx: &Ctx, line: &Line, start: &str, source: Option<&Line>, w: &Path) -> Result<Vec<String>> {
        let mut taken = Vec::new();
        for (name, folder, sid) in self.place_caches(ctx, line, start, source, w)? {
            let info = self.need_overlay(&name)?;
            let orepo = self.overlay_repo(&name)?;
            let _lock = orepo.p.lock()?;
            orepo.ensure_overlay_line(ctx, &Repo::overlay_line_name(&info, &line.id, line.owner), &sid, line.owner)?;
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
    pub fn copy_caches(&self, ctx: &Ctx, line: &Line, start: &str, w: &Path) -> Result<Vec<String>> {
        let taken: Vec<String> = self.place_caches(ctx, line, start, None, w)?.into_iter().map(|(_, folder, _)| folder).collect();
        if let Err(e) = crate::source_times::apply(w, &taken) {
            eprintln!("warning: source file times of the caches not set: {e:#}");
        }
        Ok(taken)
    }

    /// The writable cache snapshots of `take_caches` in `w`: overlay name, folder and cache state.
    fn place_caches(&self, ctx: &Ctx, line: &Line, start: &str, source: Option<&Line>, w: &Path) -> Result<Vec<(String, String, String)>> {
        let mut taken = Vec::new();
        if self.p.is_overlay() {
            return Ok(taken);
        }
        // The start state (its caches are read) and the source head stay for the command.
        let mut held = vec![start];
        held.extend(source.map(|s| s.head.as_str()));
        self.hold(self.pin(&held)?);
        let ignored = Rules::new(w);
        let (uid, gid) = self.owner_ids(line);
        for (name, folder) in self.folder_overlays()? {
            // Versioned files win: a folder that the start state holds files in comes from the
            // state. Otherwise the cache below replaces what the folder holds now: an old cache
            // of the state (`states/<id>@<folder>`, applied before), else the line would start
            // with that stale copy.
            if !ignored.ignored(&folder, true) || crate::repo::holds_files(&self.state_root(start)?, &folder) {
                continue;
            }
            if !self.applies_in(&name, &folder, w)? {
                continue;
            }
            // A machine that does not read the overlay (no key here yet) starts this folder cold.
            if !self.has_overlay_here(&name, &folder) {
                continue;
            }
            let from = match source {
                Some(s) => Source::Line(s),
                None => Source::State(start),
            };
            let key = self.cache_key(ctx, &name, w, uid)?;
            let Some(sid) = self.cache_source(&name, &from, uid, &key)? else {
                self.report_key_miss(&name, &folder, &from, uid, &key.parts);
                continue;
            };
            let orepo = self.overlay_repo(&name)?;
            orepo.hold(orepo.pin(&[sid.as_str()])?);
            let _t = crate::trace::named(|| format!("take_caches {folder} of line {} from state {sid} (line {})", line.name, line.id));
            let cp = fsutil::safe_join(w, &folder)?;
            if cp.symlink_metadata().is_ok() {
                fsutil::remove_entry(w, &folder)?;
            }
            fsutil::make_parents(w, &folder, Some((uid, gid)))?;
            let uniform = btrfs::cache_snapshot(&orepo.p.state_path(&sid), &cp)?;
            let source_owner = orepo.p.db.state(&sid)?.and_then(|s| s.cache_by);
            crate::repo::give_cache_tree(&cp, uniform, source_owner, uid, gid)?;
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
            Source::State(s) => s.to_string(),
        };
        self.hold(self.pin(&[line.head.as_str(), source_head.as_str()])?);
        let w = self.working(line);
        let (uid, gid) = self.owner_ids(line);
        let own = self.own(line);
        let shared = self.policy().rule_for(&line.name).is_some();
        for (name, folder) in self.folder_overlays()? {
            if skip.contains(&folder) || !self.applies_in(&name, &folder, &w)? || !self.has_overlay_here(&name, &folder) {
                continue;
            }
            let key = self.cache_key(ctx, &name, &w, uid)?;
            let Some(sid) = self.cache_source(&name, from, uid, &key)? else {
                self.report_key_miss(&name, &folder, from, uid, &key.parts);
                continue;
            };
            let info = self.need_overlay(&name)?;
            let orepo = self.overlay_repo(&name)?;
            orepo.hold(orepo.pin(&[sid.as_str()])?);
            let ol_name = Repo::overlay_line_name(&info, &line.id, line.owner);
            let made = {
                let _lock = orepo.p.lock()?;
                let ol = orepo.ensure_overlay_line(ctx, &ol_name, &info.root, line.owner)?;
                if ol.head == sid || orepo.is_ancestor(&sid, &ol.head)? {
                    continue;
                }
                let fast = ol.head == info.root || orepo.is_ancestor(&ol.head, &sid)?;
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
                let cp = fsutil::safe_join(&w, &folder)?;
                let src = orepo.p.state_path(&sid);
                let empty = cp.symlink_metadata().is_err() || (btrfs::is_subvolume(&cp) && files_of(&cp)?.is_empty());
                let r = if crate::repo::holds_files(&self.state_root(&line.head)?, &folder) {
                    Err(exit(1, format!("error: {folder} holds files of the line's head; it is no cache")))
                } else if empty {
                    (|| -> Result<()> {
                        if cp.symlink_metadata().is_ok() {
                            fsutil::remove_entry(&w, &folder)?;
                        }
                        fsutil::make_parents(&w, &folder, Some((uid, gid)))?;
                        btrfs::snapshot(&src, &cp, false)?;
                        crate::repo::give_tree(&cp, uid, gid)
                    })()
                } else {
                    let ours: BTreeSet<String> = files_of(&cp)?.into_iter().collect();
                    crate::overlay::union_ops(&ours, files_of(&src)?).apply(&cp, &src, own)
                };
                if let Err(e) =
                    r.and_then(|_| crate::overlay::move_overlay_line(&orepo, ctx, "merge", &ol, &head, states.clone()))
                {
                    orepo.discard_states(&states);
                    return Err(e);
                }
                head
            };
            self.after_overlay_move(ctx, &name);
            self.relocate_caches(line, std::slice::from_ref(&folder), &w);
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
        let done = crate::cache_hook::run_mode(
            crate::cache_hook::Mode::Warm,
            setting.as_deref(),
            &path,
            uid,
            crate::cache_hook::WARM_LIMIT,
        );
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

    /// The relocation hook for the cache folders `folders` of `line` in `root` (its working folder, or the folder
    /// that becomes it): they came from another path. The adapter of each makes stale the outputs that name the old
    /// path. It never fails: the outcome is one log line on stderr, none when nothing changed.
    pub fn relocate_caches(&self, line: &Line, folders: &[String], root: &Path) {
        let (uid, _) = self.owner_ids(line);
        let _t = crate::trace::named(|| format!("relocate caches of line {}", line.name));
        self.relocate_folders(uid, folders, root, &self.working(line));
    }

    /// The cache folders `folders` of `root`, used below `at` from now on (a line's working folder, or the place
    /// where a check sees its copy): the relocation hook of each, run as `uid`.
    pub fn relocate_folders(&self, uid: u32, folders: &[String], root: &Path, working: &Path) {
        for folder in folders {
            let setting = match self.folder_overlay_of(folder).and_then(|o| o.map(|name| self.overlay_props(&name)).transpose()) {
                Ok(props) => props.and_then(|p| p.prune),
                Err(e) => {
                    eprintln!("layr: relocate {folder}: not relocated: {e:#}");
                    continue;
                }
            };
            let paths = fsutil::safe_join(root, folder).and_then(|p| Ok((p, fsutil::safe_join(working, folder)?)));
            let (path, at) = match paths {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("layr: relocate {folder}: not relocated: {e:#}");
                    continue;
                }
            };
            if !path.is_dir() {
                continue;
            }
            let _t = crate::trace::named(|| format!("relocate {folder}"));
            let done = crate::cache_hook::relocate(setting.as_deref(), &path, &at, uid);
            if !done.log.is_empty() {
                eprintln!("layr: relocate {}", done.log);
            }
        }
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
        let (uid, _) = self.owner_ids(line);
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
        let done = crate::cache_hook::run(setting.as_deref(), &path, uid, Self::prune_limit());
        if let Err(e) = self.set_damaged(line, folder, !done.folder_ok) {
            eprintln!("layr: prune {folder}: cannot record the damaged state: {e:#}");
        }
        if !done.log.is_empty() {
            eprintln!("layr: prune {}", done.log);
        }
    }

    /// The lock of one line for the cache hook. The hooks of a line run one at a time. It is the line lock
    /// (`Project::line_lock`): a command that already holds it in this process takes it again without waiting.
    fn prune_lock(&self, line: &Line) -> Result<crate::store::LineLock> {
        self.p.line_lock(&line.id, &line.name)
    }

    /// The cache folders of `line` that an interrupted hook left damaged. A save skips them: a prune of a folder
    /// that succeeds clears its mark.
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

    /// `gc lru:<size>` of the folder overlays: in the working folder of each line, delete the
    /// files of the folder that were used least recently (the newer of access and change time)
    /// until the folder is at most the size. Returns the deleted bytes.
    pub fn cache_gc_lru(&self) -> Result<u64> {
        let mut freed = 0;
        for (name, folder) in self.folder_overlays()? {
            let Some(limit) = self.overlay_props(&name)?.gc_lru else { continue };
            for line in self.lines()? {
                let w = self.working(&line);
                if !self.applies_in(&name, &folder, &w)? {
                    continue;
                }
                let Ok(cp) = fsutil::safe_join(&w, &folder) else { continue };
                if !btrfs::is_subvolume(&cp) {
                    continue;
                }
                freed += lru_trim(&cp, limit)?;
            }
        }
        Ok(freed)
    }
}

/// Delete the least recently used files below `root` until their sizes add up to `limit` at most.
pub fn lru_trim(root: &Path, limit: u64) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;
    let mut files: Vec<(i64, u64, String)> = Vec::new();
    let mut total = 0u64;
    fsutil::walk(root, "", &mut |rel, m| {
        if !m.is_dir() {
            let used = std::fs::symlink_metadata(root.join(rel)).map(|x| x.atime().max(x.mtime())).unwrap_or(0);
            files.push((used, m.size, rel.to_string()));
            total += m.size;
        }
        Ok(true)
    })?;
    if total <= limit {
        return Ok(0);
    }
    files.sort();
    let mut freed = 0;
    for (_, size, rel) in files {
        if total - freed <= limit {
            break;
        }
        if fsutil::remove_entry(root, &rel).is_ok() {
            freed += size;
        }
    }
    Ok(freed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_paths_and_cache_names() {
        assert_eq!(folder_path("target/").unwrap(), "target");
        assert_eq!(folder_path("web/node_modules").unwrap(), "web/node_modules");
        for bad in ["", "/abs", "a/../b", "./x", ".git", ".git/x", "a//b"] {
            assert!(folder_path(bad).is_err(), "{bad}");
        }
        assert_eq!(cache_overlay_name("target"), "cache.target");
        assert_eq!(cache_overlay_name("web/node_modules"), "cache.web.node_modules");
        let odd = cache_overlay_name("a b/c");
        assert!(odd.starts_with("cache.") && odd.len() == 18, "{odd}");
        assert_eq!(cache_props().describe(), ["history=last", "quiet", "merge=union-theirs"]);
    }

    #[test]
    fn an_unknown_toolchain_is_no_difference() {
        let parts = |t: &str, env: &str| BTreeMap::from([("toolchain".to_string(), t.to_string()), ("env:CC".to_string(), env.to_string())]);
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

    #[test]
    fn lru_trim_deletes_the_oldest_files_first() {
        let root = std::env::temp_dir().join(format!("layr-lru-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("d")).unwrap();
        for (i, name) in ["old", "d/mid", "new"].iter().enumerate() {
            let p = root.join(name);
            std::fs::write(&p, vec![0u8; 100]).unwrap();
            let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000 + i as u64 * 1000);
            let f = std::fs::File::options().write(true).open(&p).unwrap();
            f.set_times(std::fs::FileTimes::new().set_accessed(t).set_modified(t)).unwrap();
        }
        assert_eq!(lru_trim(&root, 300).unwrap(), 0, "under the limit nothing goes");
        assert_eq!(lru_trim(&root, 150).unwrap(), 200);
        assert!(!root.join("old").exists() && !root.join("d/mid").exists() && root.join("new").exists());
        assert_eq!(tree_size(&root), 100);
        let _ = std::fs::remove_dir_all(&root);
    }
}
