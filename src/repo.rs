//! Repository operations on one project: snapshots, lines, staging, status and in-place updates.

use crate::access::{Policy, Role, Rule, Who};
use crate::changes::{self, Change};
use crate::ctx::Ctx;
use crate::db::need;
use crate::fsutil;
#[cfg(target_os = "macos")]
use crate::fsutil::Src;
use crate::ids;
use crate::ignore_rules::Rules;
use crate::model::{Conflict, Line, LineChange, LineState, Record, StateKind, StateRec};
use crate::storage as btrfs;
use crate::store::{check_name, Project};
use crate::tree::Tree;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// A tree that hot paths read: on APFS the layers of a state, elsewhere its folder.
#[cfg(target_os = "macos")]
pub type Snap = crate::thin::Layered;
#[cfg(not(target_os = "macos"))]
pub type Snap = PathBuf;

pub struct Repo<'a> {
    pub p: Project<'a>,
    /// The command that opened this repo. A restore of an evicted state connects with it.
    pub ctx: Option<&'a Ctx>,
}

/// A complete folder as a tree to read.
#[cfg(target_os = "macos")]
pub fn snap(p: &Path) -> Snap {
    crate::thin::Layered::complete(p)
}
#[cfg(not(target_os = "macos"))]
pub fn snap(p: &Path) -> Snap {
    p.to_path_buf()
}

/// True when two trees are the same tree (not only equal content).
#[cfg(target_os = "macos")]
pub fn same_tree(a: &Snap, b: &Snap) -> bool {
    a.top() == b.top()
}
#[cfg(not(target_os = "macos"))]
pub fn same_tree(a: &Snap, b: &Snap) -> bool {
    a == b
}

/// A temporary read-only or writable snapshot, deleted on drop.
pub struct Temp {
    pub path: PathBuf,
}

impl Drop for Temp {
    fn drop(&mut self) {
        crate::store::discard_tree(&self.path);
    }
}

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct Status {
    pub staged: Vec<Change>,
    pub unstaged: Vec<Change>,
    pub untracked: Vec<String>,
    pub untracked_dirs: Vec<String>,
    pub ignored: Vec<String>,
    pub conflicts: Vec<Conflict>,
}

impl Status {
    /// Paths with local changes that an in-place update must not overwrite.
    pub fn local_paths(&self) -> BTreeSet<String> {
        let mut s = BTreeSet::new();
        for c in self.unstaged.iter().chain(self.staged.iter()) {
            s.insert(c.path.clone());
            if let Some(o) = &c.old_path {
                s.insert(o.clone());
            }
        }
        for u in &self.untracked {
            s.insert(u.clone());
        }
        s
    }
    /// Leave out `paths` (the files of quiet overlays) from what the status shows.
    pub fn drop_paths(&mut self, paths: &BTreeSet<String>) {
        if paths.is_empty() {
            return;
        }
        self.staged.retain(|c| !paths.contains(&c.path));
        self.unstaged.retain(|c| !paths.contains(&c.path));
        self.untracked.retain(|p| !paths.contains(p));
        self.conflicts.retain(|c| !paths.contains(&c.path));
    }
    pub fn clean(&self) -> bool {
        self.staged.is_empty() && self.unstaged.is_empty()
    }
}

/// Whether a cache saved on `saved` (None: by an older layr) may warm a line on the platform
/// `local`. `here`: the cache was saved on this machine.
pub fn cache_fits(saved: Option<&str>, local: &str, here: bool) -> bool {
    match saved {
        Some(p) => p == local,
        None => here,
    }
}

/// A write of the working folder that a command made under its line lock, before its commit. It
/// keeps the tree that the folder had (`before`) and the tree written (`after`). Dropped without
/// `commit`, it writes `before` back, so a failed commit leaves the folder as the command found it.
/// The paths of a command were not changed by the user (the command checked them), so the inverse
/// write restores exactly their content.
pub struct WorkingWrite<'a> {
    repo: &'a Repo<'a>,
    wdir: PathBuf,
    before: PathBuf,
    after: PathBuf,
    own: Option<(u32, u32)>,
    view: Option<Temp>,
    done: bool,
}

impl<'a> WorkingWrite<'a> {
    /// `view` is the tree of an overlay view that `after` names (kept for the write back).
    pub fn new(
        repo: &'a Repo<'a>,
        wdir: PathBuf,
        own: Option<(u32, u32)>,
        before: PathBuf,
        after: PathBuf,
        view: Option<Temp>,
    ) -> Self {
        WorkingWrite { repo, wdir, before, after, own, view, done: false }
    }

    /// Keep the write. Returns the overlay view that the write used, if any.
    pub fn commit(mut self) -> Option<Temp> {
        self.done = true;
        self.view.take()
    }
}

impl Drop for WorkingWrite<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let none = BTreeSet::new();
        if let Err(e) = self.repo.apply_delta(&self.after, &self.before, &self.wdir, self.own, &none, true) {
            eprintln!(
                "error: the working folder of {} could not be put back after a refused command: {e:#}; check it with 'layr status'",
                self.wdir.display()
            );
        }
    }
}

/// Delete the states that no record names (a command that failed before its commit).
pub fn discard_unrecorded(repo: &Repo, states: &[StateRec]) {
    let unrecorded: Vec<StateRec> =
        states.iter().filter(|s| repo.p.db.state(&s.id).is_ok_and(|known| known.is_none())).cloned().collect();
    repo.discard_states(&unrecorded);
}

/// The error of a command whose line head moved during the command. It names both heads.
/// `TargetMoved` lets the merge queue retry the entry once.
pub fn line_moved(name: &str, from: &str, to: &str) -> anyhow::Error {
    crate::cmd::exit(
        1,
        format!(
            "error: line {name} moved during the command (head {} -> {}); run it again",
            crate::ids::short(from),
            crate::ids::short(to)
        ),
    )
    .context(crate::cmd::mergecmd::TargetMoved)
}

/// The error of a command whose line was deleted during the command.
pub fn line_gone(name: &str) -> anyhow::Error {
    crate::cmd::exit(1, format!("error: line {name} was deleted during the command; run it again"))
}

pub fn skip_path(p: &str) -> bool {
    p == ".git" || p.starts_with(".git/") || cache_journal_name(p)
}

/// The kinds of change to a line, for `Repo::authorize_line`.
#[derive(Clone, Copy, Debug)]
pub enum LineOp<'a> {
    /// Snapshots of the working content (save, overlay repair): the owner.
    Save,
    /// Commit, reset, add, apply, stash and the other work commands: the owner, and only if the
    /// line is not protected or its rule allows direct changes.
    Direct,
    /// A merge into the line: the owner; for a protected line the rule's update role.
    /// `hint` is the ready command the refusal of a protected line names.
    Merge { expect: bool, hint: Option<&'a str> },
    /// Delete, rename or adopt: the owner, or a maintainer.
    Manage,
    /// Moving the head back: the owner; for a protected line the rule's rewrite role.
    Rewrite,
}

impl<'a> Repo<'a> {
    pub fn open(ctx: &'a Ctx, project: &str) -> Result<Repo<'a>> {
        let p = Project::open(&ctx.store, project)?;
        let r = Repo { p, ctx: Some(ctx) };
        r.can_read(ctx)?;
        Ok(r)
    }

    // ----- permissions (docs/design.md, section 3; src/access.rs) -----

    pub fn policy(&self) -> Policy {
        Policy::load(&self.p)
    }

    pub fn who(ctx: &Ctx) -> Who {
        Who::of(ctx.caller.uid, ctx.caller.gid)
    }

    /// The caller may take a project action (`access::ACTIONS`), or an error that says why not.
    /// Owning a line includes reading the project: the reader actions (`read`, `try`, `export`)
    /// are open to line owners too, unless a deny rule takes them away.
    pub fn require(&self, ctx: &Ctx, action: &str) -> Result<()> {
        if ctx.caller.is_root() {
            return Ok(());
        }
        let pol = self.policy();
        let who = Self::who(ctx);
        match pol.check(&who, action) {
            Ok(()) => Ok(()),
            Err(reason) => {
                let reader_action = crate::access::action_role(action) == Some(Role::Reader);
                // An overlay is read by its readers only: a former reader keeps its overlay lines.
                if reader_action
                    && !self.p.is_overlay()
                    && pol.denied_by(&who, action).is_none()
                    && self.p.db.lines()?.iter().any(|l| l.owner == ctx.caller.uid)
                {
                    return Ok(());
                }
                Err(anyhow::anyhow!("permission denied: {reason}"))
            }
        }
    }

    /// The caller has the admin role (root has).
    pub fn is_admin(&self, ctx: &Ctx) -> bool {
        ctx.caller.is_root() || self.policy().role(&Self::who(ctx)) == Role::Admin
    }

    /// Reading needs the reader role, or owning a line of the project (unless a deny rule takes
    /// `read` away).
    pub fn can_read(&self, ctx: &Ctx) -> Result<()> {
        if ctx.caller.is_root() {
            return Ok(());
        }
        let pol = self.policy();
        let who = Self::who(ctx);
        if pol.check(&who, "read").is_ok() {
            return Ok(());
        }
        if !self.p.is_overlay()
            && pol.denied_by(&who, "read").is_none()
            && self.p.db.lines()?.iter().any(|l| l.owner == ctx.caller.uid)
        {
            return Ok(());
        }
        bail!("permission denied: user {} may not read project {}", ctx.caller.user, self.p.name)
    }

    /// Whether the caller may make this kind of change to a line. Returns the line's protection
    /// rule. Only the owner changes a line; a protected line changes through merges by the role
    /// its rule names; moving a protected line back needs the rule's rewrite role.
    pub fn authorize_line(&self, ctx: &Ctx, line: &Line, op: LineOp<'_>) -> Result<Option<Rule>> {
        let pol = self.policy();
        let rule = pol.rule_for(&line.name);
        if ctx.caller.is_root() {
            return Ok(rule);
        }
        let name = &line.name;
        if line.owner == 0 {
            bail!("permission denied: only root changes the root-owned line '{name}'");
        }
        let who = Self::who(ctx);
        let role = pol.role(&who);
        let owner = ctx.caller.uid == line.owner;
        let not_owner = || anyhow::anyhow!("permission denied: line '{name}' belongs to another user");
        match op {
            LineOp::Save => {
                if !owner {
                    return Err(not_owner());
                }
            }
            LineOp::Direct => {
                if rule.as_ref().map(|r| !r.direct).unwrap_or(false) {
                    bail!("line '{name}' is protected: it changes only through 'layr merge <line> --expect <state>'");
                }
                if !owner {
                    return Err(not_owner());
                }
            }
            LineOp::Merge { expect, hint } => match &rule {
                None if !owner => return Err(not_owner()),
                None => {}
                Some(r) => {
                    pol.check(&who, "merge").map_err(|e| anyhow::anyhow!("permission denied: {e}"))?;
                    if role < r.update {
                        bail!(
                            "permission denied: merging into the protected line '{name}' needs the role {}; you are {}",
                            r.update.name(),
                            role.name()
                        );
                    }
                    if r.expect && !expect {
                        let hint = hint.map(|h| format!(" ({h})")).unwrap_or_default();
                        bail!("the protected line '{name}' takes only a reviewed state: add --expect <state>{hint}");
                    }
                }
            },
            LineOp::Manage => {
                if !(owner && rule.is_none()) {
                    self.require(ctx, "line.manage")?;
                }
                if let Some(r) = &rule {
                    if role < r.rewrite {
                        bail!(
                            "permission denied: this change of the protected line '{name}' needs the role {}",
                            r.rewrite.name()
                        );
                    }
                }
            }
            LineOp::Rewrite => match &rule {
                None if !owner => return Err(not_owner()),
                None => {}
                Some(r) if role < r.rewrite => {
                    bail!(
                        "permission denied: moving the protected line '{name}' back needs the role {}; you are {}",
                        r.rewrite.name(),
                        role.name()
                    );
                }
                Some(_) => {}
            },
        }
        Ok(rule)
    }

    /// A move of a line's head that is not forward (reset, amend, undo of a merge) rewrites the
    /// line's history.
    pub fn check_move(&self, ctx: &Ctx, line: &Line, new_head: &str) -> Result<()> {
        if new_head == line.head || self.is_ancestor(&line.head, new_head)? {
            return Ok(());
        }
        self.authorize_line(ctx, line, LineOp::Rewrite).map(|_| ())
    }

    // ----- lookups -----

    pub fn line(&self, name: &str) -> Result<Line> {
        if let Some(l) = self.p.db.line_by_name(name)? {
            return self.localize(l);
        }
        if let Some(l) = self.p.db.line_by_id(name)? {
            return self.localize(l);
        }
        bail!("line '{name}' not found")
    }

    /// A line continued on several machines has one head per machine. On a machine with a
    /// working folder of the line, its own head is the head; the others are `<line>@<machine>`.
    pub fn localize(&self, mut l: Line) -> Result<Line> {
        let me = &self.p.store.machine.id;
        if l.machine == *me {
            return Ok(l);
        }
        if let Some((_, h)) = self.p.db.line_heads(&l.id)?.into_iter().find(|(m, _)| m == me) {
            if self.working(&l).exists() {
                l.head = h;
                l.machine = me.clone();
            }
        }
        Ok(l)
    }

    pub fn lines(&self) -> Result<Vec<Line>> {
        self.p.db.lines()?.into_iter().map(|l| self.localize(l)).collect()
    }

    pub fn state(&self, id: &str) -> Result<StateRec> {
        need(self.p.db.state(id)?, &format!("state {}", ids::short(id)))
    }

    /// Source history cannot contain cache or derived states.
    pub fn source_state(&self, id: &str) -> Result<StateRec> {
        let state = self.state(id)?;
        if state.kind.auxiliary() {
            bail!(
                "state {} is {}; cache and derived states are outside source history, diff, merge and Git export",
                ids::short(id),
                crate::db::kind_str(state.kind)
            );
        }
        Ok(state)
    }

    /// The complete tree of a state. On APFS a thin state gets a view (`thin::complete_tree`);
    /// commands that read few paths use `state_src` instead.
    pub fn state_root(&self, id: &str) -> Result<PathBuf> {
        self.check_state(id)?;
        #[cfg(target_os = "macos")]
        return crate::thin::complete_tree(&self.p, id);
        #[cfg(not(target_os = "macos"))]
        Ok(self.p.state_path(id))
    }

    /// Every command that reads the tree of a state comes here (`state_root`, `state_src`). A
    /// state that retention evicted comes back from a store remote that holds it.
    fn check_state(&self, id: &str) -> Result<()> {
        if self.p.has_state(id) {
            return Ok(());
        }
        // An imported state is built from git (docs/ownership.md, "States imported from Git"). A
        // copy on a store remote is the fallback when git cannot give the objects.
        if crate::gitbridge::buildable(self, id) {
            return crate::gitbridge::build_tree(self, id, None).or_else(|e| {
                crate::store_remote::restore_state(self, id)
                    .map_err(|_| e.context(format!("cannot build state {} from Git", ids::short(id))))
            });
        }
        match self.p.db.state_flags(id)? {
            Some((_, true)) => bail!("state {} was freed: no line, ref, tag or undo record keeps it", ids::short(id)),
            Some(_) => crate::store_remote::restore_state(self, id).map_err(|e| {
                if crate::gitbridge::missing_objects(self, id) {
                    e.context(format!(
                        "state {} is an imported Git commit whose Git objects are not on this machine; fetch from a store remote that holds its Git pack",
                        ids::short(id)
                    ))
                } else {
                    e
                }
            })?,
            None => bail!("unknown state {}", ids::short(id)),
        }
        Ok(())
    }

    /// A state to read without a complete copy: on APFS its layers, elsewhere its folder.
    /// Pin saved states that a command reads (`free::pin`): they stay on disk until the guard goes.
    /// Fails clearly for a state that another command freed (read it again).
    pub fn pin(&self, states: &[&str]) -> Result<crate::free::Pin> {
        crate::free::pin(&self.p, states)
    }

    /// Keep pins until the command ends (`free::hold`).
    pub fn hold(&self, pin: crate::free::Pin) {
        crate::free::hold(pin)
    }

    /// A state to read without a complete copy: on APFS its layers, elsewhere its folder.
    pub fn state_src(&self, id: &str) -> Result<Snap> {
        self.check_state(id)?;
        #[cfg(target_os = "macos")]
        return crate::thin::Layered::of_state(&self.p, id);
        #[cfg(not(target_os = "macos"))]
        Ok(self.p.state_path(id))
    }

    /// The head of a line to read (`state_src`). A line with overlays reads its base.
    pub fn head_src(&self, line: &Line) -> Result<Snap> {
        let layers = if self.p.is_overlay() { Vec::new() } else { self.layers(line)? };
        if layers.is_empty() {
            return self.state_src(&line.head);
        }
        let base = self.head_tree(line)?;
        #[cfg(target_os = "macos")]
        return Ok(crate::thin::Layered::complete(&base));
        #[cfg(not(target_os = "macos"))]
        Ok(base)
    }

    /// A complete folder of a tree. A layered tree gets a temporary read-only copy in `keep`.
    pub fn complete(&self, tree: Snap, keep: &mut Vec<Temp>) -> Result<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            if tree.is_complete() {
                return Ok(tree.bottom().to_path_buf());
            }
            let t = Temp { path: self.p.scan_dir().join(crate::store::temp_name()) };
            tree.materialize(&t.path, true)?;
            btrfs::write_info(
                &t.path,
                &btrfs::SubvolInfo { uuid: ids::new_id(), received_uuid: None, parent_uuid: None, readonly: true, base: None },
            )?;
            btrfs::seal(&t.path, true, false)?;
            let p = t.path.clone();
            keep.push(t);
            Ok(p)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = keep;
            Ok(tree)
        }
    }

    /// Copy a state to `dst`: a writable folder, or a read-only snapshot.
    pub fn snapshot_state(&self, id: &str, dst: &Path, readonly: bool) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let src = self.state_src(id)?;
            if src.is_complete() {
                return btrfs::snapshot(src.bottom(), dst, readonly);
            }
            let parent = btrfs::subvol_info(src.top())?.uuid;
            src.materialize(dst, readonly)?;
            let marked = (|| -> Result<()> {
                let info = btrfs::SubvolInfo {
                    uuid: ids::new_id(),
                    received_uuid: None,
                    parent_uuid: Some(parent),
                    readonly,
                    base: None,
                };
                btrfs::write_info(dst, &info)?;
                if readonly {
                    btrfs::seal(dst, true, false)?;
                }
                Ok(())
            })();
            if marked.is_err() {
                let _ = btrfs::delete_tree(dst);
            }
            marked
        }
        #[cfg(not(target_os = "macos"))]
        btrfs::snapshot(&self.state_root(id)?, dst, readonly)
    }

    /// The working folder of a line: its recorded path (`layr line move`), else `lines/<name>`.
    pub fn working(&self, line: &Line) -> PathBuf {
        self.p.moved_path(&line.id).unwrap_or_else(|| self.p.default_line_path(&line.name))
    }

    pub fn owner_ids(&self, line: &Line) -> (u32, u32) {
        let c = crate::ctx::Caller::from_uid(line.owner);
        (line.owner, c.gid)
    }

    pub fn own(&self, line: &Line) -> Option<(u32, u32)> {
        Some(self.owner_ids(line))
    }

    pub fn line_state(&self, line: &Line, head: &str) -> LineState {
        LineState { name: line.name.clone(), head: head.to_string(), owner: line.owner, machine: line.machine.clone() }
    }

    // ----- snapshots -----

    /// Snapshot `src` into a new state folder (read-only). The record is not written yet.
    pub fn new_state(
        &self,
        ctx: &Ctx,
        src: &Path,
        kind: StateKind,
        parents: Vec<String>,
        message: &str,
        line: Option<&Line>,
    ) -> Result<StateRec> {
        let id = ids::new_id();
        let dst = self.p.state_path(&id);
        if let Err(e) = btrfs::snapshot(src, &dst, true) {
            let _ = btrfs::delete_tree(&dst);
            return Err(e);
        }
        let made = self.describe_state(ctx, id, kind, parents, message, line);
        if made.is_err() {
            let _ = btrfs::delete_tree(&dst);
        }
        made
    }

    /// A new state with the content of `base`: on APFS an empty thin layer over it, elsewhere a
    /// snapshot. The record is not written yet.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn same_state(
        &self,
        ctx: &Ctx,
        base: &str,
        kind: StateKind,
        parents: Vec<String>,
        message: &str,
        line: Option<&Line>,
    ) -> Result<StateRec> {
        // Overlay projects read their states as folders: they keep complete trees.
        #[cfg(target_os = "macos")]
        if !self.p.is_overlay() {
            let id = ids::new_id();
            let src = self.state_src(base)?;
            let dst = self.p.delta_path(&id);
            crate::thin::write_layer(&src.at("")?, &dst, &crate::thin::Diff::default(), base, false, None)?;
            self.p.db.record_base(&id, base)?;
            return self.describe_state(ctx, id, kind, parents, message, line);
        }
        self.new_state(ctx, &self.state_root(base)?, kind, parents, message, line)
    }

    /// Describe an already prepared immutable tree. No record is written here.
    pub fn describe_state(
        &self,
        ctx: &Ctx,
        id: String,
        kind: StateKind,
        parents: Vec<String>,
        message: &str,
        line: Option<&Line>,
    ) -> Result<StateRec> {
        let mut dst = self.p.state_path(&id);
        if !dst.exists() && self.p.delta_path(&id).exists() {
            dst = self.p.delta_path(&id);
        }
        let info = btrfs::subvol_info(&dst)?;
        Ok(self.state_record(ctx, id, kind, parents, message, line, info.uuid))
    }

    /// The record of a state whose snapshot has the UUID `subvol`. Empty `subvol`: a state with no
    /// tree on this machine (an imported state until `gitbridge::build_tree`).
    #[allow(clippy::too_many_arguments)]
    pub fn state_record(
        &self,
        ctx: &Ctx,
        id: String,
        kind: StateKind,
        parents: Vec<String>,
        message: &str,
        line: Option<&Line>,
        subvol: String,
    ) -> StateRec {
        StateRec {
            id,
            kind,
            change_id: (kind == StateKind::Commit).then(ids::new_id),
            derived_from: if kind.auxiliary() { parents.first().cloned() } else { None },
            context: None,
            parents: if kind.auxiliary() { Vec::new() } else { parents },
            line: if kind.auxiliary() { None } else { line.map(|l| l.id.clone()) },
            subvol,
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
            cache_key_parts: Default::default(),
        }
    }

    /// True when the tree of a state can be read now or built from git (`gitbridge::buildable`).
    pub fn tree_available(&self, id: &str) -> bool {
        self.p.has_state(id) || crate::gitbridge::buildable(self, id)
    }

    /// An automatic state of a line's working content. For a line with overlays the snapshot
    /// leaves the overlay files out (a writable snapshot, the files removed, read-only): no state
    /// of the project holds a file of an overlay. The caller holds the project lock.
    pub fn working_snapshot(&self, ctx: &Ctx, line: &Line, parents: Vec<String>, message: &str) -> Result<StateRec> {
        self.working_snapshot_over(ctx, line, parents, message, &line.head)
    }

    /// `working_snapshot` with its thin layer over `base` (APFS): over a state close to the
    /// working content, only the few entries that differ are copied.
    pub fn working_snapshot_over(
        &self,
        ctx: &Ctx,
        line: &Line,
        parents: Vec<String>,
        message: &str,
        base: &str,
    ) -> Result<StateRec> {
        let w = self.working(line);
        recover_cache_replacements(&w)?;
        self.restore_caches(line)?;
        let private = self.private_paths(line)?;
        #[cfg(not(target_os = "macos"))]
        let _ = base;
        #[cfg(target_os = "macos")]
        if private.is_empty() && self.thin_line(line)? {
            // A thin layer over the base: only the entries that changed are copied.
            let id = ids::new_id();
            let base = if self.p.has_state(base) { base } else { line.head.as_str() };
            let head = self.state_src(base)?;
            let dst = self.p.delta_path(&id);
            match crate::thin::snapshot_over(&head, base, &w, &dst, false) {
                Ok(Some(_)) => {
                    self.p.db.record_base(&id, base)?;
                    return self.describe_state(ctx, id, StateKind::Auto, parents, message, Some(line));
                }
                Ok(None) => {
                    crate::thin::write_layer(&head.at("")?, &dst, &crate::thin::Diff::default(), base, false, None)?;
                    self.p.db.record_base(&id, base)?;
                    return self.describe_state(ctx, id, StateKind::Auto, parents, message, Some(line));
                }
                // Hard links need a complete tree.
                Err(e) if crate::thin::needs_complete(&e) => {}
                Err(e) => return Err(e),
            }
        }
        if private.is_empty() {
            return self.new_state(ctx, &w, StateKind::Auto, parents, message, Some(line));
        }
        let id = ids::new_id();
        let dst = self.p.state_path(&id);
        btrfs::snapshot(&w, &dst, false)?;
        let made = (|| -> Result<btrfs::SubvolInfo> {
            let t = Tree::open(&dst)?;
            for p in &private {
                if t.stat(p).is_some() {
                    t.remove(p)?;
                }
            }
            drop(t);
            btrfs::set_readonly(&dst)?;
            // An overlay file under another name leaves too: moved from an overlay path, or a
            // new file with the content of an overlay file (a copy, an editor's extra copy).
            let copies = self.private_copies(line, &dst, &private)?;
            if !copies.is_empty() {
                btrfs::set_writable(&dst)?;
                let t = Tree::open(&dst)?;
                for p in &copies {
                    t.remove(p)?;
                }
                drop(t);
                btrfs::set_readonly(&dst)?;
            }
            btrfs::subvol_info(&dst)
        })();
        let info = match made {
            Ok(i) => i,
            Err(e) => {
                let _ = btrfs::delete_tree(&dst);
                return Err(e);
            }
        };
        Ok(StateRec {
            id,
            kind: StateKind::Auto,
            change_id: None,
            derived_from: None,
            context: None,
            parents,
            line: Some(line.id.clone()),
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
            cache_key_parts: Default::default(),
        })
    }

    /// True when the states of a line can be thin layers: APFS, no overlays.
    #[cfg(target_os = "macos")]
    fn thin_line(&self, line: &Line) -> Result<bool> {
        Ok(!self.p.is_overlay() && self.layers(line)?.is_empty())
    }

    /// Files of a snapshot of the working folder that hold an overlay file under another name:
    /// moved from a path in `private`, or new with the same content as one of them.
    fn private_copies(&self, line: &Line, snap: &Path, private: &BTreeSet<String>) -> Result<Vec<String>> {
        let base = self.head_tree(line)?;
        let mut by_size: std::collections::HashMap<u64, Vec<&String>> = std::collections::HashMap::new();
        for p in private {
            if let Some(m) = fsutil::lmeta(&base, p).filter(|m| !m.is_dir()) {
                by_size.entry(m.size).or_default().push(p);
            }
        }
        let mut out = Vec::new();
        for c in changes::file_changes(&base, snap)? {
            let n = match &c.new {
                Some(n) => n,
                None => continue,
            };
            if c.old_path.as_ref().map(|o| private.contains(o)).unwrap_or(false) {
                out.push(c.path.clone());
                continue;
            }
            if c.old.is_none() {
                for p in by_size.get(&n.size).into_iter().flatten() {
                    if fsutil::same_entry(&base, p, snap, &c.path, None)? {
                        out.push(c.path.clone());
                        break;
                    }
                }
            }
        }
        Ok(out)
    }

    /// Remove state snapshots that were made for a record that was not written.
    pub fn discard_states(&self, states: &[StateRec]) {
        for s in states {
            #[cfg(target_os = "macos")]
            if crate::apfs_commit::preserves(&self.p, &s.id).unwrap_or(true) {
                // The journal owns rollback of the only staged copy.
                continue;
            }
            let _ = self.p.delete_state(&s.id);
        }
    }

    /// Read authenticated records already accepted into the local database.
    pub fn read_model(&self) -> Result<layr_model::Project> {
        let records = self
            .p
            .db
            .all_lines()?
            .iter()
            .map(|line| crate::oplog::parse_line(line, None).map(|s| s.record))
            .collect::<Result<Vec<_>>>()?;
        layr_model::Project::from_records(&self.p.id, records, &self.p.store.machine.id)
    }

    /// Append a completed named transfer through the normal signed record path.
    pub fn record_transfer(&self, ctx: &Ctx, op: &str, remote: &str, url: &str, transfer: &crate::model::Transfer) -> Result<()> {
        let _lock = self.p.lock()?;
        let mut rec = Record::new(op);
        let mut transfer = transfer.clone();
        if !self.p.is_overlay() {
            transfer.overlays = transfer
                .overlays
                .into_iter()
                .filter(|name| self.overlay_info(name).ok().flatten().is_some_and(|info| !info.hidden))
                .collect();
        }
        rec.data = serde_json::to_value(transfer)?;
        rec.data["remote"] = serde_json::json!(remote);
        rec.data["url"] = serde_json::json!(layr_model::operations::public_url(url));
        rec.data["result"] = serde_json::json!("success");
        if let Some((_, overlay)) = self.p.name.split_once('%') {
            rec.data["overlay"] = serde_json::json!(overlay);
        }
        self.commit_record(ctx, rec)?;
        Ok(())
    }

    /// Write a record. On failure, delete the states it names.
    pub fn commit_record(&self, ctx: &Ctx, rec: Record) -> Result<Record> {
        let states = rec.states.clone();
        match self.p.append(rec, &ctx.actor()) {
            Ok(r) => Ok(r),
            Err(e) => {
                // The record and its rows are one transaction, but COMMIT can fail after the commit
                // point (a full disk). A state that the database names is recorded: keep it.
                let unrecorded: Vec<StateRec> =
                    states.into_iter().filter(|s| self.p.db.state(&s.id).is_ok_and(|known| known.is_none())).collect();
                self.discard_states(&unrecorded);
                Err(e)
            }
        }
    }

    pub fn temp_snapshot(&self, src: &Path, readonly: bool) -> Result<Temp> {
        let dst = self.p.scan_dir().join(crate::store::temp_name());
        btrfs::snapshot(src, &dst, readonly)?;
        Ok(Temp { path: dst })
    }

    pub fn temp_work(&self, src: &Path) -> Result<Temp> {
        let dst = self.p.work_dir().join(crate::store::temp_name());
        btrfs::snapshot(src, &dst, false)?;
        Ok(Temp { path: dst })
    }

    /// Read-only snapshot of a line's working content.
    pub fn scan(&self, line: &Line) -> Result<Temp> {
        let w = self.working(line);
        if !w.exists() {
            bail!("line '{}' has no working folder on this machine", line.name);
        }
        let dst = self.p.scan_dir().join(crate::store::temp_name());
        btrfs::scan_snapshot(&w, &dst)?;
        Ok(Temp { path: dst })
    }

    /// `scan` limited to the paths that `specs` name (APFS): a restore of a few files then
    /// copies those files, not the whole working folder (seconds to minutes in a large line,
    /// under the store lock). A pattern or the whole folder takes the complete scan.
    #[cfg(target_os = "macos")]
    pub fn scan_matching(&self, line: &Line, specs: &[String]) -> Result<Temp> {
        if specs.iter().any(|s| s.is_empty() || s.contains(['*', '?', '['])) {
            return self.scan(line);
        }
        let w = self.working(line);
        let mut paths = BTreeSet::new();
        for spec in specs {
            let Ok(root) = fsutil::safe_join(&w, spec) else { continue };
            let Ok(m) = std::fs::symlink_metadata(&root) else { continue };
            paths.insert(spec.clone());
            if m.is_dir() {
                let mut stack = vec![spec.clone()];
                while let Some(dir) = stack.pop() {
                    for e in std::fs::read_dir(w.join(&dir))? {
                        let e = e?;
                        let rel = format!("{dir}/{}", e.file_name().to_string_lossy());
                        if e.file_type()?.is_dir() {
                            stack.push(rel.clone());
                        }
                        paths.insert(rel);
                    }
                }
            }
        }
        let dst = self.p.scan_dir().join(crate::store::temp_name());
        btrfs::create_subvolume(&dst)?;
        let t = Temp { path: dst };
        btrfs::copy_selected(&w, &t.path, &paths)?;
        Ok(t)
    }

    /// The newest automatic state of a line, if any.
    pub fn last_auto(&self, line: &Line) -> Result<Option<StateRec>> {
        Ok(self.p.db.line_states(&line.id, StateKind::Auto)?.into_iter().find(|s| self.p.has_state(&s.id)))
    }

    /// The state that a save compares the working content with: the newest automatic state made
    /// over the line's current head, else the head. An automatic state over an earlier head is
    /// older than the head and no base for "changed since".
    pub fn save_base(&self, line: &Line) -> Result<String> {
        let auto = self.last_auto(line)?.filter(|s| s.parents.contains(&line.head));
        Ok(auto.map(|s| s.id).unwrap_or_else(|| line.head.clone()))
    }

    /// The id of a state equal to the working content: a new automatic state, or the previous
    /// automatic state (or head) when nothing changed. The caller holds the project lock.
    /// `line delete`, before the project lock: the automatic state of the working folder, made as
    /// far as possible without the lock (issue #194). On APFS its layer goes to a new id and its
    /// complete tree is made here, since the final state of a deleted line keeps no layer. None when
    /// the folder has nothing to save, and always off APFS (the lock then makes the state as before).
    pub fn prepare_final_state(&self, line: &Line) -> Result<Option<FinalPrepared>> {
        self.prepare_save(line)?;
        #[cfg(target_os = "macos")]
        {
            let Some(pre) = take_prepared_save(&line.id) else { return Ok(None) };
            let id = ids::new_id();
            if let Err(e) = btrfs::move_tree(&pre.path, &self.p.delta_path(&id)) {
                let _ = self.p.trash(&pre.path);
                return Err(e);
            }
            if let Err(e) = crate::thin::complete_tree(&self.p, &id) {
                let _ = self.p.delete_state(&id);
                return Err(e);
            }
            return Ok(Some(FinalPrepared { id, head: pre.head, scan: pre.scan }));
        }
        #[cfg(not(target_os = "macos"))]
        Ok(None)
    }

    /// `line delete`, under the project lock: records the prepared state when the line head is the
    /// one it was made from. When the working folder changed since (a build writes into it), the
    /// prepared state stays as an automatic state and the state under the lock copies only what
    /// changed since it, as a named save does. A moved head discards the state, and the caller
    /// prepares again. Without a prepared state, the lock makes the working state as before.
    pub fn record_final_state(&self, ctx: &Ctx, line: &Line, prepared: &mut Option<FinalPrepared>) -> Result<FinalState> {
        #[cfg(target_os = "macos")]
        if let Some(pre) = prepared.take() {
            let w = self.working(line);
            if line.head != pre.head || !w.exists() {
                self.p.delete_state(&pre.id)?;
                return Ok(FinalState::Moved);
            }
            let changed = btrfs::scan_with_data(&w, true)? != pre.scan;
            let mut st = self.describe_state(ctx, pre.id.clone(), StateKind::Auto, vec![line.head.clone()], "before line delete", Some(line))?;
            st.running = crate::fsutil::take_unsettled();
            let before = self.recorded_working(line)?;
            let mut rec = Record::new("save");
            rec.states.push(st.clone());
            rec.data = serde_json::json!({"line": line.id, "head": line.head, "working_before": before, "working_after": st.id});
            self.commit_record(ctx, rec)?;
            if !changed {
                if let Err(e) = self.save_cache_states(ctx, line, &st.id) {
                    eprintln!("warning: cache overlay of line {} not saved: {e:#}", line.name);
                }
                clear_regenerable_memo();
                return Ok(FinalState::Recorded(Some(st.id)));
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = prepared;
        Ok(FinalState::Recorded(self.working_state(ctx, line, "before line delete")?))
    }

    /// Discards a prepared state that is not recorded (a failed command, or moved inputs). Under the
    /// project lock the delete is a rename; the state would otherwise stay as an unowned leak.
    pub fn discard_final_state(&self, prepared: Option<FinalPrepared>) {
        #[cfg(target_os = "macos")]
        if let Some(pre) = prepared {
            let _ = self.p.delete_state(&pre.id);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = prepared;
    }

    pub fn working_state(&self, ctx: &Ctx, line: &Line, message: &str) -> Result<Option<String>> {
        if !self.working(line).exists() {
            return Ok(None);
        }
        Ok(match self.save(ctx, line, message, false, false)? {
            Some(s) => Some(s.id),
            None => Some(self.save_base(line)?),
        })
    }

    /// The state that the layer of the next automatic state goes over (APFS): the previous
    /// automatic state over this head, so a save copies only what changed since that save, not
    /// every change since the head (a whole node_modules at each save took minutes). A chain of
    /// `thin.chain` layers starts again over the head.
    #[cfg(target_os = "macos")]
    fn save_layer_base(&self, line: &Line, prev: &str) -> String {
        let max = self.p.config_u64("thin.chain", crate::thin::MAX_CHAIN) as usize;
        if prev != line.head && self.p.delta_path(prev).is_dir() && crate::thin::chain_len(&self.p, prev) < max {
            prev.to_string()
        } else {
            line.head.clone()
        }
    }

    /// The time in seconds since which `base` equals the working folder in extended attributes: the time of
    /// an automatic state of this line that a layer chain continues over (`save_layer_base`);
    /// 0 for a head, which may come from elsewhere.
    #[cfg(target_os = "macos")]
    fn since_of(&self, line: &Line, base_id: &str) -> i64 {
        if base_id == line.head {
            return 0;
        }
        self.state(base_id).map(|s| s.time / 1000).unwrap_or(0)
    }

    /// True when the working folder `w` differs from `base` by what `status` reports: content,
    /// type, link target, executable bit, added or removed files. The thin diff `d` also lists
    /// times, modes, owners and folders; those make no automatic state, as on every backend.
    #[cfg(target_os = "macos")]
    fn thin_changed(&self, base: &Snap, w: &Path, d: &crate::thin::Diff) -> Result<bool> {
        let wp = w.to_path_buf();
        for p in d.write.iter().chain(d.removed.iter()).filter(|p| !skip_path(p)) {
            let c = changes::Change {
                path: p.clone(),
                old_path: None,
                old: fsutil::lmeta(base, p),
                new: fsutil::lmeta(&wp, p),
                ranges: None,
            };
            if !changes::file_changes_of(base, &wp, vec![c])?.is_empty() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The layer of an automatic state at `dst`, over `save_layer_base`.
    #[cfg(target_os = "macos")]
    fn write_save_layer(
        &self,
        line: &Line,
        prev: &str,
        w: &Path,
        new: &BTreeMap<String, btrfs::ScanEntry>,
        dst: &Path,
    ) -> Result<()> {
        let base_id = self.save_layer_base(line, prev);
        let base = self.state_src(&base_id)?;
        let base_scan = base.scan(true)?;
        let d = crate::thin::diff(&base_scan, &|k| base.path(k), new, w, self.since_of(line, &base_id))?;
        crate::thin::write_checked(w, &base, &base_scan, new, &d, dst, &base_id, false)
    }

    /// Write the automatic state of the working folder before the caller takes the store lock
    /// (APFS). Copying the changed files took seconds to minutes under the lock and stopped
    /// every other command of the project. Under the lock `save` scans the folder again and
    /// takes this layer with one rename when the folder, the head and the previous state did
    /// not change; else it writes the state as before.
    /// The reads of a save before its lock: the head and the save base are pinned for the rest of
    /// the command (`hold`), then the automatic state is written (APFS).
    pub fn prepare_save(&self, line: &Line) -> Result<()> {
        let prev = self.save_base(line)?;
        self.hold(self.pin(&[&line.head, &prev])?);
        // One scan of the working folder serves the candidates and the save layer below (macOS).
        #[cfg(target_os = "macos")]
        let known = {
            let w = self.working(line);
            if w.exists() {
                let scan = btrfs::scan_with_data(&w, true)?;
                set_regenerable_memo(&w, regenerable_in_scan(&w, &scan));
                // Nothing to convert: the scan still describes the folder that the save layer reads.
                self.plain_caches(line)?.is_empty().then_some(scan)
            } else {
                None
            }
        };
        // Cache folders become layers before the lock: a copy (btrfs) must not run under it.
        self.restore_caches(line)?;
        self.prepare_cache_states(line)?;
        #[cfg(target_os = "macos")]
        if let Some(scan) = known {
            // No folder was converted, so the scan still describes the working folder.
            let w = self.working(line);
            return btrfs::with_known_scan(&w, &scan, || self.prepare_save_layer(line));
        }
        self.prepare_save_layer(line)
    }

    #[cfg(target_os = "macos")]
    fn prepare_save_layer(&self, line: &Line) -> Result<()> {
        let w = self.working(line);
        if !w.exists() || !self.private_paths(line)?.is_empty() || !self.thin_line(line)? {
            return Ok(());
        }
        // A cache that a tool made again as a plain folder is made a cache under the lock first.
        if !self.plain_caches(line)?.is_empty() {
            return Ok(());
        }
        let _t = crate::trace::span("prepare save");
        let prev = self.save_base(line)?;
        let new = btrfs::scan_with_data(&w, true)?;
        let base_id = self.save_layer_base(line, &prev);
        let base = self.state_src(&base_id)?;
        let base_scan = base.scan(true)?;
        let d = crate::thin::diff(&base_scan, &|k| base.path(k), &new, &w, self.since_of(line, &base_id))?;
        // Nothing changed since the previous save: the save under the lock makes no state. With the
        // same base as the save, that is recorded with the scan, so the lock only compares the scan.
        let changed_base = self.thin_changed(&base, &w, &d)?;
        if !changed_base && base_id == prev {
            set_prepared_clean(&line.id, &line.head, &prev, &new);
            return Ok(());
        }
        if !changed_base
            || (base_id != prev && self.p.has_state(&prev) && {
                let p = self.state_src(&prev)?;
                !self.thin_changed(
                    &p,
                    &w,
                    &crate::thin::diff(&p.scan(true)?, &|k| p.path(k), &new, &w, self.since_of(line, &prev))?,
                )?
            })
        {
            return Ok(());
        }
        let path = self.p.work_dir().join(format!("prepared-save-{}", crate::store::temp_name()));
        std::fs::create_dir_all(self.p.work_dir())?;
        match crate::thin::write_checked_scan(&w, &base, &base_scan, &new, &d, &path, &base_id, false) {
            Ok(Some(scan)) => {
                PREPARED_SAVES.with(|p| {
                    p.borrow_mut().push(PreparedSave { line: line.id.clone(), head: line.head.clone(), prev, scan, path })
                });
            }
            Ok(None) => self.p.trash(&path)?,
            Err(_) => {
                let _ = self.p.trash(&path);
            }
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    fn prepare_save_layer(&self, _line: &Line) -> Result<()> {
        Ok(())
    }

    /// True when state `id` differs from the save base of `line`. Without a base, every state
    /// differs. Confirmed file changes count: the raw candidates also list folders and files that
    /// only got new times, such as the working folder after a named state.
    fn changed_since_save_base(&self, line: &Line, id: &str) -> Result<bool> {
        let prev = self.save_base(line)?;
        if !self.tree_available(&prev) {
            return Ok(true);
        }
        let base = self.state_src(&prev)?;
        Ok(changes::file_changes(&base, &self.state_src(id)?)?.into_iter().any(|c| !skip_path(&c.path)))
    }

    /// Save the working content as an automatic state if it changed since the previous one.
    /// The caller holds the project lock.
    pub fn save(&self, ctx: &Ctx, line: &Line, message: &str, running: bool, force: bool) -> Result<Option<StateRec>> {
        let st = self.save_working(ctx, line, message, running, force)?;
        // A cache folder that has no cache overlay state on this line yet gets its first one
        // (the cache of the line, as `save --caches` makes it). A failure keeps the state.
        let of = match &st {
            Some(s) => s.id.clone(),
            None => self.save_base(line)?,
        };
        if let Err(e) = self.save_cache_states(ctx, line, &of) {
            eprintln!("warning: cache overlay of line {} not saved: {e:#}", line.name);
        }
        clear_regenerable_memo();
        Ok(st)
    }

    fn save_working(&self, ctx: &Ctx, line: &Line, message: &str, running: bool, force: bool) -> Result<Option<StateRec>> {
        let _t = crate::trace::span("save working state");
        let w = self.working(line);
        if !w.exists() {
            return Ok(None);
        }
        recover_cache_replacements(&w)?;
        self.restore_caches(line)?;
        #[cfg(target_os = "macos")]
        if !force && self.private_paths(line)?.is_empty() && self.thin_line(line)? {
            // Compare before any copy: an unchanged folder makes no state. One scan of the
            // working folder serves the comparison and the layer over the head.
            let prev = self.save_base(line)?;
            let new = btrfs::scan_with_data(&w, true)?;
            // A layer prepared for this head, base and content was written only because the folder
            // changed (`prepare_save_layer` writes nothing else). The comparison below would repeat
            // that under the lock, with a read of the stored metadata of every entry.
            #[cfg(target_os = "macos")]
            if prepared_unchanged(&line.id, &line.head, &prev, &new) {
                return Ok(None);
            }
            #[cfg(target_os = "macos")]
            let prepared_change = prepared_matches(&line.id, &line.head, &prev, &new);
            #[cfg(not(target_os = "macos"))]
            let prepared_change = false;
            let head = self.state_src(&line.head)?;
            let changed = if prepared_change {
                true
            } else if prev == line.head {
                let head_scan = head.scan(true)?;
                let head_diff = crate::thin::diff(&head_scan, &|k| head.path(k), &new, &w, 0)?;
                self.thin_changed(&head, &w, &head_diff)?
            } else if self.tree_available(&prev) {
                let base = self.state_src(&prev)?;
                self.thin_changed(
                    &base,
                    &w,
                    &crate::thin::diff(&base.scan(true)?, &|k| base.path(k), &new, &w, self.since_of(line, &prev))?,
                )?
            } else {
                true
            };
            if !changed {
                return Ok(None);
            }
            let id = ids::new_id();
            let dst = self.p.delta_path(&id);
            // A layer written before the lock (`prepare_save`) for exactly this content and
            // head: one rename instead of copying the changed files under the lock.
            let mut earlier: Option<StateRec> = None;
            let written = match take_prepared_save(&line.id) {
                Some(pre) if pre.head == line.head && pre.prev == prev && pre.scan == new => {
                    let _t = crate::trace::span("take prepared save");
                    btrfs::move_tree(&pre.path, &dst).map(|_| ())
                }
                // The folder changed while that layer was written (a build or an install writes
                // into it). The layer becomes the automatic state of the folder at that moment,
                // and this state is a layer over it with only the entries that changed since:
                // writing the whole change again took minutes under the lock.
                Some(pre) if pre.head == line.head && pre.prev == prev => {
                    let _t = crate::trace::span("layer over prepared save");
                    let pid = ids::new_id();
                    btrfs::move_tree(&pre.path, &self.p.delta_path(&pid))?;
                    let s1 =
                        self.describe_state(ctx, pid.clone(), StateKind::Auto, vec![line.head.clone()], message, Some(line))?;
                    let base = self.state_src(&pid)?;
                    let base_scan = base.scan(true)?;
                    let d = crate::thin::diff(&base_scan, &|k| base.path(k), &new, &w, 0)?;
                    earlier = Some(s1);
                    crate::thin::write_checked(&w, &base, &base_scan, &new, &d, &dst, &pid, false)
                }
                Some(pre) => {
                    self.p.trash(&pre.path)?;
                    self.write_save_layer(line, &prev, &w, &new, &dst)
                }
                None => self.write_save_layer(line, &prev, &w, &new, &dst),
            };
            let made = match written {
                Ok(()) => self.describe_state(ctx, id.clone(), StateKind::Auto, vec![line.head.clone()], message, Some(line)),
                // Hard links need a complete tree; the failed thin layer is removed first.
                Err(e) if crate::thin::needs_complete(&e) => {
                    let _ = self.p.delete_state(&id);
                    self.new_state(ctx, &w, StateKind::Auto, vec![line.head.clone()], message, Some(line))
                }
                Err(e) => Err(e),
            };
            let mut st = match made {
                Ok(st) => st,
                Err(e) => {
                    // A failed save leaves no snapshot and no record: the layer and the earlier state go.
                    let _ = self.p.delete_state(&id);
                    self.discard_states(earlier.as_slice());
                    return Err(e);
                }
            };
            st.running = running || crate::fsutil::take_unsettled();
            // The working content the save changes from: undo of the save goes back to it.
            let before = self.recorded_working(line)?;
            let mut rec = Record::new("save");
            rec.states.extend(earlier);
            rec.states.push(st.clone());
            rec.data = serde_json::json!({"line": line.id, "head": line.head, "working_before": before, "working_after": st.id});
            self.commit_record(ctx, rec)?;
            return Ok(Some(st));
        }
        let mut st = self.working_snapshot(ctx, line, vec![line.head.clone()], message)?;
        st.running = running || crate::fsutil::take_unsettled();
        if !force {
            let changed = match self.changed_since_save_base(line, &st.id) {
                Ok(changed) => changed,
                Err(e) => {
                    self.discard_states(std::slice::from_ref(&st));
                    return Err(e);
                }
            };
            if !changed {
                self.discard_states(std::slice::from_ref(&st));
                return Ok(None);
            }
        }
        // The working content the save changes from: undo of the save goes back to it.
        let before = self.recorded_working(line)?;
        let mut rec = Record::new("save");
        rec.states.push(st.clone());
        rec.data = serde_json::json!({"line": line.id, "head": line.head, "working_before": before, "working_after": st.id});
        self.commit_record(ctx, rec)?;
        Ok(Some(st))
    }

    /// The working content that the newest record of `line` left in the working folder: the state
    /// of an automatic save, the working state of a record, else the head. Undo of a save reverts
    /// its changes to this content. Reads at most 50 records, newest first.
    pub fn recorded_working(&self, line: &Line) -> Result<String> {
        for r in self.p.db.records_naming(&line.id, 50)? {
            if let Some(ch) = r.lines.get(&line.id) {
                let head = ch.after.as_ref().map(|s| s.head.clone());
                return Ok(ch.working_after.clone().or(head).unwrap_or_else(|| line.head.clone()));
            }
            if r.op == "save" && r.data["line"].as_str() == Some(line.id.as_str()) {
                if let Some(w) = r.data["working_after"].as_str() {
                    return Ok(w.to_string());
                }
            }
        }
        Ok(line.head.clone())
    }

    /// The state that `save --caches` gives a line's caches to: on a protected line with no
    /// changes since its head, the head, so the next lines made from that head start warm; else a
    /// new automatic state. Only a protected line gives caches to a state that other lines share,
    /// so such a state never holds a cache that only its maker may use. The caller holds the
    /// project lock. `prepared` (`prepare_caches_state`) is used when the head, the cache paths
    /// and the working folder are the ones it was made from; else the work is done here.
    pub fn caches_state(
        &self,
        ctx: &Ctx,
        line: &Line,
        message: &str,
        running: bool,
        caches: &[String],
        prepared: Option<PreparedCaches>,
    ) -> Result<StateRec> {
        let w = self.working(line);
        if !w.exists() {
            if let Some(p) = prepared {
                self.discard_states(std::slice::from_ref(&p.st));
            }
            return Err(crate::cmd::exit(1, format!("error: the line '{}' has no working folder", line.name)));
        }
        #[cfg(target_os = "macos")]
        let fresh = match prepared {
            Some(p) if p.head == line.head && p.caches == caches && btrfs::scan_with_data(&w, true)? == p.scan => Some(p),
            Some(p) => {
                self.discard_states(std::slice::from_ref(&p.st));
                None
            }
            None => None,
        };
        // Off APFS nothing is prepared (`prepare_caches_state`).
        #[cfg(not(target_os = "macos"))]
        let fresh = prepared;
        let (mut st, same) = match fresh {
            Some(p) => (p.st, p.same),
            None => self.caches_snapshot(ctx, line, message, caches)?,
        };
        if same {
            self.discard_states(std::slice::from_ref(&st));
            return self.state(&line.head);
        }
        st.running = running;
        let mut rec = Record::new("save");
        rec.states.push(st.clone());
        // The working state this record leaves: the next save of the line changes from it.
        rec.data = serde_json::json!({"line": line.id, "working_after": st.id});
        self.commit_record(ctx, rec)?;
        Ok(st)
    }

    /// `save --caches` before the project lock (issue #194): the snapshot of the working folder and
    /// its compare with the head, with the scan of the folder before them. The command holds the
    /// line lock. Off APFS a snapshot costs little under the lock: None.
    pub fn prepare_caches_state(&self, ctx: &Ctx, line: &Line, message: &str, caches: &[String]) -> Result<Option<PreparedCaches>> {
        #[cfg(target_os = "macos")]
        {
            let w = self.working(line);
            if !w.exists() {
                return Ok(None);
            }
            let _t = crate::trace::span("prepare caches state");
            let scan = btrfs::scan_with_data(&w, true)?;
            let (st, same) = self.caches_snapshot(ctx, line, message, caches)?;
            Ok(Some(PreparedCaches { head: line.head.clone(), caches: caches.to_vec(), scan, st, same }))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (ctx, line, message, caches);
            Ok(None)
        }
    }

    /// Drop a prepared state that the command did not take.
    pub fn discard_prepared_caches(&self, prepared: Option<PreparedCaches>) {
        if let Some(p) = prepared {
            self.discard_states(std::slice::from_ref(&p.st));
        }
    }

    /// The snapshot of the working folder for `caches_state`, not recorded, and true when it has
    /// no changes since the head on a protected line (the caches then go to the head).
    fn caches_snapshot(&self, ctx: &Ctx, line: &Line, message: &str, caches: &[String]) -> Result<(StateRec, bool)> {
        let st = self.working_snapshot(ctx, line, vec![line.head.clone()], message)?;
        if let Some(head) = self.p.db.state(&line.head)? {
            if self.policy().rule_for(&line.name).is_some() && self.p.has_state(&head.id) {
                let base = self.state_src(&head.id)?;
                // Changes as git would see them: a cache is an empty folder in a snapshot, a
                // merge leaves new owners and times on unchanged files, and ignored files (such
                // as other build output) are not changes.
                let in_cache = |path: &str| caches.iter().any(|c| path == c || path.starts_with(&format!("{c}/")));
                let made = self.state_src(&st.id)?;
                let rules = Rules::of(&made);
                let changed = changes::file_changes(&base, &made)?
                    .into_iter()
                    .any(|c| !skip_path(&c.path) && !in_cache(&c.path) && !rules.ignored(&c.path, false));
                return Ok((st, !changed));
            }
        }
        Ok((st, false))
    }

    // ----- staging -----

    pub fn stage(&self, line: &Line) -> Option<PathBuf> {
        let p = self.p.stage_path(&line.id);
        if p.exists() {
            Some(p)
        } else {
            None
        }
    }

    fn stage_base_file(&self, line: &Line) -> PathBuf {
        self.p.meta_dir().join("stage").join(format!("{}.base", line.id))
    }

    /// The staging line, created from the head (the full tree of a line with overlays) if
    /// missing or based on an older head or stack. On APFS it is a thin layer over the head
    /// when the line has no overlays; `ensure_full_stage` gives commands a complete tree.
    pub fn ensure_stage(&self, line: &Line) -> Result<PathBuf> {
        let p = self.p.stage_path(&line.id);
        let basef = self.stage_base_file(line);
        let key = self.stage_key(line)?;
        if p.exists() {
            let base = std::fs::read_to_string(&basef).unwrap_or_default();
            if base.trim() == key {
                return Ok(p);
            }
            self.drop_stage(line)?;
        }
        #[cfg(target_os = "macos")]
        let thin = !self.p.is_overlay() && self.layers(line)?.is_empty();
        #[cfg(not(target_os = "macos"))]
        let thin = false;
        if thin {
            #[cfg(target_os = "macos")]
            {
                let head = self.head_src(line)?;
                crate::thin::empty_layer(&p, &line.head, &head.at("")?)?;
            }
        } else {
            btrfs::snapshot(&self.head_tree(line)?, &p, false)?;
        }
        crate::fsutil::atomic_write(&basef, key.as_bytes(), 0o600)?;
        Ok(p)
    }

    /// The staging line as a complete writable tree, for commands that change it in ways a
    /// thin layer does not support (a patch, a move, a removal, a restore).
    pub fn ensure_full_stage(&self, line: &Line) -> Result<PathBuf> {
        let p = self.ensure_stage(line)?;
        #[cfg(target_os = "macos")]
        if crate::thin::is_thin(&p) {
            let tree = self.stage_tree(line, &p)?;
            let tmp = self.p.work_dir().join(crate::store::temp_name());
            tree.materialize(&tmp, false)?;
            let done = (|| -> Result<()> {
                let info = btrfs::SubvolInfo {
                    uuid: ids::new_id(),
                    received_uuid: None,
                    parent_uuid: None,
                    readonly: false,
                    base: None,
                };
                btrfs::write_info(&tmp, &info)?;
                // One atomic swap: a stop at any point leaves one complete index in place.
                crate::sys::apfs_swap(&tmp, &p)?;
                btrfs::delete_tree(&tmp)
            })();
            if done.is_err() {
                let _ = btrfs::delete_tree(&tmp);
            }
            done?;
        }
        Ok(p)
    }

    /// The tree of a staging line: a thin layer over the head, or a complete tree.
    #[cfg(target_os = "macos")]
    pub fn stage_tree(&self, line: &Line, stage: &Path) -> Result<Snap> {
        if crate::thin::is_thin(stage) {
            crate::thin::Layered::over(stage, self.head_src(line)?)
        } else {
            Ok(crate::thin::Layered::complete(stage))
        }
    }
    #[cfg(not(target_os = "macos"))]
    #[allow(dead_code)]
    fn stage_tree(&self, _line: &Line, stage: &Path) -> Result<Snap> {
        Ok(stage.to_path_buf())
    }

    /// Write the key of a staging line made for `line` (a soft reset keeps the index for a new head).
    pub fn mark_stage(&self, line: &Line) -> Result<()> {
        crate::fsutil::atomic_write(&self.stage_base_file(line), self.stage_key(line)?.as_bytes(), 0o600)
    }

    /// The staging line if it was made from the current head. A reader never deletes a stale
    /// one: `line` may be older than the head another command just moved, and then the index
    /// is that command's new one. Writers replace a stale index under the lock (`ensure_stage`)
    /// and a commit drops it after its own read.
    pub fn current_stage(&self, line: &Line) -> Result<Option<PathBuf>> {
        match self.stage(line) {
            Some(s) => {
                let base = std::fs::read_to_string(self.stage_base_file(line)).unwrap_or_default();
                Ok((base.trim() == self.stage_key(line)?).then_some(s))
            }
            None => Ok(None),
        }
    }

    pub fn drop_stage(&self, line: &Line) -> Result<()> {
        let p = self.p.stage_path(&line.id);
        if p.exists() {
            // Under the project lock the stage is only renamed into the trash (emptied after the lock).
            self.p.trash(&p)?;
        }
        let _ = std::fs::remove_file(self.stage_base_file(line));
        Ok(())
    }

    /// The index to read and the staging folder when there is one. The caller must hold the
    /// project lock while it reads a mutable index.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn index_source(&self, line: &Line) -> Result<(Snap, Option<PathBuf>)> {
        match self.current_stage(line)? {
            Some(s) => Ok((self.stage_tree(line, &s)?, Some(s))),
            None => Ok((self.head_src(line)?, None)),
        }
    }

    /// The staging content as a read-only snapshot, or the head state when nothing is staged.
    pub fn index_view(&self, line: &Line) -> Result<(Snap, Option<Temp>)> {
        match self.current_stage(line)? {
            Some(s) => {
                let t = self.temp_snapshot(&s, true)?;
                #[cfg(target_os = "macos")]
                let tree = if crate::thin::is_thin(&s) {
                    crate::thin::Layered::over(&t.path, self.head_src(line)?)?
                } else {
                    crate::thin::Layered::complete(&t.path)
                };
                #[cfg(not(target_os = "macos"))]
                let tree = t.path.clone();
                Ok((tree, Some(t)))
            }
            None => Ok((self.head_src(line)?, None)),
        }
    }

    // ----- status -----

    /// Changes of the working content: staged (head to index), unstaged (index to working),
    /// untracked and ignored paths, and conflicts left by a merge.
    pub fn status_of(&self, line: &Line, scan: &Path, index: &Snap) -> Result<Status> {
        self.status_of_inner(line, scan, index, true)
    }

    /// Add only needs paths that differ between the index and the working tree.
    /// `limit` names the paths (with their ignore files) that the caller reads: only they are
    /// scanned and compared, and the lists hold nothing else (APFS).
    pub fn status_for_add(&self, line: &Line, scan: &Path, index: &Snap, limit: Option<&BTreeSet<String>>) -> Result<Status> {
        #[cfg(target_os = "macos")]
        return btrfs::with_stored_compare(|| self.status_for_add_inner(line, scan, index, limit));
        #[cfg(not(target_os = "macos"))]
        self.status_for_add_inner(line, scan, index, limit)
    }

    fn status_for_add_inner(&self, line: &Line, scan: &Path, index: &Snap, limit: Option<&BTreeSet<String>>) -> Result<Status> {
        #[cfg(target_os = "macos")]
        if let Some(paths) = limit {
            let mut st = Status::default();
            let list = changes::file_changes_of(
                index,
                scan,
                btrfs::candidates_scanned(index, scan, btrfs::scan_paths(index, paths)?, btrfs::scan_paths(scan, paths)?)?,
            )?;
            classify(scan, list, &mut st);
            self.finish_status(line, scan, index, false, &mut st)?;
            return Ok(st);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = limit;
        self.status_of_inner(line, scan, index, false)
    }

    fn status_of_inner(&self, line: &Line, scan: &Path, index: &Snap, full: bool) -> Result<Status> {
        let mut st = Status::default();
        if full {
            let head = self.head_src(line)?;
            if !same_tree(index, &head) {
                let _t = crate::trace::span("status: head to index diff");
                // The index holds what `add` staged, an attribute-only change included.
                #[cfg(target_os = "macos")]
                let diff = btrfs::with_stored_compare(|| changes::file_changes(&head, index))?;
                #[cfg(not(target_os = "macos"))]
                let diff = changes::file_changes(&head, index)?;
                let staged: Vec<Change> = diff.into_iter().filter(|c| !skip_path(&c.path)).collect();
                st.staged = changes::with_renames(&head, index, staged);
            }
        }
        classify(scan, changes::file_changes(index, scan)?, &mut st);
        self.finish_status(line, scan, index, full, &mut st)?;
        Ok(st)
    }

    /// Untracked folders and open conflicts of a status whose lists are complete.
    fn finish_status(&self, line: &Line, scan: &Path, index: &Snap, full: bool, st: &mut Status) -> Result<()> {
        // Untracked folders: the topmost folder that does not exist in the index.
        let mut dirs: BTreeSet<String> = BTreeSet::new();
        for f in &st.untracked {
            let comps: Vec<&str> = f.split('/').collect();
            let mut top = None;
            for i in 1..comps.len() {
                let d = comps[..i].join("/");
                if fsutil::lmeta(index, &d).is_none() {
                    top = Some(d);
                    break;
                }
            }
            if let Some(d) = top {
                dirs.insert(d);
            }
        }
        st.untracked_dirs = dirs.into_iter().collect();
        st.conflicts.clear();
        if !full {
            return Ok(());
        }
        let head_rec = self.state(&line.head)?;
        for c in &head_rec.conflicts {
            if conflict_open(scan, c)? {
                st.conflicts.push(c.clone());
            }
        }
        // Conflicts of a merge of the line's follow overlays.
        if !self.p.is_overlay() {
            for l in self.layers(line)? {
                if let Some(rec) = self.overlay_repo(&l.overlay).ok().and_then(|o| o.p.db.state(&l.state).ok().flatten()) {
                    for c in &rec.conflicts {
                        if conflict_open(scan, c)? {
                            st.conflicts.push(c.clone());
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The head and the index content of a line: a read before the store lock is still valid
    /// under it when this did not change.
    #[cfg(target_os = "macos")]
    pub fn index_token(&self, line: &Line) -> Result<IndexToken> {
        let stage = match self.current_stage(line)? {
            Some(s) => Some(btrfs::scan_with_data(&s, true)?.into_iter().map(|(k, v)| (k, v.fingerprint)).collect()),
            None => None,
        };
        Ok((line.head.clone(), stage))
    }

    /// Read the status of a line before the store lock (APFS): its copy of the working folder
    /// took seconds to minutes under the lock. `locked_status` takes it when the head and the
    /// index did not change; the lock never covers the working folder.
    pub fn prepare_status(&self, line: &Line) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let token = self.index_token(line)?;
            let (st, scan, _, _) = self.status(line)?;
            PREPARED_STATUS.with(|p| *p.borrow_mut() = Some(PreparedStatus { line: line.id.clone(), token, st, scan }));
        }
        #[cfg(not(target_os = "macos"))]
        let _ = line;
        Ok(())
    }

    /// `status` under the store lock: the one that `prepare_status` read when it still holds.
    pub fn locked_status(&self, line: &Line) -> Result<(Status, Temp, Option<Temp>, Snap)> {
        #[cfg(target_os = "macos")]
        if let Some(p) = PREPARED_STATUS.with(|p| p.borrow_mut().take()) {
            if p.line == line.id && p.token == self.index_token(line)? {
                let (index, itemp) = self.index_view(line)?;
                return Ok((p.st, p.scan, itemp, index));
            }
        }
        self.status(line)
    }

    /// Drop what a command prepared before the lock and did not take: a status, and prepared
    /// snapshot copies (to the trash). Call after the lock.
    pub fn discard_prepared(&self) {
        #[cfg(target_os = "macos")]
        {
            PREPARED_STATUS.with(|p| p.borrow_mut().take());
            PREPARED_CLEAN.with(|p| p.borrow_mut().clear());
            for p in btrfs::take_prepared() {
                let _ = self.p.trash(&p);
            }
            for p in PREPARED_SAVES.with(|p| std::mem::take(&mut *p.borrow_mut())) {
                let _ = self.p.trash(&p.path);
            }
        }
    }

    pub fn status(&self, line: &Line) -> Result<(Status, Temp, Option<Temp>, Snap)> {
        let scan = self.scan(line)?;
        let (index, itemp) = self.index_view(line)?;
        let st = self.status_of(line, &scan.path, &index)?;
        Ok((st, scan, itemp, index))
    }

    /// The status `add` needs: unstaged and untracked paths only, without the head to index diff.
    pub fn status_light(&self, line: &Line) -> Result<(Status, Temp, Option<Temp>, Snap)> {
        let _t = crate::trace::span("status for add");
        let scan = self.scan(line)?;
        let (index, itemp) = self.index_view(line)?;
        let st = self.status_for_add(line, &scan.path, &index, None)?;
        Ok((st, scan, itemp, index))
    }

    /// A status report needs no retained snapshot. Call with the project lock held.
    /// Commands that use the returned file bytes must still call `status`.
    #[cfg(target_os = "macos")]
    pub fn status_report(&self, line: &Line) -> Result<Status> {
        let work = self.working(line);
        let (index, stage) = self.index_source(line)?;
        // LAYR_NO_STATUS_CACHE=1 compares the whole folder every time (for comparisons).
        let key = match std::env::var_os("LAYR_NO_STATUS_CACHE") {
            Some(_) => None,
            None => self.status_key(line, &work, &stage)?,
        };
        // One scan of the working folder: it finds the changed paths for the saved status, and
        // it is the baseline of the next saved status.
        let now = match &key {
            Some(_) => Some(btrfs::scan_with_data(&work, true)?),
            None => None,
        };
        if let (Some(key), Some(now)) = (&key, &now) {
            match self.cached_status(line, &work, &index, key, now) {
                Ok(Some(st)) => return Ok(st),
                Ok(None) => {}
                Err(e) => eprintln!("warning: saved status failed, comparing the whole folder: {e:#}"),
            }
        }
        // States are immutable; the lock keeps the index still. Only the working folder moves.
        let mut roots = vec![work.as_path()];
        if let Some(s) = &stage {
            roots.push(s.as_path());
        }
        let read = || crate::storage::stable_read(&roots, || self.status_of(line, &work, &index));
        let st = match match &now {
            // The scan for the saved status is the first scan of the validated read too.
            Some(now) => crate::storage::with_known_scan(&work, now, read)?,
            None => read()?,
        } {
            Some(st) => st,
            None => self.status(line)?.0,
        };
        if let (Some(key), Some(now)) = (key, now) {
            // `now` comes before the read: a change during the read differs from it next time.
            let _ = self.save_status_cache(line, &StatusCache::new(key, &now, st.clone()));
        }
        Ok(st)
    }

    #[cfg(target_os = "macos")]
    fn status_cache_path(&self, line: &Line) -> PathBuf {
        self.p.meta_dir().join("status").join(format!("{}.cache", line.id))
    }

    #[cfg(target_os = "macos")]
    fn save_status_cache(&self, line: &Line, cache: &StatusCache) -> Result<()> {
        // A lost cache costs only time: no fsync, but never a partial file.
        let path = self.status_cache_path(line);
        let dir = path.parent().context("status cache folder")?;
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".{}.tmp", ids::new_id()));
        std::fs::write(&tmp, cache.encode()?)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&tmp, &path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        Ok(())
    }

    /// What a saved status depends on besides the working folder: the head, the index layer,
    /// and the working folder itself. `None` when a saved status cannot be used: a line with
    /// overlays, or a complete index.
    #[cfg(target_os = "macos")]
    fn status_key(&self, line: &Line, work: &Path, stage: &Option<PathBuf>) -> Result<Option<String>> {
        use layr_hashers::{Digest, GitCompatible};
        if self.p.is_overlay() || !self.layers(line)?.is_empty() {
            return Ok(None);
        }
        let mut h = GitCompatible::new();
        let w = std::fs::symlink_metadata(work)?;
        h.update(format!("{}|{}|{}|{}|", line.head, w.dev(), w.ino(), work.display()));
        if let Some(s) = stage {
            if !crate::thin::is_thin(s) {
                return Ok(None);
            }
            h.update(std::fs::read(self.stage_base_file(line)).unwrap_or_default());
            h.update(btrfs::subvol_info(s)?.uuid.as_bytes());
            h.update(serde_json::to_vec(&crate::thin::removed_of(s)?)?);
            for (k, e) in btrfs::scan_with_data(s, true)? {
                h.update(format!("{k}|{:?}|{:?}\n", e.fingerprint, e.data).as_bytes());
            }
        }
        Ok(Some(hex::encode(h.finalize())))
    }

    /// The saved status, brought up to date for the entries of the working folder whose
    /// metadata, change counter, or clone ID differ from the saved scan (as git's index does
    /// with stat data). `now` is a scan of the working folder made before this read. `None`
    /// when the saved status cannot answer: then the caller compares the whole folder.
    #[cfg(target_os = "macos")]
    fn cached_status(
        &self,
        line: &Line,
        work: &Path,
        index: &Snap,
        key: &str,
        now: &BTreeMap<String, btrfs::ScanEntry>,
    ) -> Result<Option<Status>> {
        let Ok(raw) = std::fs::read(self.status_cache_path(line)) else {
            return Ok(None);
        };
        let Some(cache) = StatusCache::decode(&raw) else {
            return Ok(None);
        };
        if cache.version != STATUS_CACHE_VERSION || cache.key != key {
            return Ok(None);
        }
        let _t = crate::trace::span("saved status");
        let paths = cache.changed(now);
        if paths.len() > STATUS_CACHE_MAX_PATHS {
            return Ok(None);
        }
        for p in &paths {
            let name = p.rsplit('/').next().unwrap_or("");
            if name == ".gitignore" || name == ".layrignore" {
                // Ignore rules decide the state of unchanged paths too.
                return Ok(None);
            }
        }
        if paths.is_empty() {
            let _used = crate::trace::span("saved status used for 0 paths");
            let mut st = cache.status;
            self.finish_status(line, work, index, true, &mut st)?;
            return Ok(Some(st));
        }
        let below = |p: &str| paths.contains(p) || p.match_indices('/').any(|(i, _)| paths.contains(&p[..i]));
        let w: BTreeMap<String, btrfs::ScanEntry> =
            now.iter().filter(|(k, _)| below(k)).map(|(k, v)| (k.clone(), v.clone())).collect();
        let list =
            changes::file_changes_of(index, work, btrfs::candidates_scanned(index, work, btrfs::scan_paths(index, &paths)?, w)?)?;
        // The content read must see the entries of the scan.
        let again = btrfs::scan_paths(work, &paths)?;
        if paths.iter().any(|p| again.get(p) != now.get(p)) {
            return Ok(None);
        }
        let mut st = cache.status;
        st.unstaged.retain(|c| !below(&c.path) && !c.old_path.as_deref().is_some_and(below));
        st.untracked.retain(|p| !below(p));
        st.ignored.retain(|p| !below(p));
        classify(work, list, &mut st);
        st.unstaged.sort_by(|a, b| a.path.cmp(&b.path));
        st.untracked.sort();
        st.ignored.sort();
        self.finish_status(line, work, index, true, &mut st)?;
        // A new baseline saves work when many paths changed. With few, the old baseline and
        // its status stay valid: the same paths are compared again next time.
        if paths.len() >= STATUS_CACHE_RESAVE {
            let _ = self.save_status_cache(line, &StatusCache::new(key.to_string(), now, st.clone()));
        }
        let _used = crate::trace::span(format!("saved status used for {} paths", paths.len()));
        Ok(Some(st))
    }

    // ----- in-place updates -----

    /// Change `target` (a working folder) from the content of `from` to the content of `to`,
    /// touching only the paths that differ. Paths in `local` hold local changes: if a change
    /// touches one, nothing is written and the paths are reported, unless `force`.
    pub fn apply_delta(
        &self,
        from: &Path,
        to: &Path,
        target: &Path,
        own: Option<(u32, u32)>,
        local: &BTreeSet<String>,
        force: bool,
    ) -> Result<Vec<Change>> {
        let _t = crate::trace::span("write working folder");
        let list: Vec<Change> = changes::file_changes(from, to)?.into_iter().filter(|c| !skip_path(&c.path)).collect();
        if !force {
            let mut hit = BTreeSet::new();
            for c in &list {
                for p in [Some(c.path.as_str()), c.old_path.as_deref()].into_iter().flatten() {
                    if local.contains(p) {
                        hit.insert(p.to_string());
                    }
                    let pre = format!("{p}/");
                    for l in local.range(pre.clone()..) {
                        if !l.starts_with(&pre) {
                            break;
                        }
                        hit.insert(l.clone());
                    }
                }
            }
            if !hit.is_empty() {
                let names: Vec<String> = hit.into_iter().map(|p| format!("\t{p}")).collect();
                return Err(crate::cmd::exit(
                    1,
                    format!(
                        "error: Your local changes to the following files would be overwritten:\n{}\nPlease save your changes or park them.",
                        names.join("\n")
                    ),
                ));
            }
        }
        // Moves keep the inode; then deletes (a folder can replace a file); then writes.
        for c in &list {
            if let Some(src) = &c.old_path {
                let t = Tree::open(target)?;
                if t.stat(src).is_some() && t.stat(&c.path).is_none() {
                    t.mkdirs_for(&c.path, own)?;
                    t.rename(src, &c.path)?;
                } else {
                    fsutil::remove_entry(target, src)?;
                }
                prune_empty_parents(target, to, src)?;
            }
        }
        for c in &list {
            if c.old_path.is_some() {
                continue;
            } else if c.new.is_none() {
                fsutil::remove_entry(target, &c.path)?;
                prune_empty_parents(target, to, &c.path)?;
            }
        }
        for c in &list {
            if c.new.is_some() {
                fsutil::copy_work_entry(to, &c.path, target, &c.path, own)?;
            }
        }
        Ok(list)
    }

    // ----- lines -----

    /// Create a line from a state: a writable snapshot at `lines/<name>`.
    pub fn create_line(&self, ctx: &Ctx, name: &str, start: &str, owner: u32) -> Result<Line> {
        self.create_line_from(ctx, name, start, owner, None)
    }

    /// Create a line; a line made from another line (`source`) shows the overlays of the source
    /// that its owner reads (docs/overlays.md).
    pub fn create_line_from(&self, ctx: &Ctx, name: &str, start: &str, owner: u32, source: Option<&Line>) -> Result<Line> {
        check_name("line", name)?;
        // The start state and its caches (their snapshots go with the state) stay for the command.
        self.hold(self.pin(&[start])?);
        if let Some(s) = source {
            self.hold(self.pin(&[&s.head])?);
        }
        self.source_state(start)?;
        if self.p.db.line_by_name(name)?.is_some() {
            bail!("a line named '{name}' already exists");
        }
        let dst = self.p.line_path(name);
        if dst.exists() {
            bail!("{} already exists", dst.display());
        }
        let line = Line {
            id: ids::new_id(),
            name: name.to_string(),
            head: start.to_string(),
            owner,
            machine: self.p.store.machine.id.clone(),
        };
        let layers = match source {
            Some(s) if !self.p.is_overlay() => self.inherit_layers(ctx, s, &line.id, owner)?,
            _ => Vec::new(),
        };
        self.materialize_with(&line, start, &dst, &layers, true)?;
        let res = (|| -> Result<Record> {
            // Not pruned: this path runs under the project lock, and only `init` uses it, which takes no caches.
            self.take_caches(ctx, &line, start, source, &dst)?;
            let mut rec = Record::new("line.create");
            rec.lines.insert(
                line.id.clone(),
                LineChange {
                    before: None,
                    after: Some(self.line_state(&line, start)),
                    working_before: None,
                    working_after: None,
                },
            );
            rec.data = serde_json::json!({"name": name, "start": start});
            if !layers.is_empty() {
                if !self.public_layers(&layers)?.is_empty() {
                    rec.data["stack"] = self.public_stack_data(&line.id, &layers)?;
                }
            }
            self.p.append(rec, &ctx.actor())
        })();
        if let Err(e) = res {
            let _ = btrfs::delete_tree(&dst);
            let _ = self.prune_bases(&line, None, 0);
            return Err(e);
        }
        // The owner of the new line may read the git objects (its .git folder uses them).
        crate::gitbridge::sync_store_access(self)?;
        Ok(line)
    }

    /// Make the working folder of a line at `dst` from a state. The folder is prepared in the
    /// service's private work folder (owner, mode, .git folder, caches) and appears at `dst` only
    /// when it is ready, so no other user can change it while root works on it.
    pub fn materialize(&self, line: &Line, state: &str, dst: &Path) -> Result<()> {
        // Only available layers that the line owner reads enter its working folder.
        let layers = self.readable_layers(line)?;
        self.materialize_with(line, state, dst, &layers, false)
    }

    /// `materialize` with the overlay files of `layers`. With `base`, the new folder (made from
    /// the line's head) is also kept as the line's base.
    pub fn materialize_with(
        &self,
        line: &Line,
        state: &str,
        dst: &Path,
        layers: &[crate::overlay::Layer],
        base: bool,
    ) -> Result<()> {
        if dst.exists() {
            bail!("{} already exists", dst.display());
        }
        let tmp = self.p.work_dir().join(crate::store::temp_name());
        self.snapshot_state(state, &tmp, false)?;
        let r = self
            .put_layers(&tmp, layers, self.own(line))
            .and_then(|_| self.prepare_at(line, state, &tmp, &[]))
            .and_then(|_| {
                if base && !layers.is_empty() {
                    let view = self.temp_snapshot(&tmp, true)?;
                    self.install_base(line, view, state, layers)?;
                }
                Ok(())
            })
            .and_then(|_| Ok(crate::fsutil::rename_tree(&tmp, dst)?));
        if r.is_err() {
            let _ = btrfs::delete_tree(&tmp);
        }
        r
    }

    /// `cached`: folders that the caller already took from a cache (`take_caches`); they are not
    /// copied again from a warm cache.
    pub(crate) fn prepare_at(&self, line: &Line, state: &str, w: &Path, cached: &[String]) -> Result<()> {
        let (uid, gid) = self.owner_ids(line);
        let root_meta = std::fs::symlink_metadata(w)?;
        if root_meta.uid() != uid {
            // Files of another owner: give the whole tree to the line owner.
            let mut paths = Vec::new();
            fsutil::walk(w, "", &mut |rel, m| {
                if m.uid != uid {
                    paths.push(rel.to_string());
                }
                Ok(true)
            })?;
            for p in paths {
                fsutil::chown_nofollow(&w.join(p), uid, gid)?;
            }
        }
        #[cfg(target_os = "macos")]
        crate::sys::apfs_clear_acl(w)?;
        fsutil::chown_nofollow(w, uid, gid)?;
        std::fs::set_permissions(w, std::fs::Permissions::from_mode(0o700))?;
        crate::gitbridge::make_git_folder(self, line, state, w)?;
        let st = self.state(state)?;
        // Caches of this machine's setting, and the caches the state carries (a live move).
        let mut paths: Vec<String> = self.p.config_strings("regenerable");
        for k in st.caches.keys() {
            if !paths.contains(k) {
                paths.push(k.clone());
            }
        }
        // These always get a cache, warm or empty. A folder that the line's `.layrattributes`
        // makes regenerable gets one only warm, from an ancestor that saved it.
        let fixed = paths.len();
        let (regenerable, ignored) = (Rules::attribute(w, "regenerable"), Rules::new(w));
        for k in self.saved_cache_paths(state)? {
            let parent = Path::new(&k).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            let parent_ok = parent.is_empty() || fsutil::lmeta(w, &parent).map(|m| m.is_dir()).unwrap_or(false);
            if !paths.contains(&k) && parent_ok && regenerable.ignored(&k, true) && ignored.ignored(&k, true) {
                paths.push(k);
            }
        }
        for (i, path) in paths.iter().enumerate() {
            // Versioned files win: a folder that holds files of the state comes from the state,
            // also when a record of an older layr lists it as a cache.
            if holds_files(w, path) || cached.contains(path) {
                continue;
            }
            let cp = fsutil::safe_join(w, path)?;
            if let Some(src) = self.warm_cache(state, path, uid)? {
                if cp.symlink_metadata().is_ok() {
                    fsutil::remove_entry(w, path)?;
                }
                fsutil::make_parents(w, path, Some((uid, gid)))?;
                let uniform = btrfs::cache_snapshot(&src, &cp)?;
                // The copy shares its blocks; only its files change owner. The saving owner is not
                // read here, so the walk always runs.
                give_cache_tree(&cp, uniform, None, uid, gid)?;
                continue;
            }
            if i < fixed {
                make_cache(w, path, uid, gid)?;
            }
        }
        Ok(())
    }

    /// The cache paths of the machine setting and of the folders that `.layrattributes` files in
    /// the working folder `w` make regenerable.
    pub fn cache_paths(&self, w: &Path) -> Result<Vec<String>> {
        let mut paths = self.p.config_strings("regenerable");
        for p in regenerable_memo(w)? {
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
        Ok(paths)
    }

    /// Cache paths of the line that are plain folders now, so that a snapshot would put their
    /// files into a state: a tool made them again (`cargo clean` removes `target/` and a build
    /// makes it again), or no cache was made yet. The candidates are the machine setting, the
    /// folder overlays and the folders that `.layrattributes` makes regenerable (and the ignore
    /// rules ignore). A folder that the head versions is no cache.
    fn plain_caches(&self, line: &Line) -> Result<Vec<String>> {
        let w = self.working(line);
        if self.p.is_overlay() {
            return Ok(Vec::new());
        }
        let setting = self.p.config_strings("regenerable");
        let mut paths = setting.clone();
        for (_, f) in self.folder_overlays()? {
            if !paths.contains(&f) {
                paths.push(f);
            }
        }
        for f in regenerable_memo(&w)? {
            if !paths.contains(&f) {
                paths.push(f);
            }
        }
        let (t, regenerable, ignored) = (Tree::open(&w)?, Rules::attribute(&w, "regenerable"), Rules::new(&w));
        let mut head = None;
        let mut stage = None;
        let mut out = Vec::new();
        for p in paths {
            let Some(m) = t.stat(&p) else { continue };
            if !m.is_dir() || fsutil::is_layer(&w, &p, &m) {
                continue;
            }
            if !(setting.contains(&p) || regenerable.ignored(&p, true)) || !ignored.ignored(&p, true) {
                continue;
            }
            if head.is_none() {
                head = Some(self.state_root(&line.head)?);
                stage = Some(self.current_stage(line)?);
            }
            // A file that the head or the stage (`add -f`) versions makes the folder no cache.
            let versioned =
                holds_files(head.as_ref().unwrap(), &p) || stage.as_ref().unwrap().as_ref().is_some_and(|s| holds_files(s, &p));
            if !versioned {
                out.push(p);
            }
        }
        Ok(out)
    }

    /// The cache swap leftovers in the working folders of the project's lines (`gc` reports them).
    /// No scan of a folder: the candidate caches are the `regenerable` setting, the folder overlays
    /// and the caches of the cache journals. A leftover sits next to its cache: the line root holds
    /// the top-level ones (any name), a nested cache is looked for in its parent folder.
    pub fn cache_leftovers(&self) -> Result<Vec<Leftover>> {
        let mut out = Vec::new();
        if self.p.is_overlay() {
            return Ok(out);
        }
        let mut configured = self.p.config_strings("regenerable");
        for (_, f) in self.folder_overlays()? {
            if !configured.contains(&f) {
                configured.push(f);
            }
        }
        for line in self.lines()? {
            let w = self.working(&line);
            let Ok(entries) = std::fs::read_dir(&w) else {
                continue;
            };
            let root: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            // Caches of the cache journals of this line (a swap that is not finished, or a crash).
            let mut caches = configured.clone();
            let t = Tree::open(&w)?;
            for name in root.iter().filter(|n| cache_journal_name(n)) {
                if let Ok(record) = read_cache_replacement(&t, name) {
                    if !caches.contains(&record.path) {
                        caches.push(record.path);
                    }
                }
            }
            let mut found: BTreeSet<String> = BTreeSet::new();
            for name in &root {
                if leftover_base(name).is_some() {
                    found.insert(name.clone());
                }
            }
            for cache in &caches {
                let Some((parent, base)) = cache.rsplit_once('/') else {
                    continue;
                };
                let Ok(entries) = std::fs::read_dir(w.join(parent)) else {
                    continue;
                };
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if is_leftover_name(&name, base) {
                        found.insert(format!("{parent}/{name}"));
                    }
                }
            }
            for rel in found {
                let path = w.join(&rel);
                if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
                    continue;
                }
                // The cache of the leftover: its name without the suffix, in the same folder.
                let (parent, name) = rel.rsplit_once('/').unwrap_or(("", rel.as_str()));
                let base = leftover_base(name).unwrap_or(name);
                let cache = if parent.is_empty() { base.to_string() } else { format!("{parent}/{base}") };
                let kept = if std::fs::symlink_metadata(w.join(cache_journal(&cache))).is_ok() {
                    Some("cache swap not finished".to_string())
                } else {
                    crate::busy::user_of(&path)?.map(|u| format!("in use by pid {} ({})", u.pid, u.name))
                };
                out.push(Leftover { line: line.name.clone(), rel, bytes: tree_bytes(&path), path, kept });
            }
        }
        Ok(out)
    }

    /// Make the cache paths that a tool made again as plain folders caches again, before a
    /// snapshot of the working folder. The caller holds the project lock.
    pub fn restore_caches(&self, line: &Line) -> Result<()> {
        let (w, (u, g)) = (self.working(line), self.owner_ids(line));
        for p in self.plain_caches(line)? {
            // APFS marks the folder in place, files included: no copy. Else reflink copy into a
            // new subvolume.
            #[cfg(target_os = "macos")]
            {
                let _ = (u, g);
                btrfs::mark_cache(&fsutil::safe_join(&w, &p)?)?;
            }
            #[cfg(not(target_os = "macos"))]
            {
                if let Err(e) = convert_cache(&w, &p, u, g) {
                    // In use, or written while copied: the folder stays plain (ignored), the next save tries again.
                    if e.downcast_ref::<CacheRefused>().is_none() {
                        return Err(e);
                    }
                    let _t = crate::trace::span(format!("cache {p} not converted: {e}"));
                }
            }
        }
        Ok(())
    }

    /// The cache folders of a line that are layers now but have no cache overlay state on this
    /// line yet. `save_cache_states` makes that first state, so a new line from a state of this
    /// line starts warm, without `save --caches`.
    fn unsaved_cache_layers(&self, line: &Line) -> Result<Vec<String>> {
        let w = self.working(line);
        if self.p.is_overlay() {
            return Ok(Vec::new());
        }
        let mut candidates = self.p.config_strings("regenerable");
        for f in regenerable_memo(&w)? {
            if !candidates.contains(&f) {
                candidates.push(f);
            }
        }
        let mut head = None;
        let mut stage = None;
        let mut out = Vec::new();
        for p in candidates {
            let cp = fsutil::safe_join(&w, &p)?;
            if !btrfs::is_subvolume(&cp) {
                continue;
            }
            if head.is_none() {
                head = Some(self.state_root(&line.head)?);
                stage = Some(self.current_stage(line)?);
            }
            if holds_files(head.as_ref().unwrap(), &p) || stage.as_ref().unwrap().as_ref().is_some_and(|s| holds_files(s, &p)) {
                continue;
            }
            let name = match self.folder_overlay_of(&p)? {
                Some(n) => n,
                None => crate::cache_overlay::cache_overlay_name(&p),
            };
            if !self.overlay_line_has_state(&name, line)? {
                out.push(p);
            }
        }
        Ok(out)
    }

    /// True when the cache overlay `name` has a state on the overlay line of `line`.
    fn overlay_line_has_state(&self, name: &str, line: &Line) -> Result<bool> {
        let Some(info) = self.overlay_info(name)? else { return Ok(false) };
        let orepo = self.overlay_repo(name)?;
        let line_name = Repo::overlay_line_name(&info, &line.id, line.owner);
        Ok(orepo.p.db.line_by_name(&line_name)?.is_some_and(|ol| ol.head != info.root))
    }

    /// Before the lock (APFS): the copy that the first cache state of each unsaved cache takes
    /// under the lock with one rename (`save_cache_overlays`), as `save --caches` makes it.
    pub fn prepare_cache_states(&self, line: &Line) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let w = self.working(line);
            for p in self.unsaved_cache_layers(line)? {
                btrfs::prepare_snapshot(&fsutil::safe_join(&w, &p)?, &self.p.work_dir(), true)?;
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = line;
        Ok(())
    }

    /// For a command that makes no automatic state (a named save): the first cache states of the
    /// line, saved with its save base. A failure is a warning. The caller holds the project lock.
    pub fn save_first_cache_states(&self, ctx: &Ctx, line: &Line) -> Result<()> {
        let of = self.save_base(line)?;
        if let Err(e) = self.save_cache_states(ctx, line, &of) {
            eprintln!("warning: cache overlay of line {} not saved: {e:#}", line.name);
        }
        Ok(())
    }

    /// The first cache state of each cache folder of `line` that has none (`unsaved_cache_layers`),
    /// saved with the automatic state `of`. The caller holds the project lock.
    fn save_cache_states(&self, ctx: &Ctx, line: &Line, of: &str) -> Result<()> {
        let paths = self.unsaved_cache_layers(line)?;
        if paths.is_empty() {
            return Ok(());
        }
        let shared = self.policy().rule_for(&line.name).is_some();
        let made = self.save_cache_overlays(ctx, line, &paths, shared, of);
        // Copies that no snapshot took (prepared before the lock) go to the trash.
        #[cfg(target_os = "macos")]
        for p in btrfs::take_prepared() {
            let _ = self.p.trash(&p);
        }
        made.map(|_| ())
    }

    /// The cache paths of a line (`cache_paths`) in two lists: the caches, and the paths whose
    /// folder holds files in the head of the line. Such a folder is versioned: never a cache.
    pub fn line_cache_paths(&self, line: &Line) -> Result<(Vec<String>, Vec<String>)> {
        let head = self.state_root(&line.head)?;
        Ok(self.cache_paths(&self.working(line))?.into_iter().partition(|p| !holds_files(&head, p)))
    }

    /// The cache paths that `state` or its ancestors saved (the search of `warm_cache`).
    fn saved_cache_paths(&self, state: &str) -> Result<BTreeSet<String>> {
        const SEARCH: usize = 200;
        let (mut seen, mut found) = (BTreeSet::new(), BTreeSet::new());
        let mut q = std::collections::VecDeque::from([state.to_string()]);
        while let Some(id) = q.pop_front() {
            if seen.len() >= SEARCH {
                break;
            }
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(rec) = self.p.db.state(&id)? {
                found.extend(rec.caches.keys().cloned());
                q.extend(rec.parents);
            }
        }
        Ok(found)
    }

    /// The cache snapshot that a line of `uid` starting at `state` takes for `path`: the state's
    /// own, or else the one of its nearest ancestor that has one on this machine (the build
    /// output closest to the line's content). A cache made by another user is taken only if it
    /// was made on a protected line (`cache_shared`): a cache holds code that review does not see.
    /// A cache is taken only on the platform it was saved on (`model::cache_platform`); a cache
    /// of an older layr that does not name its platform only on the machine that saved it.
    pub fn warm_cache(&self, state: &str, path: &str, uid: u32) -> Result<Option<PathBuf>> {
        const SEARCH: usize = 200;
        let local = self.p.store.machine.id.clone();
        let platform = crate::model::cache_platform();
        let mut seen = BTreeSet::new();
        let mut q = std::collections::VecDeque::from([state.to_string()]);
        while let Some(id) = q.pop_front() {
            if seen.len() >= SEARCH {
                break;
            }
            if !seen.insert(id.clone()) {
                continue;
            }
            let rec = match self.p.db.state(&id)? {
                Some(r) => r,
                None => continue,
            };
            if let Some(snap) = rec.caches.get(path) {
                let src = self.p.cache_path(&id, snap);
                let owner_of_line = rec.line.as_ref().and_then(|l| self.p.db.line_by_id(l).ok().flatten()).map(|l| l.owner);
                let allowed = rec.cache_shared || rec.cache_by == Some(uid) || owner_of_line == Some(uid);
                // Another state's cache only from this machine; the state's own also after a move.
                let here = self.p.db.state_machine(&id)?.as_deref() == Some(local.as_str());
                if src.exists() && allowed && cache_fits(rec.cache_platform.as_deref(), &platform, here) && (id == state || here)
                {
                    return Ok(Some(src));
                }
            }
            q.extend(rec.parents);
        }
        Ok(None)
    }

    /// Record a new head for a line (and optionally a working state change).
    #[allow(clippy::too_many_arguments)]
    /// Move the head of `line` to `new_head` and record it, under the project lock.
    pub fn move_head(
        &self,
        ctx: &Ctx,
        op: &str,
        line: &Line,
        new_head: &str,
        states: Vec<StateRec>,
        working_before: Option<String>,
        data: serde_json::Value,
    ) -> Result<Record> {
        let rec = self.head_record(ctx, op, line, new_head, states, working_before, data)?;
        let r = self.commit_record(ctx, rec)?;
        self.after_head_moved(line, new_head);
        Ok(r)
    }

    /// The record of a head move, with the snapshot of the working folder after it. No project lock
    /// is needed: a command that holds the line lock makes it before `commit_line` checks the head.
    #[allow(clippy::too_many_arguments)]
    pub fn head_record(
        &self,
        ctx: &Ctx,
        op: &str,
        line: &Line,
        new_head: &str,
        states: Vec<StateRec>,
        working_before: Option<String>,
        data: serde_json::Value,
    ) -> Result<Record> {
        crate::gitbridge::check_git_head(self, line)?;
        let mut rec = Record::new(op);
        rec.states = states;
        let working_after = match &working_before {
            Some(_) => self.after_state(ctx, line, op, &mut rec)?,
            None => None,
        };
        rec.lines.insert(
            line.id.clone(),
            LineChange {
                before: Some(self.line_state(line, &line.head)),
                after: Some(self.line_state(line, new_head)),
                working_before,
                working_after,
            },
        );
        rec.data = data;
        Ok(rec)
    }

    /// The `.git` folder of a line follows its head. A failure only warns: the move is done.
    pub fn after_head_moved(&self, line: &Line, new_head: &str) {
        let mut moved = line.clone();
        moved.head = new_head.to_string();
        if let Err(e) = crate::gitbridge::refresh_git_folder(self, &moved) {
            eprintln!("warning: .git folder of line {} not updated: {e:#}", line.name);
        }
    }

    /// The lock of `line` for a whole command: it moves the head of the line or writes its working
    /// folder. Take it before anything else, and never under the project lock (store.rs `LineLock`).
    pub fn line_lock(&self, line: &Line) -> Result<crate::store::LineLock> {
        self.p.line_lock(&line.id, &line.name)
    }

    /// Run `commit` under the project lock, after a check that `line` still exists and has the head
    /// `head` that the caller started from. Only the records and the renames of a command go here.
    /// The caller holds the line lock, so the head can change only by a command that does not take
    /// it (line delete, follow, fetch). Such a change is refused: nothing is recorded, and the error
    /// names the move. The caller undoes its working folder change.
    pub fn commit_line<T>(&self, line: &Line, head: &str, commit: impl FnOnce(&Line) -> Result<T>) -> Result<T> {
        if self.p.holds_lock() {
            bail!("internal error: line {} committed while the project lock is held", line.name);
        }
        // Test hooks (tests/t122-line-locks.sh), named by line: a crash after the folder was written
        // and before the record, and a head that another command moved.
        let hook = |name: &str| std::env::var(name).ok().is_some_and(|v| v == line.name);
        if hook("LAYR_TEST_CRASH_BEFORE_COMMIT") {
            std::process::exit(86);
        }
        let _lock = self.p.lock()?;
        let now = match self.p.db.line_by_id(&line.id)? {
            Some(l) => self.localize(l)?,
            None => return Err(line_gone(&line.name)),
        };
        if hook("LAYR_TEST_MOVE_HEAD_AT_COMMIT") {
            let parent = self.state(head).ok().and_then(|s| s.parents.first().cloned()).unwrap_or_else(|| head.to_string());
            return Err(line_moved(&line.name, head, &parent));
        }
        if now.head != head {
            return Err(line_moved(&line.name, head, &now.head));
        }
        commit(&now)
    }

    /// A snapshot of the working content right after an operation, so undo can revert only the
    /// changes of that operation and keep later work.
    pub fn after_state(&self, ctx: &Ctx, line: &Line, op: &str, rec: &mut Record) -> Result<Option<String>> {
        let w = self.working(line);
        if !w.exists() {
            return Ok(None);
        }
        // Over the state saved before the operation: only what the operation changed is copied.
        let base = self.save_base(line)?;
        let st = self.working_snapshot_over(ctx, line, vec![line.head.clone()], &format!("after {op}"), &base)?;
        let id = st.id.clone();
        rec.states.push(st);
        Ok(Some(id))
    }

    // ----- ancestry -----

    pub fn parents(&self, id: &str) -> Result<Vec<String>> {
        Ok(self.source_state(id)?.parents)
    }

    /// All ancestors of `id` (including itself), breadth first.
    pub fn ancestors(&self, id: &str) -> Result<Vec<String>> {
        self.ancestors_of(vec![id.to_string()])
    }

    /// All ancestors of the `seeds` (including them), breadth first, each state read once.
    fn ancestors_of(&self, seeds: Vec<String>) -> Result<Vec<String>> {
        let mut seen = BTreeSet::new();
        let mut order = Vec::new();
        let mut q = std::collections::VecDeque::from(seeds);
        while let Some(x) = q.pop_front() {
            if !seen.insert(x.clone()) {
                continue;
            }
            order.push(x.clone());
            if let Some(s) = self.p.db.state(&x)? {
                if s.kind.auxiliary() {
                    self.source_state(&x)?;
                }
                for p in s.parents {
                    q.push_back(p);
                }
            }
        }
        Ok(order)
    }

    pub fn is_ancestor(&self, a: &str, b: &str) -> Result<bool> {
        Ok(self.ancestors(b)?.iter().any(|x| x == a))
    }

    /// The paths that are not ignored and that an automatic state changes against `base` (its
    /// parent, or the current head of its line): work that no named state holds. Build output and
    /// other ignored files are not work.
    pub fn unsaved_paths(&self, base: &str, auto: &StateRec) -> Result<Vec<String>> {
        let parent = base;
        let root = self.state_src(&auto.id)?;
        let rules = Rules::of(&root);
        let list = crate::changes::file_changes(&self.state_src(parent)?, &root)?;
        Ok(list
            .into_iter()
            .filter(|c| {
                let dir = c.new.as_ref().or(c.old.as_ref()).is_some_and(|m| m.is_dir());
                !skip_path(&c.path) && !rules.ignored(&c.path, dir)
            })
            .map(|c| c.path)
            .collect())
    }

    /// The states under `state` that `line` started on, that no push carries yet and that
    /// another line made: a push of `state` would publish that line's work as well. Automatic
    /// states pass to their parent (`line new` refuses one with unsaved work).
    pub fn foreign_base(&self, line: &Line, state: &str) -> Result<Vec<StateRec>> {
        let Some(start) = self.p.db.line_start(&line.id)? else { return Ok(Vec::new()) };
        if !self.is_ancestor(&start, state)? {
            return Ok(Vec::new());
        }
        let mut found = Vec::new();
        let mut next = Some(start);
        while let Some(id) = next {
            if self.p.db.git_commit_of(&id)?.is_some() {
                break;
            }
            let s = self.state(&id)?;
            next = s.parents.first().cloned();
            if s.line.as_deref().is_some_and(|l| l != line.id) && !matches!(s.kind, StateKind::Auto | StateKind::Import) {
                found.push(s);
            }
        }
        Ok(found)
    }

    /// The parent edges of the project, read once for many merge-base searches (`History`).
    pub fn history(&self) -> Result<History> {
        let _t = crate::trace::span("overlap history");
        let mut parents: HashMap<String, Vec<String>> = HashMap::new();
        for (state, parent) in self.p.db.parent_edges()? {
            parents.entry(state).or_default().push(parent);
        }
        let auxiliary = self.p.db.state_auxiliary()?.into_iter().filter(|(_, aux)| *aux).map(|(id, _)| id).collect();
        Ok(History { parents, auxiliary })
    }

    /// The best common ancestor: a common ancestor that is not an ancestor of another one.
    /// One walk finds every state that is a proper ancestor of a common ancestor.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>> {
        let aa = self.ancestors(a)?;
        let bs: BTreeSet<String> = self.ancestors(b)?.into_iter().collect();
        let common: Vec<String> = aa.into_iter().filter(|x| bs.contains(x)).collect();
        let mut parents = Vec::new();
        for c in &common {
            if let Some(s) = self.p.db.state(c)? {
                parents.extend(s.parents);
            }
        }
        let below: BTreeSet<String> = self.ancestors_of(parents)?.into_iter().collect();
        Ok(common.into_iter().find(|c| !below.contains(c)))
    }

    /// Commits reachable from `id` by first parents.
    pub fn first_parent_chain(&self, id: &str) -> Result<Vec<String>> {
        self.first_parents_until(id, &BTreeSet::new())
    }

    /// The first-parent chain of `id` down to the first state that `upstream` has, newest first: the states of
    /// `upstream..id` on the first-parent path. A merge of `upstream` into the line puts their merge base on a
    /// second parent, so a stop at the merge base alone would pass it and reach the root.
    pub fn first_parents_since(&self, id: &str, upstream: &str) -> Result<Vec<String>> {
        self.first_parents_until(id, &self.ancestors(upstream)?.into_iter().collect())
    }

    fn first_parents_until(&self, id: &str, known: &BTreeSet<String>) -> Result<Vec<String>> {
        let mut v = Vec::new();
        let mut cur = Some(id.to_string());
        while let Some(c) = cur.filter(|c| !known.contains(c)) {
            cur = self.p.db.state(&c)?.and_then(|s| s.parents.first().cloned());
            v.push(c);
        }
        Ok(v)
    }
}

/// The parent edges and the auxiliary states of a project in memory (`Repo::history`). Its merge
/// bases equal `Repo::merge_base`, which reads one state per step of the walk.
pub struct History {
    parents: HashMap<String, Vec<String>>,
    auxiliary: HashSet<String>,
}

impl History {
    /// All ancestors of the `seeds` (including them), breadth first, as `Repo::ancestors_of`.
    /// `None` when the walk reaches an auxiliary state: `Repo::merge_base` then refuses it.
    fn ancestors<'s>(&'s self, seeds: Vec<&'s str>) -> Option<Vec<&'s str>> {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut order = Vec::new();
        let mut queue: VecDeque<&str> = seeds.into();
        while let Some(x) = queue.pop_front() {
            if !seen.insert(x) {
                continue;
            }
            if self.auxiliary.contains(x) {
                return None;
            }
            order.push(x);
            if let Some(parents) = self.parents.get(x) {
                queue.extend(parents.iter().map(String::as_str));
            }
        }
        Some(order)
    }

    /// The merge base of `a` and `b`, as `Repo::merge_base`. `None` when a walk reaches an auxiliary state.
    pub fn merge_base(&self, a: &str, b: &str) -> Option<Option<String>> {
        let aa = self.ancestors(vec![a])?;
        let bs: HashSet<&str> = self.ancestors(vec![b])?.into_iter().collect();
        let common: Vec<&str> = aa.into_iter().filter(|x| bs.contains(x)).collect();
        let seeds: Vec<&str> = common.iter().flat_map(|c| self.parents.get(*c).into_iter().flatten().map(String::as_str)).collect();
        let below: HashSet<&str> = self.ancestors(seeds)?.into_iter().collect();
        Some(common.into_iter().find(|c| !below.contains(c)).map(str::to_string))
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;

    /// The walk of `Repo::merge_base` over a parent map: the reference that `History` must equal.
    fn reference_merge_base(parents: &HashMap<String, Vec<String>>, a: &str, b: &str) -> Option<String> {
        let ancestors = |seeds: Vec<String>| {
            let mut seen = BTreeSet::new();
            let mut order = Vec::new();
            let mut q: VecDeque<String> = seeds.into();
            while let Some(x) = q.pop_front() {
                if !seen.insert(x.clone()) {
                    continue;
                }
                order.push(x.clone());
                for p in parents.get(&x).into_iter().flatten() {
                    q.push_back(p.clone());
                }
            }
            order
        };
        let aa = ancestors(vec![a.to_string()]);
        let bs: BTreeSet<String> = ancestors(vec![b.to_string()]).into_iter().collect();
        let common: Vec<String> = aa.into_iter().filter(|x| bs.contains(x)).collect();
        let below: BTreeSet<String> = ancestors(common.iter().flat_map(|c| parents.get(c).cloned().unwrap_or_default()).collect())
            .into_iter()
            .collect();
        common.into_iter().find(|c| !below.contains(c))
    }

    fn history(parents: &HashMap<String, Vec<String>>) -> History {
        History { parents: parents.clone(), auxiliary: HashSet::new() }
    }

    /// Random histories with merges, criss-cross merges included: every pair gives the reference
    /// merge base, also when the heads are the same state or a root.
    #[test]
    fn history_merge_bases_equal_the_reference_walk() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for round in 0..150 {
            let n = 2 + next(28) as usize;
            let ids: Vec<String> = (0..n).map(|i| format!("r{round}-{i:02}")).collect();
            let mut parents: HashMap<String, Vec<String>> = HashMap::new();
            for i in 1..n {
                let count = 1 + next(2) as usize;
                let picked: Vec<String> = (0..count).map(|_| ids[next(i as u64) as usize].clone()).collect();
                parents.insert(ids[i].clone(), picked);
            }
            let h = history(&parents);
            for a in &ids {
                for b in &ids {
                    assert_eq!(h.merge_base(a, b), Some(reference_merge_base(&parents, a, b)), "round {round}: {a} {b}");
                }
            }
        }
    }

    /// A walk that reaches an auxiliary state gives no answer, so the caller uses `Repo::merge_base`.
    #[test]
    fn an_auxiliary_state_in_the_walk_gives_no_answer() {
        let parents = HashMap::from([("top".to_string(), vec!["cache".to_string()]), ("cache".to_string(), vec![])]);
        let h = History { parents, auxiliary: HashSet::from(["cache".to_string()]) };
        assert_eq!(h.merge_base("top", "top"), None);
        assert_eq!(h.merge_base("cache", "cache"), None);
    }
}

/// A saved status of a line and the scan of its working folder that it matches
/// (`Repo::status_report`). Each entry keeps its type, inode, mode, size, times, change counter,
/// and clone ID: a write changes the change counter, as with git's stat data.
#[cfg(target_os = "macos")]
struct StatusCache {
    version: u32,
    key: String,
    /// Path, then fingerprint and data identity of each entry, in path order.
    entries: Vec<(String, [i64; 10])>,
    status: Status,
}

#[cfg(target_os = "macos")]
#[derive(serde::Serialize, serde::Deserialize)]
struct StatusHead {
    version: u32,
    key: String,
    status: Status,
}

#[cfg(target_os = "macos")]
const STATUS_CACHE_VERSION: u32 = 2;
#[cfg(target_os = "macos")]
const STATUS_CACHE_MAGIC: &[u8] = b"layr status cache\n";

/// More changed paths than this compare the whole folder.
#[cfg(target_os = "macos")]
const STATUS_CACHE_MAX_PATHS: usize = 20_000;

/// From this many changed paths on, a saved status gets a new baseline.
#[cfg(target_os = "macos")]
const STATUS_CACHE_RESAVE: usize = 256;

#[cfg(target_os = "macos")]
fn entry_key(e: &btrfs::ScanEntry) -> [i64; 10] {
    let f = e.fingerprint;
    let d = e.data.unwrap_or_default();
    [
        f.0 as i64,
        f.1 as i64,
        f.2 as i64,
        f.3 as i64,
        f.4,
        f.5,
        f.6,
        f.7,
        d.generation.map(|g| g as i64).unwrap_or(-1),
        d.clone_id.map(|c| c as i64).unwrap_or(-1),
    ]
}

#[cfg(target_os = "macos")]
impl StatusCache {
    fn new(key: String, scan: &BTreeMap<String, btrfs::ScanEntry>, status: Status) -> Self {
        StatusCache {
            version: STATUS_CACHE_VERSION,
            key,
            entries: scan.iter().map(|(k, e)| (k.clone(), entry_key(e))).collect(),
            status,
        }
    }

    /// A header in JSON, then each entry as a path length, the path, and ten numbers.
    fn encode(&self) -> Result<Vec<u8>> {
        let head = serde_json::to_vec(&StatusHead { version: self.version, key: self.key.clone(), status: self.status.clone() })?;
        let mut out = Vec::with_capacity(head.len() + self.entries.len() * 120);
        out.extend_from_slice(STATUS_CACHE_MAGIC);
        out.extend_from_slice(&(head.len() as u64).to_le_bytes());
        out.extend_from_slice(&head);
        out.extend_from_slice(&(self.entries.len() as u64).to_le_bytes());
        for (k, v) in &self.entries {
            out.extend_from_slice(&(k.len() as u32).to_le_bytes());
            out.extend_from_slice(k.as_bytes());
            for n in v {
                out.extend_from_slice(&n.to_le_bytes());
            }
        }
        Ok(out)
    }

    fn decode(raw: &[u8]) -> Option<Self> {
        let mut r = raw.strip_prefix(STATUS_CACHE_MAGIC)?;
        let mut take = |n: usize| -> Option<&[u8]> {
            let (a, b) = (r.get(..n)?, r.get(n..)?);
            r = b;
            Some(a)
        };
        let head_len = u64::from_le_bytes(take(8)?.try_into().ok()?) as usize;
        let head: StatusHead = serde_json::from_slice(take(head_len)?).ok()?;
        let count = u64::from_le_bytes(take(8)?.try_into().ok()?) as usize;
        let mut entries = Vec::with_capacity(count.min(10_000_000));
        for _ in 0..count {
            let len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
            let path = String::from_utf8(take(len)?.to_vec()).ok()?;
            let mut v = [0i64; 10];
            for n in v.iter_mut() {
                *n = i64::from_le_bytes(take(8)?.try_into().ok()?);
            }
            entries.push((path, v));
        }
        Some(StatusCache { version: head.version, key: head.key, entries, status: head.status })
    }

    /// Paths whose entries differ from the saved scan, and files without a change counter (a
    /// file that a process maps can change without a visible sign). A folder that only changed
    /// its times is left out: each changed entry inside it is in the list itself.
    fn changed(&self, now: &BTreeMap<String, btrfs::ScanEntry>) -> BTreeSet<String> {
        let kind = |v: &[i64; 10]| v[2] & libc::S_IFMT as i64;
        const DIR: i64 = libc::S_IFDIR as i64;
        const FILE: i64 = libc::S_IFREG as i64;
        let mut out = BTreeSet::new();
        // Both lists are in path order: one walk over both.
        let mut old = self.entries.iter().peekable();
        for (k, e) in now {
            while let Some((ok, _)) = old.peek() {
                if ok.as_str() < k.as_str() {
                    out.insert(ok.clone());
                    old.next();
                } else {
                    break;
                }
            }
            let new = entry_key(e);
            let same = match old.peek() {
                Some((ok, ov)) if ok == k => {
                    let same = *ov == new || (kind(ov) == DIR && kind(&new) == DIR);
                    old.next();
                    same
                }
                _ => false,
            };
            let no_counter = kind(&new) == FILE && new[8] < 0;
            if !k.is_empty() && (!same || no_counter) {
                out.insert(k.clone());
            }
        }
        out.extend(old.map(|(k, _)| k.clone()));
        out.remove("");
        out
    }
}

/// Sort index-to-working changes into unstaged, untracked, and ignored paths.
fn classify(scan: &Path, list: Vec<Change>, st: &mut Status) {
    let rules = Rules::new(scan);
    for c in changes::without_moves(list) {
        if skip_path(&c.path) {
            continue;
        }
        if c.old.is_none() {
            if rules.ignored(&c.path, false) {
                st.ignored.push(c.path.clone());
            } else {
                st.untracked.push(c.path.clone());
            }
            continue;
        }
        st.unstaged.push(c);
    }
}

/// True if a conflict is still open in `root`. A content conflict is open while the file holds
/// conflict markers (a merge writes markers only into files of at most `MAX_TEXT` bytes, so a
/// larger file has none). Other kinds (moves, deletes, types, binary files) stay open until the
/// next commit of the line, which is the admin's decision.
pub fn conflict_open(root: &(impl fsutil::Src + ?Sized), c: &Conflict) -> Result<bool> {
    if c.kind == "content" || c.kind == "add/add" {
        return Ok(match fsutil::lmeta(root, &c.path) {
            Some(m) if m.is_file() && m.size <= fsutil::MAX_TEXT => has_markers(&fsutil::read_file(root, &c.path)?),
            _ => false,
        });
    }
    Ok(true)
}

pub fn has_markers(data: &[u8]) -> bool {
    let s = String::from_utf8_lossy(data);
    let mut a = false;
    let mut b = false;
    for l in s.lines() {
        if l.starts_with("<<<<<<< ") {
            a = true;
        } else if l.starts_with(">>>>>>> ") && a {
            b = true;
        }
    }
    a && b
}

/// Remove parent folders of `rel` in `target` that became empty and do not exist in `to`.
fn prune_empty_parents(target: &Path, to: &Path, rel: &str) -> Result<()> {
    let t = Tree::open(target)?;
    let mut comps: Vec<&str> = rel.split('/').collect();
    comps.pop();
    while !comps.is_empty() {
        let d = comps.join("/");
        if fsutil::lmeta(to, &d).is_some() || !t.rmdir(&d) {
            break;
        }
        comps.pop();
    }
    Ok(())
}

/// Give every entry of a tree (a new cache copy, still private) to `uid:gid`, without following
/// links and without entering nested subvolumes.
/// Whether a cache copy may keep the owners that its clone gave, without a walk. Every condition
/// is needed:
/// - `uniform`: the copy is one clone, not a copy file by file;
/// - the saving line's owner (`source_owner`) is the line owner;
/// - this process runs as that owner, not as root: a root clone keeps each entry's own owner;
/// - the root already has the owner and the group.
pub(crate) fn owner_walk_skippable(
    uniform: bool,
    euid: u32,
    uid: u32,
    gid: u32,
    source_owner: Option<u32>,
    root: (u32, u32),
) -> bool {
    uniform && euid != 0 && euid == uid && source_owner == Some(uid) && root == (uid, gid)
}

/// `give_tree` for a cache copy: the walk is skipped only when `owner_walk_skippable` holds.
pub(crate) fn give_cache_tree(root: &Path, uniform: bool, source_owner: Option<u32>, uid: u32, gid: u32) -> Result<()> {
    let m = std::fs::symlink_metadata(root)?;
    let euid = rustix::process::geteuid().as_raw();
    if owner_walk_skippable(uniform, euid, uid, gid, source_owner, (m.uid(), m.gid())) {
        return Ok(());
    }
    give_tree(root, uid, gid)
}

pub(crate) fn give_tree(root: &Path, uid: u32, gid: u32) -> Result<()> {
    let _t = crate::trace::span(format!("give tree {}", root.display()));
    let mut paths = Vec::new();
    fsutil::walk(root, "", &mut |rel, m| {
        if m.uid != uid || m.gid != gid {
            paths.push(rel.to_string());
        }
        Ok(true)
    })?;
    for p in paths {
        fsutil::chown_nofollow(&root.join(p), uid, gid)?;
    }
    fsutil::chown_nofollow(root, uid, gid)
}

/// The folders of the working folder `w` that `.layrattributes` files make `regenerable` and
/// that the ignore rules ignore. A regenerable folder that is not ignored is no cache: its files
/// are in history, and a cache must not hide them.
///
/// On macOS the folders come from the save's scan (`regenerable_in_scan`). Elsewhere they come from
/// a walk (`regenerable_by_walk`): the walk enters the folders that the root `.gitignore` does not
/// ignore, so it may enter a folder that a nested rule file ignores. Such a folder and its children
/// are no candidate, as in the scan. A folder is no candidate when an ignored folder is above it.
pub fn regenerable_folders(w: &Path) -> Result<Vec<String>> {
    // macOS: the save scans the working folder anyway, and that scan lists every entry, so the
    // folders come from it: a walk of 3k folders cost 200 ms here, against a 20 ms budget.
    #[cfg(target_os = "macos")]
    if let Ok(scan) = btrfs::scan_with_data(w, true) {
        return Ok(regenerable_in_scan(w, &scan));
    }
    regenerable_by_walk(w)
}

/// `regenerable_folders` from a scan of the working folder `w` (every entry, folders and rule
/// files; a layer is listed without its content). A folder inside an ignored folder is no
/// candidate, as the walk does not enter ignored folders.
#[cfg(target_os = "macos")]
fn regenerable_in_scan(w: &Path, scan: &BTreeMap<String, btrfs::ScanEntry>) -> Vec<String> {
    let mut rule_dirs = std::collections::HashSet::from([String::new()]);
    for (rel, e) in scan {
        let (dir, name) = rel.rsplit_once('/').unwrap_or(("", rel.as_str()));
        if !e.meta.is_dir() && matches!(name, ".gitignore" | ".layrignore" | ".layrattributes") {
            rule_dirs.insert(dir.to_string());
        }
    }
    let (regenerable, ignored) = (Rules::limited(w, Some("regenerable"), rule_dirs.clone()), Rules::limited(w, None, rule_dirs));
    let mut found = Vec::new();
    for (rel, e) in scan {
        if rel.is_empty() || !e.meta.is_dir() || rel == ".git" || rel.starts_with(".git/") {
            continue;
        }
        let parent = rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        if !parent.is_empty() && ignored.ignored(parent, true) {
            continue;
        }
        if regenerable.ignored(rel, true) && ignored.ignored(rel, true) {
            found.push(rel.clone());
        }
    }
    found.sort();
    found
}

/// `regenerable_folders` by a walk of the folders (the other systems, and a scan that fails).
fn regenerable_by_walk(w: &Path) -> Result<Vec<String>> {
    let _t = crate::trace::span("regenerable walk");
    let t = Tree::open(w)?;
    let root_only = Rules::limited(w, None, std::collections::HashSet::from([String::new()]));
    let mut rule_dirs = std::collections::HashSet::from([String::new()]);
    let mut children = Vec::new();
    t.walk_folders(|dir, entries| {
        let mut enter = Vec::new();
        for (name, is_dir) in entries {
            if matches!(name.as_str(), ".gitignore" | ".layrignore" | ".layrattributes") {
                rule_dirs.insert(dir.to_string());
            }
            let rel = if dir.is_empty() { name.clone() } else { format!("{dir}/{name}") };
            if !is_dir || rel == ".git" {
                continue;
            }
            if !root_only.ignored(&rel, true) {
                enter.push(name.clone());
            }
            children.push(rel);
        }
        enter
    })?;
    let (regenerable, ignored) = (Rules::limited(w, Some("regenerable"), rule_dirs.clone()), Rules::limited(w, None, rule_dirs));
    let mut found: Vec<String> = children
        .into_iter()
        .filter(|rel| {
            let parent = rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
            (parent.is_empty() || !ignored.ignored(parent, true)) && regenerable.ignored(rel, true) && ignored.ignored(rel, true)
        })
        .collect();
    found.sort();
    Ok(found)
}

thread_local! {
    /// The regenerable folders of the working folder of the running command (`regenerable_memo`).
    static REGENERABLE: std::cell::RefCell<Option<(PathBuf, Vec<String>)>> = const { std::cell::RefCell::new(None) };
}

/// `regenerable_folders`, once per command: the save paths ask several times (before the store
/// lock and under it). `clear_regenerable_memo` starts a new command.
pub fn regenerable_memo(w: &Path) -> Result<Vec<String>> {
    REGENERABLE.with(|m| {
        let mut m = m.borrow_mut();
        if let Some((path, found)) = m.as_ref() {
            if path == w {
                return Ok(found.clone());
            }
        }
        let found = regenerable_folders(w)?;
        *m = Some((w.to_path_buf(), found.clone()));
        Ok(found)
    })
}

/// Set the regenerable folders of the command for the working folder `w` (from a scan).
pub fn set_regenerable_memo(w: &Path, found: Vec<String>) {
    REGENERABLE.with(|m| *m.borrow_mut() = Some((w.to_path_buf(), found)));
}

/// Forget the regenerable folders. The dispatcher calls `begin_command` at the start of every
/// command (CLI and daemon), so no command reads the list of an earlier one. A save clears it after
/// its state, because a command can save twice (and a merge can change `.layrattributes`).
pub fn clear_regenerable_memo() {
    REGENERABLE.with(|m| *m.borrow_mut() = None);
}

/// Start a command: its regenerable folders are its own.
pub fn begin_command() {
    clear_regenerable_memo();
}

/// Whether a list of regenerable folders is held (for tests).
#[cfg(test)]
pub fn regenerable_memo_is_set() -> bool {
    REGENERABLE.with(|m| m.borrow().is_some())
}

/// Create an empty nested subvolume for a regenerable cache at `path` in the working folder.
pub fn make_cache(w: &Path, path: &str, uid: u32, gid: u32) -> Result<()> {
    let t = Tree::open(w)?;
    recover_cache_replacement(&t, path)?;
    if let Some(m) = t.stat(path) {
        if fsutil::is_layer(w, path, &m) {
            return Ok(());
        }
        if m.is_dir() && t.list(path)?.is_empty() {
            t.rmdir(path);
        } else {
            return Ok(());
        }
    }
    t.mkdirs_for(path, Some((uid, gid)))?;
    t.create_subvolume(path).with_context(|| format!("create cache {path}"))?;
    t.chown(path, uid, gid)?;
    Ok(())
}

/// Copy a folder within one tree (both sides may change under us: all through descriptors).
fn copy_tree(
    t: &Tree,
    from: &str,
    to: &str,
    own: Option<(u32, u32)>,
    linked: &mut std::collections::HashMap<(u64, u64), String>,
) -> Result<()> {
    // Names must be UTF-8: a lossy name would copy one file twice and skip another.
    for name in t.list_strict(from)? {
        copy_entry_to(t, &format!("{from}/{name}"), &format!("{to}/{name}"), own, linked)?;
    }
    Ok(())
}

/// Copy one entry (a folder with its content) from `s` to the new name `d`.
fn copy_entry_to(
    t: &Tree,
    s: &str,
    d: &str,
    own: Option<(u32, u32)>,
    linked: &mut std::collections::HashMap<(u64, u64), String>,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let m = t.stat_checked(s)?;
    match m.ftype {
        fsutil::FType::Dir => {
            t.mkdir_private(d, own)?;
            copy_tree(t, s, d, own, linked)?;
            t.copy_directory_metadata(s, d)?;
        }
        fsutil::FType::Symlink => {
            let target = t.read_link(s)?;
            t.symlink(d, &target)?;
            if let Some((u, g)) = own {
                t.chown(d, u, g)?;
            }
            t.copy_symlink_xattrs(s, d)?;
        }
        fsutil::FType::File => {
            let src = t.open_read(s)?;
            let meta = src.metadata()?;
            let inode = (meta.dev(), meta.ino());
            if let Some(first) = linked.get(&inode) {
                t.hard_link(first, d)?;
                return Ok(());
            }
            #[cfg(target_os = "macos")]
            let cloned = t.clone_new_file(&src, d)?;
            #[cfg(not(target_os = "macos"))]
            let cloned: Option<std::fs::File> = None;
            let (dst, cloned) = match cloned {
                Some(dst) => (dst, true),
                None => (t.create(d, m.mode)?, false),
            };
            if !cloned && btrfs::clone_file(&src, &dst).is_err() {
                std::io::copy(&mut &src, &mut &dst)?;
            }
            fsutil::copy_metadata(&src, &dst)?;
            if let Some((u, g)) = own {
                t.chown(d, u, g)?;
            }
            if meta.nlink() > 1 {
                linked.insert(inode, d.to_string());
            }
        }
        fsutil::FType::Special => anyhow::bail!("cannot copy special cache entry {s}"),
    }
    Ok(())
}

/// What a folder looked like: the signature of every entry below it, by relative path.
type Manifest = BTreeMap<String, crate::tree::Signature>;

fn scan_tree(t: &Tree, dir: &str, rel: &str, out: &mut Manifest) -> Result<()> {
    for name in t.list_strict(dir)? {
        let (s, r) = (format!("{dir}/{name}"), if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") });
        let sig = t.signature(&s)?;
        let is_dir = sig.is_dir();
        out.insert(r.clone(), sig);
        if is_dir {
            scan_tree(t, &s, &r, out)?;
        }
    }
    Ok(())
}

/// Rounds to carry late writes of other processes from the old folder to the new one.
const CARRY_ROUNDS: usize = 5;

#[cfg(test)]
thread_local! {
    static COPY_HOOK: std::cell::RefCell<Option<Box<dyn FnMut()>>> = const { std::cell::RefCell::new(None) };
    static CARRY_HOOK: std::cell::RefCell<Option<Box<dyn FnMut(usize, &str)>>> = const { std::cell::RefCell::new(None) };
}

/// The old folder is renamed away: new opens go to the new folder, but a process that had a
/// file open, or wrote between the copy and the rename, changed the old one. Carry each entry
/// that is new or changed since `seen` (the state before the copy) to the new folder, in rounds,
/// until a scan finds nothing changed. Entries removed in the old folder stay in the new one.
/// Returns whether the old folder is settled: only then may it be deleted.
fn carry_changes(
    t: &Tree,
    old: &str,
    new: &str,
    own: Option<(u32, u32)>,
    mut seen: Manifest,
    carried: &mut std::collections::BTreeSet<String>,
) -> Result<bool> {
    for round in 0..=CARRY_ROUNDS {
        #[cfg(test)]
        CARRY_HOOK.with(|h| h.borrow_mut().as_mut().map(|f| f(round, old)));
        let mut now = Manifest::new();
        scan_tree(t, old, "", &mut now)?;
        let changed: Vec<&String> = now.iter().filter(|(k, v)| seen.get(*k) != Some(*v)).map(|(k, _)| k).collect();
        if changed.is_empty() {
            return Ok(true);
        }
        if round == CARRY_ROUNDS {
            return Ok(false);
        }
        let (mut linked, mut made) = (std::collections::HashMap::new(), Vec::new());
        for rel in changed {
            carried.insert(rel.clone());
            let (s, d) = (format!("{old}/{rel}"), format!("{new}/{rel}"));
            let is_dir = now[rel].is_dir();
            match t.stat(&d) {
                Some(m) if is_dir && m.is_dir() => continue,
                Some(_) => t.remove(&d)?,
                None => {}
            }
            if is_dir {
                t.mkdir_private(&d, own)?;
                made.push((s, d));
            } else {
                copy_entry_to(t, &s, &d, own, &mut linked)?;
            }
        }
        for (s, d) in made.iter().rev() {
            t.copy_directory_metadata(s, d)?;
        }
        seen = now;
    }
    Ok(false)
}

/// Turn a plain folder at a cache path back into a nested subvolume (after a tool deleted the
/// cache and made the folder again). Copies with reflinks: O(files) once.
pub fn convert_cache(w: &Path, path: &str, uid: u32, gid: u32) -> Result<bool> {
    let t = Tree::open(w)?;
    recover_cache_replacement(&t, path)?;
    let m = match t.stat(path) {
        Some(m) => m,
        None => {
            make_cache(w, path, uid, gid)?;
            return Ok(true);
        }
    };
    if !m.is_dir() || fsutil::is_layer(w, path, &m) {
        return Ok(false);
    }
    refuse_busy(w, path)?;
    replace_folder(&t, path, true, uid, gid)?;
    Ok(true)
}

/// Turn a cache (a nested subvolume) at `path` back into a plain folder with the same files, so
/// that states hold them again: the folder holds versioned files. Copies with reflinks.
pub fn uncache(w: &Path, path: &str, uid: u32, gid: u32) -> Result<bool> {
    let t = Tree::open(w)?;
    recover_cache_replacement(&t, path)?;
    match t.stat(path) {
        Some(m) if fsutil::is_layer(w, path, &m) => {}
        _ => return Ok(false),
    }
    refuse_busy(w, path)?;
    replace_folder(&t, path, false, uid, gid)?;
    Ok(true)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CacheReplacement {
    path: String,
    root: crate::tree::DirectoryRecord,
    source: crate::tree::DirectoryRecord,
    replacement: Option<crate::tree::DirectoryRecord>,
    /// The temporary copy and the moved source: unique names that only this operation made.
    tmp: String,
    old: String,
    /// The old folder was verified unchanged since its last copy: recovery may delete it.
    settled: bool,
}

const CACHE_JOURNAL_PREFIX: &str = ".layr-cache-replace-";

fn cache_journal(path: &str) -> String {
    use layr_hashers::Digest;
    format!("{CACHE_JOURNAL_PREFIX}{}.json", hex::encode(layr_hashers::ContentAddress::digest(path.as_bytes())))
}

fn cache_journal_name(name: &str) -> bool {
    let Some(name) = name.strip_prefix(CACHE_JOURNAL_PREFIX) else {
        return false;
    };
    let name = name.strip_suffix(".tmp").unwrap_or(name);
    name.strip_suffix(".json").is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn read_cache_replacement(t: &Tree, journal: &str) -> Result<CacheReplacement> {
    use std::io::Read;
    let file = t.open_read(journal)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.uid() == rustix::process::geteuid().as_raw()
            && metadata.mode() & 0o077 == 0
            && metadata.nlink() == 1
            && metadata.len() <= 4096,
        "invalid cache replacement journal: {journal}"
    );
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn recover_cache_replacement(t: &Tree, path: &str) -> Result<()> {
    let journal = cache_journal(path);
    if t.stat(&journal).is_none() {
        return Ok(());
    }
    let record = read_cache_replacement(t, &journal)?;
    let (old, temporary) = (record.old.clone(), record.tmp.clone());
    let root = t.directory_record("")?;
    anyhow::ensure!(
        record.path == path && root.device == record.root.device && root.inode == record.root.inode,
        "cache replacement journal belongs to another folder"
    );
    let Some(replacement) = &record.replacement else {
        anyhow::ensure!(
            t.matches_directory(path, &record.source) && t.stat(&old).is_none(),
            "cache source identity changed before the copy: {path}"
        );
        t.restore_directory(path, &record.source)?;
        if t.stat(&temporary).is_some() {
            t.remove(&temporary)?;
        }
        t.remove(&journal)?;
        return t.sync_parent(&journal);
    };
    if t.matches_directory(path, replacement) {
        t.restore_directory(path, replacement)?;
        if t.matches_directory(&old, &record.source) {
            t.restore_directory(&old, &record.source)?;
            if record.settled {
                t.remove(&old)?;
            } else {
                eprintln!(
                    "layr: {path} was written by another process while it was copied; the previous folder is kept at {old} ({})",
                    crate::cmd::admin::human_size(tree_bytes(&t.path.join(&old)))
                );
            }
        } else {
            anyhow::ensure!(t.stat(&old).is_none(), "cache backup identity changed: {old}");
        }
    } else {
        if !t.matches_directory(path, &record.source) {
            anyhow::ensure!(
                t.stat(path).is_none() && t.matches_directory(&old, &record.source),
                "cache source identity changed: {path}"
            );
            t.rename_directory_checked(&old, path, &record.source)?;
        }
        t.restore_directory(path, &record.source)?;
        if t.matches_directory(&temporary, replacement) {
            t.restore_directory(&temporary, replacement)?;
            t.remove(&temporary)?;
        } else if t.stat(&temporary).is_some_and(|metadata| metadata.ftype == fsutil::FType::Symlink) {
            t.remove(&temporary)?;
        } else {
            anyhow::ensure!(t.stat(&temporary).is_none(), "cache replacement identity changed: {temporary}");
        }
    }
    t.remove(&journal)?;
    t.sync_parent(&journal)
}

/// The caller holds the project lock. Read-only commands must not call recovery.
pub(crate) fn recover_cache_replacements(w: &Path) -> Result<()> {
    let t = Tree::open(w)?;
    for name in t.list("")? {
        if cache_journal_name(&name) && name.ends_with(".json") {
            let record = read_cache_replacement(&t, &name)?;
            anyhow::ensure!(cache_journal(&record.path) == name, "cache journal path does not match its name");
            recover_cache_replacement(&t, &record.path)?;
        }
    }
    Ok(())
}

/// Replace the folder at `path` with a copy of its files in a new subvolume or plain folder.
fn replace_folder(t: &Tree, path: &str, subvolume: bool, uid: u32, gid: u32) -> Result<()> {
    recover_cache_replacement(t, path)?;
    // Names that no user folder has. Only the journal proves a folder is ours.
    let id = crate::ids::short(&crate::ids::new_id());
    let (tmp, old) = (format!("{path}.layr-new-{id}"), format!("{path}.layr-old-{id}"));
    let journal = cache_journal(path);
    let mut record = CacheReplacement {
        path: path.to_string(),
        root: t.directory_record("")?,
        source: t.directory_record(path)?,
        replacement: None,
        tmp: tmp.clone(),
        old: old.clone(),
        settled: false,
    };
    t.publish_private_file(&journal, &serde_json::to_vec(&record)?)?;
    let copy = (|| {
        let mut seen = Manifest::new();
        scan_tree(t, path, "", &mut seen)?;
        if subvolume {
            t.create_subvolume(&tmp)?;
            t.chmod(&tmp, 0o700)?;
        } else {
            t.mkdir_private(&tmp, None)?;
        }
        #[cfg(test)]
        crate::tree::cache_test_fault("copy-incomplete");
        copy_tree(t, path, &tmp, Some((uid, gid)), &mut std::collections::HashMap::new())?;
        t.copy_directory_metadata(path, &tmp)?;
        #[cfg(test)]
        COPY_HOOK.with(|h| h.borrow_mut().as_mut().map(|f| f()));
        Ok(seen)
    })();
    let seen = match copy {
        Ok(seen) => seen,
        Err(error) => {
            recover_cache_replacement(t, path).with_context(|| format!("remove incomplete cache {tmp} after {error:#}"))?;
            return Err(error);
        }
    };
    record.replacement = Some(t.directory_record(&tmp)?);
    t.publish_private_file(&journal, &serde_json::to_vec(&record)?)?;
    // A write to the folder since the scan makes the copy incomplete: discard it, do not swap.
    match scan_differs(t, path, &seen) {
        Ok(false) => {}
        Ok(true) => {
            recover_cache_replacement(t, path)?;
            return Err(CacheRefused::Changed { path: path.to_string() }.into());
        }
        Err(error) => {
            recover_cache_replacement(t, path)?;
            return Err(error);
        }
    }
    #[cfg(test)]
    crate::tree::cache_test_pause();
    t.rename_directory_checked(path, &old, &record.source)?;
    #[cfg(test)]
    crate::tree::cache_test_fault("between-renames");
    // The copy before it becomes the cache: nobody writes to it by name until the next rename.
    let baseline = {
        let mut m = Manifest::new();
        scan_tree(t, &tmp, "", &mut m).ok().map(|()| m)
    };
    t.rename_directory_checked(&tmp, path, record.replacement.as_ref().unwrap())?;
    t.chown(path, uid, gid)?;
    let mut carried = std::collections::BTreeSet::new();
    // An error here (a name that is not UTF-8, a read-only folder) leaves the old folder in place.
    let settled = carry_changes(t, &old, path, Some((uid, gid)), seen, &mut carried).unwrap_or_else(|error| {
        eprintln!("layr: cannot carry late writes of {path}: {error:#}");
        false
    });
    if settled {
        record.settled = true;
        t.publish_private_file(&journal, &serde_json::to_vec(&record)?)?;
    } else if let Some(baseline) = baseline {
        // The old folder holds every write. The copy takes its place back when no process wrote to it.
        let mut now = Manifest::new();
        if scan_tree(t, path, "", &mut now).is_ok() && !writes_since(&baseline, &now, &carried) {
            return swap_back(t, path, &tmp, &old, &journal, &record);
        }
    }
    recover_cache_replacement(t, path)?;
    Ok(())
}

/// The old folder becomes the cache again, and the copy is deleted (the new folder got no write).
fn swap_back(t: &Tree, path: &str, tmp: &str, old: &str, journal: &str, record: &CacheReplacement) -> Result<()> {
    t.rename_directory_checked(path, tmp, record.replacement.as_ref().unwrap())?;
    t.rename_directory_checked(old, path, &record.source)?;
    t.remove(tmp)?;
    t.remove(journal)?;
    t.sync_parent(journal)
}

/// Whether the folder at `dir` differs from the scan `seen` (a write since the scan).
fn scan_differs(t: &Tree, dir: &str, seen: &Manifest) -> Result<bool> {
    let mut now = Manifest::new();
    scan_tree(t, dir, "", &mut now)?;
    Ok(now != *seen)
}

/// Whether an entry that carry did not write differs between two scans of the new folder.
fn writes_since(baseline: &Manifest, now: &Manifest, carried: &std::collections::BTreeSet<String>) -> bool {
    baseline.keys().chain(now.keys()).filter(|k| !carried.contains(*k)).any(|k| baseline.get(k) != now.get(k))
}

/// A cache folder that was not made a cache: a process uses it, or it was written while it was
/// copied. The automatic save skips it; `save --caches` and `overlay repair` report it.
#[derive(Debug)]
pub enum CacheRefused {
    Busy { path: String, pid: u32, name: String },
    Changed { path: String },
}

impl std::fmt::Display for CacheRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy { path, pid, name } => {
                write!(f, "{path}: not made a cache: in use by pid {pid} ({name}); run again when the build ends")
            }
            Self::Changed { path } => {
                write!(f, "{path}: not made a cache: written while it was copied; run again when the build ends")
            }
        }
    }
}

impl std::error::Error for CacheRefused {}

/// Refuse a folder that a process uses: a copy and a swap would lose the writes that come later.
fn refuse_busy(w: &Path, path: &str) -> Result<()> {
    if let Some(user) = crate::busy::user_of(&fsutil::safe_join(w, path)?)? {
        return Err(CacheRefused::Busy { path: path.to_string(), pid: user.pid, name: user.name }.into());
    }
    Ok(())
}

/// Allocated bytes of a folder tree (links are not followed).
pub fn tree_bytes(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let Ok(m) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !m.is_dir() {
        return m.blocks() * 512;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries.flatten().map(|e| tree_bytes(&e.path())).sum()
}

/// A folder that a cache swap left: `<cache path>.layr-old-<id>` (the previous folder, kept while
/// a process wrote to it) or `<cache path>.layr-new-<id>` (a copy that did not become the cache).
pub struct Leftover {
    pub line: String,
    /// The path in the working folder of the line.
    pub rel: String,
    pub path: PathBuf,
    pub bytes: u64,
    /// Why `gc --repair` keeps it: a process uses it, or the swap of its cache is not finished.
    pub kept: Option<String>,
}

/// The base name of a leftover: `<base>.layr-old-<id>` or `<base>.layr-new-<id>`, where `<id>` is the
/// short id that `replace_folder` makes (seven lowercase hex digits). `None` for any other name.
fn leftover_base(name: &str) -> Option<&str> {
    for sep in [".layr-old-", ".layr-new-"] {
        if let Some(pos) = name.rfind(sep) {
            let id = &name[pos + sep.len()..];
            if pos > 0 && id.len() == 7 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
                return Some(&name[..pos]);
            }
        }
    }
    None
}

/// Whether `name` is a leftover of the cache with the base name `base`.
fn is_leftover_name(name: &str, base: &str) -> bool {
    leftover_base(name) == Some(base)
}

/// Whether the tree at `root` has a file, link or other entry that is not a folder at `path` or
/// below it (a folder of a state that holds versioned files). A path below such an entry counts;
/// an error counts too, so that a folder whose files cannot be read never becomes a cache.
pub fn holds_files(root: &Path, path: &str) -> bool {
    let start = match fsutil::safe_join(root, path) {
        Ok(p) => p,
        Err(_) => return true,
    };
    match std::fs::symlink_metadata(&start) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    }
    let mut stack = vec![start];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return true,
        };
        for e in entries {
            match e.and_then(|e| Ok((e.path(), e.file_type()?))) {
                Ok((p, t)) if t.is_dir() => stack.push(p),
                _ => return true,
            }
        }
    }
    false
}

#[cfg(test)]
mod regenerable_walk_tests {
    use super::*;

    #[test]
    fn the_walk_finds_the_regenerable_folders_the_ignore_rules_ignore() -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-regen-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["a/target/debug", "a/src", "b/node_modules/pkg", "b/keep", "c/dist", "d/target", "e/build/target/x"] {
            std::fs::create_dir_all(root.join(d))?;
        }
        std::fs::write(root.join(".gitignore"), "target/\nnode_modules/\ndist/\nbuild/\n")?;
        std::fs::write(root.join(".layrattributes"), "target regenerable\n")?;
        std::fs::write(root.join("b/.layrattributes"), "node_modules regenerable\n")?;
        std::fs::write(root.join("c/.layrattributes"), "dist regenerable\n")?;
        // `build` is ignored by a nested .gitignore, so the target below it is no candidate.
        std::fs::write(root.join("e/.gitignore"), "build/\n")?;
        let found = regenerable_by_walk(&root)?;
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(found, vec!["a/target", "b/node_modules", "c/dist", "d/target"]);
        Ok(())
    }
}

#[cfg(test)]
mod regenerable_memo_tests {
    use super::*;

    #[test]
    fn a_folder_the_walk_cannot_read_is_an_error_not_a_short_list() -> Result<()> {
        // Root reads every folder: the test needs a folder that its owner cannot open.
        if rustix::process::geteuid().is_root() {
            return Ok(());
        }
        use std::os::unix::fs::PermissionsExt;
        // A folder of its own: a leftover of another run or test must not be shared.
        let root = std::env::temp_dir().join(format!("layr-regen-unreadable-{}", crate::ids::new_id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("locked/target"))?;
        std::fs::create_dir_all(root.join("a/target"))?;
        std::fs::write(root.join(".gitignore"), "target/\n")?;
        std::fs::write(root.join(".layrattributes"), "target regenerable\n")?;
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o000))?;
        let found = regenerable_by_walk(&root);
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o755))?;
        let _ = std::fs::remove_dir_all(&root);
        assert!(found.is_err(), "the walk returned a short list: {found:?}");
        Ok(())
    }

    fn a_new_command_does_not_read_the_list_of_an_earlier_command() -> Result<()> {
        let w = Path::new("/layr-test/regenerable-memo/w");
        set_regenerable_memo(w, vec!["target".to_string()]);
        assert!(regenerable_memo_is_set());
        // The same path gives the list it holds, with no walk, inside one command.
        assert_eq!(regenerable_memo(w)?, vec!["target".to_string()]);
        // The dispatcher (`cmd::run`, `service::run_as`) starts every command with `begin_command`.
        begin_command();
        assert!(!regenerable_memo_is_set());
        Ok(())
    }
}

#[cfg(test)]
mod cache_copy_tests {
    use super::*;
    use std::os::unix::{fs::PermissionsExt, net::UnixListener};

    fn cache_command_context(root: &Path) -> Result<crate::ctx::Ctx> {
        Ok(crate::ctx::Ctx {
            store: crate::store::Store::open(&root.join("store"))?,
            caller: crate::ctx::Caller::from_uid(rustix::process::geteuid().as_raw()),
            cwd: root.join("store/projects/concurrent/lines/main"),
            env: std::collections::HashMap::from([
                ("LAYR_PROJECT".into(), "concurrent".into()),
                ("LAYR_LINE".into(), "main".into()),
            ]),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
        })
    }

    #[test]
    fn a_line_lock_taken_again_in_the_same_process_does_not_wait() -> Result<()> {
        // The cache hook takes the line lock of its line while a command may hold it: the second take must
        // return at once. A wait would hang the thread, and the main thread then reports it.
        let root = std::env::temp_dir().join(format!("layr-line-lock-again-{}", crate::ids::new_id()));
        std::fs::create_dir(&root)?;
        #[cfg(target_os = "linux")]
        if !btrfs::is_supported(&root) {
            std::fs::remove_dir(&root)?;
            return Ok(());
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_root = root.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<()> {
                let ctx = cache_command_context(&worker_root)?;
                let run = |ctx: &crate::ctx::Ctx, args: &[&str]| {
                    crate::cmd::run(ctx, &args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                };
                anyhow::ensure!(run(&ctx, &["init", "concurrent", "-q"])? == 0, "init failed");
                let repo = Repo::open(&ctx, "concurrent")?;
                let line = repo.line("main")?;
                let first = repo.line_lock(&line)?;
                let second = repo.p.line_lock(&line.id, &line.name)?;
                drop(second);
                drop(first);
                Ok(())
            })();
            let _ = tx.send(result.map_err(|e| format!("{e:#}")));
        });
        let outcome = rx.recv_timeout(std::time::Duration::from_secs(60));
        let _ = std::fs::remove_dir_all(&root);
        match outcome {
            Ok(result) => result.map_err(anyhow::Error::msg),
            Err(_) => anyhow::bail!("the second take of the line lock waited"),
        }
    }

    #[test]
    fn overlay_list_does_not_recover_an_active_replacement() -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-cache-list-{}", crate::ids::new_id()));
        std::fs::create_dir(&root)?;
        #[cfg(target_os = "linux")]
        if !btrfs::is_supported(&root) {
            std::fs::remove_dir(&root)?;
            return Ok(());
        }
        let mut ctx = cache_command_context(&root)?;
        ctx.cwd = root.clone();
        let run =
            |ctx: &crate::ctx::Ctx, args: &[&str]| crate::cmd::run(ctx, &args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(run(&ctx, &["init", "concurrent", "-q"])?, 0);
        ctx.cwd = root.join("store/projects/concurrent/lines/main");
        std::fs::write(ctx.cwd.join(".layrattributes"), "cache regenerable\n")?;
        assert_eq!(
            run(
                &ctx,
                &[
                    "overlay",
                    "create",
                    "cache.cache",
                    "--folder",
                    "cache",
                    "--history",
                    "last",
                    "--quiet",
                    "--merge",
                    "union-theirs",
                    "-q"
                ]
            )?,
            0
        );
        std::fs::write(ctx.cwd.join(".gitignore"), "cache/\n")?;
        std::fs::create_dir(ctx.cwd.join("cache"))?;
        std::fs::write(ctx.cwd.join("cache/data"), "original")?;
        std::fs::set_permissions(ctx.cwd.join("cache"), std::fs::Permissions::from_mode(0o555))?;
        let mut child = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "repo::cache_copy_tests::cache_replacement_fault_child", "--nocapture"])
            .env("LAYR_TEST_CACHE_ROOT", &root)
            .env("LAYR_TEST_CACHE_PAUSE", &root)
            .spawn()?;
        let start = std::time::Instant::now();
        while !root.join("ready").exists() {
            assert!(child.try_wait()?.is_none(), "overlay repair stopped before the pause");
            assert!(start.elapsed() < std::time::Duration::from_secs(30));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let journal = ctx.cwd.join(cache_journal("cache"));
        let before = std::fs::read(&journal)?;
        let staging_path = std::fs::read_dir(&ctx.cwd)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("cache.layr-new-")))
            .expect("the staging folder exists during the pause");
        let staging = std::fs::symlink_metadata(&staging_path)?;
        let listed = run(&ctx, &["overlay", "list"]);
        let after = std::fs::read(&journal);
        let same_staging =
            std::fs::symlink_metadata(&staging_path).is_ok_and(|m| m.dev() == staging.dev() && m.ino() == staging.ino());
        std::fs::write(root.join("resume"), "resume")?;
        let repaired = child.wait()?;
        assert_eq!(listed?, 0);
        assert_eq!(after?, before);
        assert!(same_staging);
        assert!(repaired.success());
        assert_eq!(run(&ctx, &["overlay", "repair", "cache.cache"])?, 0);
        assert_eq!(std::fs::read_to_string(ctx.cwd.join("cache/data"))?, "original");
        assert_eq!(std::fs::metadata(ctx.cwd.join("cache"))?.mode() & 0o777, 0o555);
        drop(ctx);
        btrfs::delete_tree(&root.join("store")).context("remove the concurrent cache test store")?;
        let tree = Tree::open(&root)?;
        for name in tree.list("")? {
            tree.remove(&name)?;
        }
        std::fs::remove_dir(root)?;
        Ok(())
    }

    #[test]
    fn cache_replacement_fault_child() -> Result<()> {
        let Some(root) = std::env::var_os("LAYR_TEST_CACHE_ROOT") else {
            return Ok(());
        };
        let root = std::path::PathBuf::from(root);
        if std::env::var_os("LAYR_TEST_CACHE_PAUSE").is_some() {
            let ctx = cache_command_context(&root)?;
            assert_eq!(crate::cmd::run(&ctx, &["overlay".into(), "repair".into(), "cache.cache".into()])?, 0);
            return Ok(());
        }
        let tree = Tree::open(&root)?;
        let result =
            replace_folder(&tree, "cache", false, rustix::process::geteuid().as_raw(), rustix::process::getegid().as_raw());
        let error = format!("{:#}", result.expect_err("the injected fault must refuse the operation"));
        let expected = if std::env::var("LAYR_TEST_CACHE_FAULT").as_deref() == Ok("restore-error") {
            "restore the renamed directory mode"
        } else {
            "cache directory changed during rename"
        };
        assert!(error.contains(expected), "{error}");
        Ok(())
    }

    #[test]
    fn cache_replacement_recovers_process_crashes_and_refuses_name_swaps() -> Result<()> {
        use std::process::Command;
        for phase in
            ["copy-incomplete", "source-writable", "between-renames", "replacement-writable", "restore-error", "symlink-swap"]
        {
            let root = std::env::temp_dir().join(format!("layr-cache-crash-{}", crate::ids::new_id()));
            std::fs::create_dir_all(root.join("cache/nested"))?;
            std::fs::create_dir(root.join("outside"))?;
            std::fs::write(root.join("cache/nested/data"), "original")?;
            std::fs::write(root.join("outside/data"), "outside")?;
            std::fs::set_permissions(root.join("cache/nested/data"), std::fs::Permissions::from_mode(0o444))?;
            std::fs::set_permissions(root.join("cache/nested"), std::fs::Permissions::from_mode(0o555))?;
            std::fs::set_permissions(root.join("cache"), std::fs::Permissions::from_mode(0o555))?;
            let status = Command::new(std::env::current_exe()?)
                .args(["--exact", "repo::cache_copy_tests::cache_replacement_fault_child", "--nocapture"])
                .env("LAYR_TEST_CACHE_ROOT", &root)
                .env("LAYR_TEST_CACHE_FAULT", phase)
                .status()?;
            if phase.ends_with("writable") || matches!(phase, "between-renames" | "copy-incomplete") {
                assert_eq!(status.code(), Some(93), "{phase}");
            } else {
                assert!(status.success(), "{phase}");
            }
            recover_cache_replacements(&root)?;
            assert!(!std::fs::symlink_metadata(root.join("cache"))?.file_type().is_symlink());
            assert_eq!(std::fs::read_to_string(root.join("cache/nested/data"))?, "original", "{phase}");
            assert_eq!(std::fs::read_to_string(root.join("outside/data"))?, "outside", "{phase}");
            for directory in ["cache", "cache/nested"] {
                assert_eq!(std::fs::metadata(root.join(directory))?.permissions().mode() & 0o777, 0o555, "{phase}");
            }
            let tree = Tree::open(&root)?;
            for name in tree.list("")? {
                tree.remove(&name)?;
            }
            std::fs::remove_dir(root)?;
        }
        Ok(())
    }

    #[test]
    fn interrupted_cache_replacement_recovers_every_recorded_boundary() -> Result<()> {
        for phase in ["prepared", "source-writable", "source-moved", "replacement-writable", "replacement-moved"] {
            let root = std::env::temp_dir().join(format!("layr-cache-recovery-{}", crate::ids::new_id()));
            std::fs::create_dir_all(root.join("cache"))?;
            std::fs::create_dir(root.join("cache.layr-new-t"))?;
            std::fs::write(root.join("cache/data"), "original")?;
            std::fs::write(root.join("cache.layr-new-t/data"), "original")?;
            std::fs::set_permissions(root.join("cache"), std::fs::Permissions::from_mode(0o555))?;
            std::fs::set_permissions(root.join("cache.layr-new-t"), std::fs::Permissions::from_mode(0o555))?;
            let tree = Tree::open(&root)?;
            let record = CacheReplacement {
                path: "cache".into(),
                root: tree.directory_record("")?,
                source: tree.directory_record("cache")?,
                replacement: Some(tree.directory_record("cache.layr-new-t")?),
                tmp: "cache.layr-new-t".into(),
                old: "cache.layr-old-t".into(),
                settled: true,
            };
            tree.publish_private_file(&cache_journal("cache"), &serde_json::to_vec(&record)?)?;
            if phase != "prepared" {
                tree.chmod("cache", 0o755)?;
            }
            if matches!(phase, "source-moved" | "replacement-writable" | "replacement-moved") {
                tree.rename("cache", "cache.layr-old-t")?;
            }
            if matches!(phase, "replacement-writable" | "replacement-moved") {
                tree.chmod("cache.layr-new-t", 0o755)?;
            }
            if phase == "replacement-moved" {
                tree.rename("cache.layr-new-t", "cache")?;
            }
            recover_cache_replacements(&root)?;
            assert_eq!(std::fs::read_to_string(root.join("cache/data"))?, "original", "{phase}");
            assert_eq!(std::fs::metadata(root.join("cache"))?.permissions().mode() & 0o777, 0o555, "{phase}");
            assert!(!root.join(cache_journal("cache")).exists(), "{phase}: journal");
            for suffix in ["layr-old-t", "layr-new-t"] {
                assert!(!root.join(format!("cache.{suffix}")).exists(), "{phase}: {suffix}");
            }
            tree.remove("cache")?;
            std::fs::remove_dir_all(root)?;
        }
        Ok(())
    }

    fn scratch_cache(name: &str) -> Result<std::path::PathBuf> {
        let root = std::env::temp_dir().join(format!("layr-cache-{name}-{}", crate::ids::new_id()));
        std::fs::create_dir_all(root.join("cache/sub"))?;
        std::fs::write(root.join("cache/a"), "one")?;
        std::fs::write(root.join("cache/sub/b"), "two")?;
        Ok(root)
    }

    fn replace_plain(root: &Path) -> Result<()> {
        let tree = Tree::open(root)?;
        replace_folder(&tree, "cache", false, rustix::process::geteuid().as_raw(), rustix::process::getegid().as_raw())
    }

    fn siblings(root: &Path, prefix: &str) -> Vec<String> {
        let mut found: Vec<String> = std::fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(prefix))
            .collect();
        found.sort();
        found
    }

    #[test]
    fn replacement_never_touches_foreign_siblings() -> Result<()> {
        let root = scratch_cache("foreign")?;
        for name in ["cache.layr-old", "cache.layr-new"] {
            std::fs::create_dir(root.join(name))?;
            std::fs::write(root.join(name).join("precious"), name)?;
        }
        replace_plain(&root)?;
        recover_cache_replacements(&root)?;
        for name in ["cache.layr-old", "cache.layr-new"] {
            assert_eq!(std::fs::read_to_string(root.join(name).join("precious"))?, name);
        }
        assert_eq!(std::fs::read_to_string(root.join("cache/a"))?, "one");
        assert_eq!(siblings(&root, "cache.layr-"), ["cache.layr-new", "cache.layr-old"]);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn late_writes_to_the_old_folder_are_carried_over() -> Result<()> {
        let root = scratch_cache("carry")?;
        CARRY_HOOK.with(|h| {
            let base = root.clone();
            *h.borrow_mut() = Some(Box::new(move |round, old| {
                if round == 0 {
                    std::fs::write(base.join(old).join("a"), "one, then a late write").unwrap();
                    std::fs::write(base.join(old).join("sub/late"), "new file").unwrap();
                    std::fs::create_dir(base.join(old).join("fresh")).unwrap();
                    std::fs::write(base.join(old).join("fresh/c"), "in a new folder").unwrap();
                }
            }));
        });
        let result = replace_plain(&root);
        CARRY_HOOK.with(|h| *h.borrow_mut() = None);
        result?;
        assert_eq!(std::fs::read_to_string(root.join("cache/a"))?, "one, then a late write");
        assert_eq!(std::fs::read_to_string(root.join("cache/sub/late"))?, "new file");
        assert_eq!(std::fs::read_to_string(root.join("cache/fresh/c"))?, "in a new folder");
        assert_eq!(std::fs::read_to_string(root.join("cache/sub/b"))?, "two");
        assert_eq!(siblings(&root, "cache.layr-"), Vec::<String>::new());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn a_write_during_the_copy_discards_the_copy() -> Result<()> {
        let root = scratch_cache("copy-write")?;
        let base = root.clone();
        COPY_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || std::fs::write(base.join("cache/a"), "during the copy").unwrap()));
        });
        let result = replace_plain(&root);
        COPY_HOOK.with(|h| *h.borrow_mut() = None);
        let err = result.expect_err("a copy that a write changed is refused");
        assert!(matches!(err.downcast_ref::<CacheRefused>(), Some(CacheRefused::Changed { .. })), "{err:#}");
        assert_eq!(std::fs::read_to_string(root.join("cache/a"))?, "during the copy");
        assert_eq!(siblings(&root, "cache.layr-"), Vec::<String>::new());
        assert!(!root.join(cache_journal("cache")).exists());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn an_old_folder_that_keeps_changing_is_kept() -> Result<()> {
        let root = scratch_cache("unsettled")?;
        CARRY_HOOK.with(|h| {
            let base = root.clone();
            *h.borrow_mut() = Some(Box::new(move |round, old| {
                std::fs::write(base.join(old).join("a"), "x".repeat(round + 10)).unwrap();
                // A process that opened the new folder by its name writes there: the old folder cannot take its place.
                std::fs::write(base.join("cache").join("by-name"), round.to_string()).unwrap();
            }));
        });
        let result = replace_plain(&root);
        CARRY_HOOK.with(|h| *h.borrow_mut() = None);
        result?;
        let kept = siblings(&root, "cache.layr-old-");
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(std::fs::read_to_string(root.join(&kept[0]).join("a"))?, "x".repeat(CARRY_ROUNDS + 10));
        assert_eq!(std::fs::read_to_string(root.join("cache/sub/b"))?, "two");
        assert!(!root.join(cache_journal("cache")).exists());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn an_old_folder_that_keeps_changing_is_swapped_back_when_the_copy_got_no_write() -> Result<()> {
        let root = scratch_cache("swapped-back")?;
        CARRY_HOOK.with(|h| {
            let base = root.clone();
            *h.borrow_mut() = Some(Box::new(move |round, old| {
                std::fs::write(base.join(old).join("a"), "x".repeat(round + 10)).unwrap();
            }));
        });
        let result = replace_plain(&root);
        CARRY_HOOK.with(|h| *h.borrow_mut() = None);
        result?;
        // The old folder holds every write: it is the cache again, and no folder is left beside it.
        assert_eq!(siblings(&root, "cache.layr-"), Vec::<String>::new());
        assert_eq!(std::fs::read_to_string(root.join("cache/a"))?, "x".repeat(CARRY_ROUNDS + 10));
        assert_eq!(std::fs::read_to_string(root.join("cache/sub/b"))?, "two");
        assert!(!root.join(cache_journal("cache")).exists());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn a_name_that_is_not_utf8_refuses_the_copy_and_changes_nothing() -> Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let root = scratch_cache("utf8")?;
        let odd = root.join("cache").join(std::ffi::OsStr::from_bytes(b"bad\xff"));
        if std::fs::write(&odd, "odd").is_err() {
            // APFS refuses such names.
            std::fs::remove_dir_all(root)?;
            return Ok(());
        }
        std::fs::write(root.join("cache").join("bad\u{fffd}"), "replacement char")?;
        assert!(replace_plain(&root).is_err());
        assert_eq!(std::fs::read(&odd)?, b"odd");
        assert_eq!(std::fs::read_to_string(root.join("cache/bad\u{fffd}"))?, "replacement char");
        assert_eq!(siblings(&root, "cache.layr-"), Vec::<String>::new());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn cache_directory_rename_restores_readonly_modes() -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-cache-rename-{}", crate::ids::new_id()));
        std::fs::create_dir_all(root.join("source"))?;
        std::fs::create_dir_all(root.join("occupied/child"))?;
        std::fs::set_permissions(root.join("source"), std::fs::Permissions::from_mode(0o555))?;
        let tree = Tree::open(&root)?;
        assert!(tree.rename_directory("source", "occupied").is_err());
        assert_eq!(std::fs::metadata(root.join("source"))?.permissions().mode() & 0o777, 0o555);
        tree.rename_directory("source", "destination")?;
        assert_eq!(std::fs::metadata(root.join("destination"))?.permissions().mode() & 0o777, 0o555);
        tree.remove("destination")?;
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn incomplete_cache_directories_are_private() -> Result<()> {
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = Scratch(std::env::temp_dir().join(format!("layr-copy-{}", crate::ids::short(&crate::ids::new_id()))));
        std::fs::create_dir_all(root.0.join("source/private"))?;
        std::fs::set_permissions(root.0.join("source/private"), std::fs::Permissions::from_mode(0o700))?;
        std::fs::create_dir(root.0.join("destination"))?;
        let _socket = UnixListener::bind(root.0.join("source/private/socket"))?;
        let tree = Tree::open(&root.0)?;
        assert!(copy_tree(&tree, "source", "destination", None, &mut std::collections::HashMap::new()).is_err());
        assert_eq!(std::fs::metadata(root.0.join("destination/private"))?.permissions().mode() & 0o777, 0o700);
        Ok(())
    }
}

#[cfg(test)]
mod cache_platform_tests {
    use super::cache_fits;

    #[test]
    fn a_cache_warms_only_lines_of_its_platform() {
        // A Mac build in a Linux line: never, also after a move to this machine.
        assert!(!cache_fits(Some("macos-aarch64"), "linux-aarch64", false));
        assert!(!cache_fits(Some("macos-aarch64"), "linux-aarch64", true));
        assert!(!cache_fits(Some("linux-x86_64"), "linux-aarch64", false));
        assert!(cache_fits(Some("linux-aarch64"), "linux-aarch64", false));
        // An older record names no platform: only a cache saved on this machine.
        assert!(cache_fits(None, "linux-aarch64", true));
        assert!(!cache_fits(None, "linux-aarch64", false));
    }
    #[test]
    fn auxiliary_states_resolve_but_source_commands_refuse_them() {
        use super::*;
        use crate::ctx::Caller;
        use crate::model::{Actor, Author};
        use crate::store::{Project, Store};
        let dir = std::env::temp_dir().join(format!("layr-auxiliary-{}", ids::new_id()));
        let ctx = Ctx {
            store: Store::open(&dir).unwrap(),
            caller: Caller::from_uid(0),
            cwd: dir.clone(),
            env: std::collections::HashMap::from([("LAYR_PROJECT".into(), "p".into())]),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
        };
        let project_dir = ctx.store.project_dir("p").unwrap();
        std::fs::create_dir_all(project_dir.join("meta/db")).unwrap();
        std::fs::create_dir(project_dir.join("states")).unwrap();
        let id = ids::new_id();
        let p = Project {
            store: &ctx.store,
            name: "p".into(),
            dir: project_dir.clone(),
            id: id.clone(),
            db: crate::db::Db::open(&project_dir.join("meta/db/layr.sqlite")).unwrap(),
        };
        let mut rec = Record::new("project");
        rec.data = serde_json::json!({"id":id,"name":"p","config":{}});
        p.append(rec, &Actor { uid: 0, name: "root".into(), session: None }).unwrap();
        for kind in [StateKind::Cache, StateKind::Derived] {
            let id = ids::new_id();
            std::fs::create_dir(p.state_path(&id)).unwrap();
            let mut rec = Record::new("state");
            rec.states.push(StateRec {
                id: id.clone(),
                kind,
                change_id: None,
                derived_from: None,
                context: Some("test-v1".into()),
                parents: vec![],
                line: None,
                subvol: String::new(),
                author: Author::default(),
                message: String::new(),
                time: ids::now_ms(),
                running: false,
                conflicts: vec![],
                caches: Default::default(),
                cache_by: None,
                cache_shared: false,
                cache_platform: None,
                cache_of: None,
                cache_key_parts: Default::default(),
            });
            p.append(rec, &ctx.actor()).unwrap();
            let repo = Repo::open(&ctx, "p").unwrap();
            assert!(repo.state_root(&id).is_ok());
            assert_eq!(crate::revs::resolve(&repo, None, &id).unwrap(), id);
            for args in [vec!["line", "new", "rejected", &id], vec!["diff", &id, &id], vec!["log", &id], vec!["show", &id]] {
                let args: Vec<String> = args.into_iter().map(String::from).collect();
                let error = crate::cmd::run(&ctx, &args).unwrap_err().to_string();
                assert!(error.contains("outside source history"), "{args:?}: {error}");
            }
            assert!(crate::gitbridge::export_history(&repo, &id).unwrap_err().to_string().contains("outside source history"));
            assert!(repo.merge_base(&id, &id).unwrap_err().to_string().contains("outside source history"));
            let args = vec!["ref".into(), "set".into(), format!("caches/{}", crate::db::kind_str(kind)), id.clone()];
            assert_eq!(crate::cmd::run(&ctx, &args).unwrap(), 0);
        }
        drop(p);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

/// The head and the index content of a line (`Repo::index_token`).
#[cfg(target_os = "macos")]
pub type IndexToken = (String, Option<Vec<(String, crate::storage::Fingerprint)>>);

/// A status read before a store lock (`Repo::prepare_status`).
#[cfg(target_os = "macos")]
struct PreparedStatus {
    line: String,
    token: IndexToken,
    st: Status,
    scan: Temp,
}
#[cfg(target_os = "macos")]
thread_local! {
    static PREPARED_STATUS: std::cell::RefCell<Option<PreparedStatus>> = const { std::cell::RefCell::new(None) };
}

/// `save --caches` work made before the project lock (`Repo::prepare_caches_state`), APFS only.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub struct PreparedCaches {
    head: String,
    caches: Vec<String>,
    #[cfg(target_os = "macos")]
    scan: BTreeMap<String, btrfs::ScanEntry>,
    st: StateRec,
    same: bool,
}

/// The automatic state that `line delete` prepares before the lock (`prepare_final_state`).
#[cfg(target_os = "macos")]
pub struct FinalPrepared {
    id: String,
    head: String,
    scan: BTreeMap<String, btrfs::ScanEntry>,
}

/// Off APFS nothing is prepared.
#[cfg(not(target_os = "macos"))]
pub struct FinalPrepared;

/// The outcome of `record_final_state`.
pub enum FinalState {
    /// The state of the working folder (None: no folder).
    Recorded(Option<String>),
    /// The head or the folder moved since the state was prepared: prepare it again.
    Moved,
}

/// An automatic state layer written before a store lock (`Repo::prepare_save`).
#[cfg(target_os = "macos")]
struct PreparedSave {
    line: String,
    head: String,
    prev: String,
    scan: BTreeMap<String, btrfs::ScanEntry>,
    path: PathBuf,
}
#[cfg(target_os = "macos")]
thread_local! {
    static PREPARED_SAVES: std::cell::RefCell<Vec<PreparedSave>> = const { std::cell::RefCell::new(Vec::new()) };
}
/// The folder of a line that `prepare_save` found unchanged since its save base: head, base, scan.
#[cfg(target_os = "macos")]
struct PreparedClean {
    line: String,
    head: String,
    prev: String,
    scan: BTreeMap<String, btrfs::ScanEntry>,
}

#[cfg(target_os = "macos")]
thread_local! {
    static PREPARED_CLEAN: std::cell::RefCell<Vec<PreparedClean>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(target_os = "macos")]
fn set_prepared_clean(line: &str, head: &str, prev: &str, scan: &BTreeMap<String, btrfs::ScanEntry>) {
    PREPARED_CLEAN.with(|p| {
        let mut p = p.borrow_mut();
        p.retain(|x| x.line != line);
        p.push(PreparedClean { line: line.into(), head: head.into(), prev: prev.into(), scan: scan.clone() });
    });
}

/// True when `prepare_save` found this head, base and content unchanged (the lock need not compare
/// the stored metadata again). The entry is kept until the command ends.
#[cfg(target_os = "macos")]
fn prepared_unchanged(line: &str, head: &str, prev: &str, scan: &BTreeMap<String, btrfs::ScanEntry>) -> bool {
    PREPARED_CLEAN.with(|p| p.borrow().iter().any(|x| x.line == line && x.head == head && x.prev == prev && x.scan == *scan))
}

/// True when a layer prepared for `line` has this head, base and content (not taken).
#[cfg(target_os = "macos")]
fn prepared_matches(line: &str, head: &str, prev: &str, scan: &BTreeMap<String, btrfs::ScanEntry>) -> bool {
    PREPARED_SAVES.with(|p| p.borrow().iter().any(|x| x.line == line && x.head == head && x.prev == prev && x.scan == *scan))
}
#[cfg(target_os = "macos")]
fn take_prepared_save(line: &str) -> Option<PreparedSave> {
    PREPARED_SAVES.with(|p| {
        let mut p = p.borrow_mut();
        let i = p.iter().position(|x| x.line == line)?;
        Some(p.remove(i))
    })
}

#[cfg(test)]
mod commit_record_tests {
    use super::*;
    use crate::ctx::Caller;
    use crate::model::{Actor, Author};
    use crate::store::{Project, Store};

    /// COMMIT can fail after the commit point (a full disk). The record is then in the database,
    /// so its state must keep its snapshot: a present state without a snapshot is damage.
    #[test]
    fn a_state_that_the_database_names_keeps_its_snapshot_after_a_failed_commit() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("layr-commit-record-{}", ids::new_id()));
        let ctx = Ctx {
            store: Store::open(&dir)?,
            caller: Caller::from_uid(0),
            cwd: dir.clone(),
            env: std::collections::HashMap::from([("LAYR_PROJECT".into(), "p".into())]),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
        };
        let project_dir = ctx.store.project_dir("p").unwrap();
        std::fs::create_dir_all(project_dir.join("meta/db"))?;
        std::fs::create_dir(project_dir.join("states"))?;
        let id = ids::new_id();
        let p = Project {
            store: &ctx.store,
            name: "p".into(),
            dir: project_dir.clone(),
            id: id.clone(),
            db: crate::db::Db::open(&project_dir.join("meta/db/layr.sqlite"))?,
        };
        let mut rec = Record::new("project");
        rec.data = serde_json::json!({"id": id, "name": "p", "config": {}});
        p.append(rec, &Actor { uid: 0, name: "root".into(), session: None })?;
        let repo = Repo { p, ctx: None };
        let state = ids::new_id();
        std::fs::create_dir(repo.p.state_path(&state))?;
        let mut rec = Record::new("state");
        rec.states.push(StateRec {
            id: state.clone(),
            kind: StateKind::Auto,
            change_id: None,
            derived_from: None,
            context: None,
            parents: vec![],
            line: None,
            subvol: String::new(),
            author: Author::default(),
            message: String::new(),
            time: ids::now_ms(),
            running: false,
            conflicts: vec![],
            caches: Default::default(),
            cache_by: None,
            cache_shared: false,
            cache_platform: None,
            cache_of: None,
            cache_key_parts: Default::default(),
        });
        crate::db::FAIL_AFTER_COMMIT.with(|f| f.set(true));
        assert!(repo.commit_record(&ctx, rec).is_err());
        assert!(repo.p.db.state(&state)?.is_some(), "the database holds the record");
        assert!(repo.p.has_state(&state), "a state that the record names keeps its snapshot");
        Ok(())
    }
}

#[cfg(test)]
mod leftover_name_tests {
    use super::{is_leftover_name, leftover_base};

    #[test]
    fn the_base_of_a_swap_name_is_any_folder_name() {
        assert_eq!(leftover_base("target.layr-old-abc1234"), Some("target"));
        assert_eq!(leftover_base("a.b.layr-new-0123456"), Some("a.b"));
        assert_eq!(leftover_base(".layr-old-abc1234"), None);
        assert_eq!(leftover_base("target.mine"), None);
    }

    #[test]
    fn only_the_swap_names_of_a_cache_are_leftovers() {
        assert!(is_leftover_name("target.layr-old-abc1234", "target"));
        assert!(is_leftover_name("target.layr-new-0123456", "target"));
        assert!(!is_leftover_name("target.mine", "target"));
        assert!(!is_leftover_name("target.layr-old-abc123", "target"));
        assert!(!is_leftover_name("target.layr-old-ABC1234", "target"));
        assert!(!is_leftover_name("target.layr-old-abc1234x", "target"));
        assert!(!is_leftover_name("other.layr-old-abc1234", "target"));
    }
}

mod owner_walk_tests {
    use super::owner_walk_skippable;

    #[test]
    fn the_walk_is_skipped_only_when_every_condition_holds() {
        // The line owner (501) runs the store, saved the cache and builds in its copy.
        assert!(owner_walk_skippable(true, 501, 501, 20, Some(501), (501, 20)));
        // Another owner (502) saved the cache state: the walk runs.
        assert!(!owner_walk_skippable(true, 501, 501, 20, Some(502), (501, 20)));
        // The saving owner is not recorded: the walk runs.
        assert!(!owner_walk_skippable(true, 501, 501, 20, None, (501, 20)));
        // A root process clones and keeps each entry's own owner: the walk runs.
        assert!(!owner_walk_skippable(true, 0, 0, 20, Some(0), (0, 20)));
        // The store runs as another user than the line owner: the walk runs.
        assert!(!owner_walk_skippable(true, 502, 501, 20, Some(501), (501, 20)));
        // A copy made file by file: the walk runs.
        assert!(!owner_walk_skippable(false, 501, 501, 20, Some(501), (501, 20)));
        // The root has another group than the line: the walk runs.
        assert!(!owner_walk_skippable(true, 501, 501, 20, Some(501), (501, 21)));
    }
}
